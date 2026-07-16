#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(test)]
extern crate std;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

static NEXT_BROKER_ID: AtomicU64 = AtomicU64::new(1);

/// Failure while constructing a broker and reserving all of its storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrokerConfigError {
    /// `usize::MAX` was supplied where an explicit finite bound is required.
    UnboundedCapacity,
    /// The request or waiter slot arrays could not be allocated.
    NoMemory,
    /// The process-wide broker identity space was exhausted without wrapping.
    BrokerIdentityExhausted,
}

/// Failure while admitting one request/waiter relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionError {
    /// Every request slot currently contains a live request.
    RequestCapacity,
    /// Every reusable free request slot exhausted its generation space.
    RequestTokenExhausted,
    /// Every waiter slot currently contains a live waiter.
    WaiterCapacity,
    /// Every reusable free waiter slot exhausted its generation space.
    WaiterTokenExhausted,
    /// Private request/waiter linkage was inconsistent before publication.
    InconsistentState,
}

/// Failure while operating on a request token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestError {
    /// The token is stale, belongs to another broker, or names no live request.
    StaleOrForeign,
    /// The request already has an immutable terminal result.
    AlreadyTerminal,
    /// The operation requires a delivered request.
    NotDelivered,
    /// The operation requires a terminal request.
    NotTerminal,
}

/// Failure while operating on a waiter token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WaiterError {
    /// The token is stale, belongs to another broker, or names no live waiter.
    StaleOrForeign,
    /// The waiter is still attached to a non-visible request.
    NotReady,
    /// Private waiter/request linkage was inconsistent.
    InconsistentState,
}

/// Failure while constructing a checked half-open fault range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FaultRangeError {
    /// A fault range must contain at least one byte.
    Empty,
    /// `start + length` overflowed the `u64` address domain.
    Overflow,
}

/// Checked half-open byte range used by generic range predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultRange {
    start: u64,
    end: u64,
}

impl FaultRange {
    /// Constructs `[start, start + length)` after checking its geometry.
    pub const fn try_new(start: u64, length: u64) -> Result<Self, FaultRangeError> {
        if length == 0 {
            return Err(FaultRangeError::Empty);
        }
        let Some(end) = start.checked_add(length) else {
            return Err(FaultRangeError::Overflow);
        };
        Ok(Self { start, end })
    }

    /// Returns the inclusive start address.
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end address.
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Returns whether two nonempty half-open ranges intersect.
    pub const fn intersects(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// Adapter-provided request key that can be selected by a generic range.
///
/// An implementation normally checks the exact request range stored inside
/// the key. Mapping identity, generation, access, and handler identity remain
/// separate equality facts and must not be weakened by this range projection.
pub trait FaultRangeKey {
    /// Returns whether this exact request intersects `range`.
    fn intersects(&self, range: FaultRange) -> bool;
}

/// Opaque identity for one live or formerly live fault request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestToken {
    broker: u64,
    slot: usize,
    generation: u64,
}

impl RequestToken {
    /// Returns the broker identity, useful for bounded diagnostics.
    pub const fn broker_id(self) -> u64 {
        self.broker
    }

    /// Returns the private slot index, useful for bounded diagnostics.
    pub const fn slot(self) -> usize {
        self.slot
    }

    /// Returns the non-reused slot generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Opaque identity for one independently cancellable fault waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WaiterToken {
    broker: u64,
    slot: usize,
    generation: u64,
}

impl WaiterToken {
    /// Returns the broker identity, useful for bounded diagnostics.
    pub const fn broker_id(self) -> u64 {
        self.broker
    }

    /// Returns the private slot index, useful for bounded diagnostics.
    pub const fn slot(self) -> usize {
        self.slot
    }

    /// Returns the non-reused slot generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Observable phase of one request without exposing its terminal payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPhase {
    /// The request is linked in the broker's FIFO delivery queue.
    Pending,
    /// A handler claimed the request; it is no longer pending for delivery.
    Delivered,
    /// A terminal result exists but is not yet visible to waiters.
    TerminalDeferred,
    /// A terminal result is visible to every retained waiter.
    TerminalVisible,
}

/// Visibility selected when a request receives its terminal result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionVisibility {
    /// Publish the result to all retained waiters immediately.
    Visible,
    /// Fix the result but defer waiter visibility until a later release.
    Deferred,
}

/// Immutable request facts returned to an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestSnapshot<K, H> {
    token: RequestToken,
    key: K,
    handler: H,
    phase: RequestPhase,
    waiter_count: usize,
}

impl<K, H> RequestSnapshot<K, H> {
    /// Returns the exact generation-tagged request identity.
    pub const fn token(&self) -> RequestToken {
        self.token
    }

    /// Returns the adapter-defined exact coalescing key.
    pub const fn key(&self) -> &K {
        &self.key
    }

    /// Returns the generation-scoped handler identity.
    pub const fn handler(&self) -> &H {
        &self.handler
    }

    /// Returns the current request phase.
    pub const fn phase(&self) -> RequestPhase {
        self.phase
    }

    /// Returns the number of independently retained waiters.
    pub const fn waiter_count(&self) -> usize {
        self.waiter_count
    }
}

/// Atomic result of admitting one request plus one waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultAdmission {
    request: RequestToken,
    waiter: WaiterToken,
    coalesced: bool,
}

impl FaultAdmission {
    /// Returns the exact request identity.
    pub const fn request(self) -> RequestToken {
        self.request
    }

    /// Returns the independently cancellable waiter identity.
    pub const fn waiter(self) -> WaiterToken {
        self.waiter
    }

    /// Returns whether this admission joined an existing exact request.
    pub const fn coalesced(self) -> bool {
        self.coalesced
    }
}

/// Current visibility observed by one waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaiterObservation<C> {
    /// The request is open or its terminal result is still deferred.
    Pending,
    /// The immutable terminal result is visible.
    Ready(C),
}

/// Result returned when one waiter is cancelled independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelledWaiter<C> {
    completion: Option<C>,
    request_reclaimed: bool,
}

impl<C> CancelledWaiter<C> {
    /// Returns a result if it had already become visible before cancellation.
    pub const fn completion(&self) -> Option<&C> {
        self.completion.as_ref()
    }

    /// Returns whether this was the final waiter and reclaimed the request.
    pub const fn request_reclaimed(&self) -> bool {
        self.request_reclaimed
    }
}

/// Terminal result consumed from one ready waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakenCompletion<C> {
    completion: C,
    request_reclaimed: bool,
}

impl<C> TakenCompletion<C> {
    /// Returns a shared view of the adapter-defined terminal result.
    pub const fn completion_ref(&self) -> &C {
        &self.completion
    }

    /// Returns the adapter-defined terminal result by value.
    pub fn into_completion(self) -> C {
        self.completion
    }

    /// Returns whether this was the final waiter and reclaimed the request.
    pub const fn request_reclaimed(&self) -> bool {
        self.request_reclaimed
    }
}

impl<C: Copy> TakenCompletion<C> {
    /// Copies the adapter-defined terminal result.
    pub const fn completion(&self) -> C {
        self.completion
    }
}

/// One request completion or deferred-result release transition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompletionEffect {
    waiters_released: usize,
}

impl CompletionEffect {
    /// Returns the number of waiters whose terminal result became visible.
    pub const fn waiters_released(self) -> usize {
        self.waiters_released
    }
}

/// Aggregate result of predicate/range completion and release work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompletionSummary {
    requests_completed: usize,
    requests_released: usize,
    waiters_released: usize,
}

impl CompletionSummary {
    /// Returns the number of open requests given a terminal result.
    pub const fn requests_completed(self) -> usize {
        self.requests_completed
    }

    /// Returns the number of deferred terminal requests made visible.
    pub const fn requests_released(self) -> usize {
        self.requests_released
    }

    /// Returns the number of waiters whose result became visible.
    pub const fn waiters_released(self) -> usize {
        self.waiters_released
    }

    fn record_completion(&mut self, effect: CompletionEffect) {
        self.requests_completed += 1;
        self.waiters_released += effect.waiters_released;
    }

    fn record_release(&mut self, effect: CompletionEffect) {
        self.requests_released += 1;
        self.waiters_released += effect.waiters_released;
    }

    fn merge(&mut self, other: Self) {
        self.requests_completed += other.requests_completed;
        self.requests_released += other.requests_released;
        self.waiters_released += other.waiters_released;
    }
}

/// Exact broker occupancy and phase counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrokerLoad {
    request_capacity: usize,
    live_requests: usize,
    pending_requests: usize,
    delivered_requests: usize,
    deferred_requests: usize,
    visible_requests: usize,
    waiter_capacity: usize,
    live_waiters: usize,
    ready_waiters: usize,
}

impl BrokerLoad {
    /// Returns the configured request-slot ceiling.
    pub const fn request_capacity(self) -> usize {
        self.request_capacity
    }

    /// Returns the number of live requests in every phase.
    pub const fn live_requests(self) -> usize {
        self.live_requests
    }

    /// Returns the number of FIFO-pending requests.
    pub const fn pending_requests(self) -> usize {
        self.pending_requests
    }

    /// Returns the number of handler-claimed requests.
    pub const fn delivered_requests(self) -> usize {
        self.delivered_requests
    }

    /// Returns the number of terminal results with deferred visibility.
    pub const fn deferred_requests(self) -> usize {
        self.deferred_requests
    }

    /// Returns the number of terminal results visible to waiters.
    pub const fn visible_requests(self) -> usize {
        self.visible_requests
    }

    /// Returns the configured waiter-slot ceiling.
    pub const fn waiter_capacity(self) -> usize {
        self.waiter_capacity
    }

    /// Returns the number of retained waiters.
    pub const fn live_waiters(self) -> usize {
        self.live_waiters
    }

    /// Returns the number of retained waiters with visible results.
    pub const fn ready_waiters(self) -> usize {
        self.ready_waiters
    }
}

#[derive(Clone, Copy)]
enum RequestState<C> {
    Pending,
    Delivered,
    Terminal { completion: C, visible: bool },
}

impl<C> RequestState<C> {
    fn phase(&self) -> RequestPhase {
        match self {
            Self::Pending => RequestPhase::Pending,
            Self::Delivered => RequestPhase::Delivered,
            Self::Terminal { visible: false, .. } => RequestPhase::TerminalDeferred,
            Self::Terminal { visible: true, .. } => RequestPhase::TerminalVisible,
        }
    }
}

#[derive(Clone, Copy)]
struct RequestRecord<K, H, C> {
    key: K,
    handler: H,
    state: RequestState<C>,
    waiter_head: Option<usize>,
    waiter_tail: Option<usize>,
    waiter_count: usize,
    pending_prev: Option<usize>,
    pending_next: Option<usize>,
}

struct RequestSlot<K, H, C> {
    generation: u64,
    record: Option<RequestRecord<K, H, C>>,
}

#[derive(Clone, Copy)]
enum WaiterState<C> {
    Active,
    Ready(C),
}

#[derive(Clone, Copy)]
struct WaiterRecord<C> {
    request: RequestToken,
    state: WaiterState<C>,
    prev: Option<usize>,
    next: Option<usize>,
}

struct WaiterSlot<C> {
    generation: u64,
    record: Option<WaiterRecord<C>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotChoiceError {
    Capacity,
    GenerationExhausted,
}

/// Preallocated generic fault-request and waiter state machine.
///
/// `K` is the exact coalescing key, `H` is a generation-scoped handler
/// identity, and `C` is the immutable terminal result. Requiring all three to
/// be `Copy` keeps arbitrary destructors and hidden allocations out of every
/// hot transition. The type uses `&mut self` for mutation so a consumer can
/// select the lock and lock-order contract appropriate to its VM design.
pub struct FaultBroker<K, H, C> {
    id: u64,
    requests: Vec<RequestSlot<K, H, C>>,
    waiters: Vec<WaiterSlot<C>>,
    pending_head: Option<usize>,
    pending_tail: Option<usize>,
}

impl<K, H, C> FaultBroker<K, H, C>
where
    K: Copy + Eq,
    H: Copy + Eq,
    C: Copy,
{
    /// Allocates every request and waiter slot used for the broker's lifetime.
    ///
    /// Zero is a valid explicit capacity and creates an always-full registry.
    /// No method after this constructor allocates or grows either slot array.
    pub fn try_new(
        request_capacity: usize,
        waiter_capacity: usize,
    ) -> Result<Self, BrokerConfigError> {
        if request_capacity == usize::MAX || waiter_capacity == usize::MAX {
            return Err(BrokerConfigError::UnboundedCapacity);
        }

        let mut requests = Vec::new();
        requests
            .try_reserve_exact(request_capacity)
            .map_err(|_| BrokerConfigError::NoMemory)?;
        for _ in 0..request_capacity {
            requests.push(RequestSlot {
                generation: 0,
                record: None,
            });
        }

        let mut waiters = Vec::new();
        waiters
            .try_reserve_exact(waiter_capacity)
            .map_err(|_| BrokerConfigError::NoMemory)?;
        for _ in 0..waiter_capacity {
            waiters.push(WaiterSlot {
                generation: 0,
                record: None,
            });
        }

        let id = NEXT_BROKER_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| BrokerConfigError::BrokerIdentityExhausted)?;
        Ok(Self {
            id,
            requests,
            waiters,
            pending_head: None,
            pending_tail: None,
        })
    }

    /// Returns the process-unique nonzero broker identity embedded in tokens.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Finds the live request that a subsequent [`Self::admit`] would reuse.
    ///
    /// This read-only probe lets an upper policy distinguish a new request,
    /// which consumes request quota, from one additional waiter on an exact
    /// existing request. The returned generation-tagged snapshot is advisory
    /// until the externally serialized caller invokes `admit`; consumers must
    /// keep both operations in the same broker critical section.
    pub fn matching_request(&self, handler: H, key: K) -> Option<RequestSnapshot<K, H>> {
        let slot = self.requests.iter().position(|slot| {
            slot.record
                .is_some_and(|record| record.handler == handler && record.key == key)
        })?;
        self.snapshot_slot(slot)
    }

    /// Admits one exact request plus one independently cancellable waiter.
    ///
    /// An existing live request is reused only when both `handler` and `key`
    /// compare equal. All capacity and generation checks complete before any
    /// request or waiter becomes visible.
    pub fn admit(&mut self, handler: H, key: K) -> Result<FaultAdmission, AdmissionError> {
        let existing = self.requests.iter().position(|slot| {
            slot.record
                .is_some_and(|record| record.handler == handler && record.key == key)
        });

        let (request_slot, request_generation, coalesced) = if let Some(slot) = existing {
            (slot, self.requests[slot].generation, true)
        } else {
            let (slot, generation) = self.choose_request_slot().map_err(|error| match error {
                SlotChoiceError::Capacity => AdmissionError::RequestCapacity,
                SlotChoiceError::GenerationExhausted => AdmissionError::RequestTokenExhausted,
            })?;
            (slot, generation, false)
        };
        let (waiter_slot, waiter_generation) =
            self.choose_waiter_slot().map_err(|error| match error {
                SlotChoiceError::Capacity => AdmissionError::WaiterCapacity,
                SlotChoiceError::GenerationExhausted => AdmissionError::WaiterTokenExhausted,
            })?;

        let next_waiter_count = if coalesced {
            self.requests[request_slot]
                .record
                .as_ref()
                .ok_or(AdmissionError::InconsistentState)?
                .waiter_count
                .checked_add(1)
                .ok_or(AdmissionError::InconsistentState)?
        } else {
            1
        };
        if next_waiter_count > self.waiters.len() {
            return Err(AdmissionError::InconsistentState);
        }

        let request = RequestToken {
            broker: self.id,
            slot: request_slot,
            generation: request_generation,
        };
        let waiter = WaiterToken {
            broker: self.id,
            slot: waiter_slot,
            generation: waiter_generation,
        };

        if !coalesced {
            self.requests[request_slot].generation = request_generation;
            self.requests[request_slot].record = Some(RequestRecord {
                key,
                handler,
                state: RequestState::Pending,
                waiter_head: None,
                waiter_tail: None,
                waiter_count: 0,
                pending_prev: self.pending_tail,
                pending_next: None,
            });
            if let Some(tail) = self.pending_tail {
                if let Some(record) = self.requests[tail].record.as_mut() {
                    record.pending_next = Some(request_slot);
                }
            } else {
                self.pending_head = Some(request_slot);
            }
            self.pending_tail = Some(request_slot);
        }

        let request_record = self.requests[request_slot]
            .record
            .as_ref()
            .ok_or(AdmissionError::InconsistentState)?;
        let waiter_state = match request_record.state {
            RequestState::Terminal {
                completion,
                visible: true,
            } => WaiterState::Ready(completion),
            _ => WaiterState::Active,
        };
        let previous = request_record.waiter_tail;

        self.waiters[waiter_slot].generation = waiter_generation;
        self.waiters[waiter_slot].record = Some(WaiterRecord {
            request,
            state: waiter_state,
            prev: previous,
            next: None,
        });
        if let Some(previous) = previous {
            let Some(previous_record) = self.waiters[previous].record.as_mut() else {
                self.waiters[waiter_slot].record = None;
                if !coalesced {
                    self.reclaim_request(request_slot);
                }
                return Err(AdmissionError::InconsistentState);
            };
            previous_record.next = Some(waiter_slot);
        }
        let Some(record) = self.requests[request_slot].record.as_mut() else {
            self.waiters[waiter_slot].record = None;
            return Err(AdmissionError::InconsistentState);
        };
        if record.waiter_head.is_none() {
            record.waiter_head = Some(waiter_slot);
        }
        record.waiter_tail = Some(waiter_slot);
        record.waiter_count = next_waiter_count;

        Ok(FaultAdmission {
            request,
            waiter,
            coalesced,
        })
    }

    /// Claims the oldest pending request for `handler`.
    ///
    /// Other handlers' requests remain linked in their original relative
    /// order. The returned request stays `Delivered` until terminal completion
    /// or final-waiter cancellation; it is never silently requeued.
    pub fn claim_next(&mut self, handler: H) -> Option<RequestSnapshot<K, H>> {
        let mut cursor = self.pending_head;
        while let Some(slot) = cursor {
            let record = self.requests.get(slot)?.record?;
            cursor = record.pending_next;
            if record.handler != handler {
                continue;
            }
            self.unlink_pending(slot);
            self.requests[slot].record.as_mut()?.state = RequestState::Delivered;
            return self.snapshot_slot(slot);
        }
        None
    }

    /// Returns a stable copy of one live request's current facts.
    pub fn request(&self, token: RequestToken) -> Result<RequestSnapshot<K, H>, RequestError> {
        let slot = self.request_index(token)?;
        self.snapshot_slot(slot).ok_or(RequestError::StaleOrForeign)
    }

    /// Gives one pending or delivered request its immutable terminal result.
    pub fn complete(
        &mut self,
        token: RequestToken,
        completion: C,
        visibility: CompletionVisibility,
    ) -> Result<CompletionEffect, RequestError> {
        let slot = self.request_index(token)?;
        if matches!(
            self.requests[slot].record.map(|record| record.state),
            Some(RequestState::Terminal { .. })
        ) {
            return Err(RequestError::AlreadyTerminal);
        }
        Ok(self.complete_slot(slot, completion, visibility))
    }

    /// Completes every open request accepted by `predicate`.
    ///
    /// The predicate receives copied request facts and must not weaken exact
    /// generation identity when selecting stale-sensitive work.
    pub fn complete_where(
        &mut self,
        mut predicate: impl FnMut(RequestSnapshot<K, H>) -> bool,
        completion: C,
        visibility: CompletionVisibility,
    ) -> CompletionSummary {
        let mut summary = CompletionSummary::default();
        for slot in 0..self.requests.len() {
            let Some(snapshot) = self.snapshot_slot(slot) else {
                continue;
            };
            if matches!(
                snapshot.phase,
                RequestPhase::Pending | RequestPhase::Delivered
            ) && predicate(snapshot)
            {
                let effect = self.complete_slot(slot, completion, visibility);
                summary.record_completion(effect);
            }
        }
        summary
    }

    /// Completes every open request whose key intersects `range`.
    pub fn complete_range(
        &mut self,
        range: FaultRange,
        completion: C,
        visibility: CompletionVisibility,
    ) -> CompletionSummary
    where
        K: FaultRangeKey,
    {
        self.complete_where(
            |snapshot| snapshot.key.intersects(range),
            completion,
            visibility,
        )
    }

    /// Makes one deferred terminal result visible without changing it.
    pub fn release(&mut self, token: RequestToken) -> Result<CompletionEffect, RequestError> {
        let slot = self.request_index(token)?;
        match self.requests[slot].record.map(|record| record.state) {
            Some(RequestState::Terminal { visible: false, .. }) => Ok(self.release_slot(slot)),
            Some(RequestState::Terminal { visible: true, .. }) => Ok(CompletionEffect::default()),
            _ => Err(RequestError::NotTerminal),
        }
    }

    /// Releases every deferred terminal request accepted by `predicate`.
    pub fn release_where(
        &mut self,
        mut predicate: impl FnMut(RequestSnapshot<K, H>) -> bool,
    ) -> CompletionSummary {
        let mut summary = CompletionSummary::default();
        for slot in 0..self.requests.len() {
            let Some(snapshot) = self.snapshot_slot(slot) else {
                continue;
            };
            if snapshot.phase == RequestPhase::TerminalDeferred && predicate(snapshot) {
                let effect = self.release_slot(slot);
                summary.record_release(effect);
            }
        }
        summary
    }

    /// Releases every deferred request whose key intersects `range`.
    pub fn release_range(&mut self, range: FaultRange) -> CompletionSummary
    where
        K: FaultRangeKey,
    {
        self.release_where(|snapshot| snapshot.key.intersects(range))
    }

    /// Terminalizes all live requests owned by one detached handler.
    ///
    /// Pending and delivered requests receive `completion` and become visible.
    /// Already-deferred requests retain their original result and are merely
    /// released. The handler owner must prevent new admissions before calling
    /// this method; this core intentionally does not own a handler registry.
    pub fn detach_handler(&mut self, handler: H, completion: C) -> CompletionSummary {
        let mut summary = self.complete_where(
            |snapshot| *snapshot.handler() == handler,
            completion,
            CompletionVisibility::Visible,
        );
        summary.merge(self.release_where(|snapshot| *snapshot.handler() == handler));
        summary
    }

    /// Observes one retained waiter's current terminal visibility.
    pub fn waiter(&self, token: WaiterToken) -> Result<WaiterObservation<C>, WaiterError> {
        let slot = self.waiter_index(token)?;
        let record = self.waiters[slot]
            .record
            .ok_or(WaiterError::StaleOrForeign)?;
        Ok(match record.state {
            WaiterState::Active => WaiterObservation::Pending,
            WaiterState::Ready(completion) => WaiterObservation::Ready(completion),
        })
    }

    /// Cancels one waiter without affecting other coalesced waiters.
    pub fn cancel_waiter(&mut self, token: WaiterToken) -> Result<CancelledWaiter<C>, WaiterError> {
        let slot = self.waiter_index(token)?;
        let completion = match self.waiters[slot]
            .record
            .ok_or(WaiterError::StaleOrForeign)?
            .state
        {
            WaiterState::Active => None,
            WaiterState::Ready(completion) => Some(completion),
        };
        let request_reclaimed = self.remove_waiter(slot)?;
        Ok(CancelledWaiter {
            completion,
            request_reclaimed,
        })
    }

    /// Consumes one visible terminal result and releases the waiter slot.
    pub fn take_waiter_completion(
        &mut self,
        token: WaiterToken,
    ) -> Result<TakenCompletion<C>, WaiterError> {
        let slot = self.waiter_index(token)?;
        let completion = match self.waiters[slot]
            .record
            .ok_or(WaiterError::StaleOrForeign)?
            .state
        {
            WaiterState::Active => return Err(WaiterError::NotReady),
            WaiterState::Ready(completion) => completion,
        };
        let request_reclaimed = self.remove_waiter(slot)?;
        Ok(TakenCompletion {
            completion,
            request_reclaimed,
        })
    }

    /// Computes exact request/waiter occupancy without allocating.
    pub fn load(&self) -> BrokerLoad {
        let mut load = BrokerLoad {
            request_capacity: self.requests.len(),
            waiter_capacity: self.waiters.len(),
            ..BrokerLoad::default()
        };
        for slot in &self.requests {
            let Some(record) = slot.record else {
                continue;
            };
            load.live_requests += 1;
            match record.state.phase() {
                RequestPhase::Pending => load.pending_requests += 1,
                RequestPhase::Delivered => load.delivered_requests += 1,
                RequestPhase::TerminalDeferred => load.deferred_requests += 1,
                RequestPhase::TerminalVisible => load.visible_requests += 1,
            }
        }
        for slot in &self.waiters {
            let Some(record) = slot.record else {
                continue;
            };
            load.live_waiters += 1;
            if matches!(record.state, WaiterState::Ready(_)) {
                load.ready_waiters += 1;
            }
        }
        load
    }

    fn choose_request_slot(&self) -> Result<(usize, u64), SlotChoiceError> {
        let mut exhausted_free = false;
        for (slot, entry) in self.requests.iter().enumerate() {
            if entry.record.is_none() {
                if let Some(generation) = entry.generation.checked_add(1) {
                    return Ok((slot, generation));
                }
                exhausted_free = true;
            }
        }
        if exhausted_free {
            Err(SlotChoiceError::GenerationExhausted)
        } else {
            Err(SlotChoiceError::Capacity)
        }
    }

    fn choose_waiter_slot(&self) -> Result<(usize, u64), SlotChoiceError> {
        let mut exhausted_free = false;
        for (slot, entry) in self.waiters.iter().enumerate() {
            if entry.record.is_none() {
                if let Some(generation) = entry.generation.checked_add(1) {
                    return Ok((slot, generation));
                }
                exhausted_free = true;
            }
        }
        if exhausted_free {
            Err(SlotChoiceError::GenerationExhausted)
        } else {
            Err(SlotChoiceError::Capacity)
        }
    }

    fn request_index(&self, token: RequestToken) -> Result<usize, RequestError> {
        if token.broker != self.id {
            return Err(RequestError::StaleOrForeign);
        }
        let Some(slot) = self.requests.get(token.slot) else {
            return Err(RequestError::StaleOrForeign);
        };
        if slot.generation != token.generation || slot.record.is_none() {
            return Err(RequestError::StaleOrForeign);
        }
        Ok(token.slot)
    }

    fn waiter_index(&self, token: WaiterToken) -> Result<usize, WaiterError> {
        if token.broker != self.id {
            return Err(WaiterError::StaleOrForeign);
        }
        let Some(slot) = self.waiters.get(token.slot) else {
            return Err(WaiterError::StaleOrForeign);
        };
        if slot.generation != token.generation || slot.record.is_none() {
            return Err(WaiterError::StaleOrForeign);
        }
        Ok(token.slot)
    }

    fn snapshot_slot(&self, slot: usize) -> Option<RequestSnapshot<K, H>> {
        let entry = self.requests.get(slot)?;
        let record = entry.record?;
        Some(RequestSnapshot {
            token: RequestToken {
                broker: self.id,
                slot,
                generation: entry.generation,
            },
            key: record.key,
            handler: record.handler,
            phase: record.state.phase(),
            waiter_count: record.waiter_count,
        })
    }

    fn unlink_pending(&mut self, slot: usize) {
        let Some(record) = self.requests[slot].record else {
            return;
        };
        let previous = record.pending_prev;
        let next = record.pending_next;
        if let Some(previous) = previous {
            if let Some(previous_record) = self.requests[previous].record.as_mut() {
                previous_record.pending_next = next;
            }
        } else {
            self.pending_head = next;
        }
        if let Some(next) = next {
            if let Some(next_record) = self.requests[next].record.as_mut() {
                next_record.pending_prev = previous;
            }
        } else {
            self.pending_tail = previous;
        }
        if let Some(record) = self.requests[slot].record.as_mut() {
            record.pending_prev = None;
            record.pending_next = None;
        }
    }

    fn complete_slot(
        &mut self,
        slot: usize,
        completion: C,
        visibility: CompletionVisibility,
    ) -> CompletionEffect {
        if matches!(
            self.requests[slot].record.map(|record| record.state),
            Some(RequestState::Pending)
        ) {
            self.unlink_pending(slot);
        }
        let visible = visibility == CompletionVisibility::Visible;
        if let Some(record) = self.requests[slot].record.as_mut() {
            record.state = RequestState::Terminal {
                completion,
                visible,
            };
        }
        let waiters_released = if visible {
            self.publish_waiters(slot, completion)
        } else {
            0
        };
        CompletionEffect { waiters_released }
    }

    fn release_slot(&mut self, slot: usize) -> CompletionEffect {
        let completion = match self.requests[slot].record.map(|record| record.state) {
            Some(RequestState::Terminal {
                completion,
                visible: false,
            }) => completion,
            _ => return CompletionEffect::default(),
        };
        if let Some(record) = self.requests[slot].record.as_mut() {
            record.state = RequestState::Terminal {
                completion,
                visible: true,
            };
        }
        CompletionEffect {
            waiters_released: self.publish_waiters(slot, completion),
        }
    }

    fn publish_waiters(&mut self, request_slot: usize, completion: C) -> usize {
        let mut released = 0;
        let mut cursor = self.requests[request_slot]
            .record
            .and_then(|record| record.waiter_head);
        while let Some(slot) = cursor {
            let Some(record) = self.waiters[slot].record else {
                break;
            };
            cursor = record.next;
            if matches!(record.state, WaiterState::Active) {
                if let Some(record) = self.waiters[slot].record.as_mut() {
                    record.state = WaiterState::Ready(completion);
                    released += 1;
                }
            }
        }
        released
    }

    fn remove_waiter(&mut self, slot: usize) -> Result<bool, WaiterError> {
        let waiter = self.waiters[slot]
            .record
            .ok_or(WaiterError::StaleOrForeign)?;
        let request_slot = self
            .request_index(waiter.request)
            .map_err(|_| WaiterError::InconsistentState)?;
        let request = self.requests[request_slot]
            .record
            .ok_or(WaiterError::InconsistentState)?;
        if request.waiter_count == 0 {
            return Err(WaiterError::InconsistentState);
        }

        if let Some(previous) = waiter.prev {
            let Some(previous_record) = self.waiters[previous].record.as_mut() else {
                return Err(WaiterError::InconsistentState);
            };
            previous_record.next = waiter.next;
        }
        if let Some(next) = waiter.next {
            let Some(next_record) = self.waiters[next].record.as_mut() else {
                return Err(WaiterError::InconsistentState);
            };
            next_record.prev = waiter.prev;
        }

        let request = self.requests[request_slot]
            .record
            .as_mut()
            .ok_or(WaiterError::InconsistentState)?;
        if request.waiter_head == Some(slot) {
            request.waiter_head = waiter.next;
        }
        if request.waiter_tail == Some(slot) {
            request.waiter_tail = waiter.prev;
        }
        request.waiter_count -= 1;
        let reclaim = request.waiter_count == 0;
        self.waiters[slot].record = None;
        if reclaim {
            self.reclaim_request(request_slot);
        }
        Ok(reclaim)
    }

    fn reclaim_request(&mut self, slot: usize) {
        if matches!(
            self.requests[slot].record.map(|record| record.state),
            Some(RequestState::Pending)
        ) {
            self.unlink_pending(slot);
        }
        self.requests[slot].record = None;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier, Mutex},
        thread,
        vec::Vec,
    };

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Access {
        Read,
        Write,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Key {
        mapping: u64,
        generation: u64,
        range: FaultRange,
        access: Access,
    }

    impl Key {
        fn page(mapping: u64, generation: u64, address: u64, access: Access) -> Self {
            Self {
                mapping,
                generation,
                range: FaultRange::try_new(address, 4096).unwrap(),
                access,
            }
        }
    }

    impl FaultRangeKey for Key {
        fn intersects(&self, range: FaultRange) -> bool {
            self.range.intersects(range)
        }
    }

    type Broker = FaultBroker<Key, u64, i32>;

    #[test]
    fn construction_rejects_accidentally_unbounded_capacity() {
        assert!(matches!(
            Broker::try_new(usize::MAX, 1),
            Err(BrokerConfigError::UnboundedCapacity)
        ));
        assert!(matches!(
            Broker::try_new(1, usize::MAX),
            Err(BrokerConfigError::UnboundedCapacity)
        ));

        let mut empty = Broker::try_new(0, 0).unwrap();
        assert_eq!(
            empty.admit(1, Key::page(1, 1, 0, Access::Read)),
            Err(AdmissionError::RequestCapacity)
        );
        assert_eq!(empty.load().request_capacity(), 0);
        assert_eq!(empty.load().waiter_capacity(), 0);
    }

    #[test]
    fn checked_ranges_preserve_half_open_edges() {
        assert_eq!(FaultRange::try_new(0, 0), Err(FaultRangeError::Empty));
        assert_eq!(
            FaultRange::try_new(u64::MAX, 1),
            Err(FaultRangeError::Overflow)
        );

        let left = FaultRange::try_new(0x1000, 0x1000).unwrap();
        let touching = FaultRange::try_new(0x2000, 0x1000).unwrap();
        let overlapping = FaultRange::try_new(0x1fff, 2).unwrap();
        assert!(!left.intersects(touching));
        assert!(left.intersects(overlapping));
        assert_eq!(left.start(), 0x1000);
        assert_eq!(left.end(), 0x2000);
    }

    #[test]
    fn exact_request_coalescing_preserves_independent_waiters() {
        let mut broker = Broker::try_new(3, 4).unwrap();
        let key = Key::page(7, 11, 0x1000, Access::Read);
        assert!(broker.matching_request(3, key).is_none());
        let first = broker.admit(3, key).unwrap();
        let matching = broker.matching_request(3, key).unwrap();
        assert_eq!(matching.token(), first.request());
        assert_eq!(matching.phase(), RequestPhase::Pending);
        assert!(broker.matching_request(4, key).is_none());
        let second = broker.admit(3, key).unwrap();
        assert!(!first.coalesced());
        assert!(second.coalesced());
        assert_eq!(first.request(), second.request());
        assert_ne!(first.waiter(), second.waiter());

        let different_generation = broker
            .admit(3, Key::page(7, 12, 0x1000, Access::Read))
            .unwrap();
        let different_access = broker
            .admit(3, Key::page(7, 11, 0x1000, Access::Write))
            .unwrap();
        assert_ne!(first.request(), different_generation.request());
        assert_ne!(first.request(), different_access.request());

        let load = broker.load();
        assert_eq!(load.live_requests(), 3);
        assert_eq!(load.live_waiters(), 4);
    }

    #[test]
    fn failed_coalesced_waiter_admission_is_atomic() {
        let mut broker = Broker::try_new(2, 1).unwrap();
        let key = Key::page(1, 1, 0, Access::Read);
        let first = broker.admit(1, key).unwrap();
        let before = broker.load();
        assert_eq!(broker.admit(1, key), Err(AdmissionError::WaiterCapacity));
        assert_eq!(broker.load(), before);
        assert_eq!(broker.request(first.request()).unwrap().waiter_count(), 1);
    }

    #[test]
    fn handler_fifo_claim_skips_without_reordering_other_handlers() {
        let mut broker = Broker::try_new(4, 4).unwrap();
        let a1 = broker
            .admit(1, Key::page(1, 1, 0x1000, Access::Read))
            .unwrap();
        let b1 = broker
            .admit(2, Key::page(2, 1, 0x2000, Access::Read))
            .unwrap();
        let a2 = broker
            .admit(1, Key::page(3, 1, 0x3000, Access::Read))
            .unwrap();

        assert_eq!(broker.claim_next(2).unwrap().token(), b1.request());
        assert_eq!(broker.claim_next(1).unwrap().token(), a1.request());
        assert_eq!(broker.claim_next(1).unwrap().token(), a2.request());
        assert!(broker.claim_next(1).is_none());
    }

    #[test]
    fn dropping_a_claim_snapshot_keeps_request_out_of_pending_fifo() {
        let mut broker = Broker::try_new(3, 3).unwrap();
        let first = broker
            .admit(9, Key::page(1, 1, 0x1000, Access::Read))
            .unwrap();
        let second = broker
            .admit(9, Key::page(2, 1, 0x2000, Access::Read))
            .unwrap();

        {
            let delivered = broker.claim_next(9).unwrap();
            assert_eq!(delivered.token(), first.request());
        }
        assert_eq!(broker.claim_next(9).unwrap().token(), second.request());
        assert!(broker.claim_next(9).is_none());
        assert_eq!(
            broker.request(first.request()).unwrap().phase(),
            RequestPhase::Delivered
        );
    }

    #[test]
    fn waiter_cancellation_is_independent_and_final_cancel_reclaims() {
        let mut broker = Broker::try_new(1, 2).unwrap();
        let key = Key::page(1, 1, 0, Access::Read);
        let first = broker.admit(1, key).unwrap();
        let second = broker.admit(1, key).unwrap();
        assert!(
            !broker
                .cancel_waiter(first.waiter())
                .unwrap()
                .request_reclaimed()
        );
        assert_eq!(broker.load().live_requests(), 1);
        assert_eq!(broker.load().live_waiters(), 1);

        assert!(
            broker
                .cancel_waiter(second.waiter())
                .unwrap()
                .request_reclaimed()
        );
        assert_eq!(broker.load().live_requests(), 0);
        assert_eq!(broker.load().pending_requests(), 0);
        assert_eq!(
            broker.request(first.request()),
            Err(RequestError::StaleOrForeign)
        );
        assert_eq!(
            broker.cancel_waiter(first.waiter()),
            Err(WaiterError::StaleOrForeign)
        );
    }

    #[test]
    fn final_waiter_reclaims_a_delivered_request() {
        let mut broker = Broker::try_new(1, 1).unwrap();
        let admission = broker.admit(1, Key::page(1, 1, 0, Access::Read)).unwrap();
        assert_eq!(
            broker.claim_next(1).unwrap().phase(),
            RequestPhase::Delivered
        );
        assert!(
            broker
                .cancel_waiter(admission.waiter())
                .unwrap()
                .request_reclaimed()
        );
        assert_eq!(broker.load().live_requests(), 0);
    }

    #[test]
    fn deferred_completion_is_fixed_until_predicate_release() {
        let mut broker = Broker::try_new(2, 2).unwrap();
        let first = broker
            .admit(1, Key::page(1, 7, 0x1000, Access::Read))
            .unwrap();
        let second = broker
            .admit(1, Key::page(2, 8, 0x2000, Access::Read))
            .unwrap();

        assert_eq!(
            broker
                .complete(first.request(), 41, CompletionVisibility::Deferred)
                .unwrap()
                .waiters_released(),
            0
        );
        assert_eq!(
            broker.waiter(first.waiter()).unwrap(),
            WaiterObservation::Pending
        );
        let release = broker.release_where(|request| request.key().generation == 7);
        assert_eq!(release.requests_released(), 1);
        assert_eq!(release.waiters_released(), 1);
        assert_eq!(
            broker.waiter(first.waiter()).unwrap(),
            WaiterObservation::Ready(41)
        );
        assert_eq!(
            broker.waiter(second.waiter()).unwrap(),
            WaiterObservation::Pending
        );

        assert_eq!(
            broker.complete(first.request(), 99, CompletionVisibility::Visible),
            Err(RequestError::AlreadyTerminal)
        );
        assert_eq!(
            broker
                .take_waiter_completion(first.waiter())
                .unwrap()
                .completion(),
            41
        );
    }

    #[test]
    fn deferred_terminal_coalescing_stays_unready_until_one_release() {
        let mut broker = Broker::try_new(1, 2).unwrap();
        let key = Key::page(1, 7, 0x1000, Access::Read);
        let first = broker.admit(1, key).unwrap();
        broker
            .complete(first.request(), 55, CompletionVisibility::Deferred)
            .unwrap();

        let second = broker.admit(1, key).unwrap();
        assert!(second.coalesced());
        assert_eq!(first.request(), second.request());
        assert_eq!(
            broker.waiter(first.waiter()).unwrap(),
            WaiterObservation::Pending
        );
        assert_eq!(
            broker.waiter(second.waiter()).unwrap(),
            WaiterObservation::Pending
        );

        let released = broker.release(first.request()).unwrap();
        assert_eq!(released.waiters_released(), 2);
        assert_eq!(
            broker.waiter(first.waiter()).unwrap(),
            WaiterObservation::Ready(55)
        );
        assert_eq!(
            broker.waiter(second.waiter()).unwrap(),
            WaiterObservation::Ready(55)
        );
        assert_eq!(
            broker.release(first.request()).unwrap().waiters_released(),
            0
        );
    }

    #[test]
    fn range_completion_covers_pending_and_delivered_requests() {
        let mut broker = Broker::try_new(3, 3).unwrap();
        let first = broker
            .admit(1, Key::page(1, 1, 0x1000, Access::Read))
            .unwrap();
        let second = broker
            .admit(1, Key::page(2, 1, 0x2000, Access::Read))
            .unwrap();
        let third = broker
            .admit(1, Key::page(3, 1, 0x9000, Access::Read))
            .unwrap();
        assert_eq!(broker.claim_next(1).unwrap().token(), first.request());

        let summary = broker.complete_range(
            FaultRange::try_new(0x1800, 0x1000).unwrap(),
            -7,
            CompletionVisibility::Visible,
        );
        assert_eq!(summary.requests_completed(), 2);
        assert_eq!(summary.waiters_released(), 2);
        assert_eq!(
            broker.waiter(first.waiter()).unwrap(),
            WaiterObservation::Ready(-7)
        );
        assert_eq!(
            broker.waiter(second.waiter()).unwrap(),
            WaiterObservation::Ready(-7)
        );
        assert_eq!(
            broker.waiter(third.waiter()).unwrap(),
            WaiterObservation::Pending
        );
        assert_eq!(broker.load().pending_requests(), 1);
    }

    #[test]
    fn handler_detach_completes_open_and_releases_deferred_without_overwrite() {
        let mut broker = Broker::try_new(4, 4).unwrap();
        let pending = broker
            .admit(4, Key::page(1, 1, 0x1000, Access::Read))
            .unwrap();
        let delivered = broker
            .admit(4, Key::page(2, 1, 0x2000, Access::Read))
            .unwrap();
        let deferred = broker
            .admit(4, Key::page(3, 1, 0x3000, Access::Read))
            .unwrap();
        let other = broker
            .admit(5, Key::page(4, 1, 0x4000, Access::Read))
            .unwrap();
        assert_eq!(broker.claim_next(4).unwrap().token(), pending.request());
        assert_eq!(broker.claim_next(4).unwrap().token(), delivered.request());
        broker
            .complete(deferred.request(), 123, CompletionVisibility::Deferred)
            .unwrap();

        let summary = broker.detach_handler(4, -19);
        assert_eq!(summary.requests_completed(), 2);
        assert_eq!(summary.requests_released(), 1);
        assert_eq!(summary.waiters_released(), 3);
        assert_eq!(
            broker.waiter(pending.waiter()).unwrap(),
            WaiterObservation::Ready(-19)
        );
        assert_eq!(
            broker.waiter(delivered.waiter()).unwrap(),
            WaiterObservation::Ready(-19)
        );
        assert_eq!(
            broker.waiter(deferred.waiter()).unwrap(),
            WaiterObservation::Ready(123)
        );
        assert_eq!(
            broker.waiter(other.waiter()).unwrap(),
            WaiterObservation::Pending
        );
    }

    #[test]
    fn terminal_visible_request_coalesces_as_immediately_ready() {
        let mut broker = Broker::try_new(1, 2).unwrap();
        let key = Key::page(1, 1, 0, Access::Read);
        let first = broker.admit(1, key).unwrap();
        broker
            .complete(first.request(), 5, CompletionVisibility::Visible)
            .unwrap();
        let second = broker.admit(1, key).unwrap();
        assert!(second.coalesced());
        assert_eq!(
            broker.waiter(second.waiter()).unwrap(),
            WaiterObservation::Ready(5)
        );
    }

    #[test]
    fn stale_request_and_waiter_tokens_cannot_target_reused_slots() {
        let mut broker = Broker::try_new(1, 1).unwrap();
        let first = broker.admit(1, Key::page(1, 1, 0, Access::Read)).unwrap();
        broker.cancel_waiter(first.waiter()).unwrap();
        let second = broker.admit(1, Key::page(2, 1, 0, Access::Read)).unwrap();
        assert_eq!(first.request().slot(), second.request().slot());
        assert_ne!(first.request().generation(), second.request().generation());
        assert_eq!(first.waiter().slot(), second.waiter().slot());
        assert_ne!(first.waiter().generation(), second.waiter().generation());
        assert_eq!(
            broker.complete(first.request(), 0, CompletionVisibility::Visible),
            Err(RequestError::StaleOrForeign)
        );
        assert_eq!(
            broker.cancel_waiter(first.waiter()),
            Err(WaiterError::StaleOrForeign)
        );
    }

    #[test]
    fn tokens_cannot_cross_broker_identity_domains() {
        let mut left = Broker::try_new(1, 1).unwrap();
        let mut right = Broker::try_new(1, 1).unwrap();
        let admission = left.admit(1, Key::page(1, 1, 0, Access::Read)).unwrap();

        assert_ne!(left.id(), right.id());
        assert_eq!(
            right.complete(admission.request(), 0, CompletionVisibility::Visible),
            Err(RequestError::StaleOrForeign)
        );
        assert_eq!(
            right.cancel_waiter(admission.waiter()),
            Err(WaiterError::StaleOrForeign)
        );
    }

    #[test]
    fn request_and_waiter_generations_exhaust_without_wrapping() {
        let key = Key::page(1, 1, 0, Access::Read);
        let mut request_exhausted = Broker::try_new(1, 1).unwrap();
        request_exhausted.requests[0].generation = u64::MAX;
        assert_eq!(
            request_exhausted.admit(1, key),
            Err(AdmissionError::RequestTokenExhausted)
        );

        let mut waiter_exhausted = Broker::try_new(1, 1).unwrap();
        waiter_exhausted.waiters[0].generation = u64::MAX;
        assert_eq!(
            waiter_exhausted.admit(1, key),
            Err(AdmissionError::WaiterTokenExhausted)
        );
        assert_eq!(waiter_exhausted.load().live_requests(), 0);
    }

    #[test]
    fn load_counts_every_phase_and_ready_waiter_exactly() {
        let mut broker = Broker::try_new(4, 5).unwrap();
        let pending = broker
            .admit(1, Key::page(1, 1, 0x1000, Access::Read))
            .unwrap();
        let delivered = broker
            .admit(1, Key::page(2, 1, 0x2000, Access::Read))
            .unwrap();
        let deferred = broker
            .admit(1, Key::page(3, 1, 0x3000, Access::Read))
            .unwrap();
        let visible = broker
            .admit(1, Key::page(4, 1, 0x4000, Access::Read))
            .unwrap();
        let coalesced = broker
            .admit(1, Key::page(4, 1, 0x4000, Access::Read))
            .unwrap();
        assert_eq!(broker.claim_next(1).unwrap().token(), pending.request());
        assert_eq!(broker.claim_next(1).unwrap().token(), delivered.request());
        broker
            .complete(deferred.request(), 1, CompletionVisibility::Deferred)
            .unwrap();
        broker
            .complete(visible.request(), 2, CompletionVisibility::Visible)
            .unwrap();

        let load = broker.load();
        assert_eq!(load.request_capacity(), 4);
        assert_eq!(load.live_requests(), 4);
        assert_eq!(load.pending_requests(), 0);
        assert_eq!(load.delivered_requests(), 2);
        assert_eq!(load.deferred_requests(), 1);
        assert_eq!(load.visible_requests(), 1);
        assert_eq!(load.waiter_capacity(), 5);
        assert_eq!(load.live_waiters(), 5);
        assert_eq!(load.ready_waiters(), 2);
        assert_eq!(
            broker.waiter(coalesced.waiter()).unwrap(),
            WaiterObservation::Ready(2)
        );
    }

    #[test]
    fn hot_operations_never_change_preallocated_slot_storage() {
        let mut broker = Broker::try_new(2, 3).unwrap();
        let request_capacity = broker.requests.capacity();
        let waiter_capacity = broker.waiters.capacity();
        for generation in 1..64 {
            let first = broker
                .admit(1, Key::page(1, generation, 0, Access::Read))
                .unwrap();
            let second = broker
                .admit(1, Key::page(1, generation, 0, Access::Read))
                .unwrap();
            broker.claim_next(1).unwrap();
            broker
                .complete(first.request(), 0, CompletionVisibility::Visible)
                .unwrap();
            broker.take_waiter_completion(first.waiter()).unwrap();
            broker.take_waiter_completion(second.waiter()).unwrap();
        }
        assert_eq!(broker.requests.capacity(), request_capacity);
        assert_eq!(broker.waiters.capacity(), waiter_capacity);
    }

    #[test]
    fn completion_cancel_race_has_one_valid_terminal_owner() {
        for _ in 0..64 {
            let mut broker = Broker::try_new(1, 1).unwrap();
            let admission = broker.admit(1, Key::page(1, 1, 0, Access::Read)).unwrap();
            let broker = Arc::new(Mutex::new(broker));
            let barrier = Arc::new(Barrier::new(3));

            let cancel_broker = Arc::clone(&broker);
            let cancel_barrier = Arc::clone(&barrier);
            let waiter = admission.waiter();
            let cancel = thread::spawn(move || {
                cancel_barrier.wait();
                cancel_broker.lock().unwrap().cancel_waiter(waiter)
            });

            let complete_broker = Arc::clone(&broker);
            let complete_barrier = Arc::clone(&barrier);
            let request = admission.request();
            let complete = thread::spawn(move || {
                complete_barrier.wait();
                complete_broker
                    .lock()
                    .unwrap()
                    .complete(request, 7, CompletionVisibility::Visible)
            });

            barrier.wait();
            let cancel_result = cancel.join().unwrap();
            let complete_result = complete.join().unwrap();
            match (cancel_result, complete_result) {
                (Ok(cancelled), Err(RequestError::StaleOrForeign)) => {
                    assert!(cancelled.request_reclaimed());
                    assert_eq!(cancelled.completion(), None);
                }
                (Ok(cancelled), Ok(effect)) => {
                    assert!(cancelled.request_reclaimed());
                    assert_eq!(cancelled.completion(), Some(&7));
                    assert_eq!(effect.waiters_released(), 1);
                }
                other => panic!("unexpected race outcome: {other:?}"),
            }
            let load = broker.lock().unwrap().load();
            assert_eq!(load.live_requests(), 0);
            assert_eq!(load.live_waiters(), 0);
        }
    }

    #[test]
    fn completion_and_release_predicates_visit_stable_copied_facts() {
        let mut broker = Broker::try_new(4, 4).unwrap();
        let mut admissions = Vec::new();
        for mapping in 1..=4 {
            admissions.push(
                broker
                    .admit(
                        mapping % 2,
                        Key::page(mapping, mapping, mapping * 0x1000, Access::Read),
                    )
                    .unwrap(),
            );
        }
        let completed = broker.complete_where(
            |request| *request.handler() == 0 && request.key().generation >= 2,
            9,
            CompletionVisibility::Deferred,
        );
        assert_eq!(completed.requests_completed(), 2);
        let released = broker.release_where(|request| request.key().mapping == 4);
        assert_eq!(released.requests_released(), 1);
        assert_eq!(
            broker.waiter(admissions[3].waiter()).unwrap(),
            WaiterObservation::Ready(9)
        );
        assert_eq!(
            broker.waiter(admissions[1].waiter()).unwrap(),
            WaiterObservation::Pending
        );
    }
}
