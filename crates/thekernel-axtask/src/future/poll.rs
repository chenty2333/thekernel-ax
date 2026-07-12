//! Bounded interrupt-to-waker registration.
//!
//! This module only connects a hardware interrupt source to generic wakers.
//! Object readiness checks, Linux `POLL*` values, retry policy, and aggregate
//! waits belong to the consumer's readiness layer. IRQ domain validation,
//! enable/disable, masking, and acknowledgement remain owned by the driver or
//! IRQ capability provider; registering a waiter has no hardware side effect.

use core::{
    fmt,
    sync::atomic::{AtomicU8, Ordering},
    task::Waker,
};

use axpoll::{PollSet, RegisterError, RegistrationToken, UpdateError};
use kspin::SpinNoIrq;

/// Maximum number of distinct IRQ sources admitted for the process lifetime.
pub const IRQ_SOURCE_CAPACITY: usize = 64;

/// Maximum number of simultaneous waiters admitted for one IRQ source.
pub const IRQ_WAITER_CAPACITY: usize = 64;

const HOOK_UNINITIALIZED: u8 = 0;
const HOOK_INSTALLING: u8 = 1;
const HOOK_READY: u8 = 2;
const HOOK_UNAVAILABLE: u8 = 3;

/// Opaque ownership of one live IRQ waker registration.
///
/// A successful registration must be updated or cancelled by its owner. An
/// interrupt consumes the registration, so cancellation then returns `false`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[must_use = "a live IRQ registration must be updated or cancelled"]
pub struct IrqWakerToken {
    irq: usize,
    source_slot: usize,
    registration: RegistrationToken,
}

/// Failure to register a waker for an interrupt source.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IrqWakerRegisterError {
    /// Another caller is installing the one global axhal IRQ hook.
    HookInstallationInProgress,
    /// Another subsystem already owns the one global axhal IRQ hook.
    HookUnavailable,
    /// Every bounded distinct-source slot has been consumed.
    SourceCapacityExhausted,
    /// The selected source rejected this waiter.
    Waiter(RegisterError),
}

impl fmt::Display for IrqWakerRegisterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HookInstallationInProgress => {
                formatter.write_str("IRQ hook installation is in progress")
            }
            Self::HookUnavailable => {
                formatter.write_str("the global IRQ hook is owned by another subsystem")
            }
            Self::SourceCapacityExhausted => formatter.write_str("IRQ source registry is full"),
            Self::Waiter(error) => error.fmt(formatter),
        }
    }
}

impl core::error::Error for IrqWakerRegisterError {}

/// Failure to update a live IRQ waker registration.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IrqWakerUpdateError {
    /// The token does not identify its original, permanently bound source.
    InvalidSource,
    /// The source rejected the token or has been closed.
    Registration(UpdateError),
}

impl fmt::Display for IrqWakerUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSource => formatter.write_str("IRQ source token is invalid"),
            Self::Registration(error) => error.fmt(formatter),
        }
    }
}

impl core::error::Error for IrqWakerUpdateError {}

struct IrqWakerRegistry {
    // Source bindings are never recycled. That makes a source slot stable for
    // interrupt dispatch without manufacturing another generation domain.
    sources: SpinNoIrq<[Option<usize>; IRQ_SOURCE_CAPACITY]>,
    waiters: [PollSet<IRQ_WAITER_CAPACITY>; IRQ_SOURCE_CAPACITY],
    hook_state: AtomicU8,
}

impl IrqWakerRegistry {
    const fn new() -> Self {
        Self {
            sources: SpinNoIrq::new([None; IRQ_SOURCE_CAPACITY]),
            waiters: [const { PollSet::new() }; IRQ_SOURCE_CAPACITY],
            hook_state: AtomicU8::new(HOOK_UNINITIALIZED),
        }
    }

    fn ensure_hook(&self) -> Result<(), IrqWakerRegisterError> {
        match self.hook_state.load(Ordering::Acquire) {
            HOOK_READY => return Ok(()),
            HOOK_INSTALLING => {
                return Err(IrqWakerRegisterError::HookInstallationInProgress);
            }
            HOOK_UNAVAILABLE => return Err(IrqWakerRegisterError::HookUnavailable),
            HOOK_UNINITIALIZED => {}
            _ => return Err(IrqWakerRegisterError::HookUnavailable),
        }

        if self
            .hook_state
            .compare_exchange(
                HOOK_UNINITIALIZED,
                HOOK_INSTALLING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return match self.hook_state.load(Ordering::Acquire) {
                HOOK_READY => Ok(()),
                HOOK_INSTALLING => Err(IrqWakerRegisterError::HookInstallationInProgress),
                _ => Err(IrqWakerRegisterError::HookUnavailable),
            };
        }

        let installed = axhal::irq::register_irq_hook(irq_hook);
        self.hook_state.store(
            if installed {
                HOOK_READY
            } else {
                HOOK_UNAVAILABLE
            },
            Ordering::Release,
        );
        if installed {
            Ok(())
        } else {
            Err(IrqWakerRegisterError::HookUnavailable)
        }
    }

    fn source_slot(&self, irq: usize) -> Result<usize, IrqWakerRegisterError> {
        let mut sources = self.sources.lock();
        if let Some(slot) = sources.iter().position(|source| *source == Some(irq)) {
            return Ok(slot);
        }
        let Some(slot) = sources.iter().position(Option::is_none) else {
            return Err(IrqWakerRegisterError::SourceCapacityExhausted);
        };
        sources[slot] = Some(irq);
        Ok(slot)
    }

    fn source_matches(&self, slot: usize, irq: usize) -> bool {
        self.sources
            .lock()
            .get(slot)
            .is_some_and(|source| *source == Some(irq))
    }

    fn dispatch(&self, irq: usize) {
        let slot = self
            .sources
            .lock()
            .iter()
            .position(|source| *source == Some(irq));
        if let Some(slot) = slot {
            // PollSet drains under its own short IRQ-safe lock and invokes all
            // wakers after that lock is released.
            self.waiters[slot].wake();
        }
    }
}

static IRQ_WAKERS: IrqWakerRegistry = IrqWakerRegistry::new();

fn irq_hook(irq: usize) {
    IRQ_WAKERS.dispatch(irq);
}

/// Registers one waker for the given IRQ and returns cancellable ownership.
///
/// Consumers must use a check-then-register-then-check sequence. Re-polling a
/// pending operation must update the retained token rather than allocate a new
/// waiter slot. The IRQ capability owner must validate and enable the source
/// before it can generate events; this registry deliberately does neither.
pub fn register_irq_waker(
    irq: usize,
    waker: &Waker,
) -> Result<IrqWakerToken, IrqWakerRegisterError> {
    IRQ_WAKERS.ensure_hook()?;
    let source_slot = IRQ_WAKERS.source_slot(irq)?;
    let registration = IRQ_WAKERS.waiters[source_slot]
        .register(waker)
        .map_err(IrqWakerRegisterError::Waiter)?;
    Ok(IrqWakerToken {
        irq,
        source_slot,
        registration,
    })
}

/// Updates the waker owned by a live IRQ registration token.
pub fn update_irq_waker(token: IrqWakerToken, waker: &Waker) -> Result<(), IrqWakerUpdateError> {
    if !IRQ_WAKERS.source_matches(token.source_slot, token.irq) {
        return Err(IrqWakerUpdateError::InvalidSource);
    }
    IRQ_WAKERS.waiters[token.source_slot]
        .update(token.registration, waker)
        .map_err(IrqWakerUpdateError::Registration)
}

/// Cancels a live IRQ registration.
///
/// Returns `false` for a stale token or for a registration already consumed by
/// an interrupt. Any owned waker is destroyed after the source lock is released.
pub fn cancel_irq_waker(token: IrqWakerToken) -> bool {
    IRQ_WAKERS.source_matches(token.source_slot, token.irq)
        && IRQ_WAKERS.waiters[token.source_slot].cancel(token.registration)
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    static SERIAL: Mutex<()> = Mutex::new(());

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    #[test]
    fn irq_registration_is_cancellable_and_consumed_by_dispatch() {
        let _serial = SERIAL.lock().unwrap();
        let cancelled_counter = Arc::new(Counter(AtomicUsize::new(0)));
        let cancelled_waker = Waker::from(cancelled_counter.clone());
        let cancelled = register_irq_waker(7, &cancelled_waker).unwrap();
        assert!(cancel_irq_waker(cancelled));
        assert!(!cancel_irq_waker(cancelled));

        let live_counter = Arc::new(Counter(AtomicUsize::new(0)));
        let live_waker = Waker::from(live_counter.clone());
        let live = register_irq_waker(7, &live_waker).unwrap();
        irq_hook(7);

        assert_eq!(cancelled_counter.0.load(Ordering::Acquire), 0);
        assert_eq!(live_counter.0.load(Ordering::Acquire), 1);
        assert!(!cancel_irq_waker(live));
    }

    #[test]
    fn irq_registration_updates_one_owned_token() {
        let _serial = SERIAL.lock().unwrap();
        let first = Arc::new(Counter(AtomicUsize::new(0)));
        let second = Arc::new(Counter(AtomicUsize::new(0)));
        let token = register_irq_waker(8, &Waker::from(first.clone())).unwrap();
        update_irq_waker(token, &Waker::from(second.clone())).unwrap();

        irq_hook(8);
        assert_eq!(first.0.load(Ordering::Acquire), 0);
        assert_eq!(second.0.load(Ordering::Acquire), 1);
        assert!(!cancel_irq_waker(token));
    }

    #[test]
    fn opaque_irq_identifier_is_not_domain_validated_or_enabled() {
        let _serial = SERIAL.lock().unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let token = register_irq_waker(usize::MAX, &Waker::from(counter.clone())).unwrap();

        irq_hook(usize::MAX);
        assert_eq!(counter.0.load(Ordering::Acquire), 1);
        assert!(!cancel_irq_waker(token));
    }
}
