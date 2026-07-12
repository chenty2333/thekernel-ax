# Vendored source record: `axpoll`

## Immutable published baseline

- Registry package: `axpoll` `0.1.2`
- crates.io archive: `axpoll-0.1.2.crate`
- crates.io archive SHA-256: `36b92f85c6903350f5146216ccb7d7a7e7b4dbd6f5927a1279db03ba52a53ae7`
- Archive URL: <https://static.crates.io/crates/axpoll/axpoll-0.1.2.crate>
- Repository declared by the package: <https://github.com/Starry-OS/axpoll>
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does
  not prove a tag name.
- Cargo records source commit `86f20f6bc1b470fc21894721e72b721f49aa20b7`
  with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256
  `3debbfb6c8878ea36d06d7e026e04e2828c73e58969992e4a4422852fb21f019`).
- Cargo source record: `.cargo_vcs_info.json`.

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the potentially different published
tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the
published archive contains no license text. This distribution adds exact
license texts from ArceOS commit
`06f5953e6c2df6a316959972b6b78db78a0db5b6`; checksums are recorded in the
repository's `docs/PROVENANCE.md`.

## Upstream tests

All published test paths are present and adapted to the maintained fork:
`tests/async.rs` and `tests/tests.rs`. Immutable originals remain recoverable
from the verified archive.

## Maintained fork

TheKernel replaced allocation-growing registration with bounded storage and
moved waker clone/drop/wake behavior outside the IRQ-safe lock. The standalone
fork further makes capacity, cancellation, stale tokens, closure, and the
generic event boundary explicit. It also enables the registry dependency's SMP
lock implementation directly; correctness does not rely on feature unification
from TheKernel. `PATCHES.md` is the release ledger.

## Rebase rule

Start from the verified registry archive, retain this original manifest, Cargo
VCS record, authors, license expression, and upstream test inventory, then
reapply and test every item in `PATCHES.md`. Do not infer API completeness from
the package name or silently drop a patch because a later upstream tree looks
similar.
