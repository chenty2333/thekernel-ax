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
- Add nonzero RR admission, saturating tick accounting, and maximum-width time
  slices.
- Add allocation-free sequence rebase before fair/real-time ordering
  exhaustion.
- Add cross-scheduler, teardown, overflow, lifecycle, class, priority, and
  ordering tests for `no_std` consumers.
