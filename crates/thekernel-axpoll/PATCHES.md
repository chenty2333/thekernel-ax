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
- Replace Linux-named event flags and `linux-raw-sys` constants with generic
  `IoEvents` names and crate-owned bit values. Linux `POLL*` translation is a
  downstream ABI-adapter responsibility.
- Make `PollSet` capacity a const generic (default 64) and return an explicit
  `RegisterError::Full` instead of overwriting and waking an existing waiter.
- Return opaque registry/slot/generation tokens, with independent registration,
  waker update, cancellation, foreign-token rejection, and stale-generation
  rejection defined explicitly.
- Remove inherited `Waker::will_wake` registration deduplication: equivalent
  wakers can represent independent waits or interests, so every `register`
  call owns a distinct token and slot. Re-polling one logical wait uses
  `update`.
- Remove the inherited `Pollable` trait rather than returning one token for a
  potentially multi-source wait. Bounded fan-in, rollback, interest changes,
  group update, and group cancellation remain an explicit downstream contract
  until a generic source/group API is proven against real consumers.
- Separate one-shot `wake()` from terminal, idempotent `close()`; dropping a set
  closes it and wakes live registrations.
- Preserve short-lock linearization while moving Waker clone, replacement,
  rejection, cancellation drop, drain drop, and wake callbacks outside the
  IRQ-safe lock.
- Enable `kspin/smp` explicitly. The standalone package must provide a real
  cross-core lock without relying on TheKernel's former feature unification.
- Cover bounded capacity, ABA slot reuse, close/drop, cancel/wake races,
  re-entrant Wake and final Waker destruction, and token-owning async futures.
