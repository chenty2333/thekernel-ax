# thekernel-ax

`thekernel-ax` is the independent home of reusable operating-system mechanism
crates maintained by TheKernel. The workspace contains these crates:

| Package | Rust crate name | Purpose |
| --- | --- | --- |
| `thekernel-axfault` | `axfault` | bounded generation-safe fault-request broker state |
| `thekernel-axsched` | `axsched` | FIFO, round-robin, fair, and real-time scheduling mechanisms |
| `thekernel-axpoll` | `axpoll` | bounded I/O readiness registration and wakeup primitives |
| `thekernel-axtask` | `axtask` | bounded task, run-queue, wait, timer, and IRQ-wake mechanisms |

The maintained-fork package names are new so releases cannot be confused with
the upstream `axsched`, `axpoll`, and `axtask` packages. Their Rust library
names stay unchanged, which lets downstream code continue to use the
established crate paths after changing only its dependency declaration.
`thekernel-axfault` is a new TheKernel-owned mechanism rather than a renamed
upstream package.

## Scope

This repository owns generic mechanisms that can be used without TheKernel's
Linux ABI personality. Linux syscall numbers, file-descriptor policy, errno
mapping, and Linux `poll(2)` bit translation belong in an ABI adapter outside
these crates.

The extracted scheduler, readiness, and task sources are maintained forks, not
claims of upstream authorship. Each of those crates keeps its upstream authors,
license expression, immutable registry baseline in `VENDOR.md`, and a
maintained delta in `PATCHES.md`. The fault broker is original Apache-2.0 code
with no vendored baseline. See [`docs/PROVENANCE.md`](docs/PROVENANCE.md) for
the complete source record.

## Build and test

The workspace is intentionally self-contained and has no root
`[patch.crates-io]` table.

```sh
scripts/test-axsched-msrv.sh
cargo +1.85.0 test -p thekernel-axfault --all-targets --locked
cargo +1.85.0 test -p thekernel-axfault --doc --locked
cargo +1.85.0 package -p thekernel-axfault --locked
cargo +1.85.0 test -p thekernel-axpoll --all-targets --locked
cargo +nightly-2025-05-20 test -p thekernel-axtask --all-targets --locked \
  --features "multitask irq preempt smp sched-cfs task-ext"
python3 scripts/check_registry_dependencies.py
scripts/publish-dry-run.sh
scripts/package-unpack.sh
```

The direct package command validates the original fault-broker artifact. The
last command packages the coordinated three-crate maintained-fork release set,
unpacks the registry artifacts in a temporary directory, and tests them without
access to TheKernel's workspace patches.

## Project policy

- [`GOVERNANCE.md`](GOVERNANCE.md) defines scope and decision making.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) defines the contribution bar.
- [`SECURITY.md`](SECURITY.md) defines private vulnerability reporting.
- [`docs/RELEASE.md`](docs/RELEASE.md) is the release checklist.
