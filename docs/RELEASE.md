# Release process

The packages have independent names and version histories even when they are
released from one workspace. The coordinated maintained-fork checkpoint is
`0.1.0` for `thekernel-axsched`, `thekernel-axpoll`, and `thekernel-axtask`.
`thekernel-axcbpf` is an independent original mechanism with its own `0.1.0`
release gate at Rust 1.85.0. User-visible changes for these checkpoints are
recorded in [`releases/0.1.0.md`](releases/0.1.0.md).

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
cargo +1.85.0 package --locked --list -p thekernel-axcbpf
cargo package --locked --list -p thekernel-axsched
cargo package --locked --list -p thekernel-axpoll
cargo +nightly-2025-05-20 package --locked --list -p thekernel-axtask
```

The original-package unpack test builds `thekernel-axcbpf` from its normalized
archive with Rust 1.85.0, `--locked`, and `--offline`. The maintained-fork
unpack test builds leaf packages directly from their normalized archives,
while the first axtask release uses only the two sibling archives whose SHA-256
values match its generated release lock. These gates prove packaged source
builds outside both this workspace and TheKernel's patch table, but the latter
is not described as a registry-only axtask check before those two leaf versions
exist.

## Publish

1. Run `scripts/publish-dry-run.sh`. It first performs a real crates.io publish
   dry-run for `thekernel-axcbpf` with Rust 1.85.0, then performs the two
   coordinated leaf-package dry-runs with the pinned nightly.
2. Publish `thekernel-axcbpf` only from the exact commit whose package, offline
   unpack, provenance, CI, and publish dry-run gates passed. Wait until that
   version is visible in the registry index before publishing the downstream
   TheKernel Linux-ABI seccomp adapter; a workspace path or patch is not a
   substitute for this dependency boundary.
3. For the first maintained-fork release, crates.io cannot resolve
   `thekernel-axtask` until the two sibling `0.1.0` packages exist. Before that
   point,
   `scripts/package-unpack.sh` is the checksum-bound substitute: it verifies the
   exact sibling archives and tests the unpacked axtask artifact without a
   workspace patch leak. This limitation is reported explicitly rather than
   calling the dependent dry-run successful.
4. Publish `thekernel-axsched` and `thekernel-axpoll` from the same verified
   commit, wait until both are visible in the registry index, then run
   `AXTASK_REGISTRY_READY=1 scripts/publish-dry-run.sh` and publish
   `thekernel-axtask` only if that final real dry-run passes.
5. Publish only after the dry run and CI pass for the exact release commit.
6. Create an exact-commit repository tag `v0.1.0`; its release record lists the
   checksum of every package published from that tag.
7. Attach release notes that summarize the maintained delta and any public API
   migration.
8. Verify the registry checksum and docs.rs build after publication.

Publishing and pushing tags are deliberate maintainer actions; local release
preparation does not imply authorization to perform either action.
