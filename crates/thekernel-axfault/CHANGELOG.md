# Changelog

## 0.1.0

- Add explicitly bounded, preallocated request and waiter registries.
- Add broker/slot/generation request and waiter tokens that never wrap.
- Coalesce only exact handler/request keys before terminal visibility, never
  reuse an older visible completion for a later same-key fault, and retain
  independent waiter cancellation ownership.
- Add FIFO pending-to-delivered claims that never requeue a request behind
  newer work when the upper layer drops its delivery snapshot.
- Add allocation-free per-handler pending observations derived from live
  request phases instead of fallible adapter-side accounting.
- Add immediate or deferred terminal visibility, predicate/range release,
  handler detach, and final-waiter reclamation.
- Keep deferred-but-unclaimed requests FIFO-readable, distinguish deferred
  pending from deferred delivered state, and expose disjoint exact phase load
  counts alongside orthogonal delivery/completion aggregates.
- Add an optional atomic finite request-credit pool shared across brokers;
  charge only new requests and return ownership on final-waiter reclamation or
  broker drop without adapter-side accounting.
- Add allocation-free iteration over copied live-request snapshots for bounded
  policy and resolver preflight without duplicating exact lookup or mutation
  APIs.
- Add exact load snapshots and deterministic race/state-machine tests.
- Keep the core `no_std`, unsafe-free, allocation-free after construction, and
  independent of tasks, Linux MM/VMA policy, readiness, and errno.
