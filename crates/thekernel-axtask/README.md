# thekernel-axtask

`thekernel-axtask` is the maintained generic task/run-queue mechanism used by
TheKernel. The registry package name is independent while the Rust library name
remains `axtask`:

```toml
[dependencies]
axtask = { package = "thekernel-axtask", version = "0.1.0", features = [
    "multitask",
    "sched-cfs",
] }
```

The crate owns task objects, per-CPU run queues, scheduling handoff, wait
queues, interrupt/timer wake mechanisms, and bounded deferred reclamation. It
does not own Linux PID allocation, credential/process policy, scheduling ABI
numbers, errno translation, or benchmark profiles.

Version 0.1.0 is tested with `nightly-2025-05-20` and does not claim a stable
`rust-version`, because fallible standard `Arc` allocation still uses
`allocator_api`.

The release contract requires explicit task/runqueue ownership, no aliased
mutable runqueue references, no allocation/drop/wake callback inside IRQ-safe
critical sections, monotonic mechanism-only task identities, an intrusive
allocation-free exited-task FIFO, finite cancellable timer/IRQ registrations,
creation-CPU timer ownership, bounded remote handoff, lost-wake-safe blocking,
and typed scheduler/lifecycle failures. IRQ waiter registration is deliberately
separate from driver-owned source validation, enable/disable, masking, and
acknowledgement. See `PATCHES.md` and `VENDOR.md` for the maintained delta and
source identity.
