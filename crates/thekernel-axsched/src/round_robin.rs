use alloc::sync::Arc;
use core::{
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
};

use linked_list_r4l::{GetLinks, Links, List};

use crate::{allocate_scheduler_id, BaseScheduler, SchedulerError, UNOWNED};

/// A task wrapper for the [`RRScheduler`].
pub struct RRTask<T, const MAX_TIME_SLICE: usize> {
    inner: T,
    time_slice: AtomicUsize,
    links: Links<Self>,
    queue_owner: AtomicUsize,
}

impl<T, const S: usize> RRTask<T, S> {
    /// Creates an unqueued [`RRTask`].
    ///
    /// A zero const budget can be represented but is rejected explicitly when
    /// the task is submitted to a scheduler.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            time_slice: AtomicUsize::new(S),
            links: Links::new(),
            queue_owner: AtomicUsize::new(UNOWNED),
        }
    }

    fn time_slice(&self) -> usize {
        self.time_slice.load(Ordering::Acquire)
    }

    fn reset_time_slice(&self) {
        self.time_slice.store(S, Ordering::Release);
    }

    /// Returns a reference to the wrapped task.
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Consumes the scheduler wrapper and returns the wrapped task.
    pub fn into_inner(self) -> T {
        self.inner
    }

    fn claim(&self, scheduler_id: usize) -> Result<(), SchedulerError> {
        match self.queue_owner.compare_exchange(
            UNOWNED,
            scheduler_id,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(owner) if owner == scheduler_id => Err(SchedulerError::AlreadyQueued),
            Err(_) => Err(SchedulerError::ForeignQueue),
        }
    }

    fn release(&self, scheduler_id: usize) {
        self.queue_owner
            .compare_exchange(scheduler_id, UNOWNED, Ordering::AcqRel, Ordering::Acquire)
            .expect("round-robin task queue owner invariant violated");
    }

    fn owner(&self) -> usize {
        self.queue_owner.load(Ordering::Acquire)
    }
}

impl<T, const S: usize> GetLinks for RRTask<T, S> {
    type EntryType = Self;

    fn get_links(data: &Self::EntryType) -> &Links<Self::EntryType> {
        &data.links
    }
}

impl<T, const S: usize> Deref for RRTask<T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A simple [Round-Robin] (RR) preemptive scheduler.
///
/// Each task has a nonzero time slice that is decremented on timer ticks. An
/// expired task rotates behind its ready peers.
///
/// [Round-Robin]: https://en.wikipedia.org/wiki/Round-robin_scheduling
pub struct RRScheduler<T, const MAX_TIME_SLICE: usize> {
    ready_queue: List<Arc<RRTask<T, MAX_TIME_SLICE>>>,
    id: usize,
}

impl<T, const S: usize> RRScheduler<T, S> {
    /// Creates a new empty [`RRScheduler`].
    ///
    /// A zero const budget is retained as an invalid configuration and every
    /// enqueue reports [`SchedulerError::InvalidTimeSlice`].
    pub const fn new() -> Self {
        Self {
            ready_queue: List::new(),
            id: UNOWNED,
        }
    }

    /// Returns the scheduler name.
    pub fn scheduler_name() -> &'static str {
        "Round-robin"
    }

    fn ensure_id(&mut self) -> Result<usize, SchedulerError> {
        if self.id == UNOWNED {
            self.id = allocate_scheduler_id()?;
        }
        Ok(self.id)
    }

    fn claim(&mut self, task: &RRTask<T, S>) -> Result<(), SchedulerError> {
        if S == 0 {
            return Err(SchedulerError::InvalidTimeSlice);
        }
        task.claim(self.ensure_id()?)
    }
}

impl<T, const S: usize> BaseScheduler for RRScheduler<T, S> {
    type SchedItem = Arc<RRTask<T, S>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) -> Result<(), SchedulerError> {
        self.claim(&task)?;
        self.ready_queue.push_back(task);
        Ok(())
    }

    fn remove_task(
        &mut self,
        task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError> {
        match task.owner() {
            UNOWNED => return Ok(None),
            owner if owner != self.id || self.id == UNOWNED => {
                return Err(SchedulerError::ForeignQueue);
            }
            _ => {}
        }

        let mut cursor = self.ready_queue.cursor_front_mut();
        loop {
            let matches = cursor
                .current()
                .is_some_and(|queued| core::ptr::eq(queued, Arc::as_ptr(task)));
            if matches {
                let removed = cursor
                    .remove_current()
                    .expect("round-robin queue cursor lost its current task");
                removed.release(self.id);
                return Ok(Some(removed));
            }
            assert!(
                cursor.current().is_some(),
                "round-robin queue owner points at a scheduler that does not contain the task"
            );
            cursor.move_next();
        }
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        let task = self.ready_queue.pop_front()?;
        task.release(self.id);
        Some(task)
    }

    fn put_prev_task(
        &mut self,
        prev: Self::SchedItem,
        preempt: bool,
    ) -> Result<(), SchedulerError> {
        self.claim(&prev)?;
        if prev.time_slice() > 0 && preempt {
            self.ready_queue.push_front(prev);
        } else {
            prev.reset_time_slice();
            self.ready_queue.push_back(prev);
        }
        Ok(())
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        let old_slice = current
            .time_slice
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |slice| {
                Some(slice.saturating_sub(1))
            })
            .unwrap_or(0);
        old_slice <= 1
    }

    fn set_priority(&mut self, _task: &Self::SchedItem, _prio: isize) -> bool {
        false
    }
}

impl<T, const S: usize> Default for RRScheduler<T, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const S: usize> Drop for RRScheduler<T, S> {
    fn drop(&mut self) {
        while let Some(task) = self.ready_queue.pop_front() {
            task.release(self.id);
        }
    }
}
