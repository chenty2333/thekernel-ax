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
- Replace the lifetime-free non-owning public `CurrentTask` wrapper with an
  independently reference-counted handle while keeping the per-CPU slot's raw
  Arc ownership distinct during context switches.
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
- Keep the inherited `tls` feature name only as a compile-time rejection for
  downstream forwarding compatibility: the registry `axhal` baseline has only
  an infallible TLS allocator, so enabling the old implementation would bypass
  the crate's typed task-creation OOM contract.
- Validate affinity against initialized run queues before publication and make
  normal scheduler selection skip possible-but-offline CPUs instead of
  stranding a ready or blocked task on an unavailable queue.
- Add `WaitQueue::wait_timeout_until_interruptible` over one complete bounded
  timer registration and an interruption source, preserving the
  condition-listener-condition lost-wake handshake without short sleep slices.
- Poll generic interruptible futures before consuming task interruption and
  recheck after interrupt-waker publication, restoring a simultaneous
  interrupt when the wrapped operation wins.
- Split prepared CFS task publication into fallible target/ownership/ordering
  reservation and allocation-free final linking. Reservation failure returns a
  public typed error owning the exact rejected `AxTaskRef`; token cancellation
  returns ownership and token lifetime excludes affinity retargeting.
- Keep idle tasks out of the ready-state transition, explicitly probe the
  scheduler from idle yield/preemption paths, and accept Ready or Running only
  for the per-CPU idle fallback.
- Release every internal owned current-task handle before abandoning an exiting
  kernel stack, while retaining the distinct per-CPU slot and exited-queue
  ownership required for a safe final context switch and deferred reclamation.
