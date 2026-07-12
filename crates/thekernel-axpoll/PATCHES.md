# Maintained patch ledger

## Baseline

The immutable baseline is registry package `axpoll` `0.1.2`, archive SHA-256
`36b92f85c6903350f5146216ccb7d7a7e7b4dbd6f5927a1279db03ba52a53ae7`.
See `VENDOR.md` before rebasing.

## Inherited TheKernel delta

- `d38fb1b`: replace an allocation-growing waker vector with a fixed 64-entry
  registry, suppress equivalent duplicate wakers, and move RawWaker
  clone/drop/wake work outside the IRQ-safe lock.

The commit ID is a source-history navigation hint. The authoritative patch is
the tested diff from the verified registry archive.

## Standalone `0.1.0` release delta

- Rename the registry package to `thekernel-axpoll` while retaining
  `[lib] name = "axpoll"` for Rust imports.
- Move maintained source into the independent `thekernel-ax` workspace.
- Resolve all dependencies from the public registry without TheKernel's root
  patch table.
- Include complete license texts, provenance, release checks, and unpacked
  package tests.

The registration lifecycle and event namespace are intentionally recorded in a
separate patch checkpoint after the standalone repository skeleton.

