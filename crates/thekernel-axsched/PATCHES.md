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
- Tests for fair/FIFO/RR ordering, preemption, priority, fork reset, and task
  migration behavior.

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

