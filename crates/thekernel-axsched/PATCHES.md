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
- Test maximum/zero real-time priorities, rejected-state immutability for a
  published task, child vruntime seeding, and cross-scheduler removal.
