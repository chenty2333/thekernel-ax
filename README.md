# thekernel-ax

`thekernel-ax` is the independent home of reusable operating-system mechanism
crates maintained by TheKernel. The workspace contains eight crates:

| Package | Rust crate name | Purpose |
| --- | --- | --- |
| `thekernel-axcbpf` | `axcbpf` | verified classic-BPF mechanism with bounded execution |
| `thekernel-axfault` | `axfault` | bounded generation-safe fault-request broker state |
| `thekernel-axrcu` | `axrcu` | bounded epoch publication and task-context reclamation primitives |
| `thekernel-axpmu` | `axpmu` | bounded opt-in PMU sessions and software diagnostics |
| `thekernel-axtlb` | `axtlb` | allocation-free SMP TLB and instruction-sync shootdown state |
| `thekernel-axsched` | `axsched` | FIFO, round-robin, EEVDF, and real-time scheduling mechanisms |
| `thekernel-axpoll` | `axpoll` | bounded I/O readiness registration and wakeup primitives |
| `thekernel-axtask` | `axtask` | bounded task, run-queue, wait, timer, and IRQ-wake mechanisms |

The maintained-fork package names are new so releases cannot be confused with
the upstream `axsched`, `axpoll`, and `axtask` packages. Their Rust library
names stay unchanged, which lets downstream code keep established crate paths
after changing only dependency declarations. The remaining packages are
TheKernel-owned mechanisms rather than renamed upstream packages.

## Scope

This repository owns generic mechanisms that can be used without TheKernel's
Linux ABI personality. Linux syscall numbers, file-descriptor policy, errno
mapping, and Linux `poll(2)` bit translation belong in an ABI adapter outside
these crates.

The extracted scheduler, readiness, and task sources are maintained forks, not
claims of upstream authorship. Each keeps its upstream authors, license
expression, immutable registry baseline in `VENDOR.md`, and maintained delta in
`PATCHES.md`. See [`docs/PROVENANCE.md`](docs/PROVENANCE.md) for the complete
source record.

## Build and test

The workspace is intentionally self-contained and has no root
`[patch.crates-io]` table. The pull-request front door is:

```sh
./scripts/ci.sh quality
```

That gate runs formatting, registry-dependency and provenance checks, the
unpacked Rust-1.76 axsched MSRV test, Rust-1.85 tests and Clippy for the stable
mechanism crates, and the nightly axtask test/Clippy/feature matrix. It includes
`thekernel-axrcu`, which was previously a workspace member but absent from both
the workflow and the old hand-written test list.

Release-only rustdoc, archive, and publish-dry-run checks are separate:

```sh
./scripts/ci.sh release
```

`thekernel-axrcu` is quality-tested but is not yet included in the package
release helpers because it does not yet carry the release documentation and
provenance asset set required by those helpers. Do not infer release readiness
from workspace membership alone.

The self-contained registry matrix type-checks and unit-tests `irq-exit` with a
test-only transport provider. A production consumer must inject its Layer 0
provider and prove the final link; TheKernel does so through its coordinated
`axruntime`/`axhal` integration.

The original-package unpack check validates the currently supported original
mechanism artifacts. The coordinated unpack check packages the three maintained
fork crates, unpacks their registry artifacts in a temporary directory, and
tests them without access to TheKernel workspace patches.

A publish dry-run never authorizes an upload. See
[`docs/RELEASE.md`](docs/RELEASE.md) for the dependency order and exact release
requirements.

## Project policy

- [`GOVERNANCE.md`](GOVERNANCE.md) defines scope and decision making.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) defines the contribution bar.
- [`SECURITY.md`](SECURITY.md) defines private vulnerability reporting.
- [`docs/RELEASE.md`](docs/RELEASE.md) is the release checklist.
