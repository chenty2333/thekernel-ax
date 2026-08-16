#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate std;

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn try_update_usize<F>(
    atomic: &AtomicUsize,
    set: Ordering,
    fail: Ordering,
    mut update: F,
) -> Result<usize, usize>
where
    F: FnMut(usize) -> Option<usize>,
{
    let mut current = atomic.load(fail);
    loop {
        let Some(next) = update(current) else {
            return Err(current);
        };
        match atomic.compare_exchange_weak(current, next, set, fail) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

/// A typed performance event understood by the generic session layer.
///
/// The TLB variants are deliberately operation-specific. Calling a read-only
/// mapping "all D-TLB misses" would overstate what the hardware reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// Variable-frequency CPU cycles.
    CpuCycles,
    /// Completed instructions.
    Instructions,
    /// Data-TLB misses caused by reads.
    DataTlbReadMisses,
    /// Data-TLB misses caused by writes.
    DataTlbWriteMisses,
    /// Instruction-TLB misses caused by instruction reads.
    InstructionTlbReadMisses,
}

impl Event {
    const fn bit(self) -> u8 {
        match self {
            Self::CpuCycles => 1 << 0,
            Self::Instructions => 1 << 1,
            Self::DataTlbReadMisses => 1 << 2,
            Self::DataTlbWriteMisses => 1 << 3,
            Self::InstructionTlbReadMisses => 1 << 4,
        }
    }
}

/// Fixed event-set bitmap used by capability discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMask {
    bits: u8,
}

impl EventMask {
    /// Empty event set.
    pub const NONE: Self = Self { bits: 0 };
    /// Every typed event known by this crate.
    pub const ALL: Self = Self { bits: (1 << 5) - 1 };

    /// Creates a set containing one event.
    pub const fn from_event(event: Event) -> Self {
        Self { bits: event.bit() }
    }

    /// Returns whether the set contains `event`.
    pub const fn contains(self, event: Event) -> bool {
        self.bits & event.bit() != 0
    }

    /// Returns the union of two event sets.
    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }
}

/// Origin of performance counters exposed by one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CounterSource {
    /// A platform-specific backend supplied by a consumer.
    Platform,
}

/// Bounded capability snapshot for one PMU backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    source: CounterSource,
    counter_count: usize,
    requestable_events: EventMask,
    consistent_snapshot: bool,
}

impl Capabilities {
    /// Constructs a capability snapshot.
    ///
    /// `requestable_events` is a negotiation surface, not a guarantee that
    /// every request will find a physical counter.
    pub const fn new(
        source: CounterSource,
        counter_count: usize,
        requestable_events: EventMask,
        consistent_snapshot: bool,
    ) -> Self {
        Self {
            source,
            counter_count,
            requestable_events,
            consistent_snapshot,
        }
    }

    /// Constructs an explicitly unavailable backend capability.
    pub const fn unsupported(source: CounterSource) -> Self {
        Self::new(source, 0, EventMask::NONE, false)
    }

    /// Returns the counter source.
    pub const fn source(self) -> CounterSource {
        self.source
    }

    /// Returns the bounded number of counters available to this backend.
    pub const fn counter_count(self) -> usize {
        self.counter_count
    }

    /// Returns events that callers may negotiate with this backend.
    pub const fn requestable_events(self) -> EventMask {
        self.requestable_events
    }

    /// Returns whether the backend produces one coherent multi-counter sample.
    pub const fn has_consistent_snapshot(self) -> bool {
        self.consistent_snapshot
    }

    /// Returns whether at least one counter is available.
    pub const fn is_available(self) -> bool {
        self.counter_count != 0
    }
}

/// Failure from a PMU backend or bounded session lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The architecture or requested event is not supported.
    Unsupported,
    /// No counter remains for this request.
    NoCounter,
    /// The caller requested an empty session.
    EmptySession,
    /// The requested event count exceeds the session's fixed capacity.
    CapacityExceeded,
    /// One event appeared more than once in the same session.
    DuplicateEvent,
    /// The session is already running.
    AlreadyRunning,
    /// The requested operation requires a running session.
    NotRunning,
    /// A backend rejected an invalid counter, flag, or other parameter.
    InvalidRequest,
    /// A counter exists but this adapter cannot read its value safely.
    ValueUnavailable,
    /// A backend returned an otherwise unmapped error code.
    BackendFailure(isize),
}

/// Platform mechanism used by a bounded PMU session.
///
/// Implementations must make `stop` and `release` idempotent. `release` must
/// make a counter inactive before relinquishing it, including when cleanup is
/// retried after a partial failure.
pub trait Backend {
    /// Opaque, copyable handle for one configured counter.
    type Counter: Copy;

    /// Returns a bounded capability snapshot without starting counters.
    fn capabilities(&self) -> Capabilities;

    /// Reserves and configures one stopped counter for `event`.
    fn configure(&mut self, event: Event) -> Result<Self::Counter, Error>;

    /// Starts one configured counter from a zero baseline.
    fn start(&mut self, counter: Self::Counter) -> Result<(), Error>;

    /// Reads one configured counter.
    fn read(&mut self, counter: Self::Counter) -> Result<u64, Error>;

    /// Stops one configured counter without releasing its event mapping.
    fn stop(&mut self, counter: Self::Counter) -> Result<(), Error>;

    /// Stops and releases one configured counter.
    fn release(&mut self, counter: Self::Counter) -> Result<(), Error>;
}

/// One fixed-capacity PMU snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot<const N: usize> {
    events: [Event; N],
    values: [u64; N],
    len: usize,
    consistent: bool,
}

impl<const N: usize> Snapshot<N> {
    /// Returns the number of populated event samples.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the snapshot has no samples.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the sampled events in request order.
    pub fn events(&self) -> &[Event] {
        &self.events[..self.len]
    }

    /// Returns counter values in the same order as [`Self::events`].
    pub fn values(&self) -> &[u64] {
        &self.values[..self.len]
    }

    /// Returns whether the backend sampled all values coherently.
    pub const fn is_consistent(&self) -> bool {
        self.consistent
    }
}

/// Fixed-capacity, allocation-free PMU session.
///
/// Opening a session reserves stopped counters. Counting begins only after
/// [`Self::start`]. Call [`Self::close`] when cleanup errors must be observed;
/// `Drop` performs a final best-effort release.
pub struct Session<'backend, B: Backend, const N: usize> {
    backend: &'backend mut B,
    counters: [Option<B::Counter>; N],
    events: [Event; N],
    len: usize,
    running: bool,
    closed: bool,
}

impl<'backend, B: Backend, const N: usize> Session<'backend, B, N> {
    /// Reserves stopped counters for the requested distinct events.
    pub fn open(backend: &'backend mut B, events: &[Event]) -> Result<Self, Error> {
        if events.is_empty() {
            return Err(Error::EmptySession);
        }
        if events.len() > N {
            return Err(Error::CapacityExceeded);
        }

        let capabilities = backend.capabilities();
        if !capabilities.is_available() {
            return Err(Error::Unsupported);
        }

        let mut counters = [None; N];
        let mut stored_events = [Event::CpuCycles; N];
        let mut configured = 0;
        for (slot, event) in events.iter().copied().enumerate() {
            if stored_events[..slot].contains(&event) {
                let cleanup = Self::release_prefix(backend, &mut counters, configured);
                return Err(cleanup.unwrap_or(Error::DuplicateEvent));
            }
            if !capabilities.requestable_events().contains(event) {
                let cleanup = Self::release_prefix(backend, &mut counters, configured);
                return Err(cleanup.unwrap_or(Error::Unsupported));
            }

            match backend.configure(event) {
                Ok(counter) => {
                    counters[slot] = Some(counter);
                    stored_events[slot] = event;
                    configured += 1;
                }
                Err(error) => {
                    let cleanup = Self::release_prefix(backend, &mut counters, configured);
                    return Err(cleanup.unwrap_or(error));
                }
            }
        }

        Ok(Self {
            backend,
            counters,
            events: stored_events,
            len: events.len(),
            running: false,
            closed: false,
        })
    }

    /// Returns the backend capability snapshot.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }

    /// Returns the configured events in request order.
    pub fn events(&self) -> &[Event] {
        &self.events[..self.len]
    }

    /// Returns whether the session is actively counting.
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// Starts every configured counter from zero.
    pub fn start(&mut self) -> Result<(), Error> {
        if self.running {
            return Err(Error::AlreadyRunning);
        }

        for (started, slot) in (0..self.len).enumerate() {
            let counter = self.counters[slot].expect("configured session slot");
            if let Err(error) = self.backend.start(counter) {
                let mut cleanup = None;
                for rollback in (0..started).rev() {
                    let counter = self.counters[rollback].expect("configured session slot");
                    if let Err(stop_error) = self.backend.stop(counter) {
                        cleanup.get_or_insert(stop_error);
                    }
                }
                self.running = cleanup.is_some();
                return Err(cleanup.unwrap_or(error));
            }
        }
        self.running = true;
        Ok(())
    }

    /// Samples every counter into a fixed array.
    pub fn snapshot(&mut self) -> Result<Snapshot<N>, Error> {
        if !self.running {
            return Err(Error::NotRunning);
        }

        let mut values = [0; N];
        for (slot, value) in values[..self.len].iter_mut().enumerate() {
            let counter = self.counters[slot].expect("configured session slot");
            *value = self.backend.read(counter)?;
        }
        Ok(Snapshot {
            events: self.events,
            values,
            len: self.len,
            consistent: self.backend.capabilities().has_consistent_snapshot(),
        })
    }

    /// Stops every configured counter while retaining the session.
    pub fn stop(&mut self) -> Result<(), Error> {
        if !self.running {
            return Err(Error::NotRunning);
        }

        let mut first_error = None;
        for slot in (0..self.len).rev() {
            let counter = self.counters[slot].expect("configured session slot");
            if let Err(error) = self.backend.stop(counter) {
                first_error.get_or_insert(error);
            }
        }
        self.running = first_error.is_some();
        first_error.map_or(Ok(()), Err)
    }

    /// Stops and releases every counter, reporting the first cleanup failure.
    pub fn close(mut self) -> Result<(), Error> {
        let result = self.cleanup();
        if result.is_ok() {
            self.closed = true;
        }
        result
    }

    fn cleanup(&mut self) -> Result<(), Error> {
        let mut first_error = None;
        for slot in (0..self.len).rev() {
            if let Some(counter) = self.counters[slot] {
                match self.backend.release(counter) {
                    Ok(()) => self.counters[slot] = None,
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        if first_error.is_none() {
            self.running = false;
        }
        first_error.map_or(Ok(()), Err)
    }

    fn release_prefix(
        backend: &mut B,
        counters: &mut [Option<B::Counter>; N],
        configured: usize,
    ) -> Option<Error> {
        let mut first_error = None;
        for slot in (0..configured).rev() {
            if let Some(counter) = counters[slot] {
                match backend.release(counter) {
                    Ok(()) => counters[slot] = None,
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        first_error
    }
}

impl<B: Backend, const N: usize> Drop for Session<'_, B, N> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.cleanup();
        }
    }
}

/// Approximate snapshot of default-off software diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareSnapshot {
    asid_tlb_flushes_avoided: usize,
    saturated: bool,
}

impl SoftwareSnapshot {
    /// Returns fast-switch decisions that avoided one full local TLB flush.
    pub const fn asid_tlb_flushes_avoided(self) -> usize {
        self.asid_tlb_flushes_avoided
    }

    /// Returns whether an enabled counter reached `usize::MAX`.
    pub const fn is_saturated(self) -> bool {
        self.saturated
    }
}

/// Default-off low-overhead software diagnostics.
///
/// This primitive records facts already classified by a higher-level ASID
/// mechanism. It does not decide whether retaining a TLB is safe.
pub struct SoftwareDiagnostics {
    enabled: AtomicBool,
    asid_tlb_flushes_avoided: AtomicUsize,
    saturated: AtomicBool,
}

impl SoftwareDiagnostics {
    /// Constructs disabled, zeroed diagnostics.
    pub const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            asid_tlb_flushes_avoided: AtomicUsize::new(0),
            saturated: AtomicBool::new(false),
        }
    }

    /// Enables or disables future increments and returns the previous state.
    pub fn set_enabled(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::Relaxed)
    }

    /// Returns whether future increments are enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Records one already-classified fast switch that avoided a full flush.
    ///
    /// The disabled path performs one relaxed atomic load. Enabled increments
    /// saturate rather than wrapping.
    #[inline]
    pub fn record_asid_tlb_flush_avoided(&self) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if try_update_usize(
            &self.asid_tlb_flushes_avoided,
            Ordering::Relaxed,
            Ordering::Relaxed,
            |value| value.checked_add(1),
        )
        .is_err()
        {
            self.saturated.store(true, Ordering::Relaxed);
        }
    }

    /// Returns an approximate lock-free snapshot without resetting counters.
    pub fn snapshot(&self) -> SoftwareSnapshot {
        SoftwareSnapshot {
            asid_tlb_flushes_avoided: self.asid_tlb_flushes_avoided.load(Ordering::Relaxed),
            saturated: self.saturated.load(Ordering::Relaxed),
        }
    }

    /// Atomically resets individual fields and returns their previous values.
    ///
    /// Concurrent increments can fall on either side of the returned snapshot;
    /// this diagnostic API does not impose a global synchronization barrier.
    pub fn snapshot_and_reset(&self) -> SoftwareSnapshot {
        SoftwareSnapshot {
            asid_tlb_flushes_avoided: self.asid_tlb_flushes_avoided.swap(0, Ordering::Relaxed),
            saturated: self.saturated.swap(false, Ordering::Relaxed),
        }
    }
}

impl Default for SoftwareDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MockCounter(usize);

    struct MockBackend {
        configured: Vec<Event>,
        running: [bool; 5],
        released: [bool; 5],
        values: [u64; 5],
        fail_configure: Option<Event>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                configured: Vec::new(),
                running: [false; 5],
                released: [false; 5],
                values: [11, 22, 33, 44, 55],
                fail_configure: None,
            }
        }
    }

    impl Backend for MockBackend {
        type Counter = MockCounter;

        fn capabilities(&self) -> Capabilities {
            Capabilities::new(CounterSource::Platform, 5, EventMask::ALL, false)
        }

        fn configure(&mut self, event: Event) -> Result<Self::Counter, Error> {
            if self.fail_configure == Some(event) {
                return Err(Error::NoCounter);
            }
            let slot = self.configured.len();
            self.configured.push(event);
            Ok(MockCounter(slot))
        }

        fn start(&mut self, counter: Self::Counter) -> Result<(), Error> {
            self.running[counter.0] = true;
            Ok(())
        }

        fn read(&mut self, counter: Self::Counter) -> Result<u64, Error> {
            Ok(self.values[counter.0])
        }

        fn stop(&mut self, counter: Self::Counter) -> Result<(), Error> {
            self.running[counter.0] = false;
            Ok(())
        }

        fn release(&mut self, counter: Self::Counter) -> Result<(), Error> {
            self.running[counter.0] = false;
            self.released[counter.0] = true;
            Ok(())
        }
    }

    #[test]
    fn session_is_stopped_until_explicit_start_and_snapshots_in_order() {
        let mut backend = MockBackend::new();
        let events = [Event::CpuCycles, Event::Instructions];
        let mut session = Session::<_, 2>::open(&mut backend, &events).unwrap();
        assert!(!session.is_running());
        assert_eq!(session.snapshot(), Err(Error::NotRunning));

        session.start().unwrap();
        let snapshot = session.snapshot().unwrap();
        assert_eq!(snapshot.events(), events);
        assert_eq!(snapshot.values(), [11, 22]);
        assert!(!snapshot.is_consistent());
        session.stop().unwrap();
        session.close().unwrap();

        assert_eq!(backend.released[..2], [true, true]);
    }

    #[test]
    fn duplicate_and_partial_configuration_release_claimed_counters() {
        let mut duplicate_backend = MockBackend::new();
        assert!(matches!(
            Session::<_, 2>::open(
                &mut duplicate_backend,
                &[Event::CpuCycles, Event::CpuCycles]
            ),
            Err(Error::DuplicateEvent)
        ));
        assert!(duplicate_backend.released[0]);

        let mut failing_backend = MockBackend::new();
        failing_backend.fail_configure = Some(Event::Instructions);
        assert!(matches!(
            Session::<_, 2>::open(
                &mut failing_backend,
                &[Event::CpuCycles, Event::Instructions]
            ),
            Err(Error::NoCounter)
        ));
        assert!(failing_backend.released[0]);
    }

    #[test]
    fn software_diagnostics_are_default_off_and_resettable() {
        let diagnostics = SoftwareDiagnostics::new();
        diagnostics.record_asid_tlb_flush_avoided();
        assert_eq!(diagnostics.snapshot().asid_tlb_flushes_avoided(), 0);

        assert!(!diagnostics.set_enabled(true));
        diagnostics.record_asid_tlb_flush_avoided();
        diagnostics.record_asid_tlb_flush_avoided();
        assert_eq!(diagnostics.snapshot().asid_tlb_flushes_avoided(), 2);
        assert_eq!(
            diagnostics.snapshot_and_reset().asid_tlb_flushes_avoided(),
            2
        );
        assert_eq!(diagnostics.snapshot().asid_tlb_flushes_avoided(), 0);
    }
}
