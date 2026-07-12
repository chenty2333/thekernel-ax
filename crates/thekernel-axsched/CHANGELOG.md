# Changelog

## 0.1.0

- Establish the independently named maintained `axsched` fork with immutable
  upstream registry provenance and complete license texts.
- Separate generic FIFO, RR, fair, and fixed-priority mechanisms from Linux
  scheduling policy.
- Add typed enqueue/remove/requeue failures and atomic per-task queue ownership.
- Make priority updates, task configuration, and fair-child vruntime seeding
  return typed unsupported/invalid/class/ownership failures.
- Add transactional ready-task reconfiguration and stable intrusive-tree keys.
- Publish class, nice value, and real-time priority as one atomic snapshot so
  concurrent readers cannot observe a torn class-specific configuration.
- Claim CFS queue ownership before reading parameters or changing vruntime,
  child-seed, or RR-slice state; rejected enqueue is side-effect free.
- Restore the exact prior task state and ready key allocation-free when a
  ready-task reconfiguration cannot obtain target ordering admission.
- Add cancellable new-task reservations and a safe, typed, owner-retaining
  commit failure for wrong-scheduler or inconsistent-owner attempts.
- Add nonzero RR admission, saturating tick accounting, and maximum-width time
  slices.
- Keep fair and real-time ordering sequences monotonic and non-reusable; report
  exhaustion explicitly while preserving already-issued reservation tickets.
- Add cross-scheduler, teardown, overflow, lifecycle, class, priority, and
  ordering tests for `no_std` consumers.
