# Release process

The packages have independent names and version histories even when they are
released from one workspace. The coordinated maintained-fork checkpoint is
`0.1.0` for `thekernel-axsched`, `thekernel-axpoll`, and `thekernel-axtask`.
`thekernel-axcbpf` and `thekernel-axpmu` are independent original mechanisms
with their own `0.1.0` release checks at Rust 1.85.0. User-visible changes for
these checkpoints are recorded in
[`releases/0.1.0.md`](releases/0.1.0.md).

## Prepare

1. Confirm the worktree contains only intended changes.
2. Review `VENDOR.md`, `PATCHES.md`, and `docs/PROVENANCE.md` for the affected
   crate.
3. Set the package version and update user-facing documentation.
4. Generate and commit `Cargo.lock` with `cargo generate-lockfile`.
5. Confirm the root manifest has no `[patch]` or `[replace]` table.
6. Require all three ordinary CI jobs to pass for the exact release commit.

## Verify

The manual `Release Check` workflow performs only release work: rustdoc,
package-unpack validation, and publish dry-runs. It does not recursively invoke
the pull-request checks. The same operations can be run locally:

```sh
# Documentation
RUSTDOCFLAGS='-D warnings' cargo +1.85.0 doc -p thekernel-axcbpf --no-deps --locked
RUSTDOCFLAGS='-D warnings' cargo +nightly doc -p thekernel-axtask --no-deps --locked \
  --features 'multitask irq preempt smp sched-eevdf task-ext irq-continuation-diagnostics irq-exit'

# Package and registry simulations
CARGO_TOOLCHAIN=1.85.0 scripts/package-unpack-original.sh
AXCBPF_CARGO_TOOLCHAIN=1.85.0 CARGO_TOOLCHAIN=nightly \
  scripts/publish-dry-run.sh
CARGO_TOOLCHAIN=nightly scripts/package-unpack.sh
```

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
The maintained-fork unpack test builds leaf packages directly from normalized
archives. The first axtask release uses only sibling archives whose SHA-256
values match its generated release lock.

## Publish

1. Publish `thekernel-axcbpf` only from the exact commit whose package, offline
   unpack, provenance, CI, and publish dry-run checks passed:

   ```sh
   cargo +1.85.0 publish --locked --registry crates-io -p thekernel-axcbpf
   ```

   Wait for the exact registry version and docs.rs build before releasing a
   downstream package.
2. Publish `thekernel-axpmu` independently from the same exact verified release
   commit and wait for its exact registry version and docs.rs build.
3. For the first maintained-fork release, publish `thekernel-axsched` and
   `thekernel-axpoll` first. Once both versions are visible, run
   `AXTASK_REGISTRY_READY=1 scripts/publish-dry-run.sh` and publish
   `thekernel-axtask` only if that registry-only dry-run passes.
4. Publish only after all checks pass for the exact release commit.
5. Create an exact-commit repository tag `v0.1.0`; its release record lists the
   checksum of every package published from that tag.
6. Verify registry checksums and docs.rs builds after publication.

Publishing and pushing tags are deliberate maintainer actions; local release
preparation does not imply authorization to perform either action.
