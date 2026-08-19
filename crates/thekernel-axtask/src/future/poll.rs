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

use axpoll::{PollSet, PreparedRegistration, RegisterError, RegistrationToken, UpdateError};
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
    // Source bindings are never recycled after a successful waiter admission.
    // A new source is reserved while its first waiter is being armed so a
    // failed admission can return the slot to Empty without consuming one of
    // the process-lifetime source slots.
    sources: SpinNoIrq<[IrqSourceState; IRQ_SOURCE_CAPACITY]>,
    // Serializes source reservation/commit transactions without holding the
    // source lock while a custom waker is cloned or dropped.
    registration: SpinNoIrq<()>,
    waiters: [PollSet<IRQ_WAITER_CAPACITY>; IRQ_SOURCE_CAPACITY],
    hook_state: AtomicU8,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IrqSourceState {
    Empty,
    Reserved(usize),
    Bound(usize),
}

impl IrqWakerRegistry {
    const fn new() -> Self {
        Self {
            sources: SpinNoIrq::new([const { IrqSourceState::Empty }; IRQ_SOURCE_CAPACITY]),
            registration: SpinNoIrq::new(()),
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

    fn source_matches(&self, slot: usize, irq: usize) -> bool {
        self.sources
            .lock()
            .get(slot)
            .is_some_and(|source| *source == IrqSourceState::Bound(irq))
    }

    fn dispatch(&self, irq: usize) {
        let slot = self
            .sources
            .lock()
            .iter()
            .position(|source| {
                matches!(source, IrqSourceState::Reserved(source_irq) | IrqSourceState::Bound(source_irq) if *source_irq == irq)
            });
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

    // Clone the waker before taking the registry transaction lock. The
    // prepared value is independent of the destination PollSet and can be
    // moved into whichever source slot the transaction admits.
    let prepared = PreparedRegistration::new(waker);

    // Reserve a previously unused source before arming its first waiter, but
    // publish it as permanently bound only after arm succeeds. This keeps the
    // source-capacity accounting failure-atomic while the registration lock
    // prevents competing callers from observing or reusing the reservation.
    let result = {
        let _registration = IRQ_WAKERS.registration.lock();
        let source_slot = {
            let mut sources = IRQ_WAKERS.sources.lock();
            if let Some(slot) = sources.iter().position(|source| {
                matches!(source, IrqSourceState::Reserved(source_irq) | IrqSourceState::Bound(source_irq) if *source_irq == irq)
            }) {
                Some(slot)
            } else if let Some(slot) = sources
                .iter()
                .position(|source| *source == IrqSourceState::Empty)
            {
                sources[slot] = IrqSourceState::Reserved(irq);
                Some(slot)
            } else {
                None
            }
        };

        match source_slot {
            None => Err((IrqWakerRegisterError::SourceCapacityExhausted, prepared)),
            Some(source_slot) => match IRQ_WAKERS.waiters[source_slot].arm(prepared) {
                Ok(registration) => {
                    let mut sources = IRQ_WAKERS.sources.lock();
                    if sources.get(source_slot) == Some(&IrqSourceState::Reserved(irq)) {
                        sources[source_slot] = IrqSourceState::Bound(irq);
                    }
                    Ok(IrqWakerToken {
                        irq,
                        source_slot,
                        registration,
                    })
                }
                Err(error) => {
                    let mut sources = IRQ_WAKERS.sources.lock();
                    if sources.get(source_slot) == Some(&IrqSourceState::Reserved(irq)) {
                        sources[source_slot] = IrqSourceState::Empty;
                    }
                    let kind = error.kind();
                    let prepared = error.into_prepared();
                    Err((IrqWakerRegisterError::Waiter(kind), prepared))
                }
            },
        }
    };

    match result {
        Ok(token) => Ok(token),
        Err((error, prepared)) => {
            drop(prepared);
            Err(error)
        }
    }
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
    use alloc::{sync::Arc, task::Wake, vec::Vec};
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

    #[test]
    fn rejected_first_waiter_does_not_consume_source_capacity() {
        let _serial = SERIAL.lock().unwrap();
        let waker = Waker::noop();
        // Arrange a source which is still in the first-waiter reservation
        // phase while its PollSet is full. This is the only state in which a
        // failed arm could accidentally consume a process-lifetime source
        // slot; the public API normally keeps the reservation and arm under
        // one short transaction, so the unit test installs the state directly.
        let (slot, irq) = {
            let mut sources = IRQ_WAKERS.sources.lock();
            let slot = sources
                .iter()
                .position(|source| *source == IrqSourceState::Empty)
                .expect("test registry has no empty source slot");
            let irq = 0x7000_0000usize + slot;
            sources[slot] = IrqSourceState::Reserved(irq);
            (slot, irq)
        };
        let mut live = Vec::with_capacity(IRQ_WAITER_CAPACITY);
        for _ in 0..IRQ_WAITER_CAPACITY {
            live.push(IRQ_WAKERS.waiters[slot].register(waker).unwrap());
        }

        assert_eq!(
            register_irq_waker(irq, waker),
            Err(IrqWakerRegisterError::Waiter(RegisterError::Full))
        );
        assert!(matches!(
            IRQ_WAKERS.sources.lock()[slot],
            IrqSourceState::Empty
        ));

        for token in live {
            assert!(IRQ_WAKERS.waiters[slot].cancel(token));
        }

        // The failed first admission returned its source slot to Empty, so
        // the same source can be admitted after waiter pressure is released.
        let retry = register_irq_waker(irq, waker).unwrap();
        assert!(cancel_irq_waker(retry));
    }
}
