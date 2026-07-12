//! Bounded I/O readiness registration and wakeup primitives.
//!
//! This crate provides generic mechanism rather than Linux ABI policy. Event
//! flags have crate-owned values; a Linux personality must translate `POLL*`
//! values at its ABI boundary.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::{sync::Arc, task::Wake};
use core::{
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use bitflags::bitflags;
use kspin::SpinNoIrq as Mutex;

bitflags! {
    /// Generic I/O readiness events.
    ///
    /// The numeric values are owned by this crate and are not Linux `POLL*`
    /// constants. ABI-facing callers must translate in both directions.
    #[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash)]
    pub struct IoEvents: u32 {
        /// Data can be read without blocking.
        const READABLE     = 1 << 0;
        /// High-priority data can be read without blocking.
        const PRIORITY     = 1 << 1;
        /// Data can be written without blocking.
        const WRITABLE     = 1 << 2;
        /// An asynchronous error is pending.
        const ERROR        = 1 << 3;
        /// The peer or underlying object has hung up.
        const HANGUP       = 1 << 4;
        /// The requested object or operation is invalid.
        const INVALID      = 1 << 5;
        /// Normal-priority data can be read.
        const READ_NORMAL  = 1 << 6;
        /// Priority-band data can be read.
        const READ_BAND    = 1 << 7;
        /// Normal-priority data can be written.
        const WRITE_NORMAL = 1 << 8;
        /// Priority-band data can be written.
        const WRITE_BAND   = 1 << 9;
        /// A message is available.
        const MESSAGE      = 1 << 10;
        /// The monitored object was removed.
        const REMOVED      = 1 << 11;
        /// The peer closed, or shut down, its writing half.
        const READ_HANGUP  = 1 << 12;

        /// Conditions that should be observed even when not requested.
        const ALWAYS = Self::ERROR.bits() | Self::HANGUP.bits();
    }
}

/// The default number of simultaneous registrations in a [`PollSet`].
pub const DEFAULT_CAPACITY: usize = 64;

/// An opaque handle to one live [`PollSet`] registration.
///
/// Tokens are bound to a registry, slot, and generation. Consequently, a token
/// from another registry or from an earlier use of the same slot cannot cancel
/// or update the current registration.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct RegistrationToken {
    registry_id: usize,
    slot: usize,
    generation: usize,
}

impl RegistrationToken {
    const fn new(registry_id: usize, slot: usize, generation: usize) -> Self {
        Self {
            registry_id,
            slot,
            generation,
        }
    }
}

/// Failure returned by [`PollSet::register`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RegisterError {
    /// Every bounded registration slot is occupied.
    Full,
    /// The registry was closed and accepts no new registrations.
    Closed,
    /// The registry or slot generation identifier space was exhausted.
    TokenSpaceExhausted,
}

impl fmt::Display for RegisterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("poll registration set is full"),
            Self::Closed => formatter.write_str("poll registration set is closed"),
            Self::TokenSpaceExhausted => {
                formatter.write_str("poll registration token space is exhausted")
            }
        }
    }
}

/// Failure returned by [`PollSet::update`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UpdateError {
    /// The registry was closed and no registration can be updated.
    Closed,
    /// The token belongs to another registry or no longer names a live slot.
    InvalidToken,
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("poll registration set is closed"),
            Self::InvalidToken => formatter.write_str("poll registration token is invalid"),
        }
    }
}

static NEXT_REGISTRY_ID: AtomicUsize = AtomicUsize::new(1);

fn allocate_registry_id() -> Option<usize> {
    NEXT_REGISTRY_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()
}

struct Slot {
    generation: usize,
    waker: Option<Waker>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            generation: 0,
            waker: None,
        }
    }
}

struct Inner<const CAPACITY: usize> {
    entries: [Slot; CAPACITY],
    registry_id: usize,
    next: usize,
    len: usize,
    closed: bool,
}

impl<const CAPACITY: usize> Inner<CAPACITY> {
    const fn new() -> Self {
        Self {
            entries: [const { Slot::new() }; CAPACITY],
            registry_id: 0,
            next: 0,
            len: 0,
            closed: false,
        }
    }

    fn register(
        &mut self,
        owned: Waker,
    ) -> (Result<RegistrationToken, RegisterError>, Option<Waker>) {
        if self.closed {
            return (Err(RegisterError::Closed), Some(owned));
        }

        if self.len == CAPACITY {
            return (Err(RegisterError::Full), Some(owned));
        }

        let Some(slot) = (0..CAPACITY)
            .map(|offset| self.next.wrapping_add(offset) % CAPACITY)
            .find(|&slot| {
                self.entries[slot].waker.is_none() && self.entries[slot].generation < usize::MAX
            })
        else {
            return (Err(RegisterError::TokenSpaceExhausted), Some(owned));
        };

        let registry_id = if self.registry_id == 0 {
            let Some(registry_id) = allocate_registry_id() else {
                return (Err(RegisterError::TokenSpaceExhausted), Some(owned));
            };
            self.registry_id = registry_id;
            registry_id
        } else {
            self.registry_id
        };

        let entry = &mut self.entries[slot];
        entry.generation += 1;
        entry.waker = Some(owned);
        self.len += 1;
        self.next = (slot + 1) % CAPACITY;

        (
            Ok(RegistrationToken::new(registry_id, slot, entry.generation)),
            None,
        )
    }

    fn update(
        &mut self,
        token: RegistrationToken,
        candidate: &Waker,
        owned: Waker,
    ) -> (Result<(), UpdateError>, Option<Waker>) {
        if self.closed {
            return (Err(UpdateError::Closed), Some(owned));
        }
        if token.registry_id == 0 || token.registry_id != self.registry_id {
            return (Err(UpdateError::InvalidToken), Some(owned));
        }

        let Some(entry) = self.entries.get_mut(token.slot) else {
            return (Err(UpdateError::InvalidToken), Some(owned));
        };
        if entry.generation != token.generation || entry.waker.is_none() {
            return (Err(UpdateError::InvalidToken), Some(owned));
        }

        if entry
            .waker
            .as_ref()
            .is_some_and(|registered| registered.will_wake(candidate))
        {
            return (Ok(()), Some(owned));
        }

        let replaced = entry.waker.replace(owned);
        (Ok(()), replaced)
    }

    fn cancel(&mut self, token: RegistrationToken) -> Option<Waker> {
        if token.registry_id == 0 || token.registry_id != self.registry_id {
            return None;
        }

        let entry = self.entries.get_mut(token.slot)?;
        if entry.generation != token.generation {
            return None;
        }

        let removed = entry.waker.take();
        if removed.is_some() {
            self.len -= 1;
            self.next = token.slot;
        }
        removed
    }

    fn drain(&mut self, pending: &mut [Option<Waker>; CAPACITY]) -> usize {
        let len = self.len;
        for (destination, entry) in pending.iter_mut().zip(&mut self.entries) {
            *destination = entry.waker.take();
        }
        self.next = 0;
        self.len = 0;
        len
    }
}

/// A bounded registry for tasks waiting on I/O readiness.
///
/// `CAPACITY` is a hard upper bound: registration never allocates a growing
/// collection and never silently overwrites an existing waiter. `wake()` drains
/// the current registrations but leaves the set open. `close()` drains and
/// wakes them while permanently rejecting future registration.
///
/// Registration, update, cancellation, wake, and close races are linearized by
/// a short IRQ-safe lock. Waker clone, destruction, and wake callbacks occur
/// outside that lock so custom RawWaker implementations may safely re-enter the
/// registry.
pub struct PollSet<const CAPACITY: usize = DEFAULT_CAPACITY>(Mutex<Inner<CAPACITY>>);

impl<const CAPACITY: usize> Default for PollSet<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> PollSet<CAPACITY> {
    /// Creates an empty, open registry.
    pub const fn new() -> Self {
        Self(Mutex::new(Inner::new()))
    }

    /// Returns the compile-time registration capacity.
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    /// Returns the number of live registrations.
    pub fn len(&self) -> usize {
        self.0.lock().len
    }

    /// Returns `true` when there are no live registrations.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` after the registry has been closed.
    pub fn is_closed(&self) -> bool {
        self.0.lock().closed
    }

    /// Registers a waker and returns its opaque cancellation token.
    ///
    /// Every call creates an independent registration, even if another slot has
    /// an equivalent waker. A full registry returns [`RegisterError::Full`]
    /// without replacing or waking another waiter. A logical waiter that is
    /// polled again must retain its token and call [`Self::update`] instead of
    /// registering a second time.
    pub fn register(&self, waker: &Waker) -> Result<RegistrationToken, RegisterError> {
        // A RawWaker clone may execute type-specific reference counting. Clone
        // before acquiring the IRQ-safe registry lock.
        let owned = waker.clone();
        let (result, deferred_drop) = self.0.lock().register(owned);
        drop(deferred_drop);
        result
    }

    /// Replaces the waker associated with a live token.
    ///
    /// The token and its generation remain unchanged. The replaced or rejected
    /// waker is destroyed after the registry lock is released.
    pub fn update(&self, token: RegistrationToken, waker: &Waker) -> Result<(), UpdateError> {
        let owned = waker.clone();
        let (result, deferred_drop) = self.0.lock().update(token, waker, owned);
        drop(deferred_drop);
        result
    }

    /// Cancels a live registration.
    ///
    /// Returns `false` for stale tokens, foreign-registry tokens, and tokens
    /// already consumed by another cancel, wake, or close operation.
    pub fn cancel(&self, token: RegistrationToken) -> bool {
        let removed = self.0.lock().cancel(token);
        let cancelled = removed.is_some();
        drop(removed);
        cancelled
    }

    /// Drains and wakes all current registrations while leaving the set open.
    ///
    /// Returns the number of registrations consumed by this operation. Every
    /// callback runs after the registry lock has been released.
    pub fn wake(&self) -> usize {
        let mut pending = [const { None }; CAPACITY];
        let len = self.0.lock().drain(&mut pending);
        for waker in pending.into_iter().flatten() {
            waker.wake();
        }
        len
    }

    /// Permanently closes the registry, drains it, and wakes its waiters.
    ///
    /// The state transition and drain are atomic with respect to registration,
    /// update, cancellation, and wake. Repeated calls are harmless and return
    /// zero after the first close.
    pub fn close(&self) -> usize {
        let mut pending = [const { None }; CAPACITY];
        let len = {
            let mut inner = self.0.lock();
            if inner.closed {
                return 0;
            }
            inner.closed = true;
            inner.drain(&mut pending)
        };
        for waker in pending.into_iter().flatten() {
            waker.wake();
        }
        len
    }
}

impl<const CAPACITY: usize> Drop for PollSet<CAPACITY> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<const CAPACITY: usize> Wake for PollSet<CAPACITY> {
    fn wake(self: Arc<Self>) {
        self.as_ref().wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.as_ref().wake();
    }
}
