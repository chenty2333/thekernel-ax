# Vendored source record: `axsched`

## Immutable published baseline

- Registry package: `axsched` `0.3.1`
- crates.io archive: `axsched-0.3.1.crate`
- crates.io archive SHA-256: `cad6b7b0b8d9ad1d52a834d8b7721114413da8cf3430af928b1c8651f911287a`
- Archive URL: <https://static.crates.io/crates/axsched/axsched-0.3.1.crate>
- Repository declared by the package: <https://github.com/arceos-org/axsched>
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does
  not prove a tag name.
- Cargo records source commit `4d86c55dce4c87dde52792515ce188081323ac07`
  with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256
  `374d6e997e4cf9db00d57c89af6c9b8b6cd3c8f31af27b5d3b95f23ad9a0ca89`).
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

The published archive contains no `tests/` files. Local/unit coverage is part
of the maintained patch.

## Maintained fork

TheKernel added fair/FIFO/RR policy mechanics and tests, cross-runqueue and
lifecycle hardening, and honest rejection of unsupported deadline scheduling.
The standalone fork additionally removes process-fork policy from scheduler
state, uses a generic nonzero `u8` real-time priority domain, makes queue
ownership explicit for every implementation, serializes ready-task
configuration, uses saturating RR/vruntime arithmetic, and rebases ordering
sequences before exhaustion. `PATCHES.md` is the release ledger for that delta.

## Rebase rule

Start from the verified registry archive, retain this original manifest, Cargo
VCS record, authors, license expression, and upstream test inventory, then
reapply and test every item in `PATCHES.md`. Do not infer API completeness from
the package name or silently drop a patch because a later upstream tree looks
similar.
