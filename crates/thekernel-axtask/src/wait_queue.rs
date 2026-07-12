use core::{fmt, time::Duration};

use axerrno::AxError;
use axhal::time::wall_time;
use event_listener::{Event, listener};

use crate::future::{
    BlockOnError, Interrupted, TimeoutError, TimerRegistrationError, block_on, interruptible,
    timeout_at,
};

/// Failure while waiting on a [`WaitQueue`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum WaitError {
    /// The current task could not start or retain its synchronous block session.
    Block(BlockOnError),
    /// The wait was interrupted by the consumer's task interruption source.
    Interrupted,
    /// The bounded timer registry could not admit a timeout.
    Timer(TimerRegistrationError),
}

impl fmt::Display for WaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block(error) => error.fmt(formatter),
            Self::Interrupted => formatter.write_str("wait interrupted"),
            Self::Timer(error) => error.fmt(formatter),
        }
    }
}

impl core::error::Error for WaitError {}

impl From<WaitError> for AxError {
    fn from(error: WaitError) -> Self {
        match error {
            WaitError::Block(error) => error.into(),
            WaitError::Interrupted => AxError::Interrupted,
            WaitError::Timer(error) => error.into(),
        }
    }
}

/// A queue to store sleeping tasks.
///
/// # Examples
///
/// ```
/// use core::sync::atomic::{AtomicU32, Ordering};
///
/// use axtask::WaitQueue;
///
/// static VALUE: AtomicU32 = AtomicU32::new(0);
/// static WQ: WaitQueue = WaitQueue::new();
///
/// axtask::init_scheduler().unwrap();
/// // spawn a new task that updates `VALUE` and notifies the main task
/// axtask::spawn(|| {
///     assert_eq!(VALUE.load(Ordering::Acquire), 0);
///     VALUE.fetch_add(1, Ordering::Release);
///     WQ.notify_one(true); // wake up the main task
/// })
/// .unwrap();
///
/// WQ.wait().unwrap(); // block until `notify()` is called
/// assert_eq!(VALUE.load(Ordering::Acquire), 1);
/// ```
pub struct WaitQueue {
    event: Event,
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitQueue {
    /// Creates an empty wait queue.
    pub const fn new() -> Self {
        Self {
            event: Event::new(),
        }
    }

    /// Blocks the current task and put it into the wait queue, until other task
    /// notifies it.
    pub fn wait(&self) -> Result<(), WaitError> {
        listener!(self.event => listener);
        block_on(listener).map_err(WaitError::Block)
    }

    /// Blocks the current task and put it into the wait queue, until the given
    /// `condition` becomes true.
    ///
    /// Note that even other tasks notify this task, it will not wake up until
    /// the condition becomes true.
    pub fn wait_until<F>(&self, mut condition: F) -> Result<(), WaitError>
    where
        F: FnMut() -> bool,
    {
        block_on(async {
            loop {
                if condition() {
                    break;
                }
                listener!(self.event => listener);
                if condition() {
                    break;
                }
                listener.await;
            }
        })
        .map_err(WaitError::Block)
    }

    /// Blocks the current task until the given `condition` becomes true, or
    /// the task is interrupted.
    pub fn wait_until_interruptible<F>(&self, mut condition: F) -> Result<(), WaitError>
    where
        F: FnMut() -> bool,
    {
        block_on(interruptible(async {
            loop {
                if condition() {
                    break;
                }
                listener!(self.event => listener);
                if condition() {
                    break;
                }
                listener.await;
            }
        }))
        .map_err(WaitError::Block)?
        .map_err(|Interrupted| WaitError::Interrupted)
    }

    /// Blocks the current task and put it into the wait queue, until other tasks
    /// notify it, or the given duration has elapsed.
    pub fn wait_timeout(&self, dur: Duration) -> Result<bool, WaitError> {
        let deadline = wall_time()
            .checked_add(dur)
            .ok_or(WaitError::Timer(TimerRegistrationError::DeadlineOverflow))?;
        block_on(async {
            listener!(self.event => listener);
            match timeout_at(Some(deadline), listener).await {
                Ok(()) => Ok(false),
                Err(TimeoutError::Elapsed(_)) => Ok(true),
                Err(TimeoutError::Timer(error)) => Err(error),
            }
        })
        .map_err(WaitError::Block)?
        .map_err(WaitError::Timer)
    }

    /// Blocks the current task and put it into the wait queue, until the given
    /// `condition` becomes true, or the given duration has elapsed.
    ///
    /// Note that even other tasks notify this task, it will not wake up until
    /// the above conditions are met.
    pub fn wait_timeout_until<F>(&self, dur: Duration, mut condition: F) -> Result<bool, WaitError>
    where
        F: FnMut() -> bool,
    {
        let deadline = wall_time()
            .checked_add(dur)
            .ok_or(WaitError::Timer(TimerRegistrationError::DeadlineOverflow))?;
        block_on(async {
            loop {
                if condition() {
                    return Ok(false);
                }
                if wall_time() >= deadline {
                    return Ok(true);
                }
                listener!(self.event => listener);
                if condition() {
                    return Ok(false);
                }
                match timeout_at(Some(deadline), listener).await {
                    Ok(()) | Err(TimeoutError::Elapsed(_)) => {}
                    Err(TimeoutError::Timer(error)) => return Err(error),
                }
            }
        })
        .map_err(WaitError::Block)?
        .map_err(WaitError::Timer)
    }

    /// Wakes up one task in the wait queue, usually the first one.
    /// This function should not be called in a loop, use `notify_many` instead.
    ///
    /// If `resched` is true, the current task will yield.
    pub fn notify_one(&self, resched: bool) -> bool {
        self.notify_many(1, resched) == 1
    }

    /// Wakes up to `count` tasks in the wait queue.
    ///
    /// If `resched` is true, the current task will yield.
    pub fn notify_many(&self, count: usize, resched: bool) -> usize {
        let n = self.event.notify(count);
        if resched {
            crate::yield_now();
        }
        n
    }

    /// Wakes all tasks in the wait queue.
    ///
    /// If `resched` is true, the current task will yield.
    pub fn notify_all(&self, resched: bool) {
        self.notify_many(usize::MAX, resched);
    }
}
