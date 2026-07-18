use alloc::{alloc::AllocError, boxed::Box, string::String, sync::Arc};
#[cfg(feature = "preempt")]
use core::sync::atomic::AtomicUsize;
use core::{
    alloc::Layout,
    cell::{Cell, UnsafeCell},
    fmt,
    future::poll_fn,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering},
    task::{Context, Poll},
};

use axhal::context::TaskContext;
use futures_util::task::AtomicWaker;
use kspin::SpinNoIrq;
use memory_addr::VirtAddr;

use crate::{AxCpuMask, AxTask, AxTaskRef, future::block_on};

/// A unique identifier for a thread.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct TaskId(u64);

/// Minimum admitted kernel-task stack size.
///
/// Context switching, scheduler bookkeeping, and the allocation-free future
/// executor all run on this stack. Smaller stacks corrupt adjacent allocator
/// state before Rust can report an error, especially on the CFS path, so the
/// mechanism rejects them before allocation instead of treating the size as a
/// performance hint.
pub const MIN_KERNEL_STACK_SIZE: usize = 16 * 1024;

const TASK_MUTATION_IDLE: u8 = 0;
#[cfg(feature = "sched-cfs")]
const TASK_MUTATION_PUBLICATION: u8 = 1;
const TASK_MUTATION_AFFINITY: u8 = 2;

/// Failure while constructing an unpublished task.
///
/// Identity exhaustion is deliberately distinct from allocation failure. A
/// Linux personality may map this mechanism error to its own PID policy, but
/// it cannot rewind or overwrite the mechanism's identity allocator.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskCreateError {
    /// The requested kernel-stack size was too small or overflowed while aligning.
    InvalidStackSize,
    /// Kernel-stack, entry, or task-wrapper allocation failed.
    OutOfMemory,
    /// Every non-zero task identity has been allocated.
    IdentifierExhausted,
}

/// Durable terminal fault recorded when a runnable task cannot be published.
///
/// Valid blocked-task wakeups are designed to be infallible after run-queue
/// initialization. These values contain violated internal contracts without
/// pretending the wake succeeded or leaving the failure visible only in a log.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskWakeFault {
    /// The selected CPU had no published run queue.
    RunQueueUnavailable = 1,
    /// Scheduler capacity or monotonic ordering space was exhausted.
    SchedulerCapacity = 2,
    /// Intrusive scheduler ownership disagreed with task lifecycle state.
    SchedulerInvariant = 3,
    /// A context-switch wake handoff token was missing or duplicated.
    HandoffCorrupt = 4,
}

/// Durable fault in allocation-free exited-task queue ownership.
///
/// The exited queue owns one strong task reference through an intrusive link
/// embedded in each task. These faults describe internal lifecycle contract
/// violations; normal task admission pressure is reported earlier by
/// [`TaskCreateError`] and cannot grow a separate exit-queue allocation.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskExitQueueFault {
    /// A task which still belonged to an exited queue was submitted again.
    DuplicateEnqueue = 1,
    /// The monotonic ownership-transfer generation could not advance.
    GenerationExhausted = 2,
    /// The queue's exact task count could not be incremented.
    LengthExhausted = 3,
    /// Embedded link state disagreed with queue membership or FIFO topology.
    CorruptLink = 4,
}

/// Failure to snapshot or replace a task name.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskNameError {
    /// String storage could not be allocated outside the task-name lock.
    OutOfMemory,
    /// The name grew between sizing and the single bounded copy attempt.
    ConcurrentMutation,
}

impl fmt::Display for TaskNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OutOfMemory => "task name allocation failed",
            Self::ConcurrentMutation => "task name changed during snapshot",
        })
    }
}

impl fmt::Display for TaskCreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidStackSize => "invalid task stack size",
            Self::OutOfMemory => "task allocation failed",
            Self::IdentifierExhausted => "task identity space exhausted",
        })
    }
}

/// Copyable ownership token for one synchronous block-wait session.
///
/// The token is intentionally not stored in a raw waker. A stale waker is
/// allowed to cause a harmless spurious wake of the task's current session,
/// while owner-only prepare/commit/end operations remain generation checked.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct BlockWaitToken(u64);

/// Failure to start a block-wait session on a task.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum BeginBlockWaitError {
    /// The task already owns an active synchronous wait session.
    Busy,
    /// The per-task wait generation cannot advance without wrapping.
    GenerationExhausted,
}

/// Action a raw waker must take after publishing a wake.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum BlockWakeAction {
    /// The task may already be blocked, so the waker must try to unblock it.
    Unblock,
    /// The block owner is committing the Running -> Blocked transition and
    /// will consume this wake itself.
    BlockOwnerWillConsume,
    /// No synchronous wait session is active; the wake is stale.
    Inactive,
}

/// Result of trying to claim the lost-wake transition before blocking.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum BlockWaitClaim {
    /// The owner exclusively holds the Running -> Blocked transition.
    Claimed,
    /// A wake was already published, so the owner must not block.
    Woken,
    /// The token no longer names the active wait session.
    Stale,
}

/// Result of publishing the Blocked state under a claimed wait transition.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum BlockWaitCommit {
    /// The task is Blocked and future wakers are responsible for enqueueing it.
    Blocked,
    /// A concurrent waker delegated its wake to the block owner. The task state
    /// has already been restored to Running.
    Woken,
    /// The token or transition ownership was invalid. The task state has been
    /// restored to Running rather than leaving an orphaned blocked task.
    Stale,
}

/// Failure to finish a block-wait session.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum EndBlockWaitError {
    /// The token no longer names the active wait session.
    Stale,
    /// The caller tried to end while its block transition was still claimed.
    TransitionInProgress,
}

const BLOCK_WAIT_ACTIVE: u64 = 1 << 0;
const BLOCK_WAIT_WOKEN: u64 = 1 << 1;
const BLOCK_WAIT_OWNER: u64 = 1 << 2;
const BLOCK_WAIT_GENERATION_SHIFT: u32 = 3;
const BLOCK_WAIT_GENERATION_MAX: u64 = u64::MAX >> BLOCK_WAIT_GENERATION_SHIFT;

struct BlockWaitState(AtomicU64);

impl BlockWaitState {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    const fn generation(state: u64) -> u64 {
        state >> BLOCK_WAIT_GENERATION_SHIFT
    }

    const fn owner_state(token: BlockWaitToken) -> u64 {
        (token.0 << BLOCK_WAIT_GENERATION_SHIFT) | BLOCK_WAIT_ACTIVE
    }

    fn begin(&self) -> Result<BlockWaitToken, BeginBlockWaitError> {
        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if observed & BLOCK_WAIT_ACTIVE != 0 {
                return Err(BeginBlockWaitError::Busy);
            }
            let generation = Self::generation(observed);
            let Some(next_generation) = generation.checked_add(1) else {
                return Err(BeginBlockWaitError::GenerationExhausted);
            };
            if next_generation > BLOCK_WAIT_GENERATION_MAX {
                return Err(BeginBlockWaitError::GenerationExhausted);
            }
            let next = (next_generation << BLOCK_WAIT_GENERATION_SHIFT) | BLOCK_WAIT_ACTIVE;
            match self
                .0
                .compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(BlockWaitToken(next_generation)),
                Err(actual) => observed = actual,
            }
        }
    }

    /// Clears a prior spurious wake before polling a future again.
    fn prepare_poll(&self, token: BlockWaitToken) -> Result<(), EndBlockWaitError> {
        let expected_owner = Self::owner_state(token);
        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if observed & !BLOCK_WAIT_WOKEN != expected_owner {
                return Err(
                    if observed & BLOCK_WAIT_OWNER != 0 && Self::generation(observed) == token.0 {
                        EndBlockWaitError::TransitionInProgress
                    } else {
                        EndBlockWaitError::Stale
                    },
                );
            }
            let next = observed & !BLOCK_WAIT_WOKEN;
            match self
                .0
                .compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(actual) => observed = actual,
            }
        }
    }

    fn mark_woken(&self) -> BlockWakeAction {
        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if observed & BLOCK_WAIT_ACTIVE == 0 {
                return BlockWakeAction::Inactive;
            }
            let next = observed | BLOCK_WAIT_WOKEN;
            match self
                .0
                .compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) if observed & BLOCK_WAIT_OWNER != 0 => {
                    return BlockWakeAction::BlockOwnerWillConsume;
                }
                Ok(_) => return BlockWakeAction::Unblock,
                Err(actual) => observed = actual,
            }
        }
    }

    fn is_woken(&self, token: BlockWaitToken) -> bool {
        let state = self.0.load(Ordering::Acquire);
        Self::generation(state) == token.0
            && state & (BLOCK_WAIT_ACTIVE | BLOCK_WAIT_WOKEN)
                == (BLOCK_WAIT_ACTIVE | BLOCK_WAIT_WOKEN)
    }

    fn claim_block(&self, token: BlockWaitToken) -> BlockWaitClaim {
        let expected = Self::owner_state(token);
        match self.0.compare_exchange(
            expected,
            expected | BLOCK_WAIT_OWNER,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => BlockWaitClaim::Claimed,
            Err(actual)
                if Self::generation(actual) == token.0
                    && actual & (BLOCK_WAIT_ACTIVE | BLOCK_WAIT_WOKEN)
                        == (BLOCK_WAIT_ACTIVE | BLOCK_WAIT_WOKEN) =>
            {
                BlockWaitClaim::Woken
            }
            Err(_) => BlockWaitClaim::Stale,
        }
    }

    /// Releases block-transition ownership. The callback restores the task's
    /// Running state before ownership becomes visible as released, so a
    /// delegated waker never observes Blocked without an enqueue owner.
    fn commit_block(
        &self,
        token: BlockWaitToken,
        mut restore_running: impl FnMut(),
    ) -> BlockWaitCommit {
        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if Self::generation(observed) != token.0
                || observed & (BLOCK_WAIT_ACTIVE | BLOCK_WAIT_OWNER)
                    != (BLOCK_WAIT_ACTIVE | BLOCK_WAIT_OWNER)
            {
                restore_running();
                return BlockWaitCommit::Stale;
            }
            if observed & BLOCK_WAIT_WOKEN != 0 {
                // State restoration precedes releasing BLOCK_WAIT_OWNER. A raw
                // waker seeing OWNER delegates to us and never enqueues.
                restore_running();
                let next = Self::owner_state(token) | BLOCK_WAIT_WOKEN;
                match self.0.compare_exchange_weak(
                    observed,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return BlockWaitCommit::Woken,
                    Err(actual) => observed = actual,
                }
            } else {
                let next = Self::owner_state(token);
                match self.0.compare_exchange_weak(
                    observed,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return BlockWaitCommit::Blocked,
                    Err(actual) => observed = actual,
                }
            }
        }
    }

    fn end(&self, token: BlockWaitToken) -> Result<(), EndBlockWaitError> {
        let mut observed = self.0.load(Ordering::Acquire);
        loop {
            if Self::generation(observed) != token.0 || observed & BLOCK_WAIT_ACTIVE == 0 {
                return Err(EndBlockWaitError::Stale);
            }
            if observed & BLOCK_WAIT_OWNER != 0 {
                return Err(EndBlockWaitError::TransitionInProgress);
            }
            let next = token.0 << BLOCK_WAIT_GENERATION_SHIFT;
            match self
                .0
                .compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(actual) => observed = actual,
            }
        }
    }
}

#[cfg(feature = "smp")]
const CPU_HANDOFF_OFF: u8 = 0;
#[cfg(feature = "smp")]
const CPU_HANDOFF_RUNNING: u8 = 1;
#[cfg(feature = "smp")]
const CPU_HANDOFF_WAKE_PENDING: u8 = 2;

/// Publication result when a blocked task is still owned by a remote CPU's
/// context-switch epilogue.
#[cfg(feature = "smp")]
pub(crate) enum WakeHandoffPublication {
    /// The old CPU now owns the strong task reference and will enqueue it.
    Deferred,
    /// The old CPU had already completed; the caller must enqueue this task.
    Ready(AxTaskRef),
    /// An impossible second handoff was already present. The caller retains
    /// the task so the failure is explicit rather than leaked or overwritten.
    Occupied(AxTaskRef),
}

/// Result returned to the old CPU when it completes a context-switch handoff.
#[cfg(feature = "smp")]
pub(crate) enum CpuHandoffCompletion {
    /// No wake raced with the context switch.
    Cleared,
    /// A remote waker delegated this owned task for publication.
    Wake(AxTaskRef),
    /// The CPU-state flag was already clear.
    AlreadyCleared,
    /// The wake state was published without its required owned task pointer.
    MissingWake,
}

struct TaskIdAllocator(AtomicU64);

impl TaskIdAllocator {
    const fn new(next: u64) -> Self {
        Self(AtomicU64::new(next))
    }

    fn allocate(&self) -> Result<TaskId, TaskCreateError> {
        self.allocate_up_to(u64::MAX)
    }

    /// Allocates one monotonic identity no greater than `maximum`.
    ///
    /// A personality with a narrower public identity type can use this before
    /// allocating any task-owned resources. Rejection does not advance the
    /// generic allocator, while an unrestricted kernel-task allocation may
    /// still consume identities above that personality's ceiling.
    fn allocate_up_to(&self, maximum: u64) -> Result<TaskId, TaskCreateError> {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                (next != 0 && next <= maximum).then(|| next.checked_add(1).unwrap_or(0))
            })
            .map(TaskId)
            .map_err(|_| TaskCreateError::IdentifierExhausted)
    }
}

/// The possible states of a task.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskState {
    /// Task is running on some CPU.
    Running = 1,
    /// Task is ready to run on some scheduler's ready queue.
    Ready = 2,
    /// Task is blocked (in the wait queue or timer list),
    /// and it has finished its scheduling process, it can be wake up by `notify()` on any run queue safely.
    Blocked = 3,
    /// Task is exited and waiting for being dropped.
    Exited = 4,
}

/// Failure to decode a raw task-state representation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct TaskStateDecodeError {
    value: u8,
}

impl TaskStateDecodeError {
    /// Returns the unknown raw representation.
    pub const fn value(self) -> u8 {
        self.value
    }
}

struct TaskAffinity {
    mask: AxCpuMask,
    #[cfg(feature = "smp")]
    pending_migration: Option<AxTaskRef>,
}

impl TaskAffinity {
    fn new(mask: AxCpuMask) -> Self {
        Self {
            mask,
            #[cfg(feature = "smp")]
            pending_migration: None,
        }
    }
}

#[cfg(feature = "smp")]
pub(crate) enum MigrationClaim {
    Allowed,
    Prepared(AxTaskRef),
    Missing,
}

/// User-defined task extended data.
#[cfg(feature = "task-ext")]
#[extern_trait::extern_trait(
    /// The impl proxy type for [`TaskExt`].
    pub AxTaskExt
)]
pub trait TaskExt {
    /// Called when the task is switched in.
    fn on_enter(&self, _task: &TaskInner) {}
    /// Called when the task is switched out.
    fn on_leave(&self, _task: &TaskInner) {}
}

/// The inner task structure.
pub struct TaskInner {
    id: TaskId,
    name: SpinNoIrq<String>,
    is_idle: bool,
    is_init: bool,

    entry: Cell<Option<Box<dyn FnOnce()>>>,
    state: AtomicU8,

    /// Serializes an unpublished runnable-publication reservation against an
    /// affinity update. The reservation fixes a destination run queue, so a
    /// concurrently published mask must not be allowed to exclude that CPU.
    /// Other task state remains protected by its existing scheduler/field
    /// ownership domains rather than by this narrow transaction word.
    mutation: AtomicU8,

    /// CPU affinity and its at-most-one pre-admitted migration helper are one
    /// publication domain. Observing a newly disallowed CPU therefore always
    /// implies that the no-allocation scheduling safe point can claim a helper.
    affinity: SpinNoIrq<TaskAffinity>,

    /// Used to indicate the CPU ID where the task is running or will run.
    cpu_id: AtomicU32,
    /// Used to indicate whether the task is running on a CPU.
    #[cfg(feature = "smp")]
    cpu_handoff: AtomicU8,
    /// At most one strong self reference delegated by a remote blocked-task
    /// wake while the old CPU completes its context-switch epilogue.
    #[cfg(feature = "smp")]
    wake_handoff: AtomicPtr<AxTask>,

    #[cfg(feature = "preempt")]
    need_resched: AtomicBool,
    #[cfg(feature = "preempt")]
    preempt_disable_count: AtomicUsize,

    /// Prevents recursive deferred-work dispatch on this task.
    deferred_work_dispatching: AtomicBool,

    interrupted: AtomicBool,
    interrupt_waker: AtomicWaker,

    /// Allocation-free, generation-checked lost-wake handshake used by
    /// synchronous future executors. Linux interruption policy stays above
    /// this generic task mechanism.
    block_wait: BlockWaitState,

    /// First terminal wake-publication fault. Zero means no fault; the first
    /// failure wins so later symptoms cannot overwrite the root cause.
    wake_fault: AtomicU8,

    /// Intrusive exited-task FIFO link. The queue turns one `Arc<AxTask>` into
    /// a raw pointer while this task is queued and reconstructs that exact
    /// ownership unit when it pops the task.
    exit_next: AtomicPtr<AxTask>,
    /// True exactly while one per-CPU exited queue owns the task's raw Arc.
    exit_queued: AtomicBool,
    /// Monotonic ownership-transfer generation; it never wraps.
    exit_queue_generation: AtomicU64,
    /// First exited-queue ownership fault. Zero means no fault.
    exit_queue_fault: AtomicU8,

    exit_code: AtomicI32,
    wait_for_exit: AtomicWaker,

    kstack: Option<TaskStack>,
    ctx: UnsafeCell<TaskContext>,

    #[cfg(feature = "task-ext")]
    task_ext: Option<AxTaskExt>,
}

impl TaskId {
    fn try_new() -> Result<Self, TaskCreateError> {
        TASK_IDS.allocate()
    }

    fn try_new_up_to(maximum: u64) -> Result<Self, TaskCreateError> {
        TASK_IDS.allocate_up_to(maximum)
    }

    /// Convert the task ID to a `u64`.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

static TASK_IDS: TaskIdAllocator = TaskIdAllocator::new(1);

impl TryFrom<u8> for TaskState {
    type Error = TaskStateDecodeError;

    #[inline]
    fn try_from(state: u8) -> Result<Self, Self::Error> {
        match state {
            1 => Ok(Self::Running),
            2 => Ok(Self::Ready),
            3 => Ok(Self::Blocked),
            4 => Ok(Self::Exited),
            value => Err(TaskStateDecodeError { value }),
        }
    }
}

unsafe impl Send for TaskInner {}
unsafe impl Sync for TaskInner {}

#[cfg(feature = "preempt")]
struct PreemptDispatchGuard<'a>(&'a TaskInner);

#[cfg(feature = "preempt")]
const MAX_PREEMPT_DISPATCH_PASSES: usize = 8;

#[cfg(feature = "preempt")]
impl<'a> PreemptDispatchGuard<'a> {
    fn new(task: &'a TaskInner) -> Self {
        task.disable_preempt();
        PreemptDispatchGuard(task)
    }
}

#[cfg(feature = "preempt")]
impl Drop for PreemptDispatchGuard<'_> {
    fn drop(&mut self) {
        // The owner drains `need_resched` iteratively. Its final decrement must
        // never re-enter the dispatcher synchronously.
        self.0.enable_preempt(false);
    }
}

impl TaskInner {
    /// Fallibly creates a new unpublished task.
    ///
    /// This constructor is intentionally fallible in the 0.1 contract: both
    /// allocation and monotonic identity exhaustion must reach the lifecycle
    /// owner before any task is published.
    pub fn new<F>(entry: F, name: String, stack_size: usize) -> Result<Self, TaskCreateError>
    where
        F: FnOnce() + Send + 'static,
    {
        Self::try_new(entry, name, stack_size)
    }

    /// Fallibly creates an unpublished task, including its entry ownership and
    /// kernel stack.
    pub fn try_new<F>(entry: F, name: String, stack_size: usize) -> Result<Self, TaskCreateError>
    where
        F: FnOnce() + Send + 'static,
    {
        Self::try_new_with_identity(entry, name, stack_size, TaskId::try_new)
    }

    /// Fallibly creates an unpublished task whose generic identity must fit a
    /// caller-owned finite identity domain.
    ///
    /// The ceiling is checked atomically against the shared monotonic allocator
    /// before allocating a stack or boxing the entry closure. This lets an OS
    /// personality reject identity exhaustion without truncation, rollback, or
    /// a second task-ID allocator.
    pub fn try_new_with_id_limit<F>(
        entry: F,
        name: String,
        stack_size: usize,
        maximum_id: u64,
    ) -> Result<Self, TaskCreateError>
    where
        F: FnOnce() + Send + 'static,
    {
        Self::try_new_with_identity(entry, name, stack_size, || {
            TaskId::try_new_up_to(maximum_id)
        })
    }

    fn try_new_with_identity<F>(
        entry: F,
        name: String,
        stack_size: usize,
        allocate_id: impl FnOnce() -> Result<TaskId, TaskCreateError>,
    ) -> Result<Self, TaskCreateError>
    where
        F: FnOnce() + Send + 'static,
    {
        let stack_size = stack_size
            .checked_add(4095)
            .map(|size| size & !4095)
            .filter(|size| *size >= MIN_KERNEL_STACK_SIZE)
            .ok_or(TaskCreateError::InvalidStackSize)?;
        let mut t = Self::new_common(allocate_id()?, name);
        debug!("new task id: {}", t.id.as_u64());
        let kstack = TaskStack::try_alloc(stack_size).map_err(|_| TaskCreateError::OutOfMemory)?;
        let entry = Box::try_new(entry).map_err(|_| TaskCreateError::OutOfMemory)?;

        let tls = VirtAddr::from(0);

        t.entry = Cell::new(Some(entry));
        t.ctx_mut()
            .init(task_entry as *const () as usize, kstack.top(), tls);
        t.kstack = Some(kstack);
        if t.name.lock().as_str() == "idle" {
            t.is_idle = true;
        }
        Ok(t)
    }

    /// Gets the ID of the task.
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// Gets the name of the task.
    pub fn name(&self) -> Result<String, TaskNameError> {
        self.try_name()
    }

    /// Fallibly snapshots the task name without allocating under its spin lock.
    pub fn try_name(&self) -> Result<String, TaskNameError> {
        let mut name = String::new();
        let required = self.name.lock().len();
        if name.capacity() < required {
            name.try_reserve_exact(required)
                .map_err(|_| TaskNameError::OutOfMemory)?;
        }
        let current = self.name.lock();
        if name.capacity() < current.len() {
            return Err(TaskNameError::ConcurrentMutation);
        }
        name.push_str(&current);
        Ok(name)
    }

    /// Set the name of the task.
    pub fn set_name(&self, name: &str) -> Result<(), TaskNameError> {
        let mut owned = String::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| TaskNameError::OutOfMemory)?;
        owned.push_str(name);
        let name = owned;
        drop(self.replace_name(name));
        Ok(())
    }

    /// Replace the task name with an already-owned string.
    ///
    /// Constructing the replacement before taking the task-name lock keeps
    /// allocator work out of the spin-locked section. Returning the previous
    /// string likewise lets callers defer its destructor until after the lock
    /// has been released.
    pub fn replace_name(&self, name: String) -> String {
        core::mem::replace(&mut *self.name.lock(), name)
    }

    /// Get a combined string of the task ID and name.
    pub fn id_name(&self) -> Result<alloc::string::String, TaskNameError> {
        use core::fmt::Write;

        let name = self.try_name()?;
        let mut description = String::new();
        description
            .try_reserve(name.len().saturating_add(32))
            .map_err(|_| TaskNameError::OutOfMemory)?;
        write!(&mut description, "Task({}, {})", self.id.as_u64(), name)
            .map_err(|_| TaskNameError::OutOfMemory)?;
        Ok(description)
    }

    /// Wait for the task to exit, and return the exit code.
    ///
    /// It will return immediately if the task has already exited (but not dropped).
    pub fn join(&self) -> Result<i32, crate::future::BlockOnError> {
        block_on(poll_fn(|cx| self.poll_join(cx)))
    }

    fn exited_code(&self) -> Option<i32> {
        (self.state() == TaskState::Exited).then(|| self.exit_code.load(Ordering::Acquire))
    }

    fn poll_join(&self, cx: &mut Context<'_>) -> Poll<i32> {
        if let Some(exit_code) = self.exited_code() {
            return Poll::Ready(exit_code);
        }

        self.register_join_waiter_and_recheck(cx)
    }

    fn register_join_waiter_and_recheck(&self, cx: &mut Context<'_>) -> Poll<i32> {
        self.wait_for_exit.register(cx.waker());

        // Exit can race between the first state check and waker publication.
        // Rechecking after registration makes both interleavings safe: either
        // the registered waker observes a later exit, or this poll observes an
        // exit whose earlier wake had no waiter to consume it.
        self.exited_code().map_or(Poll::Pending, Poll::Ready)
    }

    #[cfg(test)]
    pub(crate) fn register_join_waiter_and_recheck_for_test(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<i32> {
        self.register_join_waiter_and_recheck(cx)
    }

    /// Returns a reference to the task extended data.
    #[cfg(feature = "task-ext")]
    pub fn task_ext(&self) -> Option<&AxTaskExt> {
        self.task_ext.as_ref()
    }

    /// Returns a mutable reference to the task extended data.
    #[cfg(feature = "task-ext")]
    pub fn task_ext_mut(&mut self) -> &mut Option<AxTaskExt> {
        &mut self.task_ext
    }

    /// Returns a mutable reference to the task context.
    #[inline]
    pub const fn ctx_mut(&mut self) -> &mut TaskContext {
        self.ctx.get_mut()
    }

    /// Returns the top address of the kernel stack.
    #[inline]
    pub const fn kernel_stack_top(&self) -> Option<VirtAddr> {
        match &self.kstack {
            Some(s) => Some(s.top()),
            None => None,
        }
    }

    /// Returns the CPU ID where the task is running or will run.
    ///
    /// Note: the task may not be running on the CPU, it just exists in the run queue.
    #[inline]
    pub fn cpu_id(&self) -> u32 {
        self.cpu_id.load(Ordering::Acquire)
    }

    /// Gets the cpu affinity mask of the task.
    ///
    /// Returns the cpu affinity mask of the task in type [`AxCpuMask`].
    #[inline]
    pub fn cpumask(&self) -> AxCpuMask {
        self.affinity.lock().mask
    }

    #[cfg(feature = "sched-cfs")]
    pub(crate) fn try_reserve_publication_mutation(&self) -> bool {
        self.mutation
            .compare_exchange(
                TASK_MUTATION_IDLE,
                TASK_MUTATION_PUBLICATION,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    #[cfg(feature = "sched-cfs")]
    pub(crate) fn release_publication_mutation(&self) {
        if self
            .mutation
            .compare_exchange(
                TASK_MUTATION_PUBLICATION,
                TASK_MUTATION_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            self.record_wake_fault(TaskWakeFault::SchedulerInvariant);
        }
    }

    pub(crate) fn try_begin_affinity_mutation(&self) -> bool {
        self.mutation
            .compare_exchange(
                TASK_MUTATION_IDLE,
                TASK_MUTATION_AFFINITY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn finish_affinity_mutation(&self) {
        if self
            .mutation
            .compare_exchange(
                TASK_MUTATION_AFFINITY,
                TASK_MUTATION_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            self.record_wake_fault(TaskWakeFault::SchedulerInvariant);
        }
    }

    /// Sets the cpu affinity mask of the task.
    ///
    /// # Arguments
    /// `cpumask` - The cpu affinity mask to be set in type [`AxCpuMask`].
    #[inline]
    pub(crate) fn set_cpumask(&self, cpumask: AxCpuMask) {
        #[cfg(feature = "smp")]
        let displaced = self.publish_affinity(cpumask, None);
        #[cfg(not(feature = "smp"))]
        {
            self.affinity.lock().mask = cpumask;
        }
        #[cfg(feature = "smp")]
        drop(displaced);
    }

    /// Atomically publishes a new mask and its already allocated migration
    /// helper. The replaced helper is returned so its stack/entry are destroyed
    /// after the no-IRQ affinity lock has been released.
    #[cfg(feature = "smp")]
    pub(crate) fn publish_affinity(
        &self,
        cpumask: AxCpuMask,
        pending_migration: Option<AxTaskRef>,
    ) -> Option<AxTaskRef> {
        let mut affinity = self.affinity.lock();
        affinity.mask = cpumask;
        core::mem::replace(&mut affinity.pending_migration, pending_migration)
    }

    /// Claims the pre-admitted migration helper iff this CPU is no longer in
    /// the published mask. No allocation or destructor runs under the lock.
    #[cfg(feature = "smp")]
    pub(crate) fn claim_migration(&self, cpu_id: usize) -> MigrationClaim {
        let mut affinity = self.affinity.lock();
        if affinity.mask.get(cpu_id) {
            MigrationClaim::Allowed
        } else if let Some(task) = affinity.pending_migration.take() {
            MigrationClaim::Prepared(task)
        } else {
            MigrationClaim::Missing
        }
    }

    /// Clears only the caller's own still-pending helper. A concurrent later
    /// setaffinity publication cannot be mistaken for this token.
    #[cfg(feature = "smp")]
    pub(crate) fn clear_migration_if(&self, expected: &AxTaskRef) -> Option<AxTaskRef> {
        let mut affinity = self.affinity.lock();
        if affinity
            .pending_migration
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(pending, expected))
        {
            affinity.pending_migration.take()
        } else {
            None
        }
    }

    /// Retires a helper which became unnecessary before it was claimed. The
    /// returned task must be dropped by a normal task-context caller after this
    /// lock has been released.
    #[cfg(feature = "smp")]
    pub(crate) fn take_allowed_migration(&self, cpu_id: usize) -> Option<AxTaskRef> {
        let mut affinity = self.affinity.lock();
        affinity
            .mask
            .get(cpu_id)
            .then(|| affinity.pending_migration.take())
            .flatten()
    }

    /// Polls whether the task has been interrupted.
    #[inline]
    pub fn poll_interrupt(&self, cx: &Context) -> Poll<()> {
        if self.interrupted.swap(false, Ordering::AcqRel) {
            Poll::Ready(())
        } else {
            self.interrupt_waker.register(cx.waker());
            if self.interrupted.swap(false, Ordering::AcqRel) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    /// Clears the interrupt state of the task.
    #[inline]
    pub fn clear_interrupt(&self) {
        self.interrupted.store(false, Ordering::Release);
    }

    /// Returns whether the task has a pending interrupt wakeup.
    #[inline]
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Acquire)
    }

    /// Interrupts the task.
    #[inline]
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
        self.interrupt_waker.wake();
    }
}

// private methods
impl TaskInner {
    fn new_common(id: TaskId, name: String) -> Self {
        Self {
            id,
            name: SpinNoIrq::new(name),
            is_idle: false,
            is_init: false,
            entry: Cell::new(None),
            state: AtomicU8::new(TaskState::Ready as u8),
            mutation: AtomicU8::new(TASK_MUTATION_IDLE),
            // By default, the task is allowed to run on all CPUs.
            affinity: SpinNoIrq::new(TaskAffinity::new(crate::api::cpu_mask_full())),
            cpu_id: AtomicU32::new(0),
            #[cfg(feature = "smp")]
            cpu_handoff: AtomicU8::new(CPU_HANDOFF_OFF),
            #[cfg(feature = "smp")]
            wake_handoff: AtomicPtr::new(core::ptr::null_mut()),
            #[cfg(feature = "preempt")]
            need_resched: AtomicBool::new(false),
            #[cfg(feature = "preempt")]
            preempt_disable_count: AtomicUsize::new(0),
            deferred_work_dispatching: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            interrupt_waker: AtomicWaker::new(),
            block_wait: BlockWaitState::new(),
            wake_fault: AtomicU8::new(0),
            exit_next: AtomicPtr::new(core::ptr::null_mut()),
            exit_queued: AtomicBool::new(false),
            exit_queue_generation: AtomicU64::new(0),
            exit_queue_fault: AtomicU8::new(0),
            exit_code: AtomicI32::new(0),
            wait_for_exit: AtomicWaker::new(),
            kstack: None,
            ctx: UnsafeCell::new(TaskContext::new()),
            #[cfg(feature = "task-ext")]
            task_ext: None,
        }
    }

    /// Creates an "init task" using the current CPU states, to use as the
    /// current task.
    ///
    /// As it is the current task, no other task can switch to it until it
    /// switches out.
    ///
    /// And there is no need to set the `entry`, `kstack` or `tls` fields, as
    /// they will be filled automatically when the task is switches out.
    pub(crate) fn new_init(name: String) -> Result<Self, TaskCreateError> {
        let mut t = Self::new_common(TaskId::try_new()?, name);
        t.is_init = true;
        #[cfg(feature = "smp")]
        t.mark_running_on_cpu();
        if t.name.lock().as_str() == "idle" {
            t.is_idle = true;
        }
        Ok(t)
    }

    pub(crate) fn into_arc(self) -> Result<AxTaskRef, TaskCreateError> {
        Arc::try_new(AxTask::new(self)).map_err(|_| TaskCreateError::OutOfMemory)
    }

    /// Fallibly constructs the scheduler-owned task object without making it
    /// runnable. Lifecycle code can therefore complete every other admission
    /// step before publishing a user-visible handle to the task.
    pub(crate) fn try_into_arc(self) -> Result<AxTaskRef, TaskCreateError> {
        self.into_arc()
    }

    /// Returns the current state of the task.
    #[inline]
    pub fn state(&self) -> TaskState {
        let raw = self.state.load(Ordering::Acquire);
        match TaskState::try_from(raw) {
            Ok(state) => state,
            Err(_) => {
                // The atomic is private and every writer stores a valid enum
                // discriminant. Treat memory corruption as terminal and leave
                // a durable diagnostic instead of exposing a public panic.
                self.record_wake_fault(TaskWakeFault::SchedulerInvariant);
                TaskState::Exited
            }
        }
    }

    #[inline]
    pub(crate) fn set_state(&self, state: TaskState) {
        self.state.store(state as u8, Ordering::Release)
    }

    /// Transition the task state from `current_state` to `new_state`,
    /// Returns `true` if the current state is `current_state` and the state is successfully set to `new_state`,
    /// otherwise returns `false`.
    #[inline]
    pub(crate) fn transition_state(&self, current_state: TaskState, new_state: TaskState) -> bool {
        self.state
            .compare_exchange(
                current_state as u8,
                new_state as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    #[inline]
    pub(crate) fn is_running(&self) -> bool {
        matches!(self.state(), TaskState::Running)
    }

    #[inline]
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self.state(), TaskState::Ready)
    }

    pub(crate) fn try_enter_deferred_work(&self) -> bool {
        self.deferred_work_dispatching
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn leave_deferred_work(&self) {
        self.deferred_work_dispatching
            .store(false, Ordering::Release);
    }

    #[inline]
    pub(crate) const fn is_init(&self) -> bool {
        self.is_init
    }

    #[inline]
    pub(crate) const fn is_idle(&self) -> bool {
        self.is_idle
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn set_preempt_pending(&self, pending: bool) {
        self.need_resched.store(pending, Ordering::Release)
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn take_preempt_pending(&self) -> bool {
        self.need_resched.swap(false, Ordering::AcqRel)
    }

    #[inline]
    #[cfg(all(feature = "preempt", feature = "smp"))]
    pub(crate) fn preserve_preempt_if_cpu_disallowed(&self, cpu_id: usize) {
        if !self.cpumask().get(cpu_id) {
            self.set_preempt_pending(true);
        }
    }

    #[inline]
    #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
    pub(crate) fn preempt_pending(&self) -> bool {
        self.need_resched.load(Ordering::Acquire)
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn can_preempt(&self, current_disable_count: usize) -> bool {
        self.preempt_disable_count.load(Ordering::Acquire) == current_disable_count
    }

    #[inline]
    #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
    pub(crate) fn preempt_disable_count(&self) -> usize {
        self.preempt_disable_count.load(Ordering::Acquire)
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn disable_preempt(&self) {
        if self
            .preempt_disable_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .is_err()
        {
            self.record_wake_fault(TaskWakeFault::SchedulerInvariant);
        }
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn enable_preempt(&self, resched: bool) {
        match self.preempt_disable_count.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |count| count.checked_sub(1),
        ) {
            Ok(1) if resched && self.need_resched.load(Ordering::Acquire) => {
                // If current task is pending to be preempted, do rescheduling.
                Self::current_check_preempt_pending();
            }
            Ok(_) => {}
            Err(_) => {
                self.record_wake_fault(TaskWakeFault::SchedulerInvariant);
            }
        }
    }

    #[cfg(feature = "preempt")]
    pub(crate) fn current_check_preempt_pending() {
        Self::current_check_preempt_pending_from(false);
    }

    #[cfg(all(feature = "preempt", feature = "irq-exit"))]
    pub(crate) fn current_check_preempt_pending_at_irq_exit() {
        Self::current_check_preempt_pending_from(true);
    }

    #[cfg(feature = "preempt")]
    fn current_check_preempt_pending_from(_at_irq_exit: bool) {
        use kernel_guard::IrqSave;
        #[cfg(feature = "irq-exit")]
        {
            #[cfg(target_os = "none")]
            let irqs_enabled = axhal::asm::irqs_enabled();
            #[cfg(not(target_os = "none"))]
            let irqs_enabled = true;
            if !crate::irq_exit::may_check_preempt(
                crate::irq_exit::in_irq_context(),
                irqs_enabled,
                _at_irq_exit,
            ) {
                return;
            }
        }
        let curr = crate::current();
        #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
        let irq_off = !axhal::asm::irqs_enabled();
        #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
        if irq_off {
            let mut flags = 0;
            if curr.is_idle() {
                flags |= crate::irq_continuation_diagnostics::FLAG_IDLE;
            }
            if curr.preempt_pending() {
                flags |= crate::irq_continuation_diagnostics::FLAG_NEED_RESCHED;
            }
            // Ordinary IRQ-off task safe points returned above. Reaching this
            // event with IRQs masked therefore proves the explicit outermost
            // IRQ-exit transport authorized this check.
            flags |= crate::irq_continuation_diagnostics::FLAG_RESCHED_ALLOWED;
            crate::irq_continuation_diagnostics::record_event(
                crate::irq_continuation_diagnostics::EVENT_PREEMPT_CHECK_IRQ_OFF,
                curr.id().as_u64(),
                0,
                flags,
                curr.preempt_disable_count(),
            );
        }
        let mut dispatched = false;
        if curr.need_resched.load(Ordering::Acquire) && curr.can_preempt(0) {
            // Keep one task-owned preemption-disable unit across every switch.
            // A task selected while this stack is suspended has its own count,
            // unlike a per-CPU IRQ-exit marker. The no-resched release also
            // prevents a guard drop from recursively growing this stack.
            {
                let _dispatch = PreemptDispatchGuard::new(&curr);
                let mut passes = 0;
                while passes < MAX_PREEMPT_DISPATCH_PASSES
                    && curr.need_resched.load(Ordering::Acquire)
                    && curr.can_preempt(1)
                {
                    let mut rq = crate::current_run_queue::<IrqSave>();
                    if !curr.need_resched.load(Ordering::Acquire) {
                        break;
                    }
                    passes += 1;
                    rq.preempt_resched();
                    dispatched = true;
                }
            }
        }
        if dispatched {
            // The task-owned dispatch guard has been released. The deferred
            // worker performs its own IRQ/preemption checks, so an IRQ-exit
            // caller leaves the work pending for a later task-context point.
            crate::run_deferred_work();
        }
        #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
        if irq_off && !axhal::asm::irqs_enabled() {
            let mut flags = 0;
            if curr.is_idle() {
                flags |= crate::irq_continuation_diagnostics::FLAG_IDLE;
            }
            if curr.preempt_pending() {
                flags |= crate::irq_continuation_diagnostics::FLAG_NEED_RESCHED;
            }
            crate::irq_continuation_diagnostics::record_event(
                crate::irq_continuation_diagnostics::EVENT_PREEMPT_CHECK_RETURN_IRQ_OFF,
                curr.id().as_u64(),
                0,
                flags,
                curr.preempt_disable_count(),
            );
        }
    }

    /// Notify all tasks that join on this task.
    pub(crate) fn notify_exit(&self, exit_code: i32) {
        self.exit_code.store(exit_code, Ordering::Release);
        // Publish the exit code before Exited. An Acquire state observation in
        // join() must never pair the terminal state with the old exit code.
        self.set_state(TaskState::Exited);
        self.wait_for_exit.wake();
    }

    #[inline]
    pub(crate) const unsafe fn ctx_mut_ptr(&self) -> *mut TaskContext {
        self.ctx.get()
    }

    /// Remove and return the kernel stack owned by this task, if any.
    pub(crate) fn take_kernel_stack(&mut self) -> Option<TaskStack> {
        self.kstack.take()
    }

    /// Set the CPU ID where the task is running or will run.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn set_cpu_id(&self, cpu_id: u32) {
        self.cpu_id.store(cpu_id, Ordering::Release);
    }

    /// Marks the task as owned by a CPU's context-switch lifecycle.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn mark_running_on_cpu(&self) {
        self.cpu_handoff
            .store(CPU_HANDOFF_RUNNING, Ordering::Release);
    }

    /// Delegates one owned task reference to the old CPU instead of spinning
    /// while its context-switch epilogue still owns the task.
    ///
    /// The pointer is published before the state CAS. If the old CPU clears
    /// first it observes `RUNNING`, while our CAS observes `OFF` and returns the
    /// same reference to the caller. If our CAS wins, the old CPU observes
    /// `WAKE_PENDING` and takes the reference. Thus exactly one side owns the
    /// enqueue without a polling loop.
    #[cfg(feature = "smp")]
    pub(crate) fn publish_wake_handoff(&self, task: AxTaskRef) -> WakeHandoffPublication {
        let raw = Arc::into_raw(task).cast_mut();
        if self
            .wake_handoff
            .compare_exchange(
                core::ptr::null_mut(),
                raw,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_err()
        {
            // Safety: this CAS failed, so the new raw pointer was never
            // published and remains the caller's one Arc ownership unit.
            return WakeHandoffPublication::Occupied(unsafe { Arc::from_raw(raw) });
        }

        match self.cpu_handoff.compare_exchange(
            CPU_HANDOFF_RUNNING,
            CPU_HANDOFF_WAKE_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => WakeHandoffPublication::Deferred,
            Err(CPU_HANDOFF_OFF) => {
                let task = self.take_wake_handoff();
                match task {
                    Some(task) => WakeHandoffPublication::Ready(task),
                    None => {
                        // The old CPU saw WAKE_PENDING and consumed ownership
                        // between our failed CAS and this take.
                        WakeHandoffPublication::Deferred
                    }
                }
            }
            Err(_) => {
                let task = self.take_wake_handoff();
                match task {
                    Some(task) => WakeHandoffPublication::Occupied(task),
                    None => WakeHandoffPublication::Deferred,
                }
            }
        }
    }

    #[cfg(feature = "smp")]
    fn take_wake_handoff(&self) -> Option<AxTaskRef> {
        let raw = self
            .wake_handoff
            .swap(core::ptr::null_mut(), Ordering::AcqRel);
        if raw.is_null() {
            None
        } else {
            // Safety: every non-null value came from exactly one Arc::into_raw
            // above, and swap gives this caller exclusive reconstruction.
            Some(unsafe { Arc::from_raw(raw) })
        }
    }

    /// Completes old-CPU ownership and claims a delegated remote wake, if any.
    #[cfg(feature = "smp")]
    pub(crate) fn finish_cpu_handoff(&self) -> CpuHandoffCompletion {
        match self.cpu_handoff.swap(CPU_HANDOFF_OFF, Ordering::AcqRel) {
            CPU_HANDOFF_RUNNING => CpuHandoffCompletion::Cleared,
            CPU_HANDOFF_WAKE_PENDING => self.take_wake_handoff().map_or(
                CpuHandoffCompletion::MissingWake,
                CpuHandoffCompletion::Wake,
            ),
            CPU_HANDOFF_OFF => CpuHandoffCompletion::AlreadyCleared,
            _ => CpuHandoffCompletion::MissingWake,
        }
    }

    /// Starts one allocation-free synchronous block-wait session.
    pub(crate) fn begin_block_wait(&self) -> Result<BlockWaitToken, BeginBlockWaitError> {
        self.block_wait.begin()
    }

    /// Clears a prior spurious wake before the owner polls its future.
    pub(crate) fn prepare_block_poll(
        &self,
        token: BlockWaitToken,
    ) -> Result<(), EndBlockWaitError> {
        self.block_wait.prepare_poll(token)
    }

    /// Publishes a wake without requiring a token in raw-waker storage.
    pub(crate) fn mark_block_woken(&self) -> BlockWakeAction {
        self.block_wait.mark_woken()
    }

    /// Returns whether this generation has a published wake.
    pub(crate) fn is_block_woken(&self, token: BlockWaitToken) -> bool {
        self.block_wait.is_woken(token)
    }

    /// Claims exclusive ownership of the Running -> Blocked transition.
    pub(crate) fn claim_block_wait(&self, token: BlockWaitToken) -> BlockWaitClaim {
        self.block_wait.claim_block(token)
    }

    /// Commits a previously claimed Blocked state or consumes a racing wake.
    pub(crate) fn commit_block_wait(&self, token: BlockWaitToken) -> BlockWaitCommit {
        self.block_wait
            .commit_block(token, || self.set_state(TaskState::Running))
    }

    /// Ends a generation-checked synchronous block-wait session.
    pub(crate) fn end_block_wait(&self, token: BlockWaitToken) -> Result<(), EndBlockWaitError> {
        self.block_wait.end(token)
    }

    /// Records the first terminal wake-publication fault for diagnostics and
    /// lifecycle containment. Returns `true` when this call installed it.
    pub(crate) fn record_wake_fault(&self, fault: TaskWakeFault) -> bool {
        self.wake_fault
            .compare_exchange(0, fault as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Returns a durable terminal wake-publication fault, if any.
    pub fn wake_fault(&self) -> Option<TaskWakeFault> {
        match self.wake_fault.load(Ordering::Acquire) {
            0 => None,
            1 => Some(TaskWakeFault::RunQueueUnavailable),
            2 => Some(TaskWakeFault::SchedulerCapacity),
            3 => Some(TaskWakeFault::SchedulerInvariant),
            4 => Some(TaskWakeFault::HandoffCorrupt),
            _ => Some(TaskWakeFault::SchedulerInvariant),
        }
    }

    /// Claims this task's embedded link for one exited-queue ownership unit.
    ///
    /// Admission is performed before `Arc::into_raw`, so every error leaves
    /// the caller with ordinary owned `Arc` storage and the queue unchanged.
    pub(crate) fn admit_exit_queue(&self) -> Result<(), TaskExitQueueFault> {
        if self
            .exit_queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let fault = TaskExitQueueFault::DuplicateEnqueue;
            self.record_exit_queue_fault(fault);
            return Err(fault);
        }

        if !self.exit_next.load(Ordering::Acquire).is_null() {
            self.exit_queued.store(false, Ordering::Release);
            let fault = TaskExitQueueFault::CorruptLink;
            self.record_exit_queue_fault(fault);
            return Err(fault);
        }

        if self
            .exit_queue_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .is_err()
        {
            self.exit_queued.store(false, Ordering::Release);
            let fault = TaskExitQueueFault::GenerationExhausted;
            self.record_exit_queue_fault(fault);
            return Err(fault);
        }

        Ok(())
    }

    /// Releases a queue admission which has not yet been linked or published.
    pub(crate) fn rollback_exit_queue_admission(&self) {
        debug_assert!(self.exit_next.load(Ordering::Acquire).is_null());
        self.exit_queued.store(false, Ordering::Release);
    }

    /// Links the next queue-owned raw Arc after this task.
    pub(crate) fn link_exit_queue_successor(
        &self,
        successor: *mut AxTask,
    ) -> Result<(), TaskExitQueueFault> {
        if successor.is_null() || !self.exit_queued.load(Ordering::Acquire) {
            let fault = TaskExitQueueFault::CorruptLink;
            self.record_exit_queue_fault(fault);
            return Err(fault);
        }
        self.exit_next
            .compare_exchange(
                core::ptr::null_mut(),
                successor,
                Ordering::Release,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| {
                let fault = TaskExitQueueFault::CorruptLink;
                self.record_exit_queue_fault(fault);
                fault
            })
    }

    /// Detaches and returns the next queue-owned raw Arc.
    pub(crate) fn take_exit_queue_successor(&self) -> *mut AxTask {
        self.exit_next.swap(core::ptr::null_mut(), Ordering::AcqRel)
    }

    /// Ends this task's exited-queue membership after its raw Arc was popped.
    pub(crate) fn finish_exit_dequeue(&self) -> Result<(), TaskExitQueueFault> {
        self.exit_queued
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| {
                let fault = TaskExitQueueFault::CorruptLink;
                self.record_exit_queue_fault(fault);
                fault
            })
    }

    /// Records the first exited-queue ownership fault.
    pub(crate) fn record_exit_queue_fault(&self, fault: TaskExitQueueFault) -> bool {
        self.exit_queue_fault
            .compare_exchange(0, fault as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Returns the first durable exited-queue ownership fault, if any.
    pub fn exit_queue_fault(&self) -> Option<TaskExitQueueFault> {
        match self.exit_queue_fault.load(Ordering::Acquire) {
            0 => None,
            1 => Some(TaskExitQueueFault::DuplicateEnqueue),
            2 => Some(TaskExitQueueFault::GenerationExhausted),
            3 => Some(TaskExitQueueFault::LengthExhausted),
            4 => Some(TaskExitQueueFault::CorruptLink),
            _ => Some(TaskExitQueueFault::CorruptLink),
        }
    }

    #[cfg(test)]
    pub(crate) fn exit_queue_generation_for_test(&self) -> u64 {
        self.exit_queue_generation.load(Ordering::Acquire)
    }
}

impl fmt::Debug for TaskInner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TaskInner")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("state", &self.state())
            .finish()
    }
}

impl Drop for TaskInner {
    fn drop(&mut self) {
        debug!("task drop: id={}", self.id.as_u64());
    }
}

pub(crate) struct TaskStack {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl TaskStack {
    pub fn try_alloc(size: usize) -> Result<Self, AllocError> {
        if size == 0 {
            return Err(AllocError);
        }
        let layout = Layout::from_size_align(size, 16).map_err(|_| AllocError)?;
        if let Some(stack) = crate::run_queue::take_cached_task_stack(layout.size(), layout.align())
        {
            return Ok(stack);
        }
        Ok(Self {
            ptr: NonNull::new(unsafe { alloc::alloc::alloc(layout) }).ok_or(AllocError)?,
            layout,
        })
    }

    pub const fn top(&self) -> VirtAddr {
        unsafe { core::mem::transmute(self.ptr.as_ptr().add(self.layout.size())) }
    }

    pub(crate) const fn layout_size(&self) -> usize {
        self.layout.size()
    }

    pub(crate) const fn layout_align(&self) -> usize {
        self.layout.align()
    }

    pub(crate) fn scrub_for_cache(&mut self) {
        // Kernel stacks are not user-readable and are overwritten as the next
        // task runs. Avoid a full-stack zero fill on every thread exit; code
        // that exposes kernel memory must initialize the data it copies out.
        let _ = self;
    }
}

impl Drop for TaskStack {
    fn drop(&mut self) {
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// An owned handle to the task which was current when the handle was created.
///
/// The per-CPU current-task slot owns a separate strong reference. Cloning that
/// reference here is required because a safe caller may retain this handle
/// across a context switch or even across reclamation of the task's scheduler
/// state. A non-owning, lifetime-free wrapper around the per-CPU raw pointer
/// would otherwise become dangling while it was still usable from safe Rust.
pub struct CurrentTask(AxTaskRef);

impl CurrentTask {
    pub(crate) fn try_get() -> Option<Self> {
        let ptr: *const super::AxTask = axhal::percpu::current_task_ptr();
        if !ptr.is_null() {
            // SAFETY: the non-null per-CPU pointer owns one strong reference.
            // Increment it before reconstructing an independently owned Arc for
            // the returned handle; the per-CPU ownership remains untouched.
            unsafe { Arc::increment_strong_count(ptr) };
            Some(Self(unsafe { AxTaskRef::from_raw(ptr) }))
        } else {
            None
        }
    }

    pub(crate) fn get() -> Self {
        Self::try_get().expect("current task is uninitialized")
    }

    /// Clone the inner `AxTaskRef`.
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> AxTaskRef {
        self.0.clone()
    }

    /// Returns `true` if the current task is the same as `other`.
    pub fn ptr_eq(&self, other: &AxTaskRef) -> bool {
        Arc::ptr_eq(&self.0, other)
    }

    pub(crate) unsafe fn init_current(init_task: AxTaskRef) {
        assert!(init_task.is_init());
        let ptr = Arc::into_raw(init_task);
        unsafe {
            axhal::percpu::set_current_task_ptr(ptr);
        }
    }

    pub(crate) unsafe fn set_current(prev: Self, next: AxTaskRef) {
        let previous_raw: *const super::AxTask = axhal::percpu::current_task_ptr();
        debug_assert_eq!(previous_raw, Arc::as_ptr(&prev.0));
        let ptr = Arc::into_raw(next);
        unsafe {
            axhal::percpu::set_current_task_ptr(ptr);
        };

        // SAFETY: `previous_raw` is the distinct strong reference owned by the
        // per-CPU current-task slot. Replacing the slot above transfers that
        // ownership out exactly once. `prev` is an additional owned handle and
        // is dropped independently at the end of this function.
        drop(unsafe { AxTaskRef::from_raw(previous_raw) });
        drop(prev);
    }
}

impl Deref for CurrentTask {
    type Target = AxTaskRef;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

extern "C" fn task_entry() -> ! {
    #[cfg(feature = "smp")]
    unsafe {
        // Clear the prev task on CPU before running the task entry function.
        crate::run_queue::clear_prev_task_on_cpu();
    }
    // Enable irq (if feature "irq" is enabled) before running the task entry function.
    #[cfg(feature = "irq")]
    axhal::asm::enable_irqs();
    #[cfg(feature = "smp")]
    {
        let task = crate::current();
        let retired = task.take_allowed_migration(axhal::percpu::this_cpu_id());
        drop(retired);
    }
    crate::run_deferred_work();
    let entry = {
        let task = crate::current();
        task.entry.take()
    };
    if let Some(entry) = entry {
        entry()
    }
    crate::exit(0);
}

#[cfg(test)]
mod mechanism_tests {
    use super::*;

    #[test]
    fn task_identity_allocates_max_once_and_never_wraps() {
        let allocator = TaskIdAllocator::new(u64::MAX);
        assert_eq!(allocator.allocate().unwrap().as_u64(), u64::MAX);
        assert_eq!(
            allocator.allocate(),
            Err(TaskCreateError::IdentifierExhausted)
        );
        assert_eq!(
            allocator.allocate(),
            Err(TaskCreateError::IdentifierExhausted)
        );
    }

    #[test]
    fn bounded_task_identity_rejects_without_advancing_generic_space() {
        let maximum = i32::MAX as u64;
        let allocator = TaskIdAllocator::new(maximum);

        assert_eq!(allocator.allocate_up_to(maximum).unwrap().as_u64(), maximum);
        assert_eq!(
            allocator.allocate_up_to(maximum),
            Err(TaskCreateError::IdentifierExhausted)
        );
        assert_eq!(allocator.allocate().unwrap().as_u64(), maximum + 1);

        let zero = TaskIdAllocator::new(0);
        assert_eq!(
            zero.allocate_up_to(maximum),
            Err(TaskCreateError::IdentifierExhausted)
        );
    }

    #[test]
    fn bounded_task_rejection_drops_unpublished_entry_ownership() {
        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let captured = DropProbe(dropped.clone());
        let allocator = TaskIdAllocator::new(i32::MAX as u64 + 1);
        let result = TaskInner::try_new_with_identity(
            move || drop(captured),
            "linux-id-exhausted".into(),
            MIN_KERNEL_STACK_SIZE,
            || allocator.allocate_up_to(i32::MAX as u64),
        );

        assert!(matches!(result, Err(TaskCreateError::IdentifierExhausted)));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn raw_task_state_decode_is_typed_and_private_corruption_is_contained() {
        assert_eq!(TaskState::try_from(0).unwrap_err().value(), 0);
        assert_eq!(TaskState::try_from(5).unwrap_err().value(), 5);

        let task = TaskInner::new_init("state-corrupt".into()).unwrap();
        task.state.store(0, Ordering::Release);
        assert_eq!(task.state(), TaskState::Exited);
        assert_eq!(task.wake_fault(), Some(TaskWakeFault::SchedulerInvariant));
    }

    #[test]
    fn block_wait_owner_closes_wake_before_block_race() {
        let state = BlockWaitState::new();
        let token = state.begin().unwrap();
        assert_eq!(state.begin(), Err(BeginBlockWaitError::Busy));

        state.prepare_poll(token).unwrap();
        assert_eq!(state.claim_block(token), BlockWaitClaim::Claimed);
        assert_eq!(state.mark_woken(), BlockWakeAction::BlockOwnerWillConsume);

        let mut restored = false;
        assert_eq!(
            state.commit_block(token, || restored = true),
            BlockWaitCommit::Woken
        );
        assert!(restored);
        assert!(state.is_woken(token));
        state.end(token).unwrap();
    }

    #[test]
    fn block_wait_consumes_wake_published_before_claim() {
        let state = BlockWaitState::new();
        let token = state.begin().unwrap();

        state.prepare_poll(token).unwrap();
        assert_eq!(state.mark_woken(), BlockWakeAction::Unblock);
        assert_eq!(state.claim_block(token), BlockWaitClaim::Woken);
        state.end(token).unwrap();
    }

    #[test]
    fn block_wait_publishes_wake_after_block_commit() {
        let state = BlockWaitState::new();
        let token = state.begin().unwrap();

        state.prepare_poll(token).unwrap();
        assert_eq!(state.claim_block(token), BlockWaitClaim::Claimed);
        assert_eq!(state.commit_block(token, || {}), BlockWaitCommit::Blocked);
        assert_eq!(state.mark_woken(), BlockWakeAction::Unblock);
        assert!(state.is_woken(token));
        state.end(token).unwrap();
    }

    #[test]
    fn block_wait_rejects_stale_owner_but_allows_spurious_raw_wake() {
        let state = BlockWaitState::new();
        let old = state.begin().unwrap();
        state.end(old).unwrap();
        assert_eq!(state.mark_woken(), BlockWakeAction::Inactive);

        let current = state.begin().unwrap();
        assert_ne!(old, current);
        assert_eq!(state.prepare_poll(old), Err(EndBlockWaitError::Stale));
        assert_eq!(state.mark_woken(), BlockWakeAction::Unblock);
        assert!(state.is_woken(current));
        state.end(current).unwrap();
    }

    #[test]
    fn block_wait_generation_exhaustion_is_explicit() {
        let state = BlockWaitState(AtomicU64::new(
            BLOCK_WAIT_GENERATION_MAX << BLOCK_WAIT_GENERATION_SHIFT,
        ));
        assert_eq!(state.begin(), Err(BeginBlockWaitError::GenerationExhausted));
    }

    #[test]
    fn first_wake_fault_is_durable() {
        let task = TaskInner::new_init("wake-fault".into()).unwrap();
        assert!(task.record_wake_fault(TaskWakeFault::SchedulerCapacity));
        assert!(!task.record_wake_fault(TaskWakeFault::HandoffCorrupt));
        assert_eq!(task.wake_fault(), Some(TaskWakeFault::SchedulerCapacity));
    }

    #[cfg(feature = "preempt")]
    #[test]
    fn preempt_guard_counter_never_wraps_on_internal_mismatch() {
        let underflow = TaskInner::new_init("preempt-underflow".into()).unwrap();
        underflow.enable_preempt(false);
        assert_eq!(underflow.preempt_disable_count.load(Ordering::Acquire), 0);
        assert_eq!(
            underflow.wake_fault(),
            Some(TaskWakeFault::SchedulerInvariant)
        );

        let overflow = TaskInner::new_init("preempt-overflow".into()).unwrap();
        overflow
            .preempt_disable_count
            .store(usize::MAX, Ordering::Release);
        overflow.disable_preempt();
        assert_eq!(
            overflow.preempt_disable_count.load(Ordering::Acquire),
            usize::MAX
        );
        assert_eq!(
            overflow.wake_fault(),
            Some(TaskWakeFault::SchedulerInvariant)
        );
    }

    #[cfg(feature = "preempt")]
    #[test]
    fn preempt_dispatch_guard_is_task_owned_and_releases_without_resched() {
        let owner = TaskInner::new_init("preempt-dispatch-owner".into()).unwrap();
        let peer = TaskInner::new_init("preempt-dispatch-peer".into()).unwrap();
        owner.set_preempt_pending(true);

        {
            let _dispatch = PreemptDispatchGuard::new(&owner);
            assert!(owner.can_preempt(1));
            assert!(peer.can_preempt(0));

            // Model a nested guard release while the dispatcher owns the task.
            owner.disable_preempt();
            assert!(owner.can_preempt(2));
            owner.enable_preempt(true);
            assert!(owner.can_preempt(1));
        }

        assert!(owner.can_preempt(0));
        assert!(owner.need_resched.load(Ordering::Acquire));
    }

    #[cfg(feature = "preempt")]
    #[test]
    fn selected_task_clear_does_not_consume_a_later_publication() {
        let selected = TaskInner::new_init("selected-preempt-publication".into()).unwrap();
        selected.set_preempt_pending(true);

        assert!(selected.take_preempt_pending());
        assert!(!selected.need_resched.load(Ordering::Acquire));

        // Model a remote affinity/wake publisher after the scheduler has
        // removed this task from its ready queue and released the scheduler
        // lock. The later publication must remain pending for the task.
        selected.set_preempt_pending(true);
        assert!(selected.need_resched.load(Ordering::Acquire));
    }

    #[cfg(all(feature = "preempt", feature = "smp"))]
    #[test]
    fn selected_task_preserves_an_already_published_affinity_exclusion() {
        let selected = TaskInner::new_init("selected-affinity-publication".into()).unwrap();
        // The host test axconfig has one CPU, so an empty mask is used only to
        // model the local "CPU 0 excluded" predicate. Public SMP admission
        // still rejects empty masks and requires another online target.
        selected.set_cpumask(AxCpuMask::new());
        selected.set_preempt_pending(true);

        assert!(selected.take_preempt_pending());
        selected.preserve_preempt_if_cpu_disallowed(0);

        assert!(selected.need_resched.load(Ordering::Acquire));
    }

    #[test]
    fn exited_queue_generation_exhaustion_is_explicit_and_rolls_back() {
        let task = TaskInner::new_init("exit-generation".into()).unwrap();
        task.exit_queue_generation
            .store(u64::MAX, Ordering::Release);

        assert_eq!(
            task.admit_exit_queue(),
            Err(TaskExitQueueFault::GenerationExhausted)
        );
        assert_eq!(
            task.exit_queue_fault(),
            Some(TaskExitQueueFault::GenerationExhausted)
        );
        assert!(!task.exit_queued.load(Ordering::Acquire));
    }

    #[test]
    fn exited_queue_stale_link_is_typed_and_durable() {
        let task = TaskInner::new_init("exit-link".into()).unwrap();
        let stale = NonNull::<AxTask>::dangling().as_ptr();
        task.exit_next.store(stale, Ordering::Release);

        assert_eq!(
            task.admit_exit_queue(),
            Err(TaskExitQueueFault::CorruptLink)
        );
        assert_eq!(
            task.exit_queue_fault(),
            Some(TaskExitQueueFault::CorruptLink)
        );
        assert!(!task.exit_queued.load(Ordering::Acquire));

        // The deliberately injected pointer was never an Arc ownership unit.
        // Clear it before the test object is dropped.
        task.exit_next
            .store(core::ptr::null_mut(), Ordering::Release);
    }

    #[cfg(feature = "smp")]
    #[test]
    fn remote_wake_handoff_transfers_exactly_one_arc() {
        let task = TaskInner::new_init("handoff".into())
            .unwrap()
            .into_arc()
            .unwrap();
        match task.publish_wake_handoff(task.clone()) {
            WakeHandoffPublication::Deferred => {}
            WakeHandoffPublication::Ready(_) | WakeHandoffPublication::Occupied(_) => {
                panic!("running task must delegate its wake to the old CPU")
            }
        }
        match task.finish_cpu_handoff() {
            CpuHandoffCompletion::Wake(owned) => assert!(Arc::ptr_eq(&owned, &task)),
            CpuHandoffCompletion::Cleared
            | CpuHandoffCompletion::AlreadyCleared
            | CpuHandoffCompletion::MissingWake => panic!("delegated wake ownership was lost"),
        }
        assert!(matches!(
            task.finish_cpu_handoff(),
            CpuHandoffCompletion::AlreadyCleared
        ));
    }

    #[cfg(feature = "smp")]
    #[test]
    fn completed_cpu_handoff_returns_wake_to_publisher() {
        let task = TaskInner::new_init("handoff-complete".into())
            .unwrap()
            .into_arc()
            .unwrap();
        assert!(matches!(
            task.finish_cpu_handoff(),
            CpuHandoffCompletion::Cleared
        ));
        match task.publish_wake_handoff(task.clone()) {
            WakeHandoffPublication::Ready(owned) => assert!(Arc::ptr_eq(&owned, &task)),
            WakeHandoffPublication::Deferred | WakeHandoffPublication::Occupied(_) => {
                panic!("completed handoff must return enqueue ownership")
            }
        }
    }
}
