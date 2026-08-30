//! Task APIs for multi-task configuration.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};

use axerrno::{AxError, AxResult};
#[cfg(feature = "sched-eevdf")]
pub use axsched::{
    EEVDF_PROFILE, EevdfProfile, EevdfTaskClass as SchedClass, EevdfTaskParams as SchedState,
    RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN, eevdf_profile,
};
use kernel_guard::NoPreemptIrqSave;
use spin::Once;

#[cfg(feature = "sched-eevdf")]
pub use crate::run_queue::PreparedTaskPublication;
#[cfg(feature = "sched-eevdf")]
pub use crate::run_queue::{IDLE_STEAL_CONFIG, IdleStealConfig, idle_steal_config};
#[cfg(feature = "idle-steal")]
pub use crate::run_queue::{IdleStealDiagnosticsSnapshot, idle_steal_diagnostics};
pub use crate::run_queue::{
    SchedulerLoadSnapshot, TaskEnqueueError, TaskEnqueueErrorKind, TaskRuntimeInitError,
    TaskSchedError, scheduler_load_snapshot,
};

/// Failure to install the late remote scheduler-kick consumer.
#[cfg(all(feature = "remote-resched", target_os = "none"))]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RemoteReschedInitError {
    /// The HAL reason broker is not ready or its reschedule lane is occupied.
    BrokerUnavailableOrOccupied,
}
pub(crate) use crate::run_queue::{current_run_queue, select_run_queue};
#[doc(cfg(all(feature = "multitask", feature = "task-ext")))]
#[cfg(feature = "task-ext")]
pub use crate::task::{AxTaskExt, SwitchReason, TaskExt};
#[doc(cfg(all(feature = "multitask", feature = "irq")))]
#[cfg(feature = "irq")]
pub use crate::timers::{
    TIMER_CALLBACK_CAPACITY, TimerCallbackRegisterError, TimerCallbackToken, cancel_timer_callback,
    register_timer_callback,
};
#[doc(cfg(feature = "multitask"))]
pub use crate::{
    task::{
        CurrentTask, MIN_KERNEL_STACK_SIZE, TaskCreateError, TaskExitQueueFault, TaskId, TaskInner,
        TaskNameError, TaskState, TaskStateDecodeError, TaskWakeFault,
    },
    wait_queue::{WaitError, WaitQueue},
};

/// The reference type of a task.
pub type AxTaskRef = Arc<AxTask>;

/// The weak reference type of a task.
pub type WeakAxTaskRef = Weak<AxTask>;

static DEFERRED_WORK_DISPATCHER: Once<fn()> = Once::new();

struct DeferredWorkGuard<'a>(&'a TaskInner);

impl Drop for DeferredWorkGuard<'_> {
    fn drop(&mut self) {
        self.0.leave_deferred_work();
    }
}

/// The wrapper type for [`cpumask::CpuMask`] with SMP configuration.
pub type AxCpuMask = cpumask::CpuMask<{ axconfig::plat::MAX_CPU_NUM }>;

/// Number of CPU bits supported by the task-affinity ABI.
///
/// This is the configured x86_64 run-queue capacity (`nr_cpu_ids`), rather
/// than the number of run queues initialized at one instant.
pub const fn task_affinity_nr_cpu_ids() -> usize {
    axconfig::plat::MAX_CPU_NUM
}

/// Linux `cpumask_size()` for the configured task-affinity ABI.
///
/// Linux rounds the bitmap up to whole `unsigned long` words.  TheKernel is
/// x86_64-only, where this is an eight-byte ABI word.
pub const fn task_affinity_mask_bytes() -> usize {
    task_affinity_nr_cpu_ids().div_ceil(usize::BITS as usize) * core::mem::size_of::<usize>()
}

cfg_if::cfg_if! {
    if #[cfg(feature = "sched-rr")] {
        const MAX_TIME_SLICE: usize = 5;
        pub(crate) type AxTask = axsched::RRTask<TaskInner, MAX_TIME_SLICE>;
        pub(crate) type Scheduler = axsched::RRScheduler<TaskInner, MAX_TIME_SLICE>;
    } else if #[cfg(feature = "sched-eevdf")] {
        pub(crate) type AxTask = axsched::EEVDFTask<TaskInner>;
        pub(crate) type Scheduler = axsched::EEVDFScheduler<TaskInner>;
    } else {
        // If no scheduler features are set, use FIFO as the default.
        pub(crate) type AxTask = axsched::FifoTask<TaskInner>;
        pub(crate) type Scheduler = axsched::FifoScheduler<TaskInner>;
    }
}

#[cfg(feature = "preempt")]
struct KernelGuardIfImpl;

#[cfg(feature = "preempt")]
#[crate_interface::impl_interface]
impl kernel_guard::KernelGuardIf for KernelGuardIfImpl {
    fn disable_preempt() {
        if let Some(curr) = current_may_uninit() {
            #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
            if !axhal::asm::irqs_enabled() {
                let mut flags = 0;
                if curr.is_idle() {
                    flags |= crate::irq_continuation_diagnostics::FLAG_IDLE;
                }
                if curr.preempt_pending() {
                    flags |= crate::irq_continuation_diagnostics::FLAG_NEED_RESCHED;
                }
                crate::irq_continuation_diagnostics::record_event(
                    crate::irq_continuation_diagnostics::EVENT_PREEMPT_DISABLE_IRQ_OFF,
                    curr.id().as_u64(),
                    0,
                    flags,
                    curr.preempt_disable_count(),
                );
            }
            curr.disable_preempt();
        }
    }

    fn enable_preempt() {
        if let Some(curr) = current_may_uninit() {
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
                crate::irq_continuation_diagnostics::record_event(
                    crate::irq_continuation_diagnostics::EVENT_PREEMPT_ENABLE_IRQ_OFF,
                    curr.id().as_u64(),
                    0,
                    flags,
                    curr.preempt_disable_count(),
                );
            }
            // The task-local counter is the first, allocation-free filter.
            // Only its final release with a pending request enters the context
            // checker, which distinguishes an ordinary task safe point from
            // the one explicit outermost IRQ-exit safe point.
            curr.enable_preempt(true);
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
                    crate::irq_continuation_diagnostics::EVENT_PREEMPT_ENABLE_RETURN_IRQ_OFF,
                    curr.id().as_u64(),
                    0,
                    flags,
                    curr.preempt_disable_count(),
                );
            }
        }
    }
}

/// Gets the current task, or returns [`None`] if the current task is not
/// initialized.
pub fn current_may_uninit() -> Option<CurrentTask> {
    CurrentTask::try_get()
}

/// Gets the current task.
///
/// # Panics
///
/// Panics if the current task is not initialized.
pub fn current() -> CurrentTask {
    CurrentTask::get()
}

/// Returns whether the current context may block the running task.
///
/// Blocking through [`WaitQueue`] requires a non-idle running task, no extra
/// preemption guard, and (on bare metal) enabled local interrupts. Low-level
/// drivers use this to choose between a real sleep and a bounded non-blocking
/// fallback.
pub fn can_block_current() -> bool {
    let Some(curr) = current_may_uninit() else {
        return false;
    };
    if !curr.is_running() || curr.is_idle() {
        return false;
    }
    #[cfg(feature = "preempt")]
    if !curr.can_preempt(0) {
        return false;
    }
    #[cfg(feature = "irq-exit")]
    if crate::irq_exit::in_irq_context() {
        return false;
    }
    #[cfg(all(feature = "irq", target_os = "none"))]
    if !axhal::asm::irqs_enabled() {
        return false;
    }
    true
}

/// Installs the single task-context deferred-work dispatcher.
///
/// The hook is generic scheduler integration; subsystem policy stays in the
/// registering kernel. It may run concurrently on different tasks or CPUs,
/// but recursion on the same task is suppressed. The dispatcher must not panic
/// and should bound each invocation so normal scheduling can continue.
///
/// Returns `false` if a different dispatcher was already installed.
#[must_use]
pub fn set_deferred_work_dispatcher(dispatcher: fn()) -> bool {
    let installed = *DEFERRED_WORK_DISPATCHER.call_once(|| dispatcher);
    core::ptr::fn_addr_eq(installed, dispatcher)
}

/// Runs deferred work outside IRQ, runqueue, and preemption-disabled critical
/// sections.
///
/// Calls from unsafe contexts are ignored. The subsystem's pending source of
/// truth must remain set so a later safe point can retry. This function does
/// not allocate before entering the registered kernel dispatcher.
///
/// This only guarantees scheduler-context safety; it cannot prove that an
/// arbitrary caller holds no subsystem lock. Dispatchers must document their
/// own lock ordering and avoid locks that their chosen safe points can retain.
pub fn run_deferred_work() {
    let Some(dispatcher) = DEFERRED_WORK_DISPATCHER.get() else {
        return;
    };
    let Some(curr) = current_may_uninit() else {
        return;
    };
    #[cfg(feature = "preempt")]
    if !curr.can_preempt(0) {
        return;
    }
    #[cfg(all(feature = "irq", target_os = "none"))]
    if !axhal::asm::irqs_enabled() {
        return;
    }
    if !curr.try_enter_deferred_work() {
        return;
    }

    let _guard = DeferredWorkGuard(&curr);
    dispatcher();
}

/// Initializes the task scheduler (for the primary CPU).
pub fn init_scheduler() -> Result<(), TaskRuntimeInitError> {
    info!("Initialize scheduling...");

    // Claim the coordinated lower-layer boundary before publishing the
    // primary runqueue/current-task runtime. A conflicting owner therefore
    // fails without leaving a partially initialized scheduler behind.
    #[cfg(feature = "irq-exit")]
    crate::irq_exit::register()?;

    // Initialize the run queue.
    crate::run_queue::init()?;

    info!("  use {} scheduler.", Scheduler::scheduler_name());
    Ok(())
}

pub(crate) fn cpu_mask_full() -> AxCpuMask {
    use spin::Lazy;

    static CPU_MASK_FULL: Lazy<AxCpuMask> = Lazy::new(|| {
        let cpu_num = axhal::cpu_num();
        let mut cpumask = AxCpuMask::new();
        for cpu_id in 0..cpu_num {
            cpumask.set(cpu_id, true);
        }
        cpumask
    });

    *CPU_MASK_FULL
}

/// Initializes the task scheduler for secondary CPUs.
pub fn init_scheduler_secondary() -> Result<(), TaskRuntimeInitError> {
    crate::run_queue::init_secondary()
}

/// Register the allocation-free remote scheduler-kick consumer.
///
/// The raw IPI broker is initialized by the platform runtime after task
/// runqueues are online, so this is intentionally a separate late-init step
/// rather than part of [`init_scheduler`]. Repeating the registration is
/// idempotent at the HAL boundary; a false result means the broker is absent or
/// another owner already occupies the reason lane.
#[cfg(all(feature = "remote-resched", target_os = "none"))]
pub fn init_remote_resched_ipi() -> Result<(), RemoteReschedInitError> {
    if axhal::irq::register_ipi_reason(
        axhal::irq::IpiReason::Reschedule,
        crate::run_queue::remote_resched_ipi_handler,
    ) {
        Ok(())
    } else {
        Err(RemoteReschedInitError::BrokerUnavailableOrOccupied)
    }
}

/// Handles periodic timer ticks for the task manager.
///
/// For example, advance scheduler states, checks timed events, etc.
#[cfg(feature = "irq")]
#[doc(cfg(feature = "irq"))]
pub fn on_timer_event() {
    #[cfg(feature = "irq-continuation-diagnostics")]
    crate::irq_continuation_diagnostics::record_timer_event();
    crate::timers::check_events();
}

/// Handles periodic timer ticks for the task manager.
///
/// For example, advance scheduler states, checks timed events, etc.
///
/// This entry point is called from the local periodic timer interrupt. The
/// caller must already have local IRQs and preemption disabled so per-CPU
/// scheduler and recycler state remain owned by the interrupted CPU.
#[cfg(feature = "irq")]
#[doc(cfg(feature = "irq"))]
pub fn on_timer_tick() {
    use kernel_guard::NoOp;
    on_timer_event();
    crate::run_queue::gc_retry_timer_tick();
    #[cfg(feature = "task-ext")]
    {
        use crate::TaskExt;

        let curr = current();
        if curr
            .task_ext()
            .is_some_and(|extension| extension.on_timer_tick(&curr))
        {
            #[cfg(feature = "preempt")]
            curr.set_preempt_pending(true);
        }
    }
    // Since irq and preemption are both disabled here,
    // we can get current run queue with the default `kernel_guard::NoOp`.
    current_run_queue::<NoOp>().scheduler_timer_tick();
}

/// Runs a pending preemption request at a voluntary kernel boundary.
pub fn resched_if_needed() {
    #[cfg(feature = "smp")]
    retire_allowed_migration_current();
    run_deferred_work();
    #[cfg(feature = "preempt")]
    TaskInner::current_check_preempt_pending();
    run_deferred_work();
    // The dispatcher may wake a worker and set need_resched. Never return to
    // userspace or an idle-WFI caller with a wake-capable action after the last
    // preemption check.
    #[cfg(feature = "preempt")]
    TaskInner::current_check_preempt_pending();
}

/// Returns aggregate scheduler idle time across all CPUs.
pub fn idle_time() -> core::time::Duration {
    let tick_nanos = axhal::time::NANOS_PER_SEC / axconfig::TICKS_PER_SEC as u64;
    core::time::Duration::from_nanos(crate::run_queue::idle_ticks().saturating_mul(tick_nanos))
}

/// Requests rescheduling of the current task at the next preemption point.
pub fn request_resched_current() {
    #[cfg(feature = "preempt")]
    current().set_preempt_pending(true);
}

fn map_task_create_error(error: TaskCreateError) -> AxError {
    match error {
        TaskCreateError::InvalidStackSize => AxError::InvalidInput,
        TaskCreateError::OutOfMemory => AxError::NoMemory,
        TaskCreateError::IdentifierExhausted => AxError::OutOfRange,
    }
}

fn map_scheduler_error(error: axsched::SchedulerError) -> AxError {
    match error {
        axsched::SchedulerError::UnsupportedOperation => AxError::OperationNotSupported,
        axsched::SchedulerError::IdentifierExhausted
        | axsched::SchedulerError::SequenceExhausted
        | axsched::SchedulerError::ArithmeticExhausted => AxError::OutOfRange,
        axsched::SchedulerError::TaskBusy => AxError::ResourceBusy,
        axsched::SchedulerError::InvalidParameters
        | axsched::SchedulerError::IncompatibleClass
        | axsched::SchedulerError::InvalidTimeSlice => AxError::InvalidInput,
        axsched::SchedulerError::AlreadyQueued
        | axsched::SchedulerError::ForeignQueue
        | axsched::SchedulerError::InconsistentState => AxError::BadState,
    }
}

fn map_task_enqueue_error(error: TaskEnqueueError) -> AxError {
    error.into_ax_error()
}

impl TaskEnqueueError {
    /// Converts a generic task-publication failure to its axerrno category,
    /// releasing the returned unpublished task owner in the caller's context.
    pub fn into_ax_error(self) -> AxError {
        let kind = self.kind;
        drop(self.task);
        match kind {
            TaskEnqueueErrorKind::RunQueueUnavailable(_) => AxError::BadState,
            TaskEnqueueErrorKind::Scheduler(error) => map_scheduler_error(error),
            TaskEnqueueErrorKind::TaskNotReady => AxError::InvalidInput,
            #[cfg(feature = "smp")]
            TaskEnqueueErrorKind::HandoffOccupied => AxError::BadState,
        }
    }
}

/// Adds the given unpublished task to a run queue.
pub fn spawn_task(task: TaskInner) -> AxResult<AxTaskRef> {
    let task_ref = task.into_arc().map_err(map_task_create_error)?;
    let publication = select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    publication.map_err(map_task_enqueue_error)?;
    Ok(task_ref)
}

/// Fallibly constructs and publishes a kernel task.
///
/// Every scheduler implementation stores ready tasks intrusively, so the only
/// failing steps (entry box, kernel stack, and scheduler wrapper) complete
/// before the task becomes runnable.
pub fn try_spawn_raw<F>(f: F, name: String, stack_size: usize) -> AxResult<AxTaskRef>
where
    F: FnOnce() + Send + 'static,
{
    let task = TaskInner::try_new(f, name, stack_size).map_err(map_task_create_error)?;
    let task_ref = task.try_into_arc().map_err(map_task_create_error)?;
    let publication = select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    publication.map_err(map_task_enqueue_error)?;
    Ok(task_ref)
}

/// Fallibly spawns a task with the default kernel-stack size.
pub fn try_spawn_with_name<F>(f: F, name: String) -> AxResult<AxTaskRef>
where
    F: FnOnce() + Send + 'static,
{
    try_spawn_raw(f, name, axconfig::TASK_STACK_SIZE)
}

/// Adds the given task to the run queue with the specified scheduling state.
#[cfg(feature = "sched-eevdf")]
pub fn spawn_task_with_sched(task: TaskInner, sched_state: SchedState) -> AxResult<AxTaskRef> {
    let task_ref = task.into_arc().map_err(map_task_create_error)?;
    task_ref
        .configure(sched_state)
        .map_err(map_scheduler_error)?;
    let publication = select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    publication.map_err(map_task_enqueue_error)?;
    Ok(task_ref)
}

/// Adds the given task to the run queue with the specified scheduling state,
/// inheriting an opaque EEVDF fork seed when the parent's class supports it.
#[cfg(feature = "sched-eevdf")]
pub fn spawn_task_with_sched_from(
    task: TaskInner,
    sched_state: SchedState,
    parent: &AxTaskRef,
) -> AxResult<AxTaskRef> {
    let task_ref = task.into_arc().map_err(map_task_create_error)?;
    task_ref
        .configure(sched_state)
        .map_err(map_scheduler_error)?;
    task_ref
        .configure_fair_runtime_ns(if parent.sched_reset_on_fork() {
            0
        } else {
            parent.fair_runtime_ns()
        })
        .map_err(map_scheduler_error)?;
    let (util_min, util_max) = parent.sched_util_clamp();
    task_ref.set_sched_util_clamp(util_min, util_max);
    install_fork_seed_if_applicable(&task_ref, parent)?;
    let publication = select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    publication.map_err(map_task_enqueue_error)?;
    Ok(task_ref)
}

/// Fallibly constructs and configures a task without publishing it to a run
/// queue. This is the allocation/admission half of task creation.
#[cfg(feature = "sched-eevdf")]
pub fn prepare_task_with_sched_from(
    task: TaskInner,
    sched_state: SchedState,
    parent: &AxTaskRef,
) -> AxResult<AxTaskRef> {
    let task_ref = task.try_into_arc().map_err(map_task_create_error)?;
    task_ref
        .configure(sched_state)
        .map_err(map_scheduler_error)?;
    task_ref
        .configure_fair_runtime_ns(if parent.sched_reset_on_fork() {
            0
        } else {
            parent.fair_runtime_ns()
        })
        .map_err(map_scheduler_error)?;
    // The child is still private here. A fork inherits the parent's effective
    // clamp; exec retains the existing task and consequently needs no action.
    let (util_min, util_max) = parent.sched_util_clamp();
    task_ref.set_sched_util_clamp(util_min, util_max);
    install_fork_seed_if_applicable(&task_ref, parent)?;
    Ok(task_ref)
}

#[cfg(feature = "sched-eevdf")]
fn install_fork_seed_if_applicable(task: &AxTaskRef, parent: &AxTaskRef) -> AxResult {
    match crate::run_queue::install_fork_seed_from_parent(task, parent) {
        Ok(()) | Err(axsched::SchedulerError::IncompatibleClass) => Ok(()),
        Err(error) => Err(map_scheduler_error(error)),
    }
}

/// Reserves a fully prepared task's selected run queue without publishing it.
///
/// Scheduler identity, queue ownership, target-CPU availability, and ordering
/// sequence admission all complete here. Dropping the returned token cancels
/// the claim. A lifecycle adapter should obtain this token before committing
/// any process, signal, or lookup-table state which cannot be rolled back.
#[cfg(feature = "sched-eevdf")]
pub fn reserve_prepared_task(task: AxTaskRef) -> Result<PreparedTaskPublication, TaskEnqueueError> {
    if !task.try_reserve_publication_mutation() {
        return Err(TaskEnqueueError {
            kind: TaskEnqueueErrorKind::Scheduler(axsched::SchedulerError::TaskBusy),
            task,
        });
    }
    select_run_queue::<NoPreemptIrqSave>(&task).reserve_claimed_new_task(task)
}

/// Publishes an already reserved task without allocation or recoverable
/// failure and returns the exact runnable task owner.
#[cfg(feature = "sched-eevdf")]
pub fn publish_prepared_task(publication: PreparedTaskPublication) -> AxTaskRef {
    publication.commit()
}

/// Spawns a new task with the given parameters.
///
/// Returns the task reference.
pub fn spawn_raw<F>(f: F, name: String, stack_size: usize) -> AxResult<AxTaskRef>
where
    F: FnOnce() + Send + 'static,
{
    let task = TaskInner::new(f, name, stack_size).map_err(map_task_create_error)?;
    spawn_task(task)
}

/// Spawns a new task with the given name and the default stack size ([`axconfig::TASK_STACK_SIZE`]).
///
/// Returns the task reference.
pub fn spawn_with_name<F>(f: F, name: String) -> AxResult<AxTaskRef>
where
    F: FnOnce() + Send + 'static,
{
    spawn_raw(f, name, axconfig::TASK_STACK_SIZE)
}

/// Spawns a new task with the default parameters.
///
/// The default task name is an empty string. The default task stack size is
/// [`axconfig::TASK_STACK_SIZE`].
///
/// Returns the task reference.
pub fn spawn<F>(f: F) -> AxResult<AxTaskRef>
where
    F: FnOnce() + Send + 'static,
{
    spawn_with_name(f, String::new())
}

/// Set the priority for current task.
///
/// The range of the priority is dependent on the underlying scheduler. For
/// example, in the [EEVDF] scheduler, the priority is the nice value, ranging from
/// -20 to 19.
///
/// Returns a typed mechanism error when the selected scheduler cannot apply
/// the update. Linux policy and errno mapping intentionally stay in the OS
/// personality above this crate.
///
/// [EEVDF]: https://en.wikipedia.org/wiki/Eligible_virtual_deadline_first
pub fn set_priority(prio: isize) -> Result<(), TaskSchedError> {
    current_run_queue::<NoPreemptIrqSave>().set_current_priority(prio)
}

/// Returns a stable snapshot of the CPUs on which `task` may currently run.
///
/// The result is the task's affinity mask intersected with initialized run
/// queues. It is sampled under the task affinity lock, so a concurrent
/// affinity update cannot yield an old mask after publication of a newer one.
/// A terminal task has no usable affinity and returns
/// [`TaskSchedError::TaskExited`]. An empty successful mask instead means no
/// run queue is initialized for an otherwise live task.
pub fn task_allowed_active_cpus(task: &AxTaskRef) -> Result<AxCpuMask, TaskSchedError> {
    task.allowed_active_cpus(crate::run_queue::active_cpu_mask())
        .ok_or(TaskSchedError::TaskExited)
}

/// Returns the runtime scheduling state of a task.
#[cfg(feature = "sched-eevdf")]
pub fn sched_state(task: &AxTaskRef) -> SchedState {
    task.sched_params()
}

#[cfg(feature = "sched-eevdf")]
pub fn scheduler_commit_version(task: &AxTaskRef) -> u64 {
    task.sched_commit_version()
}

/// Exact scheduler state committed by one serialized scheduler transaction.
#[cfg(feature = "sched-eevdf")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSchedCommit {
    pub state: SchedState,
    pub reset_on_fork: bool,
    /// Effective utilization-clamp bounds in Linux's 0..=1024 scale.
    ///
    /// This build has no cgroup clamp controller, so the effective bounds are
    /// the task request.  Keeping them in the scheduler commit (rather than
    /// the syscall layer) makes that fact explicit and keeps future cgroup
    /// composition in the same transaction domain.
    pub util_min: u32,
    pub util_max: u32,
    /// Scheduler-owned requested fair slice sampled with this commit.
    pub fair_runtime_ns: u64,
    pub version: u64,
}

/// Complete scheduler-owned data exported by Linux `sched_getattr`.
///
/// The tuple is produced under the same stable run-queue ownership check as
/// [`TaskSchedCommit`]. Deadline fields are reserved for scheduler classes
/// which do not exist in this build. Utilization clamps are scheduler-owned
/// and therefore sampled from the same transaction as policy and priority.
#[cfg(feature = "sched-eevdf")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSchedAttrSnapshot {
    pub commit: TaskSchedCommit,
    /// The configured fair-class request slice in nanoseconds, or zero for
    /// real-time classes.
    pub fair_runtime_ns: u64,
    pub deadline_ns: u64,
    pub period_ns: u64,
    pub util_min: u32,
    pub util_max: u32,
}

/// One complete scheduler update request, including utilization clamps.
///
/// Callers must validate `util_min <= util_max <= 1024` before submitting the
/// request. Keeping validation at the Linux personality boundary preserves
/// its precise errno and capability ordering; this type carries only an
/// already-authorized scheduler transaction.
#[cfg(feature = "sched-eevdf")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSchedAttrUpdate {
    pub state: SchedState,
    pub reset_on_fork: bool,
    /// Requested fair runtime in ns; zero selects the scheduler profile.
    pub fair_runtime_ns: u64,
    pub util_min: u32,
    pub util_max: u32,
}

/// Snapshots a task's scheduler state and commit version under the scheduler
/// lock of the run queue that currently owns the task.  Callers that need to
/// carry scheduler state across another synchronization domain must use this
/// rather than separately reading the state and version.
#[cfg(feature = "sched-eevdf")]
pub fn scheduler_state_snapshot(task: &AxTaskRef) -> Result<TaskSchedCommit, TaskSchedError> {
    crate::run_queue::scheduler_state_snapshot_stable(task)
}

/// Returns one complete Linux scheduler-attribute snapshot while resolving
/// run-queue migration and terminal task races.
#[cfg(feature = "sched-eevdf")]
pub fn scheduler_attr_snapshot(task: &AxTaskRef) -> Result<TaskSchedAttrSnapshot, TaskSchedError> {
    crate::run_queue::scheduler_attr_snapshot_stable(task)
}

/// Applies the runtime scheduling state of a task.
#[cfg(feature = "sched-eevdf")]
pub fn set_sched_state(task: &AxTaskRef, sched_state: SchedState) -> Result<(), TaskSchedError> {
    crate::run_queue::set_task_sched_state_stable(task, sched_state).map(|_| ())
}

#[cfg(feature = "sched-eevdf")]
pub fn set_sched_state_versioned(
    task: &AxTaskRef,
    sched_state: SchedState,
) -> Result<TaskSchedCommit, TaskSchedError> {
    crate::run_queue::set_task_sched_state_stable(task, sched_state)
}

#[cfg(feature = "sched-eevdf")]
pub fn set_sched_state_versioned_with_reset(
    task: &AxTaskRef,
    sched_state: SchedState,
    reset_on_fork: bool,
) -> Result<TaskSchedCommit, TaskSchedError> {
    crate::run_queue::set_task_sched_state_with_reset_stable(task, sched_state, reset_on_fork)
}

/// Atomically decides and, on success, applies a complete scheduler state
/// update while holding the task's owning run-queue lock.
///
/// The callback receives one coherent old state, reset-on-fork bit, and
/// scheduler commit version. Returning `Err` leaves all three unchanged; an
/// outer error is a task/scheduler mechanism failure. The callback is invoked
/// at most once, after a stable run-queue owner has been established.
///
/// The nested result deliberately keeps policy failures in `E` separate from
/// mechanism failures in [`TaskSchedError`], so OS personalities can map
/// their policy decision without attempting a stale rollback.
#[cfg(feature = "sched-eevdf")]
pub fn update_sched_state_versioned_with_reset<E, F>(
    task: &AxTaskRef,
    update: F,
) -> Result<Result<TaskSchedCommit, E>, TaskSchedError>
where
    F: FnMut(TaskSchedCommit) -> Result<(SchedState, bool), E>,
{
    crate::run_queue::update_task_sched_state_versioned_with_reset_stable(task, update)
}

/// Atomically decides and commits scheduler policy, reset-on-fork, and
/// utilization clamps. This is the `sched_setattr` entry point: no reader can
/// observe its successful clamp update paired with an older policy commit.
#[cfg(feature = "sched-eevdf")]
pub fn update_sched_attr_versioned<E, F>(
    task: &AxTaskRef,
    update: F,
) -> Result<Result<TaskSchedCommit, E>, TaskSchedError>
where
    F: FnMut(TaskSchedCommit) -> Result<TaskSchedAttrUpdate, E>,
{
    crate::run_queue::update_task_sched_attr_versioned_stable(task, update)
}

/// Initializes the ABI policy flag before a prepared task is published.  No
/// scheduler owner exists yet, therefore no concurrent snapshot can observe
/// the initialization separately from first publication.
#[cfg(feature = "sched-eevdf")]
pub fn set_prepared_task_sched_reset_on_fork(task: &AxTaskRef, reset_on_fork: bool) {
    debug_assert!(!task.is_running());
    task.set_sched_reset_on_fork(reset_on_fork);
}

/// Initializes clamp state for a not-yet-published task. Fork creation uses
/// this to transfer its scheduler snapshot before the child is visible.
#[cfg(feature = "sched-eevdf")]
pub fn set_prepared_task_sched_util_clamp(task: &AxTaskRef, util_min: u32, util_max: u32) {
    debug_assert!(!task.is_running());
    debug_assert!(util_min <= util_max && util_max <= 1024);
    task.set_sched_util_clamp(util_min, util_max);
}

/// Initializes a not-yet-published task's Linux fair runtime request.
#[cfg(feature = "sched-eevdf")]
pub fn set_prepared_task_sched_fair_runtime(
    task: &AxTaskRef,
    fair_runtime_ns: u64,
) -> Result<(), TaskSchedError> {
    task.configure_fair_runtime_ns(fair_runtime_ns)
        .map_err(TaskSchedError::from)
}

/// Changes only a task's nice value while its owning scheduler lock is held.
/// This preserves a concurrent policy/class update instead of publishing a
/// stale whole-state snapshot.
#[cfg(feature = "sched-eevdf")]
pub fn set_task_nice(task: &AxTaskRef, nice: i8) -> Result<TaskSchedCommit, TaskSchedError> {
    crate::run_queue::set_task_nice_stable(task, nice)
}

/// Requests reclamation of exited tasks queued on the current CPU.
///
/// This complements the dedicated GC task for workloads that reap large child
/// bursts and immediately continue with more forks, where waiting for the GC
/// task to run can retain many dead task stacks and address spaces longer than
/// necessary.
///
/// The public caller only observes the queue and publishes an owner-local wake
/// inside one short no-migration interval. The permanently pinned per-CPU GC
/// task remains the sole owner of queue removal, stack recycling, and
/// `TaskInner`/`TaskExt` destruction.
///
/// Returns `true` if that current-CPU observation found queued tasks.
/// IRQ-enabled runtimes also retain a bounded per-CPU timer retry; cooperative
/// runtimes without timer ticks require a later exit or another explicit
/// reclaim request.
pub fn reclaim_exited_tasks() -> bool {
    crate::run_queue::request_exited_task_reclaim_current_cpu()
}

/// Repeats a reclaim request/observation around a bounded number of yields.
///
/// The production request samples the CPU on which each iteration runs. Since
/// the yield may migrate the caller, consecutive observations need not refer to
/// the same exited-task queue. The return value is only the final iteration's
/// observation; `false` is neither a global/per-original-CPU empty proof nor a
/// destructor-completion barrier.
pub(crate) fn drive_reclaim_until_clear(
    max_yields: usize,
    mut reclaim: impl FnMut() -> bool,
    mut yield_now: impl FnMut(),
) -> bool {
    for _ in 0..max_yields {
        if !reclaim() {
            return false;
        }
        yield_now();
    }
    reclaim()
}

/// Requests exited-task reclamation, yielding between bounded owner-local
/// requests while scheduler-side handoff references still keep some task
/// objects alive.
///
/// Each iteration samples the CPU on which it runs, and the yield may migrate
/// the caller between iterations. The returned boolean therefore reports only
/// the final current-CPU queue observation. In particular, `false` does not
/// prove that all CPUs, or even the CPU sampled by an earlier iteration, are
/// empty, and it does not wait for task destructors to finish.
pub fn reclaim_exited_tasks_until_clear(max_yields: usize) -> bool {
    drive_reclaim_until_clear(max_yields, reclaim_exited_tasks, yield_now)
}

#[cfg(any(feature = "smp", test))]
pub(crate) fn admit_affinity_then_publish<T, E, R>(
    needed: bool,
    prepare: impl FnOnce() -> Result<T, E>,
    publish: impl FnOnce(Option<T>) -> R,
) -> Result<R, E> {
    let prepared = if needed { Some(prepare()?) } else { None };
    Ok(publish(prepared))
}

#[cfg(feature = "smp")]
fn try_prepare_migration_task(migrated: &AxTaskRef) -> AxResult<AxTaskRef> {
    const MIGRATION_TASK_STACK_SIZE: usize = MIN_KERNEL_STACK_SIZE;
    const MIGRATION_TASK_NAME: &str = "migration-task";

    let mut name = String::new();
    name.try_reserve_exact(MIGRATION_TASK_NAME.len())
        .map_err(|_| AxError::NoMemory)?;
    name.push_str(MIGRATION_TASK_NAME);
    let migrated = Arc::downgrade(migrated);
    let mut task = TaskInner::try_new(
        move || {
            if let Some(migrated) = migrated.upgrade() {
                crate::run_queue::migrate_entry(migrated);
            }
        },
        name,
        MIGRATION_TASK_STACK_SIZE,
    )
    .map_err(map_task_create_error)?;
    task.mark_migration_helper();
    task.try_into_arc().map_err(map_task_create_error)
}

#[cfg(feature = "smp")]
fn retire_allowed_migration_current() {
    let curr = current();
    let retired = curr.take_allowed_migration(axhal::percpu::this_cpu_id());
    drop(retired);
}

struct AffinityMutation<'a>(&'a AxTaskRef);

impl<'a> AffinityMutation<'a> {
    fn try_begin(task: &'a AxTaskRef) -> AxResult<Self> {
        // Affinity changes are serialized with the short wake/block
        // publication windows.  Those owners always complete without waiting
        // for this caller, so retrying here preserves the operation instead of
        // leaking an implementation-only EBUSY to an ordinary concurrent
        // sched_setaffinity caller.
        while !task.try_begin_affinity_mutation() {
            if matches!(task.state(), TaskState::Exited)
                || {
                    #[cfg(feature = "sched-eevdf")]
                    {
                        task.affinity_mutation_is_publication_reservation()
                    }
                    #[cfg(not(feature = "sched-eevdf"))]
                    {
                        false
                    }
                }
            {
                return Err(AxError::NoSuchProcess);
            }
            // A mutation owner may be completing a wake or block handoff on
            // another CPU.  Never burn this CPU (or starve that owner) while
            // waiting for that bounded handoff: reschedule before retrying.
            // No task/rq lock is held here, and the owner releases the word
            // before any wait that could depend on this caller.  The next
            // loop iteration rechecks terminalization before attempting to
            // acquire ownership again.
            yield_now();
        }
        Ok(Self(task))
    }
}

/// Validates an affinity mask against the scheduler's current active CPU set.
/// This is shared by live-task migration and durable zombie updates so both
/// reject empty or possible-but-offline masks identically.
pub fn validate_affinity_mask(cpumask: AxCpuMask) -> AxResult {
    if cpumask.is_empty() {
        return Err(AxError::InvalidInput);
    }

    #[cfg(feature = "smp")]
    if !crate::run_queue::affinity_has_online_cpu(cpumask) {
        return Err(AxError::InvalidInput);
    }

    #[cfg(not(feature = "smp"))]
    if !cpumask.get(0) {
        return Err(AxError::InvalidInput);
    }

    Ok(())
}

impl Drop for AffinityMutation<'_> {
    fn drop(&mut self) {
        match self.0.finish_affinity_mutation() {
            crate::task::AffinityMutationCompletion::Released => {}
            crate::task::AffinityMutationCompletion::WakeClaimed => {
                // The affinity owner inherited a raw-waker obligation. All
                // affinity/runqueue temporaries created by the syscall-facing
                // operation have been released before this guard is dropped,
                // so the claimed wake can publish without nested queue locks.
                crate::future::wake_task_claimed(self.0);
            }
            crate::task::AffinityMutationCompletion::Corrupt => {
                error!(
                    "task {} affinity/wake mutation ownership is corrupt",
                    self.0.id().as_u64()
                );
                axhal::power::system_off();
            }
        }
    }
}

/// Set the affinity for the current task.
/// [`AxCpuMask`] is used to specify the CPU affinity.
///
/// Allocation needed to pre-admit a possible migration is completed before
/// publishing the new mask. In particular, [`AxError::NoMemory`] leaves the
/// old affinity unchanged instead of collapsing the failure into an invalid
/// mask error at the caller.
pub fn set_current_affinity(cpumask: AxCpuMask) -> AxResult {
    validate_affinity_mask(cpumask)?;

    let curr = current().clone();
    let _mutation = AffinityMutation::try_begin(&curr)?;
    #[cfg(feature = "smp")]
    {
        // The task can be preempted and migrated while allocation is in
        // progress. Any restrictive mask therefore needs a helper; only the
        // all-online-CPU mask proves that every possible resume CPU is allowed.
        let needs_migration = cpumask != cpu_mask_full();
        // Admission is complete before the mask becomes observable. On OOM the
        // old affinity and any old pending helper remain untouched.
        let displaced = admit_affinity_then_publish(
            needs_migration,
            || try_prepare_migration_task(&curr),
            |migration| curr.publish_affinity(cpumask, migration),
        )?;
        drop(displaced);
        retire_allowed_migration_current();

        // Dropping the affinity lock may have honored a pending preemption and
        // already migrated this task. Otherwise claim the prepared token under
        // the runqueue guard; that safe point performs no allocation.
        if !cpumask.get(axhal::percpu::this_cpu_id()) {
            current_run_queue::<NoPreemptIrqSave>().migrate_current_if_needed();
        }
        if cpumask.get(axhal::percpu::this_cpu_id()) {
            Ok(())
        } else {
            Err(AxError::BadState)
        }
    }

    #[cfg(not(feature = "smp"))]
    {
        curr.set_cpumask(cpumask);
        Ok(())
    }
}

/// Sets the affinity for an arbitrary task.
///
/// For the current task this follows the existing migrate-current path. For a
/// remote ready task, the task is moved onto a run queue allowed by the new
/// mask immediately. For a remote running task, the new mask is recorded and
/// the task is nudged so it can self-migrate at its next scheduling point.
pub fn set_task_affinity(task: &AxTaskRef, cpumask: AxCpuMask) -> AxResult {
    validate_affinity_mask(cpumask)?;

    if current().ptr_eq(task) {
        return set_current_affinity(cpumask);
    }

    let _mutation = AffinityMutation::try_begin(task)?;

    #[cfg(feature = "smp")]
    {
        if matches!(task.state(), TaskState::Exited) {
            return Err(AxError::NoSuchProcess);
        }
        // A Ready/Blocked task may become Running or migrate while admission is
        // in progress. Only an all-online-CPU mask can omit the helper without
        // reopening allocation at the scheduling safe point.
        let needs_migration = cpumask != cpu_mask_full();
        let (expected, displaced) = admit_affinity_then_publish(
            needs_migration,
            || try_prepare_migration_task(task),
            |migration| {
                let expected = migration.as_ref().cloned();
                let displaced = task.publish_affinity(cpumask, migration);
                (expected, displaced)
            },
        )?;
        drop(displaced);

        let (result, retire_expected) = match task.state() {
            TaskState::Ready => {
                let migrated = crate::run_queue::task_run_queue::<NoPreemptIrqSave>(task)
                    .migrate_ready_task(task);
                if !migrated {
                    #[cfg(feature = "preempt")]
                    task.set_preempt_pending(true);
                    task.interrupt();
                }
                (!matches!(task.state(), TaskState::Exited), migrated)
            }
            TaskState::Running => {
                if !cpumask.get(task.cpu_id() as usize) {
                    #[cfg(feature = "preempt")]
                    task.set_preempt_pending(true);
                    task.interrupt();
                }
                (true, false)
            }
            TaskState::Blocked => {
                if !cpumask.get(task.cpu_id() as usize) {
                    task.set_cpu_id(crate::run_queue::select_run_queue_index(cpumask) as _);
                }
                (true, false)
            }
            TaskState::Exited => (false, true),
        };

        // A successful ready-queue move no longer needs its helper. Running or
        // racing tasks claim it themselves; the pointer check preserves a newer
        // concurrent setaffinity token. Destruction occurs outside all locks.
        if retire_expected && let Some(expected) = expected.as_ref() {
            let retired = task.clear_migration_if(expected);
            drop(retired);
        }
        if result {
            Ok(())
        } else {
            Err(AxError::NoSuchProcess)
        }
    }

    #[cfg(not(feature = "smp"))]
    {
        task.set_cpumask(cpumask);
        if matches!(task.state(), TaskState::Exited) {
            Err(AxError::NoSuchProcess)
        } else {
            Ok(())
        }
    }
}

/// Current task gives up the CPU time voluntarily, and switches to another
/// ready task.
pub fn yield_now() {
    #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
    if !axhal::asm::irqs_enabled() {
        let curr = current();
        let mut flags = 0;
        if curr.is_idle() {
            flags |= crate::irq_continuation_diagnostics::FLAG_IDLE;
        }
        if curr.preempt_pending() {
            flags |= crate::irq_continuation_diagnostics::FLAG_NEED_RESCHED;
        }
        crate::irq_continuation_diagnostics::record_event(
            crate::irq_continuation_diagnostics::EVENT_YIELD_ENTER_IRQ_OFF,
            curr.id().as_u64(),
            0,
            flags,
            curr.preempt_disable_count(),
        );
    }
    #[cfg(feature = "smp")]
    retire_allowed_migration_current();
    run_deferred_work();
    current_run_queue::<NoPreemptIrqSave>().yield_current();
    #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
    if !axhal::asm::irqs_enabled() {
        let curr = current();
        let mut flags = 0;
        if curr.is_idle() {
            flags |= crate::irq_continuation_diagnostics::FLAG_IDLE;
        }
        if curr.preempt_pending() {
            flags |= crate::irq_continuation_diagnostics::FLAG_NEED_RESCHED;
        }
        crate::irq_continuation_diagnostics::record_event(
            crate::irq_continuation_diagnostics::EVENT_YIELD_RETURN_IRQ_OFF,
            curr.id().as_u64(),
            0,
            flags,
            curr.preempt_disable_count(),
        );
    }
    run_deferred_work();
    #[cfg(feature = "preempt")]
    TaskInner::current_check_preempt_pending();
}

/// Linux `sched_yield(2)` scheduling boundary.
///
/// This deliberately enters the local run queue's `Yield` enqueue path rather
/// than requesting a generic reschedule, preserving scheduler-class policy
/// (fair eligible placement and RT same-priority tail placement).
pub fn sched_yield() {
    yield_now();
}

/// Current task is going to sleep for the given duration.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub fn sleep(dur: core::time::Duration) -> AxResult<()> {
    let deadline = axhal::time::wall_time()
        .checked_add(dur)
        .ok_or(AxError::OutOfRange)?;
    sleep_until(deadline)
}

/// Current task is going to sleep, it will be woken up at the given deadline.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub fn sleep_until(deadline: axhal::time::TimeValue) -> AxResult<()> {
    #[cfg(feature = "irq")]
    {
        crate::future::block_on(crate::future::sleep_until(deadline))
            .map_err(AxError::from)?
            .map_err(AxError::from)
    }
    #[cfg(not(feature = "irq"))]
    {
        axhal::time::busy_wait_until(deadline);
        Ok(())
    }
}

/// Exits the current task without unwinding its kernel stack.
///
/// Destructors for caller-owned local values do not run. Code which invokes
/// this function directly must release resources requiring deterministic drop
/// before the call; normal task-entry return performs its internal cleanup
/// before reaching this path.
pub fn exit(exit_code: i32) -> ! {
    run_deferred_work();
    current_run_queue::<NoPreemptIrqSave>().exit_current(exit_code)
}

/// The idle task routine.
///
/// It runs an infinite loop that keeps calling [`yield_now()`].
pub fn run_idle() -> ! {
    loop {
        yield_now();
        #[cfg(all(feature = "irq-continuation-diagnostics", target_os = "none"))]
        if !axhal::asm::irqs_enabled() {
            let curr = current();
            let mut flags = crate::irq_continuation_diagnostics::FLAG_IDLE;
            if curr.preempt_pending() {
                flags |= crate::irq_continuation_diagnostics::FLAG_NEED_RESCHED;
            }
            crate::irq_continuation_diagnostics::record_event(
                crate::irq_continuation_diagnostics::EVENT_IDLE_AFTER_YIELD_IRQ_OFF,
                curr.id().as_u64(),
                0,
                flags,
                curr.preempt_disable_count(),
            );
        }
        // A dispatcher running after the yield may make a blocked task ready.
        // Honor that wakeup before entering the architecture idle instruction.
        resched_if_needed();
        #[cfg(feature = "idle-steal")]
        match crate::run_queue::current_run_queue::<NoPreemptIrqSave>().idle_steal_once() {
            crate::run_queue::IdleStealOutcome::Stole
            | crate::run_queue::IdleStealOutcome::LocalWorkWon => {
                // A successful pull or a local-work race publishes/observes
                // runnable work on this CPU. Turn that result into the normal
                // bounded preemption boundary; the steal itself never
                // dispatches while holding both queues.
                current().set_preempt_pending(true);
                resched_if_needed();
            }
            crate::run_queue::IdleStealOutcome::NoWork => {}
        }
        trace!("idle task: waiting for IRQs...");
        #[cfg(feature = "irq")]
        axhal::asm::wait_for_irqs();
    }
}
