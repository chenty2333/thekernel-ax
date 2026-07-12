use alloc::sync::Arc;
use core::{
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
};

use linked_list_r4l::{GetLinks, Links, List};

use crate::{allocate_scheduler_id, BaseScheduler, SchedulerError, UNOWNED};

/// A task wrapper for the [`FifoScheduler`].
pub struct FifoTask<T> {
    inner: T,
    links: Links<Self>,
    queue_owner: AtomicUsize,
}

impl<T> FifoTask<T> {
    /// Creates an unqueued task wrapper.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            links: Links::new(),
            queue_owner: AtomicUsize::new(UNOWNED),
        }
    }

    /// Returns a reference to the wrapped task.
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Consumes the wrapper and returns the wrapped task.
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
            .expect("FIFO task queue owner invariant violated");
    }

    fn owner(&self) -> usize {
        self.queue_owner.load(Ordering::Acquire)
    }
}

impl<T> GetLinks for FifoTask<T> {
    type EntryType = Self;

    fn get_links(data: &Self::EntryType) -> &Links<Self::EntryType> {
        &data.links
    }
}

impl<T> Deref for FifoTask<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A simple FIFO (First-In-First-Out) cooperative scheduler.
///
/// When a task is added to the scheduler, it's placed at the end of the ready
/// queue. When picking the next task to run, the head of the ready queue is
/// taken.
///
/// As it's a cooperative scheduler, it does nothing when the timer tick occurs.
pub struct FifoScheduler<T> {
    ready_queue: List<Arc<FifoTask<T>>>,
    id: usize,
}

impl<T> FifoScheduler<T> {
    /// Creates a new empty [`FifoScheduler`].
    pub const fn new() -> Self {
        Self {
            ready_queue: List::new(),
            id: UNOWNED,
        }
    }

    /// Returns the scheduler name.
    pub fn scheduler_name() -> &'static str {
        "FIFO"
    }

    fn ensure_id(&mut self) -> Result<usize, SchedulerError> {
        if self.id == UNOWNED {
            self.id = allocate_scheduler_id()?;
        }
        Ok(self.id)
    }
}

impl<T> BaseScheduler for FifoScheduler<T> {
    type SchedItem = Arc<FifoTask<T>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) -> Result<(), SchedulerError> {
        let id = self.ensure_id()?;
        task.claim(id)?;
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
                    .expect("FIFO queue cursor lost its current task");
                removed.release(self.id);
                return Ok(Some(removed));
            }
            assert!(
                cursor.current().is_some(),
                "FIFO queue owner points at a scheduler that does not contain the task"
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
        _preempt: bool,
    ) -> Result<(), SchedulerError> {
        self.add_task(prev)
    }

    fn task_tick(&mut self, _current: &Self::SchedItem) -> bool {
        false
    }

    fn set_priority(
        &mut self,
        _task: &Self::SchedItem,
        _prio: isize,
    ) -> Result<(), SchedulerError> {
        Err(SchedulerError::UnsupportedOperation)
    }
}

impl<T> Default for FifoScheduler<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for FifoScheduler<T> {
    fn drop(&mut self) {
        while let Some(task) = self.ready_queue.pop_front() {
            task.release(self.id);
        }
    }
}
