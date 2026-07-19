# thekernel-axfault

`thekernel-axfault` is a `no_std`, unsafe-free, bounded state core for fault
request brokers. It owns request identity, exact-request coalescing, FIFO
delivery, waiter cancellation, terminal visibility, and handler teardown. It
does not know about Linux VMAs, `userfaultfd` flags, errno, page tables, the
current task, or how a consumer sleeps and wakes tasks.

## Boundary

The adapter supplies three small `Copy` values:

- an exact request key containing every identity fact needed to reject a stale
  reply, normally address-space identity, mapping identity and generation,
  range, access, and mode;
- a generation-scoped handler identity;
- a terminal result meaningful to that adapter.

The broker never interprets those values. Linux `userfaultfd` registration,
event encoding, copyout, resolution commands, errno, VMA locking, and actual
task wakeup belong above this crate. A handler owner must stop new admission
before calling `detach_handler`; generation-scoped handler identities prevent a
later handler from being confused with the detached one.

## Resource and state contract

`FaultBroker::try_new(requests, waiters)` allocates and initializes the complete
request and waiter slot arrays. `usize::MAX` is rejected as an accidental
unbounded setting. Broker-owned storage never allocates after construction and
cannot grow either array. Adapter-defined equality and predicate code executes
inline; consumers that need a hard nonblocking hot path must keep that code
allocation-free as well. Request and waiter tokens contain the broker identity,
slot, and a monotonically increasing generation; an exhausted generation is
reported instead of wrapping or reusing an old identity.

One admission always creates one independently cancellable waiter. An exact
`(handler, key)` match coalesces onto the existing request; a different mapping
generation or access mode must therefore be represented in the key and cannot
coalesce accidentally. Cancelling or consuming the final waiter immediately
reclaims its request, including a pending or delivered request.

New requests enter one FIFO pending list. `claim_next(handler)` removes the
oldest matching request and leaves it in `Delivered`. A deferred result does
not implicitly consume an unclaimed request: it remains `DeferredPending` in
the same FIFO position and can still be claimed, becoming
`DeferredDelivered`. Dropping the returned snapshot does not requeue the
request or append it behind newer work. Whether a failed upper-layer delivery
is retried is deliberately outside this crate; the generic broker preserves
the claimed phase until release, visible completion, or final-waiter
cancellation.

`pending_count(handler)` and `has_pending(handler)` scan the fixed request-slot
array and derive readiness from the authoritative `Pending` and
`DeferredPending` phases. They do not cache an adapter-side count.
Consequently, visible completion before claim, range release, final-waiter
cancellation, handler detach, and generation-safe slot reuse cannot leave
phantom pending work. The scan allocates no storage and is bounded by the
request capacity selected at construction.

Completion may be immediately `Visible` or `Deferred`. Deferred completion is
the generic mechanism for separating immutable-result installation from waiter
publication: the terminal result is fixed and waiter visibility is held back,
while FIFO delivery ownership remains independently claimable.
`release`, `release_where`, or `release_range` publishes the result and removes
any still-pending entry. Predicate and range completion cover open pending and
delivered requests. Handler detach completes open requests and releases both
deferred-pending and deferred-delivered results without overwriting them.

`BrokerLoad` exposes five disjoint phase counts whose sum is exactly
`live_requests`: open pending, open delivered, deferred pending, deferred
delivered, and visible. Its aggregate `pending_requests`, `delivered_requests`,
and `deferred_requests` observations are intentionally orthogonal delivery and
completion dimensions; a deferred-pending request appears in both the pending
and deferred aggregates, but only once in the exact phase counts and
`live_requests`.

The type is deliberately externally serialized. A kernel may place it inside
the lock appropriate to its address-space/handler ownership. Keeping locks,
Wakers, wake callbacks, and task blocking out of this crate prevents a broker
transition from sleeping or publishing readiness while state is being mutated.
Transition summaries tell an adapter exactly how many waiters became visible
so it can publish readiness after releasing its state lock.

## Example

```rust
use axfault::{CompletionVisibility, FaultBroker};

let mut broker = FaultBroker::<u64, u64, i32>::try_new(8, 16).unwrap();
let admission = broker.admit(7, 0x1000).unwrap();
let delivered = broker.claim_next(7).unwrap();
assert_eq!(delivered.token(), admission.request());

broker
    .complete(delivered.token(), 0, CompletionVisibility::Visible)
    .unwrap();
assert_eq!(
    broker
        .take_waiter_completion(admission.waiter())
        .unwrap()
        .completion(),
    0
);
```

See `CHANGELOG.md` for the public 0.1 contract.
