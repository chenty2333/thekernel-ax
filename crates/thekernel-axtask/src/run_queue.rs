use alloc::sync::Arc;
#[cfg(feature = "smp")]
use alloc::sync::Weak;
use core::{
    fmt,
    future::poll_fn,
    sync::atomic::{AtomicPtr, AtomicU64, Ordering},
    task::{Context, Poll},
};

use axhal::{mem::total_ram_size, percpu::this_cpu_id};
#[cfg(feature = "sched-cfs")]
use axsched::CfsTaskReservation;
use axsched::{BaseScheduler, EnqueueReason, SchedulerError};
use futures_util::task::AtomicWaker;
use kernel_guard::BaseGuard;
use kspin::SpinRaw;
use lazyinit::LazyInit;

#[cfg(feature = "smp")]
use crate::task::{CpuHandoffCompletion, MigrationClaim, WakeHandoffPublication};
use crate::{
    AxCpuMask, AxTask, AxTaskRef, Scheduler, TaskInner,
    future::block_on,
    task::{
        BlockWaitClaim, BlockWaitCommit, BlockWaitToken, CurrentTask, TaskCreateError,
        TaskExitQueueFault, TaskStack, TaskState, TaskWakeFault,
    },
};

macro_rules! percpu_static {
    ($(
        $(#[$comment:meta])*
        $name:ident: $ty:ty = $init:expr
    ),* $(,)?) => {
        $(
            $(#[$comment])*
            #[percpu::def_percpu]
            static $name: $ty = $init;
        )*
    };
}

/// Per-CPU allocation-free FIFO of exited tasks.
///
/// Each non-null pointer represents exactly one strong `Arc<AxTask>` owned by
/// this queue. The successor link lives in `TaskInner`, so task exit never
/// grows a secondary heap container. Access is confined to the current CPU
/// with IRQs and preemption excluded by the run-queue lifecycle.
struct ExitedTaskQueue {
    head: *mut AxTask,
    tail: *mut AxTask,
    len: usize,
}

// Safety: the raw pointers are owned Arc units, not borrowed task references.
// The per-CPU API never transfers the queue between CPUs while it is live.
unsafe impl Send for ExitedTaskQueue {}

struct ExitedTaskEnqueueError {
    fault: TaskExitQueueFault,
    task: AxTaskRef,
}

struct ExitedTaskDequeue {
    task: AxTaskRef,
    fault: Option<TaskExitQueueFault>,
}

impl fmt::Debug for ExitedTaskEnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExitedTaskEnqueueError")
            .field("fault", &self.fault)
            .field("task_id", &self.task.id())
            .finish()
    }
}

impl ExitedTaskQueue {
    const fn new() -> Self {
        Self {
            head: core::ptr::null_mut(),
            tail: core::ptr::null_mut(),
            len: 0,
        }
    }

    #[cfg(test)]
    const fn len(&self) -> usize {
        self.len
    }

    const fn is_empty(&self) -> bool {
        self.head.is_null() && self.tail.is_null()
    }

    fn reclaim_len(&self) -> usize {
        if self.is_empty() { 0 } else { self.len.max(1) }
    }

    fn push_back(&mut self, task: AxTaskRef) -> Result<(), ExitedTaskEnqueueError> {
        let empty = self.head.is_null();
        if empty != self.tail.is_null() || empty != (self.len == 0) {
            let fault = TaskExitQueueFault::CorruptLink;
            task.record_exit_queue_fault(fault);
            return Err(ExitedTaskEnqueueError { fault, task });
        }

        let Some(next_len) = self.len.checked_add(1) else {
            let fault = TaskExitQueueFault::LengthExhausted;
            task.record_exit_queue_fault(fault);
            return Err(ExitedTaskEnqueueError { fault, task });
        };

        if let Err(fault) = task.admit_exit_queue() {
            return Err(ExitedTaskEnqueueError { fault, task });
        }

        let raw = Arc::into_raw(task).cast_mut();
        if empty {
            self.head = raw;
            self.tail = raw;
            self.len = next_len;
            return Ok(());
        }

        // Safety: a non-empty queue owns one raw Arc for `tail`, and only the
        // current CPU mutates its embedded successor while it remains queued.
        let tail = unsafe { &*self.tail };
        if let Err(fault) = tail.link_exit_queue_successor(raw) {
            // Safety: the new raw pointer was not linked after the failed CAS,
            // so it still represents exactly the Arc passed to this method.
            let task = unsafe { Arc::from_raw(raw) };
            task.rollback_exit_queue_admission();
            task.record_exit_queue_fault(fault);
            return Err(ExitedTaskEnqueueError { fault, task });
        }

        self.tail = raw;
        self.len = next_len;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<ExitedTaskDequeue> {
        let mut raw = self.head;
        let mut fault = None;
        if raw.is_null() {
            if self.tail.is_null() {
                self.len = 0;
                return None;
            }
            // Salvage the queue-owned tail Arc rather than losing it with the
            // inconsistent head metadata. No traversal or allocation occurs.
            raw = self.tail;
            self.head = raw;
            self.len = 1;
            // Safety: even with inconsistent topology, `tail` is still a raw
            // Arc owned by this queue.
            unsafe { &*raw }.record_exit_queue_fault(TaskExitQueueFault::CorruptLink);
            fault = Some(TaskExitQueueFault::CorruptLink);
        }

        // Safety: `head` is one queue-owned Arc unit and remains live until it
        // is reconstructed below. Its embedded link is exclusively ours.
        let task = unsafe { &*raw };
        let next = task.take_exit_queue_successor();
        if next.is_null() {
            if self.tail != raw || self.len != 1 {
                task.record_exit_queue_fault(TaskExitQueueFault::CorruptLink);
                fault = Some(TaskExitQueueFault::CorruptLink);
            }
            self.head = core::ptr::null_mut();
            self.tail = core::ptr::null_mut();
            self.len = 0;
        } else {
            if self.tail == raw || self.len <= 1 {
                task.record_exit_queue_fault(TaskExitQueueFault::CorruptLink);
                fault = Some(TaskExitQueueFault::CorruptLink);
            }
            self.head = next;
            // Keep at least one accounted node because `next` is non-null.
            self.len = self.len.saturating_sub(1).max(1);
        }
        if let Err(error) = task.finish_exit_dequeue() {
            fault.get_or_insert(error);
        }

        // Safety: removing `head` transfers its one raw Arc ownership unit
        // from the queue back to the caller exactly once.
        Some(ExitedTaskDequeue {
            task: unsafe { Arc::from_raw(raw) },
            fault,
        })
    }
}

percpu_static! {
    RUN_QUEUE: LazyInit<AxRunQueue> = LazyInit::new(),
    EXITED_TASKS: ExitedTaskQueue = ExitedTaskQueue::new(),
    WAIT_FOR_EXIT: AtomicWaker = AtomicWaker::new(),
    STACK_CACHE: kspin::SpinNoIrq<PerCpuStackCache> = kspin::SpinNoIrq::new(PerCpuStackCache::new()),
    IDLE_TASK: LazyInit<AxTaskRef> = LazyInit::new(),
    /// Stores the weak reference to the previous task that is running on this CPU.
    #[cfg(feature = "smp")]
    PREV_TASK: Weak<crate::AxTask> = Weak::new(),
}

const MIB: usize = 1024 * 1024;
static IDLE_TICKS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn idle_ticks() -> u64 {
    IDLE_TICKS.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct StackCacheKey {
    size: usize,
    align: usize,
}

struct StackCacheBucket {
    key: StackCacheKey,
    stack: TaskStack,
}

const STACK_CACHE_SLOTS: usize = 64;

struct PerCpuStackCache {
    cached_bytes: usize,
    budget_bytes: usize,
    slots: [Option<StackCacheBucket>; STACK_CACHE_SLOTS],
}

impl PerCpuStackCache {
    const fn new() -> Self {
        Self {
            cached_bytes: 0,
            budget_bytes: 0,
            slots: [const { None }; STACK_CACHE_SLOTS],
        }
    }

    fn take(&mut self, size: usize, align: usize) -> Option<TaskStack> {
        let key = StackCacheKey { size, align };
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|bucket| bucket.key == key))?;
        let bucket = slot.take()?;
        self.cached_bytes = self.cached_bytes.saturating_sub(size);
        Some(bucket.stack)
    }

    /// Returns the stack when it cannot be cached so its deallocation can occur
    /// after the per-CPU no-IRQ lock has been released.
    fn recycle(&mut self, mut stack: TaskStack) -> Option<TaskStack> {
        let size = stack.layout_size();
        let align = stack.layout_align();
        let budget = self.budget_bytes();
        if size == 0 || budget < size || self.cached_bytes > budget.saturating_sub(size) {
            return Some(stack);
        }

        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
            return Some(stack);
        };
        stack.scrub_for_cache();
        *slot = Some(StackCacheBucket {
            key: StackCacheKey { size, align },
            stack,
        });
        self.cached_bytes += size;
        None
    }

    fn budget_bytes(&mut self) -> usize {
        if self.budget_bytes == 0 {
            self.budget_bytes = per_cpu_stack_cache_budget_bytes();
        }
        self.budget_bytes
    }
}

fn system_stack_cache_budget_bytes() -> usize {
    let ram = total_ram_size();
    if ram <= 256 * MIB {
        0
    } else if ram <= 512 * MIB {
        4 * MIB
    } else if ram <= 2 * 1024 * MIB {
        32 * MIB
    } else {
        64 * MIB
    }
}

fn per_cpu_stack_cache_budget_bytes() -> usize {
    // Keep stack reuse lock-local, but avoid hoarding exited-task stacks on
    // low-memory guests where short-lived process bursts are common.
    let cpu_num = axhal::cpu_num().max(1);
    system_stack_cache_budget_bytes() / cpu_num
}

pub(crate) fn take_cached_task_stack(size: usize, align: usize) -> Option<TaskStack> {
    STACK_CACHE.with_current(|cache| cache.lock().take(size, align))
}

fn recycle_task_stack(stack: TaskStack) {
    let rejected = STACK_CACHE.with_current(|cache| cache.lock().recycle(stack));
    drop(rejected);
}

/// Published shared views of the per-CPU-owned run queues.
///
/// Each queue is owned for the lifetime of the kernel by its per-CPU
/// [`RUN_QUEUE`] slot. The registry only publishes immutable pointers after
/// initialization; all mutable scheduler state stays behind [`SpinRaw`]. It
/// therefore cannot manufacture aliased `&'static mut AxRunQueue` values.
static RUN_QUEUES: [AtomicPtr<AxRunQueue>; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; axconfig::plat::MAX_CPU_NUM];

/// Typed cause of a failed runnable-task publication.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskEnqueueErrorKind {
    /// The selected CPU has no initialized run queue.
    RunQueueUnavailable(usize),
    /// The scheduler rejected task ownership or ordering admission.
    Scheduler(SchedulerError),
    #[cfg(feature = "smp")]
    /// A remote context-switch handoff already contained an owned wake.
    HandoffOccupied,
    /// The submitted task was not in the unpublished Ready state.
    TaskNotReady,
}

/// Failed runnable-task publication with ownership returned to the caller.
///
/// No error variant represents partial publication. The scheduler/runqueue
/// locks have been released when this value is returned, and [`Self::into_task`]
/// recovers the exact task reference supplied to the operation for rollback or
/// terminal containment.
pub struct TaskEnqueueError {
    pub(crate) kind: TaskEnqueueErrorKind,
    pub(crate) task: AxTaskRef,
}

impl TaskEnqueueError {
    /// Returns the typed publication failure without consuming task ownership.
    pub const fn kind(&self) -> TaskEnqueueErrorKind {
        self.kind
    }

    /// Returns the unpublished or safely contained task.
    pub const fn task(&self) -> &AxTaskRef {
        &self.task
    }

    /// Recovers the exact task ownership returned by the failed publication.
    pub fn into_task(self) -> AxTaskRef {
        self.task
    }
}

impl fmt::Debug for TaskEnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskEnqueueError")
            .field("kind", &self.kind)
            .field("task_id", &self.task.id())
            .finish()
    }
}

impl fmt::Display for TaskEnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "task {} publication failed: {:?}",
            self.task.id().as_u64(),
            self.kind
        )
    }
}

impl core::error::Error for TaskEnqueueError {}

/// Reserved final publication of one new CFS task.
///
/// The token retains the exact permanent destination run queue and the
/// scheduler's private ownership/ordering reservation. It is not runnable
/// until [`crate::publish_prepared_task`] consumes this value. Dropping it
/// cancels scheduler admission without ever publishing the task.
#[cfg(feature = "sched-cfs")]
#[must_use = "dropping the token cancels runnable-task publication"]
pub struct PreparedTaskPublication {
    run_queue: &'static AxRunQueue,
    reservation: Option<CfsTaskReservation<TaskInner>>,
}

#[cfg(feature = "sched-cfs")]
impl PreparedTaskPublication {
    /// Returns the exact unpublished task held by this reservation.
    pub fn task(&self) -> &AxTaskRef {
        self.reservation
            .as_ref()
            .expect("live task publication always owns its reservation")
            .task()
    }

    /// Cancels publication and returns an owned reference to the task.
    pub fn cancel(self) -> AxTaskRef {
        let task = Arc::clone(self.task());
        drop(self);
        task
    }

    pub(crate) fn commit(mut self) -> AxTaskRef {
        let reservation = self
            .reservation
            .take()
            .expect("live task publication always owns its reservation");
        // The constructor stores the exact permanent run queue whose scheduler
        // created `reservation`; neither field is publicly mutable, and the
        // task-level mutation claim excludes parameter/affinity changes.
        match self
            .run_queue
            .scheduler
            .lock()
            .commit_reserved_task(reservation)
        {
            Ok(task) => {
                task.release_publication_mutation();
                task
            }
            Err(error) => {
                let kind = error.kind();
                let reservation = error.into_reservation();
                let task = Arc::clone(reservation.task());
                drop(reservation);
                task.release_publication_mutation();
                task.record_wake_fault(TaskWakeFault::SchedulerInvariant);
                error!(
                    "reserved task {} final publication invariant failed: {:?}",
                    task.id().as_u64(),
                    kind
                );
                // Lifecycle state may already be externally visible. Returning
                // or pretending publication succeeded could strand that state;
                // fail-stop after preserving the exact task and durable fault.
                axhal::power::system_off()
            }
        }
    }
}

#[cfg(feature = "sched-cfs")]
impl fmt::Debug for PreparedTaskPublication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedTaskPublication")
            .field("task_id", &self.task().id())
            .field("cpu_id", &self.run_queue.cpu_id)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "sched-cfs")]
impl Drop for PreparedTaskPublication {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.as_ref() {
            reservation.task().release_publication_mutation();
        }
    }
}

/// Failure to initialize one CPU's generic task runtime.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskRuntimeInitError {
    Task(TaskCreateError),
    Scheduler(SchedulerError),
    DuplicateCpu(usize),
}

/// Failure to update one task's runtime scheduling parameters.
///
/// This type preserves the mechanism-level cause so an OS personality can
/// map policy and errno without guessing what a legacy `false` meant.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskSchedError {
    /// Runtime updates are not implemented by the selected scheduler.
    Unsupported,
    /// The target task has already reached its terminal lifecycle state.
    TaskExited,
    /// The CPU recorded as owning the task has no published run queue.
    RunQueueUnavailable(usize),
    /// The scheduler rejected the atomic parameter transaction.
    Scheduler(SchedulerError),
}

impl From<SchedulerError> for TaskSchedError {
    fn from(error: SchedulerError) -> Self {
        match error {
            SchedulerError::UnsupportedOperation => Self::Unsupported,
            error => Self::Scheduler(error),
        }
    }
}

impl From<TaskCreateError> for TaskRuntimeInitError {
    fn from(error: TaskCreateError) -> Self {
        Self::Task(error)
    }
}

pub(crate) enum WakeTaskOutcome {
    Enqueued,
    #[cfg(feature = "smp")]
    Deferred,
    AlreadyRunnable,
    Rejected(TaskEnqueueError),
}

enum PutTaskOutcome {
    Enqueued,
    #[cfg(feature = "smp")]
    Deferred,
    StateMismatch,
    Rejected(TaskEnqueueError),
}

fn wake_fault_for(kind: TaskEnqueueErrorKind) -> TaskWakeFault {
    match kind {
        TaskEnqueueErrorKind::RunQueueUnavailable(_) => TaskWakeFault::RunQueueUnavailable,
        TaskEnqueueErrorKind::Scheduler(
            SchedulerError::IdentifierExhausted | SchedulerError::SequenceExhausted,
        ) => TaskWakeFault::SchedulerCapacity,
        #[cfg(feature = "smp")]
        TaskEnqueueErrorKind::HandoffOccupied => TaskWakeFault::HandoffCorrupt,
        TaskEnqueueErrorKind::Scheduler(_) | TaskEnqueueErrorKind::TaskNotReady => {
            TaskWakeFault::SchedulerInvariant
        }
    }
}

fn contain_enqueue_failure(error: &TaskEnqueueError, previous_state: TaskState) {
    let restored = error
        .task
        .transition_state(TaskState::Ready, previous_state);
    let recorded = error.task.record_wake_fault(wake_fault_for(error.kind));
    error!(
        "task {} enqueue containment: kind={:?}, state_restored={}, first_fault={}",
        error.task.id().as_u64(),
        error.kind,
        restored,
        recorded
    );
}

fn current_run_queue_inner() -> &'static AxRunQueue {
    // Safety: scheduler APIs are unavailable until `init()` (or
    // `init_secondary()`) initializes this CPU's permanent per-CPU slot.
    unsafe { RUN_QUEUE.current_ref_raw().get_unchecked() }
}

fn register_current_run_queue(cpu_id: usize) -> bool {
    let run_queue = current_run_queue_inner() as *const AxRunQueue as *mut AxRunQueue;
    RUN_QUEUES.get(cpu_id).is_some_and(|slot| {
        slot.compare_exchange(
            core::ptr::null_mut(),
            run_queue,
            Ordering::Release,
            Ordering::Acquire,
        )
        .is_ok()
    })
}

/// Returns a reference to the current run queue in [`CurrentRunQueueRef`].
///
/// ## Safety
///
/// This function returns a static reference to the current run queue, which
/// is inherently unsafe. It assumes that the `RUN_QUEUE` has been properly
/// initialized and is not accessed concurrently in a way that could cause
/// data races or undefined behavior.
///
/// ## Returns
///
/// * [`CurrentRunQueueRef`] - a static reference to the current [`AxRunQueue`].
#[inline(always)]
pub(crate) fn current_run_queue<G: BaseGuard>() -> CurrentRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    CurrentRunQueueRef {
        inner: current_run_queue_inner(),
        current_task: Some(crate::current()),
        state: irq_state,
        _phantom: core::marker::PhantomData,
    }
}

/// Selects the run queue index based on a CPU set bitmap and load balancing.
///
/// This function filters the available run queues based on the provided `cpumask` and
/// selects the run queue index for the next task. The selection is based on a round-robin algorithm.
///
/// ## Arguments
///
/// * `cpumask` - A bitmap representing the CPUs that are eligible for task execution.
///
/// ## Returns
///
/// The index (cpu_id) of the selected run queue.
///
#[cfg(feature = "smp")]
// The modulo operation is safe here because `axconfig::plat::MAX_CPU_NUM` is always greater than 1 with "smp" enabled.
#[allow(clippy::modulo_one)]
#[inline]
pub(crate) fn select_run_queue_index(cpumask: AxCpuMask) -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static RUN_QUEUE_INDEX: AtomicUsize = AtomicUsize::new(0);

    if cpumask.is_empty() {
        return usize::MAX;
    }

    // Search at most one complete CPU set. The cursor wraps explicitly because
    // it is only a fairness hint, never an identity or ownership generation.
    let start = RUN_QUEUE_INDEX
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cursor| {
            Some(cursor.checked_add(1).unwrap_or(0))
        })
        .unwrap_or(0)
        % axconfig::plat::MAX_CPU_NUM;
    for offset in 0..axconfig::plat::MAX_CPU_NUM {
        let index = (start + offset) % axconfig::plat::MAX_CPU_NUM;
        if cpumask.get(index) && !RUN_QUEUES[index].load(Ordering::Acquire).is_null() {
            return index;
        }
    }
    usize::MAX
}

/// Returns whether an affinity mask contains at least one initialized run
/// queue. Possible-but-offline CPUs are not sufficient publication targets.
#[cfg(feature = "smp")]
pub(crate) fn affinity_has_online_cpu(cpumask: AxCpuMask) -> bool {
    (0..axconfig::plat::MAX_CPU_NUM)
        .any(|index| cpumask.get(index) && !RUN_QUEUES[index].load(Ordering::Acquire).is_null())
}

/// Retrieves the initialized shared run queue for a CPU.
#[cfg(feature = "smp")]
#[inline]
fn get_run_queue(index: usize) -> Option<&'static AxRunQueue> {
    let pointer = RUN_QUEUES.get(index)?.load(Ordering::Acquire);
    // Safety: registry pointers are published only from permanent per-CPU
    // storage after initialization, and no mutable reference is ever derived
    // from this pointer.
    unsafe { pointer.as_ref() }
}

/// Selects the appropriate run queue for the provided task.
///
/// * In a single-core system, this function always returns a reference to the global run queue.
/// * In a multi-core system, this function selects the run queue based on the task's CPU affinity and load balance.
///
/// ## Arguments
///
/// * `task` - A reference to the task for which a run queue is being selected.
///
/// ## Returns
///
/// * [`AxRunQueueRef`] - a static reference to the selected [`AxRunQueue`] (current or remote).
///
/// ## TODO
///
/// 1. Implement better load balancing across CPUs for more efficient task distribution.
/// 2. Use a more generic load balancing algorithm that can be customized or replaced.
#[inline]
pub(crate) fn select_run_queue<G: BaseGuard>(task: &AxTaskRef) -> AxRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        // When SMP is disabled, all tasks are scheduled on the same global run queue.
        AxRunQueueRef {
            inner: Some(current_run_queue_inner()),
            selected_cpu: 0,
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        // A blocked task already owns a target CPU chosen by the blocking or
        // affinity transaction. Keeping its wake on that owner run queue makes
        // wake and scheduler-parameter updates share one scheduler lock. This
        // excludes a transient CFS CONFIGURING owner from the valid wake path.
        let index = if matches!(task.state(), TaskState::Blocked) {
            task.cpu_id() as usize
        } else {
            select_run_queue_index(task.cpumask())
        };
        AxRunQueueRef {
            inner: get_run_queue(index),
            selected_cpu: index,
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

/// Returns the run queue that currently owns the task, if any.
#[inline]
#[cfg(any(feature = "smp", feature = "sched-cfs"))]
pub(crate) fn task_run_queue<G: BaseGuard>(task: &AxTaskRef) -> AxRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        AxRunQueueRef {
            inner: Some(current_run_queue_inner()),
            selected_cpu: 0,
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        let index = task.cpu_id() as usize;
        AxRunQueueRef {
            inner: get_run_queue(index),
            selected_cpu: index,
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

/// [`AxRunQueue`] represents a run queue for global system or a specific CPU.
pub(crate) struct AxRunQueue {
    /// The ID of the CPU this run queue is associated with.
    cpu_id: usize,
    /// The core scheduler of this run queue.
    /// Since irq and preempt are preserved by the kernel guard hold by `AxRunQueueRef`,
    /// we just use a simple raw spin lock here.
    scheduler: SpinRaw<Scheduler>,
}

/// A reference to the run queue with specific guard.
///
/// Note:
/// [`AxRunQueueRef`] is used to get a reference to the run queue on current CPU
/// or a remote CPU, which is used to add tasks to the run queue or unblock tasks.
/// If you want to perform scheduling operations on the current run queue,
/// see [`CurrentRunQueueRef`].
pub(crate) struct AxRunQueueRef<'a, G: BaseGuard> {
    inner: Option<&'a AxRunQueue>,
    selected_cpu: usize,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

impl<G: BaseGuard> Drop for AxRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// A reference to the current run queue with specific guard.
///
/// Note:
/// [`CurrentRunQueueRef`] is used to get a reference to the run queue on current CPU,
/// in which scheduling operations can be performed.
pub(crate) struct CurrentRunQueueRef<'a, G: BaseGuard> {
    inner: &'a AxRunQueue,
    current_task: Option<CurrentTask>,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

impl<G: BaseGuard> Drop for CurrentRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// Management operations for run queue, including adding tasks, unblocking tasks, etc.
impl<G: BaseGuard> AxRunQueueRef<'_, G> {
    fn unavailable(&self, task: AxTaskRef) -> TaskEnqueueError {
        let error = TaskEnqueueError {
            kind: TaskEnqueueErrorKind::RunQueueUnavailable(self.selected_cpu),
            task,
        };
        error
            .task
            .record_wake_fault(TaskWakeFault::RunQueueUnavailable);
        error
    }

    /// Adds a task to the scheduler.
    ///
    /// This function is used to add a new task to the scheduler.
    pub fn add_task(&mut self, task: AxTaskRef) -> Result<(), TaskEnqueueError> {
        let Some(run_queue) = self.inner else {
            return Err(self.unavailable(task));
        };
        debug!(
            "task add: id={} on run_queue {}",
            task.id().as_u64(),
            run_queue.cpu_id
        );
        if !task.is_ready() {
            return Err(TaskEnqueueError {
                kind: TaskEnqueueErrorKind::TaskNotReady,
                task,
            });
        }
        #[cfg(feature = "smp")]
        task.set_cpu_id(run_queue.cpu_id as _);
        run_queue.enqueue_task(task, EnqueueReason::New)
    }

    /// Unblock one task by inserting it into the run queue.
    ///
    /// This function does nothing if the task is not in [`TaskState::Blocked`],
    /// which means the task is already unblocked by other cores.
    pub fn unblock_task(&mut self, task: AxTaskRef, resched: bool) -> WakeTaskOutcome {
        let task_id = task.id().as_u64();
        let Some(run_queue) = self.inner else {
            return WakeTaskOutcome::Rejected(self.unavailable(task));
        };
        // Try to change the state of the task from `Blocked` to `Ready`,
        // if successful, the task will be put into this run queue,
        // otherwise, the task is already unblocked by other cores.
        // Note:
        // target task can not be insert into the run queue until it finishes its scheduling process.
        match run_queue.put_task_with_state(task, TaskState::Blocked, resched) {
            PutTaskOutcome::Enqueued => {
                // Since now, the task to be unblocked is in the `Ready` state.
                let cpu_id = run_queue.cpu_id;
                debug!("task unblock: id={task_id} on run_queue {cpu_id}");
                // Note: when the task is unblocked on another CPU's run queue,
                // we just ingiore the `resched` flag.
                if resched && cpu_id == this_cpu_id() {
                    #[cfg(feature = "preempt")]
                    crate::current().set_preempt_pending(true);
                }
                WakeTaskOutcome::Enqueued
            }
            #[cfg(feature = "smp")]
            PutTaskOutcome::Deferred => WakeTaskOutcome::Deferred,
            PutTaskOutcome::StateMismatch => WakeTaskOutcome::AlreadyRunnable,
            PutTaskOutcome::Rejected(error) => {
                #[cfg(feature = "preempt")]
                if resched {
                    crate::current().set_preempt_pending(true);
                }
                WakeTaskOutcome::Rejected(error)
            }
        }
    }

    #[cfg(feature = "sched-cfs")]
    pub fn set_task_sched_state(
        &mut self,
        task: &AxTaskRef,
        sched_state: axsched::CfsTaskParams,
    ) -> Result<(), TaskSchedError> {
        if matches!(task.state(), TaskState::Exited) {
            return Err(TaskSchedError::TaskExited);
        }
        let run_queue = self
            .inner
            .ok_or(TaskSchedError::RunQueueUnavailable(self.selected_cpu))?;
        run_queue.set_task_sched_state(task, sched_state)
    }

    #[cfg(feature = "smp")]
    pub fn migrate_ready_task(&mut self, task: &AxTaskRef) -> bool {
        self.inner
            .is_some_and(|run_queue| run_queue.migrate_ready_task(task))
    }
}

#[cfg(feature = "sched-cfs")]
impl<G: BaseGuard> AxRunQueueRef<'static, G> {
    /// Reserves final publication of a brand-new CFS task.
    pub(crate) fn reserve_claimed_new_task(
        &mut self,
        task: AxTaskRef,
    ) -> Result<PreparedTaskPublication, TaskEnqueueError> {
        let Some(run_queue) = self.inner else {
            task.release_publication_mutation();
            return Err(self.unavailable(task));
        };
        if !task.is_ready() {
            task.release_publication_mutation();
            return Err(TaskEnqueueError {
                kind: TaskEnqueueErrorKind::TaskNotReady,
                task,
            });
        }

        let reservation = match run_queue.scheduler.lock().reserve_new_task(&task) {
            Ok(reservation) => reservation,
            Err(error) => {
                task.release_publication_mutation();
                return Err(TaskEnqueueError {
                    kind: TaskEnqueueErrorKind::Scheduler(error),
                    task,
                });
            }
        };
        #[cfg(feature = "smp")]
        task.set_cpu_id(run_queue.cpu_id as _);
        drop(task);
        Ok(PreparedTaskPublication {
            run_queue,
            reservation: Some(reservation),
        })
    }
}

/// Core functions of run queue.
impl<G: BaseGuard> CurrentRunQueueRef<'_, G> {
    fn current_task(&self) -> &CurrentTask {
        self.current_task
            .as_ref()
            .expect("current task ownership was already released")
    }

    #[cfg(feature = "smp")]
    fn maybe_migrate_current(&mut self) -> bool {
        let curr = self.current_task();
        match curr.claim_migration(self.inner.cpu_id) {
            MigrationClaim::Allowed => false,
            MigrationClaim::Prepared(migration_task) => {
                self.migrate_current(migration_task);
                true
            }
            MigrationClaim::Missing => {
                // All public affinity updates admit the helper before publishing
                // an excluding mask. Keep running rather than allocating or
                // panicking inside this runqueue/no-IRQ safe point if an internal
                // caller ever violates that contract.
                #[cfg(feature = "preempt")]
                curr.set_preempt_pending(true);
                false
            }
        }
    }

    #[cfg(feature = "smp")]
    pub(crate) fn migrate_current_if_needed(&mut self) -> bool {
        self.maybe_migrate_current()
    }

    #[cfg(feature = "irq")]
    pub fn scheduler_timer_tick(&mut self) {
        let curr = self.current_task();
        #[cfg(feature = "smp")]
        if !curr.cpumask().get(self.inner.cpu_id) {
            #[cfg(feature = "preempt")]
            curr.set_preempt_pending(true);
            return;
        }
        if curr.is_idle() {
            // Diagnostic time may wrap only after `u64::MAX` scheduler ticks;
            // unlike an identity or ownership generation it is not used for
            // correctness and must not add a contended CAS retry loop here.
            IDLE_TICKS.fetch_add(1, Ordering::Relaxed);
        } else if self.inner.scheduler.lock().task_tick(curr) {
            #[cfg(feature = "preempt")]
            curr.set_preempt_pending(true);
        }
    }

    /// Yield the current task and reschedule.
    /// This function will put the current task into this run queue with `Ready` state,
    /// and reschedule to the next task on this run queue.
    pub fn yield_current(&mut self) {
        let curr = self.current_task().clone();
        trace!("task yield: id={}", curr.id().as_u64());
        assert!(
            curr.is_running(),
            "yielding task id={} is not running: {:?}",
            curr.id().as_u64(),
            curr.state()
        );

        #[cfg(feature = "smp")]
        if self.maybe_migrate_current() {
            return;
        }

        if curr.is_idle() {
            // The idle task is never a ready-queue member. Keep its lifecycle
            // state Running and still probe the scheduler so a wake published
            // without immediate preemption can take the CPU.
            self.inner.resched();
            return;
        }

        match self
            .inner
            .put_task_with_state(curr, TaskState::Running, false)
        {
            PutTaskOutcome::Enqueued => self.inner.resched(),
            PutTaskOutcome::Rejected(_) | PutTaskOutcome::StateMismatch => {}
            #[cfg(feature = "smp")]
            PutTaskOutcome::Deferred => {}
        }
    }

    /// Migrate the current task to a new run queue matching its CPU affinity and reschedule.
    /// This function will spawn a new `migration_task` to perform the migration, which will set
    /// current task to `Ready` state and select a proper run queue for it according to its CPU affinity,
    /// switch to the migration task immediately after migration task is prepared.
    ///
    /// Note: the ownership if migrating task (which is current task) is handed over to the migration task,
    /// before the migration task inserted it into the target run queue.
    #[cfg(feature = "smp")]
    pub fn migrate_current(&mut self, migration_task: AxTaskRef) {
        let curr = self.current_task();
        trace!("task migrate: id={}", curr.id().as_u64());
        assert!(curr.is_running());

        // Mark current task's state as `Ready`,
        // but, do not put current task to the scheduler of this run queue.
        curr.set_state(TaskState::Ready);

        // Call `switch_to` to reschedule to the migration task that performs the migration directly.
        self.inner.switch_to(crate::current(), migration_task);
    }

    /// Preempts the current task and reschedules.
    /// This function is used to preempt the current task and reschedule
    /// to next task on current run queue.
    ///
    /// This function is called by `current_check_preempt_pending` with IRQs and preemption disabled.
    ///
    /// Note:
    /// preemption may happened in `enable_preempt`, which is called
    /// each time a [`kspin::NoPreemptGuard`] is dropped.
    #[cfg(feature = "preempt")]
    pub fn preempt_resched(&mut self) {
        // There is no need to disable IRQ and preemption here, because
        // they both have been disabled in `current_check_preempt_pending`.
        let curr = self.current_task().clone();
        assert!(curr.is_running());

        // When we call `preempt_resched()`, both IRQs and preemption must
        // have been disabled by `kernel_guard::NoPreemptIrqSave`. So we need
        // to set `current_disable_count` to 1 in `can_preempt()` to obtain
        // the preemption permission.
        let can_preempt = curr.can_preempt(1);

        trace!(
            "current task id={} is to be preempted, allow={}",
            curr.id().as_u64(),
            can_preempt
        );
        if can_preempt {
            #[cfg(feature = "smp")]
            if self.maybe_migrate_current() {
                return;
            }
            if curr.is_idle() {
                self.inner.resched();
                return;
            }
            match self
                .inner
                .put_task_with_state(curr.clone(), TaskState::Running, true)
            {
                PutTaskOutcome::Enqueued => self.inner.resched(),
                PutTaskOutcome::Rejected(_) | PutTaskOutcome::StateMismatch => {
                    curr.set_preempt_pending(false)
                }
                #[cfg(feature = "smp")]
                PutTaskOutcome::Deferred => curr.set_preempt_pending(false),
            }
        } else {
            curr.set_preempt_pending(true);
        }
    }

    /// Exit the current task with the specified exit code.
    /// This function will never return.
    pub fn exit_current(&mut self, exit_code: i32) -> ! {
        let curr = self
            .current_task
            .take()
            .expect("current task ownership was already released");
        debug!(
            "task exit: id={}, exit_code={}",
            curr.id().as_u64(),
            exit_code
        );
        assert!(curr.is_running(), "task is not running: {:?}", curr.state());
        assert!(!curr.is_idle());
        if curr.is_init() {
            clear_exited_tasks();
            axhal::power::system_off();
        } else {
            // Notify the joiner task.
            curr.notify_exit(exit_code);

            // Push current task to the `EXITED_TASKS` list, which will be
            // consumed by the GC task.
            if let Err(error) = push_exited_task(curr.clone()) {
                error!(
                    "cannot retain exiting task {} safely: {:?}",
                    error.task.id().as_u64(),
                    error.fault
                );
                // Continuing the context switch without the queue-owned Arc
                // could free the stack underneath this exit path. Preserve
                // memory safety after recording the durable typed fault.
                axhal::power::system_off();
            }

            // This stack will never unwind after the context switch. Release
            // the runqueue guard's independent current-task owner explicitly;
            // the per-CPU current slot and exited queue still retain the task
            // until switch completion and GC respectively.
            drop(curr);

            // Schedule to next task.
            self.inner.resched();
        }
        unreachable!("task exited!");
    }

    /// Allocation-free lost-wake-safe blocking for raw-waker executors.
    ///
    /// The owner claims the transition before publishing `Blocked`. A racing
    /// raw waker either sees the task outside that transition and performs the
    /// normal unblock, or marks the claim so this owner restores `Running`.
    /// No waker waits for the owner and no wake can fall between the state
    /// check and the scheduler handoff.
    pub(crate) fn blocked_resched_atomic(&mut self, token: BlockWaitToken) -> BlockWaitCommit {
        let curr = self.current_task();
        if !curr.is_running() || curr.is_idle() {
            return BlockWaitCommit::Stale;
        }
        #[cfg(all(feature = "preempt", target_os = "none"))]
        if !curr.can_preempt(1) {
            return BlockWaitCommit::Stale;
        }
        #[cfg(all(feature = "preempt", not(target_os = "none")))]
        if !curr.can_preempt(0) {
            return BlockWaitCommit::Stale;
        }

        match curr.claim_block_wait(token) {
            BlockWaitClaim::Woken => return BlockWaitCommit::Woken,
            BlockWaitClaim::Stale => return BlockWaitCommit::Stale,
            BlockWaitClaim::Claimed => {}
        }

        #[cfg(feature = "smp")]
        if !curr.cpumask().get(self.inner.cpu_id) {
            let target = select_run_queue_index(curr.cpumask());
            if get_run_queue(target).is_some() {
                curr.set_cpu_id(target as _);
            }
        }

        curr.set_state(TaskState::Blocked);
        match curr.commit_block_wait(token) {
            BlockWaitCommit::Blocked => {
                debug!("task block: id={}", curr.id().as_u64());
                self.inner.resched();
                BlockWaitCommit::Blocked
            }
            outcome => outcome,
        }
    }

    pub fn set_current_priority(&mut self, priority: isize) -> Result<(), TaskSchedError> {
        self.inner
            .scheduler
            .lock()
            .set_priority(self.current_task(), priority)
            .map_err(TaskSchedError::from)
    }
}

impl AxRunQueue {
    fn enqueue_task(&self, task: AxTaskRef, reason: EnqueueReason) -> Result<(), TaskEnqueueError> {
        // Retain caller ownership across the scheduler call. Scheduler traits
        // consume their item even on rejection, so this clone is performed
        // before taking the scheduler lock and lets the caller retry or report
        // the exact task after the lock is released.
        let scheduler_task = task.clone();
        let result = self.scheduler.lock().enqueue_task(scheduler_task, reason);
        result.map_err(|error| TaskEnqueueError {
            kind: TaskEnqueueErrorKind::Scheduler(error),
            task,
        })
    }

    #[cfg(feature = "smp")]
    fn migrate_ready_task(&self, task: &AxTaskRef) -> bool {
        if !matches!(task.state(), TaskState::Ready) {
            return false;
        }

        let target_index = select_run_queue_index(task.cpumask());
        if target_index == self.cpu_id {
            return true;
        }

        let task = match self.scheduler.lock().remove_task(task) {
            Ok(Some(task)) => task,
            Ok(None) | Err(_) => return false,
        };

        let Some(target) = get_run_queue(target_index) else {
            // The task remains owned here; restore it before reporting that no
            // initialized target queue is available.
            if let Err(error) = self.enqueue_task(task, EnqueueReason::Wakeup) {
                contain_enqueue_failure(&error, TaskState::Ready);
            }
            return false;
        };
        task.set_cpu_id(target.cpu_id as _);
        match target.enqueue_task(task, EnqueueReason::Wakeup) {
            Ok(()) => true,
            Err(error) => {
                contain_enqueue_failure(&error, TaskState::Ready);
                let task = error.task;
                task.set_cpu_id(self.cpu_id as _);
                if let Err(restore_error) = self.enqueue_task(task, EnqueueReason::Wakeup) {
                    contain_enqueue_failure(&restore_error, TaskState::Ready);
                }
                false
            }
        }
    }

    #[cfg(feature = "sched-cfs")]
    fn set_task_sched_state(
        &self,
        task: &AxTaskRef,
        sched_state: axsched::CfsTaskParams,
    ) -> Result<(), TaskSchedError> {
        if matches!(task.state(), TaskState::Exited) {
            return Err(TaskSchedError::TaskExited);
        }
        self.scheduler
            .lock()
            .set_task_params(task, sched_state)
            .map_err(TaskSchedError::from)
    }

    /// Create a new run queue for the specified CPU.
    /// The run queue is initialized with a per-CPU gc task in its scheduler.
    fn new(cpu_id: usize) -> Result<Self, TaskRuntimeInitError> {
        let gc_task = TaskInner::new(
            || {
                if let Err(error) = block_on(poll_fn(poll_gc)) {
                    error!("exited-task recycler stopped: {error}");
                    crate::exit(-1);
                }
            },
            "gc".into(),
            axconfig::TASK_STACK_SIZE,
        )?
        .into_arc()?;
        // gc task should be pinned to the current CPU.
        gc_task.set_cpumask(AxCpuMask::one_shot(cpu_id));
        #[cfg(feature = "sched-cfs")]
        gc_task
            .configure(axsched::CfsTaskParams {
                // Exited-task stacks are only recycled after the GC task runs.
                // Keep it in the normal fair class so join-heavy thread bursts
                // cannot outrun cleanup and exhaust kernel stack memory.
                class: axsched::CfsTaskClass::Normal,
                nice: 0,
                rt_priority: 0,
            })
            .map_err(TaskRuntimeInitError::Scheduler)?;

        let mut scheduler = Scheduler::new();
        scheduler
            .add_task(gc_task)
            .map_err(TaskRuntimeInitError::Scheduler)?;
        Ok(Self {
            cpu_id,
            scheduler: SpinRaw::new(scheduler),
        })
    }

    /// Puts target task into current run queue with `Ready` state
    /// if its state matches `current_state` (except idle task).
    ///
    /// If `preempt`, keep current task's time slice, otherwise reset it.
    ///
    /// Returns `true` if the target task is put into this run queue successfully,
    /// otherwise `false`.
    fn put_task_with_state(
        &self,
        task: AxTaskRef,
        current_state: TaskState,
        preempt: bool,
    ) -> PutTaskOutcome {
        // If the task's state matches `current_state`, set its state to `Ready` and
        // put it back to the run queue (except idle task).
        if task.is_idle() {
            return PutTaskOutcome::StateMismatch;
        }
        if task.transition_state(current_state, TaskState::Ready) {
            let reason = match current_state {
                TaskState::Blocked => EnqueueReason::Wakeup,
                TaskState::Running if preempt => EnqueueReason::Preempt,
                TaskState::Running => EnqueueReason::Yield,
                TaskState::Ready | TaskState::Exited => EnqueueReason::New,
            };
            #[cfg(feature = "smp")]
            task.set_cpu_id(self.cpu_id as _);

            #[cfg(feature = "smp")]
            let task = if current_state == TaskState::Blocked {
                match task.publish_wake_handoff(task.clone()) {
                    WakeHandoffPublication::Deferred => return PutTaskOutcome::Deferred,
                    WakeHandoffPublication::Ready(owned) => {
                        drop(task);
                        owned
                    }
                    WakeHandoffPublication::Occupied(owned) => {
                        drop(task);
                        let error = TaskEnqueueError {
                            kind: TaskEnqueueErrorKind::HandoffOccupied,
                            task: owned,
                        };
                        contain_enqueue_failure(&error, current_state);
                        return PutTaskOutcome::Rejected(error);
                    }
                }
            } else {
                task
            };

            match self.enqueue_task(task, reason) {
                Ok(()) => PutTaskOutcome::Enqueued,
                Err(error) => {
                    contain_enqueue_failure(&error, current_state);
                    PutTaskOutcome::Rejected(error)
                }
            }
        } else {
            PutTaskOutcome::StateMismatch
        }
    }

    /// Core reschedule subroutine.
    /// Pick the next task to run and switch to it.
    fn resched(&self) {
        let next = match self.scheduler.lock().pick_next_task() {
            Some(next) => {
                assert!(
                    next.is_ready(),
                    "selected task id={} is not ready: {:?}",
                    next.id().as_u64(),
                    next.state()
                );
                next
            }
            None => {
                let idle = unsafe {
                    // Safety: IRQs must be disabled at this time.
                    IDLE_TASK.current_ref_raw().get_unchecked().clone()
                };
                assert!(
                    is_valid_idle_fallback(&idle),
                    "idle fallback id={} has invalid state: {:?}",
                    idle.id().as_u64(),
                    idle.state()
                );
                idle
            }
        };
        self.switch_to(crate::current(), next);
    }

    fn switch_to(&self, prev_task: CurrentTask, next_task: AxTaskRef) {
        // Make sure that IRQs are disabled by kernel guard or other means.
        #[cfg(all(target_os = "none", feature = "irq"))] // Note: irq is faked under unit tests.
        assert!(
            !axhal::asm::irqs_enabled(),
            "IRQs must be disabled during scheduling"
        );
        trace!(
            "context switch: id={} -> id={}",
            prev_task.id().as_u64(),
            next_task.id().as_u64()
        );
        #[cfg(feature = "preempt")]
        next_task.set_preempt_pending(false);
        next_task.set_state(TaskState::Running);
        if prev_task.ptr_eq(&next_task) {
            return;
        }

        // Claim the task as running, we do this before switching to it
        // such that any running task will have this set.
        #[cfg(feature = "smp")]
        next_task.mark_running_on_cpu();

        #[cfg(feature = "task-ext")]
        {
            use crate::TaskExt;

            if let Some(ext) = prev_task.task_ext() {
                ext.on_leave(&prev_task)
            }
            if let Some(ext) = next_task.task_ext() {
                ext.on_enter(&next_task)
            }
        }

        unsafe {
            let prev_ctx_ptr = prev_task.ctx_mut_ptr();
            let next_ctx_ptr = next_task.ctx_mut_ptr();

            // Store the weak pointer of **prev_task** in percpu variable `PREV_TASK`.
            #[cfg(feature = "smp")]
            {
                *PREV_TASK.current_ref_mut_raw() = Arc::downgrade(&prev_task);
            }

            // `prev_task` is an owned public handle in addition to the per-CPU
            // current-task reference. Switching drops both; a runnable,
            // blocked, or exiting lifecycle owner must retain at least one more
            // reference until it is safe to reclaim the old kernel stack.
            assert!(Arc::strong_count(&prev_task) > 2);
            assert!(Arc::strong_count(&next_task) >= 1);

            CurrentTask::set_current(prev_task, next_task);

            (*prev_ctx_ptr).switch_to(&*next_ctx_ptr);

            // Current it's **next_task** running on this CPU, clear the `prev_task`'s `on_cpu` field
            // to indicate that it has finished its scheduling process and no longer running on this CPU.
            #[cfg(feature = "smp")]
            clear_prev_task_on_cpu();
        }
    }
}

fn is_valid_idle_fallback(task: &AxTaskRef) -> bool {
    task.is_idle() && (task.is_ready() || task.is_running())
}

fn poll_gc(cx: &mut Context<'_>) -> Poll<()> {
    loop {
        let retained = reclaim_exited_tasks_current_cpu();
        // Note: we cannot block current task with preemption disabled,
        // use `current_ref_raw` to get the `WAIT_FOR_EXIT`'s reference here to avoid
        // the use of `NoPreemptGuard`. Since gc task is pinned to the current
        // CPU, there is no affection if the gc task is preempted during the process.
        unsafe { WAIT_FOR_EXIT.current_ref_raw() }.register(cx.waker());

        // New tasks might be added during the above section, recheck it to
        // prevent us from sleeping indefinitely.
        if EXITED_TASKS.with_current(|exited_tasks| exited_tasks.is_empty()) {
            break;
        }
        // A just-exited child can still be held by the clone/wakeup path that
        // spawned it. Re-polling immediately would spin the GC task against a
        // transient reference and steal CPU from fork/thread-heavy workloads.
        // Later exits wake the GC again, and explicit reclaim points in clone
        // and wait drain any retained task once that reference is gone.
        if retained {
            break;
        }

        crate::yield_now();
    }

    Poll::Pending
}

fn push_exited_task(task: AxTaskRef) -> Result<(), ExitedTaskEnqueueError> {
    EXITED_TASKS.with_current(|exited_tasks| exited_tasks.push_back(task))?;
    // Safety: exit_current runs with IRQs + preemption disabled. Re-push
    // from the reclaim loop runs under the same percpu context.
    unsafe { WAIT_FOR_EXIT.current_ref_mut_raw().wake() };
    Ok(())
}

fn clear_exited_tasks() {
    while let Some(dequeued) = EXITED_TASKS.with_current(ExitedTaskQueue::pop_front) {
        if let Some(fault) = dequeued.fault {
            error!(
                "exited task {} dequeued with fault: {:?}",
                dequeued.task.id().as_u64(),
                fault
            );
        }
        // Drop after the per-CPU queue access has ended. Task/task-ext/stack
        // destructors may run arbitrary code and must not run inside it.
        drop(dequeued.task);
    }
}

pub(crate) fn has_exited_tasks() -> bool {
    !EXITED_TASKS.with_current(|exited_tasks| exited_tasks.is_empty())
}

fn reclaim_exited_tasks_current_cpu_batch(max_tasks: Option<usize>) -> (bool, bool) {
    // Snapshot the current queue depth so that tasks re-pushed because
    // Arc::try_unwrap failed are deferred to a later round rather than
    // keeping this loop spinning forever.
    let n = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.reclaim_len());
    let budget = max_tasks.map_or(n, |max_tasks| n.min(max_tasks.max(1)));
    let mut retained = false;
    for _ in 0..budget {
        let Some(dequeued) = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.pop_front())
        else {
            break;
        };
        if let Some(fault) = dequeued.fault {
            error!(
                "exited task {} dequeued with fault: {:?}",
                dequeued.task.id().as_u64(),
                fault
            );
        }
        let task = dequeued.task;
        match Arc::try_unwrap(task) {
            Ok(task) => {
                let mut task = task.into_inner();
                if let Some(stack) = task.take_kernel_stack() {
                    recycle_task_stack(stack);
                }
                drop(task);
            }
            Err(task) => {
                // Still held by a joiner or scheduler handoff; push back for a
                // later round.
                retained = true;
                if let Err(error) = push_exited_task(task) {
                    error!(
                        "cannot requeue exited task {}: {:?}",
                        error.task.id().as_u64(),
                        error.fault
                    );
                    // The task is no longer executing, so releasing this queue
                    // ownership unit is safe. Its durable fault remains visible
                    // through any external task handle which kept unwrap from
                    // succeeding.
                    drop(error.task);
                }
            }
        }
    }
    let remaining = !EXITED_TASKS.with_current(|exited_tasks| exited_tasks.is_empty());
    (retained, remaining)
}

pub(crate) fn reclaim_exited_tasks_current_cpu() -> bool {
    reclaim_exited_tasks_current_cpu_batch(None).0
}

pub(crate) fn reclaim_exited_tasks_current_cpu_bounded(max_tasks: usize) -> bool {
    reclaim_exited_tasks_current_cpu_batch(Some(max_tasks)).1
}

/// The task routine for migrating the current task to the correct CPU.
///
/// It calls `select_run_queue` to get the correct run queue for the task, and
/// then puts the task to the scheduler of target run queue.
#[cfg(feature = "smp")]
pub(crate) fn migrate_entry(migrated_task: AxTaskRef) -> Result<(), TaskEnqueueError> {
    let target = select_run_queue::<kernel_guard::NoPreemptIrqSave>(&migrated_task);
    let Some(run_queue) = target.inner else {
        let error = TaskEnqueueError {
            kind: TaskEnqueueErrorKind::RunQueueUnavailable(target.selected_cpu),
            task: migrated_task,
        };
        contain_enqueue_failure(&error, TaskState::Ready);
        return Err(error);
    };
    migrated_task.set_cpu_id(run_queue.cpu_id as _);
    let result = run_queue.enqueue_task(migrated_task, EnqueueReason::Yield);
    if let Err(error) = &result {
        contain_enqueue_failure(error, TaskState::Ready);
    }
    result
}

/// Clear the `on_cpu` field of previous task running on this CPU.
#[cfg(feature = "smp")]
pub(crate) unsafe fn clear_prev_task_on_cpu() {
    let Some(previous) = (unsafe { PREV_TASK.current_ref_raw() }).upgrade() else {
        return;
    };
    match previous.finish_cpu_handoff() {
        CpuHandoffCompletion::Cleared | CpuHandoffCompletion::AlreadyCleared => {}
        CpuHandoffCompletion::Wake(task) => {
            let target_cpu = task.cpu_id() as usize;
            let result = match get_run_queue(target_cpu) {
                Some(run_queue) => run_queue.enqueue_task(task, EnqueueReason::Wakeup),
                None => Err(TaskEnqueueError {
                    kind: TaskEnqueueErrorKind::RunQueueUnavailable(target_cpu),
                    task,
                }),
            };
            if let Err(error) = result {
                contain_enqueue_failure(&error, TaskState::Blocked);
                #[cfg(feature = "preempt")]
                crate::current().set_preempt_pending(true);
            }
        }
        CpuHandoffCompletion::MissingWake => {
            previous.record_wake_fault(TaskWakeFault::HandoffCorrupt);
            error!(
                "CPU handoff for task {} lost its owned wake token",
                previous.id().as_u64()
            );
        }
    }
}
pub(crate) fn init() -> Result<(), TaskRuntimeInitError> {
    let cpu_id = this_cpu_id();

    // Create the `idle` task (not current task).
    // The idle task will run when there is no other runnable task.
    // Stack size of idle task should be large because traps/interrupts may happen in idle task,
    // which need more stack space.
    const IDLE_TASK_STACK_SIZE: usize = 16384;
    let idle_task =
        TaskInner::new(|| crate::run_idle(), "idle".into(), IDLE_TASK_STACK_SIZE)?.into_arc()?;
    let main_task = TaskInner::new_init("main".into())?.into_arc()?;
    let run_queue = AxRunQueue::new(cpu_id)?;

    // idle task should be pinned to the current CPU.
    idle_task.set_cpumask(AxCpuMask::one_shot(cpu_id));
    if !IDLE_TASK.with_current(|i| i.call_once(|| idle_task).is_some()) {
        return Err(TaskRuntimeInitError::DuplicateCpu(cpu_id));
    }

    // Put the subsequent execution into the `main` task.
    main_task.set_state(TaskState::Running);
    unsafe { CurrentTask::init_current(main_task) }

    let initialized = RUN_QUEUE.with_current(|rq| rq.call_once(|| run_queue).is_some());
    if !initialized || !register_current_run_queue(cpu_id) {
        return Err(TaskRuntimeInitError::DuplicateCpu(cpu_id));
    }
    Ok(())
}

pub(crate) fn init_secondary() -> Result<(), TaskRuntimeInitError> {
    let cpu_id = this_cpu_id();

    // Put the subsequent execution into the `idle` task.
    let idle_task = TaskInner::new_init("idle".into())?.into_arc()?;
    let run_queue = AxRunQueue::new(cpu_id)?;

    idle_task.set_state(TaskState::Running);
    if !IDLE_TASK.with_current(|i| i.call_once(|| idle_task.clone()).is_some()) {
        return Err(TaskRuntimeInitError::DuplicateCpu(cpu_id));
    }
    unsafe { CurrentTask::init_current(idle_task) }

    let initialized = RUN_QUEUE.with_current(|rq| rq.call_once(|| run_queue).is_some());
    if !initialized || !register_current_run_queue(cpu_id) {
        return Err(TaskRuntimeInitError::DuplicateCpu(cpu_id));
    }
    Ok(())
}

#[cfg(test)]
mod exited_queue_tests {
    use super::*;

    fn task(name: &str) -> AxTaskRef {
        TaskInner::new_init(name.into())
            .unwrap()
            .into_arc()
            .unwrap()
    }

    #[test]
    fn idle_probe_does_not_publish_a_fake_ready_state() {
        let idle = task("idle");
        assert!(is_valid_idle_fallback(&idle));
        idle.set_state(TaskState::Running);
        let run_queue = AxRunQueue {
            cpu_id: 0,
            scheduler: SpinRaw::new(Scheduler::new()),
        };

        assert!(matches!(
            run_queue.put_task_with_state(idle.clone(), TaskState::Running, false),
            PutTaskOutcome::StateMismatch
        ));
        assert_eq!(idle.state(), TaskState::Running);
        assert!(is_valid_idle_fallback(&idle));
        idle.set_state(TaskState::Blocked);
        assert!(!is_valid_idle_fallback(&idle));
    }

    fn pop_clean(queue: &mut ExitedTaskQueue) -> AxTaskRef {
        let dequeued = queue.pop_front().unwrap();
        assert_eq!(dequeued.fault, None);
        dequeued.task
    }

    #[test]
    fn intrusive_exited_queue_is_fifo_and_transfers_one_arc() {
        let first = task("exit-first");
        let second = task("exit-second");
        let mut queue = ExitedTaskQueue::new();

        queue.push_back(first.clone()).unwrap();
        queue.push_back(second.clone()).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(Arc::strong_count(&first), 2);
        assert_eq!(Arc::strong_count(&second), 2);

        let popped_first = pop_clean(&mut queue);
        assert!(Arc::ptr_eq(&popped_first, &first));
        assert_eq!(queue.len(), 1);
        drop(popped_first);
        assert_eq!(Arc::strong_count(&first), 1);

        let popped_second = pop_clean(&mut queue);
        assert!(Arc::ptr_eq(&popped_second, &second));
        assert!(queue.is_empty());
        drop(popped_second);
        assert_eq!(Arc::strong_count(&second), 1);
    }

    #[test]
    fn duplicate_exited_enqueue_is_typed_durable_and_does_not_grow() {
        let task = task("exit-duplicate");
        let mut queue = ExitedTaskQueue::new();
        queue.push_back(task.clone()).unwrap();

        let error = queue.push_back(task.clone()).unwrap_err();
        assert_eq!(error.fault, TaskExitQueueFault::DuplicateEnqueue);
        assert!(Arc::ptr_eq(&error.task, &task));
        assert_eq!(
            task.exit_queue_fault(),
            Some(TaskExitQueueFault::DuplicateEnqueue)
        );
        assert_eq!(queue.len(), 1);

        drop(error.task);
        drop(pop_clean(&mut queue));
        assert!(queue.is_empty());
    }

    #[test]
    fn exited_queue_length_exhaustion_preserves_both_owners() {
        let queued = task("exit-queued");
        let rejected = task("exit-rejected");
        let mut queue = ExitedTaskQueue::new();
        queue.push_back(queued.clone()).unwrap();
        queue.len = usize::MAX;

        let error = queue.push_back(rejected.clone()).unwrap_err();
        assert_eq!(error.fault, TaskExitQueueFault::LengthExhausted);
        assert!(Arc::ptr_eq(&error.task, &rejected));
        assert_eq!(
            rejected.exit_queue_fault(),
            Some(TaskExitQueueFault::LengthExhausted)
        );
        assert_eq!(Arc::strong_count(&queued), 2);
        assert_eq!(Arc::strong_count(&rejected), 2);

        // Restore the deliberately corrupted test counter before draining the
        // still-valid ownership chain.
        queue.len = 1;
        drop(error.task);
        drop(pop_clean(&mut queue));
    }

    #[test]
    fn exited_dequeue_reports_topology_fault_and_restores_arc() {
        let task = task("exit-topology");
        let mut queue = ExitedTaskQueue::new();
        queue.push_back(task.clone()).unwrap();
        queue.len = 2;

        let dequeued = queue.pop_front().unwrap();
        assert_eq!(dequeued.fault, Some(TaskExitQueueFault::CorruptLink));
        assert!(Arc::ptr_eq(&dequeued.task, &task));
        assert_eq!(
            task.exit_queue_fault(),
            Some(TaskExitQueueFault::CorruptLink)
        );
        assert!(queue.is_empty());
        drop(dequeued.task);
        assert_eq!(Arc::strong_count(&task), 1);
    }

    #[test]
    fn exited_dequeue_salvages_orphaned_tail_arc() {
        let task = task("exit-tail-salvage");
        let mut queue = ExitedTaskQueue::new();
        queue.push_back(task.clone()).unwrap();
        queue.head = core::ptr::null_mut();

        assert!(!queue.is_empty());
        assert_eq!(queue.reclaim_len(), 1);
        let dequeued = queue.pop_front().unwrap();
        assert_eq!(dequeued.fault, Some(TaskExitQueueFault::CorruptLink));
        assert!(Arc::ptr_eq(&dequeued.task, &task));
        assert!(queue.is_empty());
        drop(dequeued.task);
        assert_eq!(Arc::strong_count(&task), 1);
    }

    #[test]
    fn exited_task_can_be_requeued_without_link_aba() {
        let task = task("exit-requeue");
        let mut queue = ExitedTaskQueue::new();

        queue.push_back(task.clone()).unwrap();
        let owned = pop_clean(&mut queue);
        queue.push_back(owned).unwrap();
        let owned = pop_clean(&mut queue);

        assert!(Arc::ptr_eq(&owned, &task));
        assert_eq!(task.exit_queue_generation_for_test(), 2);
        assert_eq!(task.exit_queue_fault(), None);
    }
}
