#![cfg_attr(not(test), no_std)]
#![doc = include_str!("../README.md")]

mod cfs;
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
pub use fifo::{FifoScheduler, FifoTask};
pub use round_robin::{RRScheduler, RRTask};

const UNOWNED: usize = 0;
const CONFIGURING: usize = usize::MAX;
static NEXT_SCHEDULER_ID: AtomicUsize = AtomicUsize::new(1);

fn allocate_scheduler_id() -> Result<usize, SchedulerError> {
    NEXT_SCHEDULER_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
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

    /// Sets the scheduler-specific priority of a task.
    ///
    /// Returns a typed error when runtime updates are unsupported, the value is
    /// invalid, or the task cannot participate in the scheduler transaction.
    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> Result<(), SchedulerError>;
}
