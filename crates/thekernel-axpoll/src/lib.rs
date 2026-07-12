//! A library for polling I/O events and waking up tasks.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::{sync::Arc, task::Wake};
use core::task::{Context, Waker};

use bitflags::bitflags;
use kspin::SpinNoIrq as Mutex;
use linux_raw_sys::general::*;

bitflags! {
    /// I/O events.
    #[derive(Debug, Clone, Copy)]
    pub struct IoEvents: u32 {
        /// Available for read
        const IN     = POLLIN;
        /// Urgent data for read
        const PRI    = POLLPRI;
        /// Available for write
        const OUT    = POLLOUT;

        /// Error condition
        const ERR    = POLLERR;
        /// Hang up
        const HUP    = POLLHUP;
        /// Invalid request
        const NVAL   = POLLNVAL;

        /// Equivalent to [`IN`](Self::IN)
        const RDNORM = POLLRDNORM;
        /// Priority band data can be read
        const RDBAND = POLLRDBAND;
        /// Equivalent to [`OUT`](Self::OUT)
        const WRNORM = POLLWRNORM;
        /// Priority data can be written
        const WRBAND = POLLWRBAND;

        /// Message
        const MSG    = POLLMSG;
        /// Remove
        const REMOVE = POLLREMOVE;
        /// Stream socket peer closed connection, or shut down writing half of connection.
        const RDHUP  = POLLRDHUP;

        /// Events that are always polled even without specifying them.
        const ALWAYS_POLL = Self::ERR.bits() | Self::HUP.bits();
    }
}

/// Trait for types that can be polled for I/O events.
pub trait Pollable {
    /// Polls for I/O events.
    fn poll(&self) -> IoEvents;

    /// Registers wakers for I/O events.
    fn register(&self, context: &mut Context<'_>, events: IoEvents);
}

const POLL_SET_CAPACITY: usize = 64;

struct Inner {
    entries: [Option<Waker>; POLL_SET_CAPACITY],
    next: usize,
    len: usize,
}

impl Inner {
    const fn new() -> Self {
        Self {
            entries: [const { None }; POLL_SET_CAPACITY],
            next: 0,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn register(&mut self, waker: &Waker, owned: Waker) -> (Option<Waker>, Option<Waker>) {
        if self
            .entries
            .iter()
            .flatten()
            .any(|registered| registered.will_wake(waker))
        {
            return (None, Some(owned));
        }

        let slot = if self.len < POLL_SET_CAPACITY {
            (0..POLL_SET_CAPACITY)
                .map(|offset| (self.next + offset) % POLL_SET_CAPACITY)
                .find(|&slot| self.entries[slot].is_none())
                .unwrap_or(self.next)
        } else {
            self.next
        };
        let replaced = self.entries[slot].replace(owned);
        if replaced.is_none() {
            self.len += 1;
        }
        self.next = (slot + 1) % POLL_SET_CAPACITY;
        (replaced, None)
    }

    fn drain(&mut self, pending: &mut [Option<Waker>; POLL_SET_CAPACITY]) -> usize {
        let len = self.len;
        for (dst, src) in pending.iter_mut().zip(&mut self.entries) {
            *dst = src.take();
        }
        self.next = 0;
        self.len = 0;
        len
    }
}

/// A data structure for waking up tasks that are waiting for I/O events.
pub struct PollSet(Mutex<Inner>);

impl Default for PollSet {
    fn default() -> Self {
        Self::new()
    }
}

impl PollSet {
    /// Creates a new empty [`PollSet`].
    pub const fn new() -> Self {
        Self(Mutex::new(Inner::new()))
    }

    /// Registers a waker.
    pub fn register(&self, waker: &Waker) {
        // A RawWaker clone/drop may run type-specific reference-counting or
        // teardown code. Keep both outside the IRQ-safe registry lock.
        let owned = waker.clone();
        let (replaced, duplicate) = self.0.lock().register(waker, owned);
        drop(duplicate);
        if let Some(replaced) = replaced {
            replaced.wake();
        }
    }

    /// Wakes up all registered wakers.
    pub fn wake(&self) -> usize {
        let mut pending = [const { None }; POLL_SET_CAPACITY];
        let mut guard = self.0.lock();
        if guard.is_empty() {
            return 0;
        }
        let len = guard.drain(&mut pending);
        drop(guard);
        for waker in pending.into_iter().flatten() {
            waker.wake();
        }
        len
    }
}

impl Drop for PollSet {
    fn drop(&mut self) {
        // Ensure all entries are dropped
        self.wake();
    }
}

impl Wake for PollSet {
    fn wake(self: Arc<Self>) {
        self.as_ref().wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.as_ref().wake();
    }
}
