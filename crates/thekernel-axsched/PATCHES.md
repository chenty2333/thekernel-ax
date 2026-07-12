# Maintained patch ledger

## Baseline

The immutable baseline is registry package `axsched` `0.3.1`, archive SHA-256
`cad6b7b0b8d9ad1d52a834d8b7721114413da8cf3430af928b1c8651f911287a`.
See `VENDOR.md` before rebasing.

## Inherited TheKernel delta

- `07de702`: runtime CFS class and state configuration.
- `e5ee2f9`: FIFO/RR policy mechanics and priority handling.
- `96df7d9` and `d38fb1b`: cross-runqueue identity, enqueue reasons, and
  lifecycle hardening.
- `909591e`: removal of false `SCHED_DEADLINE` capability from the generic
  scheduler surface.
- Tests for fair/FIFO/RR ordering, preemption, priority, child-task fairness,
  and task migration behavior.

These commit IDs are source-history navigation hints. The authoritative patch
is the tested diff from the verified registry archive.

## Standalone `0.1.0` release delta

- Rename the registry package to `thekernel-axsched` while retaining
  `[lib] name = "axsched"` for Rust imports.
- Move maintained source into the independent `thekernel-ax` workspace.
- Resolve all dependencies from the public registry without TheKernel's root
  patch table.
- Include complete license texts, provenance, release checks, and unpacked
  package tests.
- Remove `reset_on_fork` from scheduler state. Child policy reset is a task
  lifecycle or ABI decision, while the generic fair-child vruntime seeding
  mechanism remains available to callers.
- Expand the generic real-time priority domain from the Linux-shaped `1..=99`
  range to the full nonzero `u8` range. ABI adapters validate narrower public
  ranges themselves.
- Make foreign-scheduler removal safe for FIFO and round-robin queues rather
  than exposing an unchecked linked-list removal through a safe trait method.
- Make every enqueue/remove/requeue operation return typed ownership,
  identifier, sequence, parameter, and configuration failures.
- Add an atomic per-task queue owner to FIFO, RR, and CFS, reject duplicate or
  foreign publication, and release every surviving owner when a scheduler is
  dropped.
- Serialize CFS task configuration against enqueue and provide a scheduler
  transaction for ready-task class/priority changes so live intrusive keys are
  never mutated.
- Replace signed RR time-slice truncation/underflow with saturating `usize`
  accounting; reject a zero slice explicitly and accept `usize::MAX`.
- Rebase CFS fair/front/back sequences allocation-free before exhaustion,
  preserving current ready order instead of wrapping into key collisions or
  permanently rejecting future work.
- Saturate long-running CFS delta/vruntime arithmetic rather than allowing a
  debug-build panic or release-build wrap.
- Test maximum/zero real-time priorities, zero/maximum/expired RR slices,
  rejected-state immutability for a published task, transactional ready-task
  configuration, child vruntime seeding, sequence rebase, scheduler teardown,
  duplicate ownership, and cross-scheduler removal.
