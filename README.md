# thekernel-ax

`thekernel-ax` is the independent home of reusable operating-system mechanism
crates maintained by TheKernel. The repository starts with two crates:

| Package | Rust crate name | Purpose |
| --- | --- | --- |
| `thekernel-axsched` | `axsched` | FIFO, round-robin, CFS, and Linux-compatible real-time scheduling mechanisms |
| `thekernel-axpoll` | `axpoll` | bounded I/O readiness registration and wakeup primitives |

The package names are new so releases cannot be confused with the upstream
`axsched` and `axpoll` packages. The Rust library names stay unchanged, which
lets downstream code continue to write `use axsched::...` and
`use axpoll::...` after changing only its dependency declaration.

## Scope

This repository owns generic mechanisms that can be used without TheKernel's
Linux ABI personality. Linux syscall numbers, file-descriptor policy, errno
mapping, and Linux `poll(2)` bit translation belong in an ABI adapter outside
these crates.

The sources are maintained forks, not claims of upstream authorship. Each crate
keeps its upstream authors, license expression, immutable registry baseline in
`VENDOR.md`, and a maintained delta in `PATCHES.md`. See
[`docs/PROVENANCE.md`](docs/PROVENANCE.md) for the complete source record.

## Build and test

The workspace is intentionally self-contained and has no root
`[patch.crates-io]` table.

```sh
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/check_registry_dependencies.py
scripts/package-unpack.sh
```

The last command packages each crate, unpacks the resulting registry artifact
in a temporary directory, and tests that artifact without access to TheKernel's
workspace patches.

## Project policy

- [`GOVERNANCE.md`](GOVERNANCE.md) defines scope and decision making.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) defines the contribution bar.
- [`SECURITY.md`](SECURITY.md) defines private vulnerability reporting.
- [`docs/RELEASE.md`](docs/RELEASE.md) is the release checklist.

