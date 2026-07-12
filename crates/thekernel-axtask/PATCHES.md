# Maintained patch ledger

## Baseline

The immutable baseline is registry package `axtask` `0.3.0-preview.2`, archive
SHA-256
`bc45120776afddf28b19bb7aba87e379c5779cf28a8f7884943a4821caeec774`.

## Inherited TheKernel delta

- Runtime scheduler class/state plumbing and child-fairness mechanisms.
- Timer deadline integration and wait-queue interruption.
- Bounded per-CPU task/stack recycling and bounded exited-task reclamation.
- SMP affinity/migration handoff and lifecycle hardening.
- Honest removal of unsupported deadline scheduling policy.
- Join registration recheck so exit cannot be lost between state observation
  and waker publication.

## `thekernel-axtask 0.1.0`

- Rename the package while retaining the `axtask` library name and original
  authorship/license/provenance.
- Resolve scheduler and poll mechanisms through the independent
  `thekernel-axsched` and `thekernel-axpoll` packages.
- Remove Linux/process identity allocation from the generic task surface.
- Allocate monotonic generic task identities exactly once, report exhaustion,
  and reject Linux PID-style rewinds or wraparound.
- Replace aliased static mutable runqueue references with guarded shared
  interior mutability and explicit CPU identity.
- Replace the allocating per-CPU exited-task `VecDeque` with a task-embedded
  intrusive FIFO that transfers exactly one raw `Arc` ownership unit, snapshots
  each reclaim batch, and records first-wins typed ownership faults.
- Keep allocation, cloning, destruction, and wake callbacks outside IRQ-safe
  runqueue/timer/registration locks.
- Make timer and IRQ registrations finite, cancellable, generation checked,
  and owned by an explicit CPU/source lifecycle.
- Restrict the generic IRQ registry to waiter ownership and hook dispatch;
  drivers retain IRQ-domain validation, enable/disable, mask, and ack ownership.
- Make synchronous future blocking allocation-free and generation checked,
  closing the poll/block lost-wake race without storing per-poll heap wakers.
- Replace remote scheduling spin handoff with a task-owned atomic state machine
  that transfers exactly one strong task reference to either CPU.
- Return typed task creation, scheduler publication/update, state decoding,
  blocking, timer, and IRQ registration errors instead of silent failure.
- Preserve `thekernel-axsched` unsupported/invalid/class/queue-ownership
  outcomes through task configuration and runtime priority adapters.
- Enforce a 16 KiB minimum kernel-task stack after focused CFS tests proved
  smaller stacks could corrupt adjacent allocator state before diagnosis.
