# Release process

The packages have independent names and version histories even when they are
released from one workspace. The first release checkpoint is `0.1.0` for all three
packages. User-visible changes for that checkpoint are recorded in
[`releases/0.1.0.md`](releases/0.1.0.md).

## Prepare

1. Confirm the worktree contains only intended changes.
2. Review `VENDOR.md`, `PATCHES.md`, and `docs/PROVENANCE.md` for the affected
   crate.
3. Set the package version and update user-facing documentation.
4. Generate and commit `Cargo.lock` with `cargo generate-lockfile`.
5. Confirm the root manifest has no `[patch]` or `[replace]` table.

## Verify

Run with the repository's pinned MSRV and again with stable:

```sh
cargo fmt --all -- --check
python3 scripts/check_registry_dependencies.py
scripts/ci.sh
scripts/publish-dry-run.sh
scripts/package-unpack.sh
```

Inspect the contents explicitly:

```sh
cargo package --locked --list -p thekernel-axsched
cargo package --locked --list -p thekernel-axpoll
cargo +nightly-2025-05-20 package --locked --list -p thekernel-axtask
```

The unpack test is a release gate: leaf packages build directly from their
normalized archives, while the first axtask release uses only the two sibling
archives whose SHA-256 values match its generated release lock. It therefore
proves packaged source builds outside both this workspace and TheKernel's patch
table, but it is not described as a registry-only axtask check before those two
leaf versions exist.

## Publish

1. Run `scripts/publish-dry-run.sh`. It performs real Cargo publish dry-runs for
   the two leaf packages.
2. For the first release, crates.io cannot resolve `thekernel-axtask` until the
   two sibling `0.1.0` packages exist. Before that point,
   `scripts/package-unpack.sh` is the checksum-bound substitute: it verifies the
   exact sibling archives and tests the unpacked axtask artifact without a
   workspace patch leak. This limitation is reported explicitly rather than
   calling the dependent dry-run successful.
3. Publish `thekernel-axsched` and `thekernel-axpoll` from the same verified
   commit, wait until both are visible in the registry index, then run
   `AXTASK_REGISTRY_READY=1 scripts/publish-dry-run.sh` and publish
   `thekernel-axtask` only if that final real dry-run passes.
4. Publish only after the dry run and CI pass for the exact release commit.
5. Create an exact-commit repository tag `v0.1.0`; its release record lists all
   three package checksums.
6. Attach release notes that summarize the maintained delta and any public API
   migration.
7. Verify the registry checksum and docs.rs build after publication.

Publishing and pushing tags are deliberate maintainer actions; local release
preparation does not imply authorization to perform either action.
