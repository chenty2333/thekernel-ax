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
    CFSTask, CFScheduler, CfsTaskClass, CfsTaskParams, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX,
    RT_PRIORITY_MIN,
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

/// Failure returned by a scheduler queue operation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SchedulerError {
    /// The task is already queued in this scheduler.
    AlreadyQueued,
    /// The task is currently owned by another scheduler instance.
    ForeignQueue,
    /// The global scheduler-instance identifier space was exhausted.
    IdentifierExhausted,
    /// A scheduler-local ordering sequence was exhausted.
    SequenceExhausted,
    /// A task is undergoing an atomic configuration transaction.
    TaskBusy,
    /// Scheduling parameters were outside the mechanism's accepted domain.
    InvalidParameters,
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
            EnqueueReason::New | EnqueueReason::Wakeup => self.add_task(task),
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
    /// Returns `false` when this scheduler does not support runtime priority
    /// changes or the value is outside its generic mechanism range.
    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool;
}
