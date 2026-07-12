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
scripts/package-unpack.sh
```

Inspect the contents explicitly:

```sh
cargo package --locked --list -p thekernel-axsched
cargo package --locked --list -p thekernel-axpoll
cargo +nightly-2025-05-20 package --locked --list -p thekernel-axtask
```

The unpack test is a release gate: it proves dependencies resolve from the
registry and that the packaged source builds outside both this workspace and
TheKernel's `[patch.crates-io]` environment.

## Publish

1. Run `cargo publish --locked --dry-run -p <package>`.
2. Publish `thekernel-axsched` and `thekernel-axpoll` before the dependent
   `thekernel-axtask`, all from the same verified commit.
3. Publish only after the dry run and CI pass for the exact release commit.
4. Create an exact-commit repository tag `v0.1.0`; its release record lists all
   three package checksums.
5. Attach release notes that summarize the maintained delta and any public API
   migration.
6. Verify the registry checksum and docs.rs build after publication.

Publishing and pushing tags are deliberate maintainer actions; local release
preparation does not imply authorization to perform either action.
