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
    #[cfg(test)]
    admissions: usize,
}

impl TimerInner {
    const fn new() -> Self {
        Self {
            slots: [const { TimerSlot::new() }; TIMER_FUTURE_CAPACITY],
            next: 0,
            len: 0,
            #[cfg(test)]
            admissions: 0,
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
        #[cfg(test)]
        {
            self.admissions += 1;
        }
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

    fn disarm(&mut self, token: TimerToken) -> Option<Waker> {
        if !self.is_live(token) {
            return None;
        }
        self.slots[token.slot].waker.take()
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

    fn disarm(&self, token: TimerToken) {
        let deferred = self.0.lock().disarm(token);
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

/// Ownership of one bounded absolute-deadline reservation.
///
/// Unlike [`TimerFuture`], this reservation may cover several consecutive
/// synchronous block sessions without repeating timer admission. Use
/// [`DeadlineReservation::race`] to borrow it for each block session. The
/// borrowed future removes its task waker when that session completes or is
/// dropped while retaining the reservation for a later session. Dropping the
/// reservation refunds a still-live timer slot exactly once.
///
/// The registry token remains private so callers cannot poll, disarm, or
/// cancel another reservation accidentally.
#[must_use = "a deadline reservation must be retained until it is elapsed or cancelled"]
pub struct DeadlineReservation {
    token: Option<TimerToken>,
}

impl DeadlineReservation {
    /// Reserves one timer-registry slot for `deadline`.
    ///
    /// An already elapsed deadline needs no slot and produces a reservation
    /// whose first race reports [`Elapsed`] unless the other future is ready.
    pub fn reserve(deadline: TimeValue) -> Result<Self, TimerRegistrationError> {
        if deadline <= wall_time() {
            return Ok(Self { token: None });
        }

        let owner_cpu = axhal::percpu::this_cpu_id();
        let token = runtime_for(owner_cpu).reserve(owner_cpu, deadline)?;
        Ok(Self { token: Some(token) })
    }

    /// Races `future` against this reservation without repeating admission.
    ///
    /// The wrapped future is checked before the deadline and rechecked after
    /// observing expiration, so work completed in the same observation window
    /// wins. Each returned future is one borrowed wait session. Finishing or
    /// dropping that session automatically removes its registered task waker,
    /// while an unexpired reservation remains available for another call.
    pub async fn race<F: IntoFuture>(&mut self, future: F) -> Result<F::Output, Elapsed> {
        let session = DeadlineSession { reservation: self };
        let mut future = core::pin::pin!(future.into_future());

        core::future::poll_fn(|cx| {
            if let Poll::Ready(output) = future.as_mut().poll(cx) {
                return Poll::Ready(Ok(output));
            }
            if session.reservation.poll_deadline(cx).is_pending() {
                return Poll::Pending;
            }

            match future.as_mut().poll(cx) {
                Poll::Ready(output) => Poll::Ready(Ok(output)),
                Poll::Pending => Poll::Ready(Err(Elapsed)),
            }
        })
        .await
    }

    fn poll_deadline(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(token) = self.token else {
            return Poll::Ready(());
        };
        let result = runtime_for(token.owner_cpu).poll(token, cx);
        if result.is_ready() {
            self.token = None;
        }
        result
    }

    fn disarm(&mut self) {
        if let Some(token) = self.token {
            runtime_for(token.owner_cpu).disarm(token);
        }
    }
}

impl Drop for DeadlineReservation {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            runtime_for(token.owner_cpu).cancel(token);
        }
    }
}

struct DeadlineSession<'a> {
    reservation: &'a mut DeadlineReservation,
}

impl Drop for DeadlineSession<'_> {
    fn drop(&mut self) {
        self.reservation.disarm();
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

#[cfg(test)]
pub(crate) fn timer_future_waker_count_for_test() -> usize {
    let cpu_id = axhal::percpu::this_cpu_id();
    runtime_for(cpu_id)
        .0
        .lock()
        .slots
        .iter()
        .filter(|slot| slot.occupied && slot.waker.is_some())
        .count()
}

#[cfg(test)]
pub(crate) fn timer_reservation_admission_count_for_test() -> usize {
    let cpu_id = axhal::percpu::this_cpu_id();
    runtime_for(cpu_id).0.lock().admissions
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
    use alloc::{sync::Arc, task::Wake};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

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

    #[test]
    fn timer_disarm_retains_reservation_and_releases_owned_waker() {
        let mut inner = TimerInner::new();
        let token = inner.reserve(0, Duration::from_secs(2)).unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let baseline = Arc::strong_count(&counter);

        let (poll, deferred) = inner.poll(token, &waker, waker.clone(), Duration::from_secs(1));
        drop(deferred);
        assert_eq!(poll, Poll::Pending);
        assert_eq!(inner.len, 1);
        assert_eq!(Arc::strong_count(&counter), baseline + 1);

        drop(inner.disarm(token));
        assert_eq!(inner.len, 1);
        assert!(inner.is_live(token));
        assert_eq!(Arc::strong_count(&counter), baseline);

        assert!(inner.cancel(token).is_none());
        assert_eq!(inner.len, 0);
        assert_eq!(counter.0.load(Ordering::Acquire), 0);
    }
}
