#![cfg_attr(not(test), no_std)]
#![doc = include_str!("../README.md")]

#[cfg(any(
    all(feature = "eevdf-balanced", feature = "eevdf-latency"),
    all(feature = "eevdf-balanced", feature = "eevdf-throughput"),
    all(feature = "eevdf-latency", feature = "eevdf-throughput"),
))]
compile_error!(
    "select at most one EEVDF profile: eevdf-balanced, eevdf-latency, or eevdf-throughput"
);

mod cfs;
mod eevdf;
mod eevdf_model;
mod eevdf_profile;
mod eevdf_tree;
mod fifo;
mod round_robin;

#[cfg(test)]
mod tests;

extern crate alloc;

use core::sync::atomic::{AtomicUsize, Ordering};

pub use cfs::{
    CFSTask, CFScheduler, CfsReservationCommitError, CfsTaskClass, CfsTaskParams,
    CfsTaskReservation, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN,
};
pub use eevdf::{
    EEVDFScheduler, EEVDFTask, EevdfForkSeed, EevdfMigration, EevdfMigrationCommitError,
    EevdfMigrationOrigin, EevdfParamUpdate, EevdfReadyCandidate, EevdfReadyCursor,
    EevdfReservationCommitError, EevdfTaskClass, EevdfTaskParams, EevdfTaskReservation,
};
pub use eevdf_profile::{eevdf_profile, EevdfProfile, EEVDF_PROFILE};
pub use fifo::{FifoScheduler, FifoTask};
pub use round_robin::{RRScheduler, RRTask};

/// A scheduler-owned runtime sample supplied by the task layer.
///
/// The scheduler must never read a platform clock while mutating a run queue:
/// a task layer owns the clock domain and publishes one explicit delta at every
/// ownership boundary. `period_ns` is the configured scheduler tick period and
/// lets schedulers retain sub-tick runtime without rounding each boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeDelta {
    elapsed_ns: u64,
    period_ns: u64,
}

impl RuntimeDelta {
    /// Constructs a runtime delta. A zero period is invalid and is represented
    /// as a zero delta so a corrupt platform configuration cannot divide by
    /// zero inside a scheduler hot path.
    pub const fn new(elapsed_ns: u64, period_ns: u64) -> Self {
        if period_ns == 0 {
            Self {
                elapsed_ns: 0,
                period_ns: 1,
            }
        } else {
            Self {
                elapsed_ns,
                period_ns,
            }
        }
    }

    /// Returns the elapsed wall-clock duration captured by the task layer.
    pub const fn elapsed_ns(self) -> u64 {
        self.elapsed_ns
    }

    /// Returns the configured scheduler period used for sub-tick carry.
    pub const fn period_ns(self) -> u64 {
        self.period_ns
    }

    /// Returns the number of complete scheduler periods in this sample.
    pub const fn whole_periods(self) -> u64 {
        self.elapsed_ns / self.period_ns
    }

    /// Whether this sample contains no elapsed runtime.
    pub const fn is_zero(self) -> bool {
        self.elapsed_ns == 0
    }
}

const UNOWNED: usize = 0;
const CONFIGURING: usize = usize::MAX;
static NEXT_SCHEDULER_ID: AtomicUsize = AtomicUsize::new(1);

pub(crate) fn try_update_usize<F>(
    atomic: &AtomicUsize,
    set: Ordering,
    fail: Ordering,
    mut update: F,
) -> Result<usize, usize>
where
    F: FnMut(usize) -> Option<usize>,
{
    let mut current = atomic.load(fail);
    loop {
        let Some(next) = update(current) else {
            return Err(current);
        };
        match atomic.compare_exchange_weak(current, next, set, fail) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

pub(crate) fn try_update_isize<F>(
    atomic: &core::sync::atomic::AtomicIsize,
    set: Ordering,
    fail: Ordering,
    mut update: F,
) -> Result<isize, isize>
where
    F: FnMut(isize) -> Option<isize>,
{
    let mut current = atomic.load(fail);
    loop {
        let Some(next) = update(current) else {
            return Err(current);
        };
        match atomic.compare_exchange_weak(current, next, set, fail) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

fn allocate_scheduler_id() -> Result<usize, SchedulerError> {
    try_update_usize(
        &NEXT_SCHEDULER_ID,
        Ordering::Relaxed,
        Ordering::Relaxed,
        |current| current.checked_add(1),
    )
    .map_err(|_| SchedulerError::IdentifierExhausted)
}

/// Failure returned by a scheduler mechanism operation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SchedulerError {
    /// The selected scheduler does not implement this operation.
    UnsupportedOperation,
    /// The task is already queued in this scheduler.
    AlreadyQueued,
    /// The task is currently owned by another scheduler instance.
    ForeignQueue,
    /// The global scheduler-instance identifier space was exhausted.
    IdentifierExhausted,
    /// A monotonic scheduler-local ordering sequence was exhausted.
    ///
    /// Ordering identities never wrap or get reused; a reservation issued
    /// before exhaustion remains valid and committable.
    SequenceExhausted,
    /// A checked virtual-time, weight, or deadline calculation overflowed.
    ArithmeticExhausted,
    /// A task is undergoing an atomic configuration transaction.
    TaskBusy,
    /// Scheduling parameters were outside the mechanism's accepted domain.
    InvalidParameters,
    /// The requested operation is not defined for this scheduling class.
    IncompatibleClass,
    /// A round-robin scheduler was instantiated with a zero tick budget.
    InvalidTimeSlice,
    /// Private queue membership metadata disagreed with the queue contents.
    ///
    /// Safe callers cannot create this state. It is reported instead of
    /// panicking so a kernel can contain and diagnose an internal defect.
    InconsistentState,
}

/// Why a runnable task is being enqueued.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EnqueueReason {
    /// A brand-new task is entering the scheduler.
    New,
    /// A previously blocked task has become runnable again.
    Wakeup,
    /// The task yielded cooperatively.
    Yield,
    /// The task was preempted and should keep as much state as possible.
    Preempt,
    /// The task is being transferred from another run queue.
    ///
    /// Unlike a wakeup, migration must not apply sleeper placement policy.
    /// Fair schedulers may use a preceding migration lifecycle hook to rebase
    /// queue-local virtual-time state at this enqueue boundary.
    Migrate,
}

/// Why a running task is leaving its scheduler's current-entity state.
///
/// The current entity is not necessarily linked into a ready queue, so this
/// hook is separate from [`BaseScheduler::remove_task`]. It gives virtual-time
/// schedulers an allocation-free place to snapshot sleep or migration state.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DeactivateReason {
    /// The task is becoming blocked.
    Sleep,
    /// The task is leaving the scheduler permanently.
    Exit,
    /// The task will be enqueued on another run queue.
    Migrate,
}

/// The base scheduler trait that all schedulers should implement.
///
/// All tasks in the scheduler are considered runnable. If a task is go to
/// sleep, it should be removed from the scheduler.
pub trait BaseScheduler {
    /// Type of scheduled entities. Often a task struct.
    type SchedItem;

    /// Initializes the scheduler.
    fn init(&mut self);

    /// Adds a task to the scheduler.
    fn add_task(&mut self, task: Self::SchedItem) -> Result<(), SchedulerError>;

    /// Removes a task by reference and returns its owned scheduler item.
    ///
    /// Returns [`None`] when the task is not linked into any scheduler and
    /// [`SchedulerError::ForeignQueue`] when another scheduler owns it.
    fn remove_task(
        &mut self,
        task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError>;

    /// Removes a ready task specifically for transfer to another run queue.
    ///
    /// The default implementation is suitable for schedulers without
    /// queue-local virtual time. Fair schedulers can override it to snapshot a
    /// relative position before releasing queue ownership.
    fn remove_task_for_migration(
        &mut self,
        task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError> {
        self.remove_task(task)
    }

    /// Records that the current, unqueued task is leaving the CPU.
    ///
    /// This is deliberately infallible: blocking, exit, and CPU-affinity
    /// enforcement cannot safely strand a task because optional scheduler
    /// bookkeeping failed. Mechanisms with no lifecycle state use this no-op.
    fn deactivate_task(&mut self, _task: &Self::SchedItem, _reason: DeactivateReason) {}

    /// Picks the next task to run, it will be removed from the scheduler.
    /// Returns [`None`] if there is not runnable task.
    fn pick_next_task(&mut self) -> Option<Self::SchedItem>;

    /// Puts the previous task back to the scheduler. The previous task is
    /// usually placed at the end of the ready queue, making it less likely
    /// to be re-scheduled.
    ///
    /// `preempt` indicates whether the previous task is preempted by the next
    /// task. In this case, the previous task may be placed at the front of the
    /// ready queue.
    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool)
        -> Result<(), SchedulerError>;

    /// Enqueues a runnable task for the specified reason.
    ///
    /// The default implementation preserves the legacy split between
    /// `add_task()` for fresh/woken tasks and `put_prev_task()` for tasks that
    /// were already running.
    fn enqueue_task(
        &mut self,
        task: Self::SchedItem,
        reason: EnqueueReason,
    ) -> Result<(), SchedulerError> {
        match reason {
            EnqueueReason::New | EnqueueReason::Wakeup | EnqueueReason::Migrate => {
                self.add_task(task)
            }
            EnqueueReason::Yield => self.put_prev_task(task, false),
            EnqueueReason::Preempt => self.put_prev_task(task, true),
        }
    }

    /// Advances the scheduler state at each timer tick. Returns `true` if
    /// re-scheduling is required.
    ///
    /// `current` is the current running task.
    fn task_tick(&mut self, current: &Self::SchedItem) -> bool;

    /// Accounts one explicit wall-clock runtime sample for `current`.
    ///
    /// Implementations with finer-grained accounting should override this
    /// method. The default preserves legacy tick schedulers while avoiding a
    /// synthetic tick for a sub-period boundary.
    fn account_runtime(&mut self, current: &Self::SchedItem, delta: RuntimeDelta) -> bool {
        let mut periods = delta.whole_periods();
        let mut reschedule = false;
        while periods != 0 {
            reschedule |= self.task_tick(current);
            periods -= 1;
        }
        reschedule
    }

    /// Sets the scheduler-specific priority of a task.
    ///
    /// Returns a typed error when runtime updates are unsupported, the value is
    /// invalid, or the task cannot participate in the scheduler transaction.
    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> Result<(), SchedulerError>;
}
