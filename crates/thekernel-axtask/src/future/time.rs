//! Bounded, owner-CPU timer futures.

use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::AxError;
use axhal::time::{TimeValue, wall_time};
use futures_util::{FutureExt, select_biased};
use kspin::SpinNoIrq;

/// Maximum number of simultaneous timer futures admitted per CPU.
pub const TIMER_FUTURE_CAPACITY: usize = 256;

/// Failure to reserve a bounded timer future.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TimerRegistrationError {
    /// Every timer slot on the owner CPU is occupied.
    CapacityExhausted,
    /// Every reusable free slot exhausted its generation space.
    TokenSpaceExhausted,
    /// Computing an absolute deadline overflowed [`TimeValue`].
    DeadlineOverflow,
}

impl fmt::Display for TimerRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExhausted => formatter.write_str("per-CPU timer registry is full"),
            Self::TokenSpaceExhausted => {
                formatter.write_str("timer registration token space is exhausted")
            }
            Self::DeadlineOverflow => formatter.write_str("timer deadline overflowed"),
        }
    }
}

impl core::error::Error for TimerRegistrationError {}

impl From<TimerRegistrationError> for AxError {
    fn from(error: TimerRegistrationError) -> Self {
        match error {
            TimerRegistrationError::CapacityExhausted => AxError::ResourceBusy,
            TimerRegistrationError::TokenSpaceExhausted
            | TimerRegistrationError::DeadlineOverflow => AxError::OutOfRange,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimerToken {
    owner_cpu: usize,
    slot: usize,
    generation: u64,
}

struct TimerSlot {
    deadline: TimeValue,
    generation: u64,
    occupied: bool,
    waker: Option<Waker>,
}

impl TimerSlot {
    const fn new() -> Self {
        Self {
            deadline: Duration::ZERO,
            generation: 0,
            occupied: false,
            waker: None,
        }
    }
}

struct TimerInner {
    slots: [TimerSlot; TIMER_FUTURE_CAPACITY],
    next: usize,
    len: usize,
}

impl TimerInner {
    const fn new() -> Self {
        Self {
            slots: [const { TimerSlot::new() }; TIMER_FUTURE_CAPACITY],
            next: 0,
            len: 0,
        }
    }

    fn reserve(
        &mut self,
        owner_cpu: usize,
        deadline: TimeValue,
    ) -> Result<TimerToken, TimerRegistrationError> {
        if self.len == TIMER_FUTURE_CAPACITY {
            return Err(TimerRegistrationError::CapacityExhausted);
        }

        let Some(slot) = (self.next..TIMER_FUTURE_CAPACITY)
            .chain(0..self.next)
            .find(|&slot| {
                let entry = &self.slots[slot];
                !entry.occupied && entry.generation < u64::MAX
            })
        else {
            return Err(TimerRegistrationError::TokenSpaceExhausted);
        };

        let entry = &mut self.slots[slot];
        entry.generation += 1;
        entry.deadline = deadline;
        entry.occupied = true;
        self.len += 1;
        self.next = if slot + 1 == TIMER_FUTURE_CAPACITY {
            0
        } else {
            slot + 1
        };

        Ok(TimerToken {
            owner_cpu,
            slot,
            generation: entry.generation,
        })
    }

    fn is_live(&self, token: TimerToken) -> bool {
        self.slots
            .get(token.slot)
            .is_some_and(|entry| entry.occupied && entry.generation == token.generation)
    }

    fn poll(
        &mut self,
        token: TimerToken,
        candidate: &Waker,
        owned: Waker,
        now: TimeValue,
    ) -> (Poll<()>, [Option<Waker>; 2]) {
        let mut deferred = [None, None];
        if !self.is_live(token) {
            deferred[0] = Some(owned);
            return (Poll::Ready(()), deferred);
        }

        let entry = &mut self.slots[token.slot];
        if entry.deadline <= now {
            entry.occupied = false;
            self.len -= 1;
            self.next = token.slot;
            deferred[0] = entry.waker.take();
            deferred[1] = Some(owned);
            return (Poll::Ready(()), deferred);
        }

        if entry
            .waker
            .as_ref()
            .is_some_and(|registered| registered.will_wake(candidate))
        {
            deferred[0] = Some(owned);
        } else {
            deferred[0] = entry.waker.replace(owned);
        }
        (Poll::Pending, deferred)
    }

    fn cancel(&mut self, token: TimerToken) -> Option<Waker> {
        if !self.is_live(token) {
            return None;
        }
        let entry = &mut self.slots[token.slot];
        entry.occupied = false;
        self.len -= 1;
        self.next = token.slot;
        entry.waker.take()
    }

    fn drain_expired(
        &mut self,
        now: TimeValue,
        pending: &mut [Option<Waker>; TIMER_FUTURE_CAPACITY],
    ) -> usize {
        let mut count = 0;
        for (slot, pending_waker) in self.slots.iter_mut().zip(pending) {
            if slot.occupied && slot.deadline <= now {
                slot.occupied = false;
                self.len -= 1;
                *pending_waker = slot.waker.take();
                count += 1;
            }
        }
        if count != 0 {
            self.next = 0;
        }
        count
    }
}

struct TimerRuntime(SpinNoIrq<TimerInner>);

impl TimerRuntime {
    const fn new() -> Self {
        Self(SpinNoIrq::new(TimerInner::new()))
    }

    fn reserve(
        &self,
        owner_cpu: usize,
        deadline: TimeValue,
    ) -> Result<TimerToken, TimerRegistrationError> {
        self.0.lock().reserve(owner_cpu, deadline)
    }

    fn poll(&self, token: TimerToken, cx: &mut Context<'_>) -> Poll<()> {
        // RawWaker clone and any replaced RawWaker destruction must stay out of
        // the IRQ-safe timer lock.
        let owned = cx.waker().clone();
        let (result, deferred) = self.0.lock().poll(token, cx.waker(), owned, wall_time());
        drop(deferred);
        result
    }

    fn cancel(&self, token: TimerToken) {
        let deferred = self.0.lock().cancel(token);
        drop(deferred);
    }

    fn wake_expired(&self) -> usize {
        let mut pending = [const { None }; TIMER_FUTURE_CAPACITY];
        let count = self.0.lock().drain_expired(wall_time(), &mut pending);
        for waker in pending.into_iter().flatten() {
            waker.wake();
        }
        count
    }
}

percpu_static! {
    TIMER_RUNTIME: TimerRuntime = TimerRuntime::new(),
}

fn runtime_for(cpu_id: usize) -> &'static TimerRuntime {
    // SAFETY: owner CPU IDs come from `this_cpu_id()` while that CPU's per-CPU
    // area is initialized. TimerRuntime has its own cross-CPU lock, so task
    // migration cannot create mutable aliases.
    unsafe { TIMER_RUNTIME.remote_ref_raw(cpu_id) }
}

#[allow(dead_code)]
pub(crate) fn check_timer_events() {
    let cpu_id = axhal::percpu::this_cpu_id();
    if runtime_for(cpu_id).wake_expired() != 0 {
        #[cfg(feature = "preempt")]
        crate::current().set_preempt_pending(true);
    }
}

/// Future returned by [`sleep`] and [`sleep_until`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct TimerFuture(TimerToken);

impl TimerFuture {
    fn reserve(deadline: TimeValue) -> Result<Option<Self>, TimerRegistrationError> {
        if deadline <= wall_time() {
            return Ok(None);
        }
        let owner_cpu = axhal::percpu::this_cpu_id();
        runtime_for(owner_cpu)
            .reserve(owner_cpu, deadline)
            .map(|token| Some(Self(token)))
    }
}

impl Future for TimerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        runtime_for(self.0.owner_cpu).poll(self.0, cx)
    }
}

impl Drop for TimerFuture {
    fn drop(&mut self) {
        runtime_for(self.0.owner_cpu).cancel(self.0);
    }
}

#[cfg(test)]
pub(crate) fn reserve_timer_for_test(
    deadline: TimeValue,
) -> Result<Option<TimerFuture>, TimerRegistrationError> {
    TimerFuture::reserve(deadline)
}

#[cfg(test)]
pub(crate) fn timer_future_count_for_test() -> usize {
    let cpu_id = axhal::percpu::this_cpu_id();
    runtime_for(cpu_id).0.lock().len
}

/// Waits until `duration` has elapsed.
pub async fn sleep(duration: Duration) -> Result<(), TimerRegistrationError> {
    let deadline = wall_time()
        .checked_add(duration)
        .ok_or(TimerRegistrationError::DeadlineOverflow)?;
    sleep_until(deadline).await
}

/// Waits until `deadline` is reached.
pub async fn sleep_until(deadline: TimeValue) -> Result<(), TimerRegistrationError> {
    if let Some(timer) = TimerFuture::reserve(deadline)? {
        timer.await;
    }
    Ok(())
}

/// Error returned when a timeout expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "deadline elapsed")
    }
}

impl core::error::Error for Elapsed {}

/// Failure returned by [`timeout`] and [`timeout_at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutError {
    /// The requested deadline elapsed before the operation completed.
    Elapsed(Elapsed),
    /// The bounded timer mechanism could not admit the timeout.
    Timer(TimerRegistrationError),
}

impl fmt::Display for TimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Elapsed(error) => error.fmt(formatter),
            Self::Timer(error) => error.fmt(formatter),
        }
    }
}

impl core::error::Error for TimeoutError {}

impl From<TimeoutError> for AxError {
    fn from(error: TimeoutError) -> Self {
        match error {
            TimeoutError::Elapsed(_) => AxError::TimedOut,
            TimeoutError::Timer(error) => error.into(),
        }
    }
}

/// Requires a `Future` to complete before the specified duration has elapsed.
pub async fn timeout<F: IntoFuture>(
    duration: Option<Duration>,
    f: F,
) -> Result<F::Output, TimeoutError> {
    let deadline =
        match duration {
            Some(duration) => Some(wall_time().checked_add(duration).ok_or(
                TimeoutError::Timer(TimerRegistrationError::DeadlineOverflow),
            )?),
            None => None,
        };
    timeout_at(deadline, f).await
}

/// Requires a `Future` to complete before the specified deadline.
pub async fn timeout_at<F: IntoFuture>(
    deadline: Option<TimeValue>,
    f: F,
) -> Result<F::Output, TimeoutError> {
    if let Some(deadline) = deadline {
        select_biased! {
            res = f.into_future().fuse() => Ok(res),
            timer = sleep_until(deadline).fuse() => match timer {
                Ok(()) => Err(TimeoutError::Elapsed(Elapsed)),
                Err(error) => Err(TimeoutError::Timer(error)),
            },
        }
    } else {
        Ok(f.await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_inner_is_bounded_and_generation_checked() {
        let mut inner = TimerInner::new();
        let deadline = Duration::from_secs(1);
        let first = inner.reserve(0, deadline).unwrap();
        assert!(inner.cancel(first).is_none());
        let second = inner.reserve(0, deadline).unwrap();
        assert_ne!(first.generation, second.generation);
        assert!(inner.cancel(first).is_none());
        assert!(inner.is_live(second));

        for _ in 1..TIMER_FUTURE_CAPACITY {
            inner.reserve(0, deadline).unwrap();
        }
        assert_eq!(
            inner.reserve(0, deadline),
            Err(TimerRegistrationError::CapacityExhausted)
        );
    }
}
