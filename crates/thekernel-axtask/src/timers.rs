//! Bounded timer-tick callback registration.

use alloc::boxed::Box;
use core::fmt;

use axhal::time::{TimeValue, wall_time};
use kspin::SpinNoIrq;

/// Maximum number of persistent timer callbacks admitted per CPU.
pub const TIMER_CALLBACK_CAPACITY: usize = 16;

type TimerCallback = dyn Fn(TimeValue) + Send + Sync;

/// Opaque ownership of one live per-CPU timer callback.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[must_use = "a persistent timer callback must be retained or cancelled"]
pub struct TimerCallbackToken {
    owner_cpu: usize,
    slot: usize,
    generation: u64,
}

/// Failure to register a timer callback.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TimerCallbackRegisterError {
    /// Allocating owned callback storage failed before publication.
    NoMemory,
    /// Every bounded callback slot on this CPU is occupied.
    CapacityExhausted,
    /// Every reusable free slot exhausted its generation space.
    TokenSpaceExhausted,
}

impl fmt::Display for TimerCallbackRegisterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => formatter.write_str("timer callback allocation failed"),
            Self::CapacityExhausted => {
                formatter.write_str("per-CPU timer callback registry is full")
            }
            Self::TokenSpaceExhausted => {
                formatter.write_str("timer callback token space is exhausted")
            }
        }
    }
}

impl core::error::Error for TimerCallbackRegisterError {}

struct CallbackSlot {
    generation: u64,
    callback: Option<Box<TimerCallback>>,
    in_flight: usize,
    closing: bool,
}

impl CallbackSlot {
    const fn new() -> Self {
        Self {
            generation: 0,
            callback: None,
            in_flight: 0,
            closing: false,
        }
    }
}

#[derive(Clone, Copy)]
struct CallbackLease {
    slot: usize,
    generation: u64,
    callback: *const TimerCallback,
}

struct CallbackInner {
    slots: [CallbackSlot; TIMER_CALLBACK_CAPACITY],
    next: usize,
    len: usize,
}

impl CallbackInner {
    const fn new() -> Self {
        Self {
            slots: [const { CallbackSlot::new() }; TIMER_CALLBACK_CAPACITY],
            next: 0,
            len: 0,
        }
    }

    fn register(
        &mut self,
        owner_cpu: usize,
        callback: Box<TimerCallback>,
    ) -> Result<TimerCallbackToken, (TimerCallbackRegisterError, Box<TimerCallback>)> {
        if self.len == TIMER_CALLBACK_CAPACITY {
            return Err((TimerCallbackRegisterError::CapacityExhausted, callback));
        }
        let Some(slot) = (self.next..TIMER_CALLBACK_CAPACITY)
            .chain(0..self.next)
            .find(|&slot| {
                let entry = &self.slots[slot];
                entry.callback.is_none() && entry.in_flight == 0 && entry.generation < u64::MAX
            })
        else {
            return Err((TimerCallbackRegisterError::TokenSpaceExhausted, callback));
        };

        let entry = &mut self.slots[slot];
        entry.generation += 1;
        entry.callback = Some(callback);
        entry.closing = false;
        self.len += 1;
        self.next = if slot + 1 == TIMER_CALLBACK_CAPACITY {
            0
        } else {
            slot + 1
        };

        Ok(TimerCallbackToken {
            owner_cpu,
            slot,
            generation: entry.generation,
        })
    }

    fn begin_dispatch(&mut self, leases: &mut [Option<CallbackLease>; TIMER_CALLBACK_CAPACITY]) {
        for (slot_index, (slot, lease)) in self.slots.iter_mut().zip(leases).enumerate() {
            if slot.closing {
                continue;
            }
            let Some(callback) = slot.callback.as_deref() else {
                continue;
            };
            let Some(in_flight) = slot.in_flight.checked_add(1) else {
                // A finite synchronous dispatch cannot realistically reach
                // this state. Refuse another lease instead of wrapping.
                continue;
            };
            slot.in_flight = in_flight;
            *lease = Some(CallbackLease {
                slot: slot_index,
                generation: slot.generation,
                callback,
            });
        }
    }

    fn finish_dispatch(&mut self, lease: CallbackLease) -> Option<Box<TimerCallback>> {
        let slot = self.slots.get_mut(lease.slot)?;
        if slot.generation != lease.generation || slot.in_flight == 0 {
            return None;
        }
        slot.in_flight -= 1;
        if slot.closing && slot.in_flight == 0 {
            self.next = lease.slot;
            return slot.callback.take();
        }
        None
    }

    fn cancel(&mut self, token: TimerCallbackToken) -> (bool, Option<Box<TimerCallback>>) {
        let Some(slot) = self.slots.get_mut(token.slot) else {
            return (false, None);
        };
        if slot.generation != token.generation || slot.callback.is_none() || slot.closing {
            return (false, None);
        }

        slot.closing = true;
        self.len -= 1;
        if slot.in_flight == 0 {
            self.next = token.slot;
            (true, slot.callback.take())
        } else {
            (true, None)
        }
    }
}

struct TimerCallbacks(SpinNoIrq<CallbackInner>);

impl TimerCallbacks {
    const fn new() -> Self {
        Self(SpinNoIrq::new(CallbackInner::new()))
    }

    fn register(
        &self,
        owner_cpu: usize,
        callback: Box<TimerCallback>,
    ) -> Result<TimerCallbackToken, TimerCallbackRegisterError> {
        let result = self.0.lock().register(owner_cpu, callback);
        match result {
            Ok(token) => Ok(token),
            Err((error, rejected)) => {
                drop(rejected);
                Err(error)
            }
        }
    }

    fn cancel(&self, token: TimerCallbackToken) -> bool {
        let (cancelled, deferred) = self.0.lock().cancel(token);
        drop(deferred);
        cancelled
    }

    fn dispatch(&self, now: TimeValue) {
        let mut leases = [None; TIMER_CALLBACK_CAPACITY];
        self.0.lock().begin_dispatch(&mut leases);

        for lease in leases.into_iter().flatten() {
            // SAFETY: begin_dispatch increments the exact slot generation's
            // in-flight count. Cancellation only marks it closing; the Box is
            // retained until finish_dispatch releases the final lease.
            unsafe { (*lease.callback)(now) };
            let deferred = self.0.lock().finish_dispatch(lease);
            drop(deferred);
        }
    }
}

percpu_static! {
    TIMER_CALLBACKS: TimerCallbacks = TimerCallbacks::new(),
}

fn callbacks_for(cpu_id: usize) -> &'static TimerCallbacks {
    // SAFETY: tokens contain a CPU ID observed from `this_cpu_id()` on an
    // initialized per-CPU area. TimerCallbacks serializes remote access.
    unsafe { TIMER_CALLBACKS.remote_ref_raw(cpu_id) }
}

/// Registers a bounded callback on the current CPU's timer tick.
///
/// Allocation completes before the callback registry lock is acquired. The
/// returned token may be cancelled from another CPU.
pub fn register_timer_callback<F>(
    callback: F,
) -> Result<TimerCallbackToken, TimerCallbackRegisterError>
where
    F: Fn(TimeValue) + Send + Sync + 'static,
{
    let callback: Box<TimerCallback> =
        Box::try_new(callback).map_err(|_| TimerCallbackRegisterError::NoMemory)?;
    let owner_cpu = axhal::percpu::this_cpu_id();
    callbacks_for(owner_cpu).register(owner_cpu, callback)
}

/// Cancels a live timer callback registration.
///
/// If the callback is currently running, destruction is deferred until that
/// invocation exits. Returning `true` guarantees no later invocation begins.
pub fn cancel_timer_callback(token: TimerCallbackToken) -> bool {
    callbacks_for(token.owner_cpu).cancel(token)
}

pub(crate) fn check_events() {
    let cpu_id = axhal::percpu::this_cpu_id();
    callbacks_for(cpu_id).dispatch(wall_time());
    crate::future::check_timer_events();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_tokens_are_generation_checked_and_bounded() {
        let mut inner = CallbackInner::new();
        let first = inner.register(0, Box::new(|_| {})).ok().unwrap();
        let (cancelled, deferred) = inner.cancel(first);
        assert!(cancelled);
        drop(deferred);

        let second = inner.register(0, Box::new(|_| {})).ok().unwrap();
        assert_ne!(first.generation, second.generation);
        assert!(!inner.cancel(first).0);

        for _ in 1..TIMER_CALLBACK_CAPACITY {
            let _token = inner.register(0, Box::new(|_| {})).ok().unwrap();
        }
        let error = inner.register(0, Box::new(|_| {})).unwrap_err().0;
        assert_eq!(error, TimerCallbackRegisterError::CapacityExhausted);
    }

    #[test]
    fn cancellation_defers_drop_until_dispatch_lease_finishes() {
        let mut inner = CallbackInner::new();
        let token = inner.register(0, Box::new(|_| {})).ok().unwrap();
        let mut leases = [None; TIMER_CALLBACK_CAPACITY];
        inner.begin_dispatch(&mut leases);
        let lease = leases[token.slot].take().unwrap();

        let (cancelled, deferred) = inner.cancel(token);
        assert!(cancelled);
        assert!(deferred.is_none());
        assert!(inner.finish_dispatch(lease).is_some());
    }
}
