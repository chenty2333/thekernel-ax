# Release process

The packages have independent names and version histories even when they are
released from one workspace. The coordinated maintained-fork checkpoint is
`0.1.0` for `thekernel-axsched`, `thekernel-axpoll`, and `thekernel-axtask`.
`thekernel-axcbpf` and `thekernel-axpmu` are independent original mechanisms
with their own `0.1.0` release gates at Rust 1.85.0. User-visible changes for
these checkpoints are recorded in
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
cargo +1.85.0 package --locked --list -p thekernel-axcbpf
cargo +1.85.0 package --locked --list -p thekernel-axpmu
cargo package --locked --list -p thekernel-axsched
cargo package --locked --list -p thekernel-axpoll
cargo +nightly package --locked --list -p thekernel-axtask
```

The original-package unpack test builds `thekernel-axcbpf` and
`thekernel-axpmu` from their normalized archives with Rust 1.85.0, `--locked`,
and `--offline`. These checks cover normalized manifests and packaged-source
boundaries rather than only the workspace source. The maintained-fork unpack
test builds leaf packages directly from their normalized archives, while the
first axtask release uses only the two sibling archives
whose SHA-256 values match its generated release lock. These gates prove
packaged source builds outside both this workspace and TheKernel's patch table,
but the latter is not described as a registry-only axtask check before those
two leaf versions exist.

## Publish

1. Run `scripts/publish-dry-run.sh`. It first performs real crates.io publish
   dry-runs for `thekernel-axcbpf` and `thekernel-axpmu` with Rust 1.85.0, then
   performs the two coordinated leaf-package dry-runs with the rolling
   `nightly` toolchain.
   A successful dry-run does not publish either original package.
2. Publish `thekernel-axcbpf` only from the exact commit whose package, offline
   unpack, provenance, CI, and publish dry-run gates passed:

   ```sh
   cargo +1.85.0 publish --locked --registry crates-io -p thekernel-axcbpf
   ```

   Wait for both exact-version checks to succeed:

   ```sh
   cargo +1.85.0 info thekernel-axcbpf@0.1.0 --registry crates-io
   curl --location --fail --silent --show-error \
     https://docs.rs/thekernel-axcbpf/0.1.0/axcbpf/
   ```

   Only then publish the downstream TheKernel Linux-ABI seccomp adapter; a
   workspace path or patch is not a substitute for this dependency boundary.
3. Publish `thekernel-axpmu` independently from the same exact verified release
   commit:

   ```sh
   cargo +1.85.0 publish --locked --registry crates-io -p thekernel-axpmu
   ```

   Wait for its exact registry version and docs.rs build before claiming that
   the package was released or switching a downstream package to the released
   dependency:

   ```sh
   cargo +1.85.0 info thekernel-axpmu@0.1.0 --registry crates-io
   curl --location --fail --silent --show-error \
     https://docs.rs/thekernel-axpmu/0.1.0/axpmu/
   ```

4. For the first maintained-fork release, crates.io cannot resolve
   `thekernel-axtask` until the two sibling `0.1.0` packages exist. Before that
   point,
   `scripts/package-unpack.sh` is the checksum-bound substitute: it verifies the
   exact sibling archives and tests the unpacked axtask artifact without a
   workspace patch leak. This limitation is reported explicitly rather than
   calling the dependent dry-run successful.
5. Publish `thekernel-axsched` and `thekernel-axpoll` from the same verified
   commit, wait until both are visible in the registry index, then run
   `AXTASK_REGISTRY_READY=1 scripts/publish-dry-run.sh` and publish
   `thekernel-axtask` only if that final real dry-run passes.
6. Publish only after the dry run and CI pass for the exact release commit.
7. Create an exact-commit repository tag `v0.1.0`; its release record lists the
   checksum of every package published from that tag.
8. Attach release notes that summarize the maintained delta and any public API
   migration.
9. Verify the registry checksum and docs.rs build after publication. Registry
   visibility, docs.rs availability, and publication claims are recorded per
   exact package version; success for one original package does not imply
   success for the other.

Publishing and pushing tags are deliberate maintainer actions; local release
preparation does not imply authorization to perform either action.
