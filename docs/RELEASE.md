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

Routine changes use the same front door as pull-request CI:

```sh
./scripts/ci.sh quality
```

Before publishing, run the explicit release tier:

```sh
./scripts/ci.sh release
```

The release tier reruns quality, denies rustdoc warnings, validates the original
and coordinated unpacked package artifacts, and performs the available publish
dry-runs. Packaging is deliberately not part of every pull request.

Inspect package contents explicitly when preparing an upload:

```sh
cargo +1.85.0 package --locked --list -p thekernel-axcbpf
cargo +1.85.0 package --locked --list -p thekernel-axfault
cargo +1.85.0 package --locked --list -p thekernel-axpmu
cargo +1.85.0 package --locked --list -p thekernel-axtlb
cargo package --locked --list -p thekernel-axsched
cargo package --locked --list -p thekernel-axpoll
cargo +nightly package --locked --list -p thekernel-axtask
```

The original-package unpack test builds `thekernel-axcbpf`,
`thekernel-axfault`, `thekernel-axpmu`, and `thekernel-axtlb` from normalized
archives with Rust 1.85.0, `--locked`, and `--offline`. Registry publish
dry-runs are currently defined only for `thekernel-axcbpf` and
`thekernel-axpmu`; archive validation is not itself publication readiness.
The maintained-fork unpack test builds leaf packages directly
from normalized archives. The first axtask release uses only sibling archives
whose SHA-256 values match its generated release lock. These checks prove that
packaged source builds outside both this workspace and TheKernel's patch table;
they do not claim a registry-only axtask check before both leaf versions exist.

## Publish

1. Run `./scripts/ci.sh release`. A successful dry-run does not publish any
   package.
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
   dependency.
4. For the first maintained-fork release, crates.io cannot resolve
   `thekernel-axtask` until the two sibling `0.1.0` packages exist.
   `scripts/package-unpack.sh` is the pre-publication substitute: it verifies
   the exact sibling archives and tests the unpacked axtask artifact without a
   workspace patch leak.
5. Publish `thekernel-axsched` and `thekernel-axpoll` from the same verified
   commit. Wait until both are visible in the registry index, then run
   `AXTASK_REGISTRY_READY=1 scripts/publish-dry-run.sh` and publish
   `thekernel-axtask` only if that final registry dry-run passes.
6. Publish only after all gates pass for the exact release commit.
7. Create an exact-commit repository tag `v0.1.0`; its release record lists the
   checksum of every package published from that tag.
8. Verify registry checksums and docs.rs builds after publication.

Publishing and pushing tags are deliberate maintainer actions; local release
preparation does not imply authorization to perform either action.
