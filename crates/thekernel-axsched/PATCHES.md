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
- Make every enqueue/remove/requeue, task configuration, fair-child seed, and
  priority update return typed unsupported, ownership, class, identifier,
  sequence, parameter, and configuration failures.
- Add an atomic per-task queue owner to FIFO, RR, and CFS, reject duplicate or
  foreign publication, and release every surviving owner when a scheduler is
  dropped.
- Serialize CFS task configuration against enqueue and provide a scheduler
  transaction for ready-task class/priority changes so live intrusive keys are
  never mutated.
- Pack class, nice value, and real-time priority into one atomic publication
  word so safe concurrent readers never observe a new class with stale
  class-specific parameters.
- Acquire one RAII queue-owner claim before any CFS enqueue reads scheduling
  parameters or mutates vruntime, fair-child seed, or RR-slice state. Duplicate,
  foreign, and exhausted admissions release the claim without side effects.
- Make ready-task reconfiguration rollback allocation-free and infallible by
  restoring the saved parameter/auxiliary snapshot and original intrusive key
  directly instead of requesting a second ordering sequence.
- Add unpublished new-task reservations with explicit cancellation and a safe
  commit API whose typed error retains the complete token after a wrong-owner
  attempt.
- Replace signed RR time-slice truncation/underflow with saturating `usize`
  accounting; reject a zero slice explicitly and accept `usize::MAX`.
- Never wrap, rebase, or reuse CFS fair/front/back ordering sequences. Return
  `SequenceExhausted` without mutation at the boundary while keeping every
  previously issued reservation ticket unique and committable.
- Saturate long-running CFS delta/vruntime arithmetic rather than allowing a
  debug-build panic or release-build wrap.
- Test maximum/zero real-time priorities, zero/maximum/expired RR slices,
  duplicate/foreign/exhausted enqueue immutability, exact ready-task rollback,
  child vruntime seeding, reservation cancellation/ownership/exhaustion,
  scheduler teardown, duplicate ownership, and cross-scheduler removal.
