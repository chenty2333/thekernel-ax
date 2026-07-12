# Release process

The packages have independent names and version histories even when they are
released from one workspace. The first release checkpoint is `0.1.0` for both
packages.

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
cargo fmt --all --check
python3 scripts/check_registry_dependencies.py
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
scripts/package-unpack.sh
```

Inspect the contents explicitly:

```sh
cargo package --locked --list -p thekernel-axsched
cargo package --locked --list -p thekernel-axpoll
```

The unpack test is a release gate: it proves dependencies resolve from the
registry and that the packaged source builds outside both this workspace and
TheKernel's `[patch.crates-io]` environment.

## Publish

1. Run `cargo publish --locked --dry-run -p <package>`.
2. Publish only after the dry run and CI pass for the exact release commit.
3. Create a package-specific tag such as `thekernel-axsched-v0.1.0` or
   `thekernel-axpoll-v0.1.0`.
4. Attach release notes that summarize the maintained delta and any public API
   migration.
5. Verify the registry checksum and docs.rs build after publication.

Publishing and pushing tags are deliberate maintainer actions; local release
preparation does not imply authorization to perform either action.

