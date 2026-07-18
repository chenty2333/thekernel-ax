use core::{fmt, future::Future, future::poll_fn, pin::Pin, task::Poll, time::Duration};

use axerrno::AxError;
use axhal::time::{TimeValue, wall_time};
use event_listener::{Event, listener};

use crate::future::{BlockOnError, DeadlineReservation, Elapsed, TimerRegistrationError, block_on};

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

fn checked_wait_deadline(now: TimeValue, dur: Duration) -> Result<TimeValue, WaitError> {
    now.checked_add(dur)
        .ok_or(WaitError::Timer(TimerRegistrationError::DeadlineOverflow))
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum WaitWake {
    Notified,
    Interrupted,
}

fn poll_listener<L>(mut listener: Pin<&mut L>, cx: &mut core::task::Context<'_>) -> Poll<WaitWake>
where
    L: Future<Output = ()> + ?Sized,
{
    listener.as_mut().poll(cx).map(|()| WaitWake::Notified)
}

fn poll_listener_interruptible<L>(
    mut listener: Pin<&mut L>,
    task: &crate::AxTask,
    cx: &mut core::task::Context<'_>,
) -> Poll<WaitWake>
where
    L: Future<Output = ()> + ?Sized,
{
    if let Poll::Ready(wake) = poll_listener(listener.as_mut(), cx) {
        return Poll::Ready(wake);
    }

    let interrupted = task.poll_interrupt(cx).is_ready();
    if let Poll::Ready(wake) = poll_listener(listener, cx) {
        if interrupted {
            task.interrupt();
        }
        return Poll::Ready(wake);
    }

    if interrupted {
        Poll::Ready(WaitWake::Interrupted)
    } else {
        Poll::Pending
    }
}

fn poll_interrupt_now(task: &crate::AxTask) -> bool {
    let cx = core::task::Context::from_waker(core::task::Waker::noop());
    task.poll_interrupt(&cx).is_ready()
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
    /// the condition becomes true. The predicate is evaluated outside the
    /// internal synchronous block session, so it may acquire sleeping locks;
    /// listener publication is protected by a check-arm-check sequence.
    pub fn wait_until<F>(&self, mut condition: F) -> Result<(), WaitError>
    where
        F: FnMut() -> bool,
    {
        loop {
            if condition() {
                return Ok(());
            }
            listener!(self.event => listener);
            if condition() {
                return Ok(());
            }
            block_on(listener).map_err(WaitError::Block)?;
        }
    }

    /// Blocks the current task until the given `condition` becomes true, or
    /// the task is interrupted. The predicate remains outside the internal
    /// synchronous block session.
    pub fn wait_until_interruptible<F>(&self, mut condition: F) -> Result<(), WaitError>
    where
        F: FnMut() -> bool,
    {
        let task = crate::current().clone();
        loop {
            if condition() {
                return Ok(());
            }
            listener!(self.event => listener);
            if condition() {
                return Ok(());
            }

            let wake = block_on(poll_fn(|cx| {
                poll_listener_interruptible(Pin::new(&mut listener), &task, cx)
            }))
            .map_err(WaitError::Block)?;
            if wake == WaitWake::Interrupted {
                if condition() {
                    task.interrupt();
                    return Ok(());
                }
                return Err(WaitError::Interrupted);
            }
        }
    }

    /// Blocks the current task and put it into the wait queue, until other tasks
    /// notify it, or the given duration has elapsed.
    pub fn wait_timeout(&self, dur: Duration) -> Result<bool, WaitError> {
        let deadline = checked_wait_deadline(wall_time(), dur)?;
        listener!(self.event => listener);
        let mut deadline = match DeadlineReservation::reserve(deadline) {
            Ok(deadline) => deadline,
            Err(error) => {
                let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
                return if poll_listener(Pin::new(&mut listener), &mut cx).is_ready() {
                    Ok(false)
                } else {
                    Err(WaitError::Timer(error))
                };
            }
        };
        let blocked =
            block_on(deadline.race(poll_fn(|cx| poll_listener(Pin::new(&mut listener), cx))));
        match blocked.map_err(WaitError::Block)? {
            Ok(WaitWake::Notified) => Ok(false),
            Ok(WaitWake::Interrupted) => {
                unreachable!("non-interruptible wait reported interruption")
            }
            Err(Elapsed) => Ok(true),
        }
    }

    /// Blocks the current task and put it into the wait queue, until the given
    /// `condition` becomes true, or the given duration has elapsed.
    ///
    /// Note that even other tasks notify this task, it will not wake up until
    /// the above conditions are met. The predicate remains outside the
    /// internal synchronous block session.
    pub fn wait_timeout_until<F>(&self, dur: Duration, mut condition: F) -> Result<bool, WaitError>
    where
        F: FnMut() -> bool,
    {
        // Match the condition-first contract of the other conditional waits:
        // an already-satisfied predicate needs neither a timer reservation nor
        // consumption of a pending interruption.
        if condition() {
            return Ok(false);
        }
        let deadline = checked_wait_deadline(wall_time(), dur)?;
        let mut deadline = match DeadlineReservation::reserve(deadline) {
            Ok(deadline) => deadline,
            Err(error) => {
                return if condition() {
                    Ok(false)
                } else {
                    Err(WaitError::Timer(error))
                };
            }
        };

        loop {
            listener!(self.event => listener);
            if condition() {
                return Ok(false);
            }
            let blocked =
                block_on(deadline.race(poll_fn(|cx| poll_listener(Pin::new(&mut listener), cx))));
            match blocked.map_err(WaitError::Block)? {
                Ok(WaitWake::Notified) => {}
                Ok(WaitWake::Interrupted) => {
                    unreachable!("non-interruptible wait reported interruption")
                }
                Err(Elapsed) => return Ok(!condition()),
            }
        }
    }

    /// Blocks until `condition` becomes true, the complete duration elapses,
    /// or the current task is interrupted.
    ///
    /// Returns `Ok(false)` when the condition wins and `Ok(true)` when the
    /// deadline wins. Interruption, bounded timer admission, and synchronous
    /// block-session failures remain distinct [`WaitError`] variants. One timer
    /// covers the whole wait; this method does not approximate the deadline by
    /// repeatedly sleeping for short polling slices. The condition has priority
    /// when it becomes true in the same observation window as interruption or
    /// timeout, and is never evaluated inside the internal synchronous block
    /// session.
    pub fn wait_timeout_until_interruptible<F>(
        &self,
        dur: Duration,
        mut condition: F,
    ) -> Result<bool, WaitError>
    where
        F: FnMut() -> bool,
    {
        if condition() {
            return Ok(false);
        }
        let deadline = checked_wait_deadline(wall_time(), dur)?;
        let task = crate::current().clone();
        let mut deadline = match DeadlineReservation::reserve(deadline) {
            Ok(deadline) => deadline,
            Err(error) => {
                if condition() {
                    return Ok(false);
                }
                let interrupted = poll_interrupt_now(&task);
                if condition() {
                    if interrupted {
                        task.interrupt();
                    }
                    return Ok(false);
                }
                return if interrupted {
                    Err(WaitError::Interrupted)
                } else {
                    Err(WaitError::Timer(error))
                };
            }
        };

        loop {
            listener!(self.event => listener);
            if condition() {
                return Ok(false);
            }
            let blocked = block_on(deadline.race(poll_fn(|cx| {
                poll_listener_interruptible(Pin::new(&mut listener), &task, cx)
            })));
            match blocked.map_err(WaitError::Block)? {
                Ok(WaitWake::Notified) => {}
                Ok(WaitWake::Interrupted) => {
                    if condition() {
                        task.interrupt();
                        return Ok(false);
                    }
                    return Err(WaitError::Interrupted);
                }
                Err(Elapsed) => return Ok(!condition()),
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_overflow_is_typed() {
        assert_eq!(
            checked_wait_deadline(Duration::from_nanos(1), Duration::MAX),
            Err(WaitError::Timer(TimerRegistrationError::DeadlineOverflow))
        );
    }
}
