//! Task APIs for multi-task configuration.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};

use axerrno::{AxError, AxResult};
#[cfg(feature = "sched-cfs")]
pub use axsched::{
    CfsTaskClass as SchedClass, CfsTaskParams as SchedState, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX,
    RT_PRIORITY_MIN,
};
use kernel_guard::NoPreemptIrqSave;
use spin::Once;

#[cfg(feature = "sched-cfs")]
pub use crate::run_queue::PreparedTaskPublication;
pub use crate::run_queue::{
    TaskEnqueueError, TaskEnqueueErrorKind, TaskRuntimeInitError, TaskSchedError,
};
pub(crate) use crate::run_queue::{current_run_queue, select_run_queue};
#[doc(cfg(all(feature = "multitask", feature = "task-ext")))]
#[cfg(feature = "task-ext")]
pub use crate::task::{AxTaskExt, TaskExt};
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

cfg_if::cfg_if! {
    if #[cfg(feature = "sched-rr")] {
        const MAX_TIME_SLICE: usize = 5;
        pub(crate) type AxTask = axsched::RRTask<TaskInner, MAX_TIME_SLICE>;
        pub(crate) type Scheduler = axsched::RRScheduler<TaskInner, MAX_TIME_SLICE>;
    } else if #[cfg(feature = "sched-cfs")] {
        pub(crate) type AxTask = axsched::CFSTask<TaskInner>;
        pub(crate) type Scheduler = axsched::CFScheduler<TaskInner>;
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
        | axsched::SchedulerError::SequenceExhausted => AxError::OutOfRange,
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
#[cfg(feature = "sched-cfs")]
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
/// inheriting the parent's fair vruntime when applicable.
#[cfg(feature = "sched-cfs")]
pub fn spawn_task_with_sched_from(
    task: TaskInner,
    sched_state: SchedState,
    parent: &AxTaskRef,
) -> AxResult<AxTaskRef> {
    let task_ref = task.into_arc().map_err(map_task_create_error)?;
    task_ref
        .configure(sched_state)
        .map_err(map_scheduler_error)?;
    inherit_fair_vruntime_if_applicable(&task_ref, parent)?;
    let publication = select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    publication.map_err(map_task_enqueue_error)?;
    Ok(task_ref)
}

/// Fallibly constructs and configures a task without publishing it to a run
/// queue. This is the allocation/admission half of task creation.
#[cfg(feature = "sched-cfs")]
pub fn prepare_task_with_sched_from(
    task: TaskInner,
    sched_state: SchedState,
    parent: &AxTaskRef,
) -> AxResult<AxTaskRef> {
    let task_ref = task.try_into_arc().map_err(map_task_create_error)?;
    task_ref
        .configure(sched_state)
        .map_err(map_scheduler_error)?;
    inherit_fair_vruntime_if_applicable(&task_ref, parent)?;
    Ok(task_ref)
}

#[cfg(feature = "sched-cfs")]
fn inherit_fair_vruntime_if_applicable(task: &AxTaskRef, parent: &AxTaskRef) -> AxResult {
    match task.inherit_fair_vruntime_from(parent) {
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
#[cfg(feature = "sched-cfs")]
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
#[cfg(feature = "sched-cfs")]
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
/// example, in the [CFS] scheduler, the priority is the nice value, ranging from
/// -20 to 19.
///
/// Returns a typed mechanism error when the selected scheduler cannot apply
/// the update. Linux policy and errno mapping intentionally stay in the OS
/// personality above this crate.
///
/// [CFS]: https://en.wikipedia.org/wiki/Completely_Fair_Scheduler
pub fn set_priority(prio: isize) -> Result<(), TaskSchedError> {
    current_run_queue::<NoPreemptIrqSave>().set_current_priority(prio)
}

/// Returns the runtime scheduling state of a task.
#[cfg(feature = "sched-cfs")]
pub fn sched_state(task: &AxTaskRef) -> SchedState {
    task.sched_params()
}

/// Applies the runtime scheduling state of a task.
#[cfg(feature = "sched-cfs")]
pub fn set_sched_state(task: &AxTaskRef, sched_state: SchedState) -> Result<(), TaskSchedError> {
    crate::run_queue::task_run_queue::<NoPreemptIrqSave>(task)
        .set_task_sched_state(task, sched_state)
}

/// Opportunistically reclaims exited tasks queued on the current CPU.
///
/// This complements the dedicated GC task for workloads that reap large child
/// bursts and immediately continue with more forks, where waiting for the GC
/// task to run can retain many dead task stacks and address spaces longer than
/// necessary.
///
/// Returns `true` if tasks are still queued after this reclaim pass. That means
/// at least one exited task is still held by another reference. IRQ-enabled
/// runtimes also retain a bounded per-CPU timer retry; cooperative runtimes
/// without timer ticks require a later exit or another explicit reclaim pass.
pub fn reclaim_exited_tasks() -> bool {
    const DEFAULT_RECLAIM_BATCH: usize = 128;

    if crate::run_queue::has_exited_tasks() {
        crate::run_queue::reclaim_exited_tasks_current_cpu_bounded(DEFAULT_RECLAIM_BATCH);
    }
    let remains = crate::run_queue::has_exited_tasks();
    // Reclaim may have run arbitrary TaskInner/TaskExt destructors. Dispatch
    // only after the per-CPU exited queue operations have returned.
    run_deferred_work();
    remains
}

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

/// Reclaims exited tasks, yielding between bounded passes while scheduler-side
/// handoff references still keep some task objects alive.
///
/// Returns `true` when tasks still remain after the bounded yield budget.
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
    let task = TaskInner::try_new(
        move || {
            if let Some(migrated) = migrated.upgrade()
                && crate::run_queue::migrate_entry(migrated).is_err()
            {
                #[cfg(feature = "preempt")]
                current().set_preempt_pending(true);
            }
        },
        name,
        MIGRATION_TASK_STACK_SIZE,
    )
    .map_err(map_task_create_error)?;
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
        task.try_begin_affinity_mutation()
            .then_some(Self(task))
            .ok_or(AxError::ResourceBusy)
    }
}

impl Drop for AffinityMutation<'_> {
    fn drop(&mut self) {
        self.0.finish_affinity_mutation();
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
    if cpumask.is_empty() {
        return Err(AxError::InvalidInput);
    }

    #[cfg(feature = "smp")]
    if !crate::run_queue::affinity_has_online_cpu(cpumask) {
        return Err(AxError::InvalidInput);
    }

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
        if !cpumask.get(0) {
            return Err(AxError::InvalidInput);
        }
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
    if cpumask.is_empty() {
        return Err(AxError::InvalidInput);
    }

    #[cfg(feature = "smp")]
    if !crate::run_queue::affinity_has_online_cpu(cpumask) {
        return Err(AxError::InvalidInput);
    }

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
        if !cpumask.get(0) {
            return Err(AxError::InvalidInput);
        }
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
        trace!("idle task: waiting for IRQs...");
        #[cfg(feature = "irq")]
        axhal::asm::wait_for_irqs();
    }
}
