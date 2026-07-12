# Changelog

## 0.1.0 - release candidate

- Establish the independent package identity and exact upstream provenance.
- Preserve TheKernel's maintained task, SMP scheduling, wait, timer, and
  bounded reclamation mechanisms while moving Linux-visible policy out.
- Make resource, ownership, IRQ callback, timer, blocking, and remote-handoff
  failure contracts explicit before release.
- Replace the allocating exited-task container with task-embedded FIFO links
  and durable duplicate/generation/length/topology diagnostics.
- Return typed scheduler update failures and typed raw task-state decoding
  instead of ambiguous booleans or a consumer-triggerable conversion panic.
- Preserve typed scheduler configuration, fair-child seeding, unsupported
  priority, class, and queue-ownership causes from `thekernel-axsched`.
- Keep IRQ waiter registration free of hardware-enable side effects; IRQ
  capability owners retain domain validation, enable/disable, and ack policy.
- Make `CurrentTask` a genuinely owned handle; safe callers may retain it
  across context switches without borrowing a lifetime-free per-CPU raw Arc.
- Retain the inherited optional `tls` name only as an explicitly rejected
  compatibility sentinel until `axhal` provides a fallible TLS-area constructor
  compatible with typed task-creation OOM.
- Reject affinity masks that contain no initialized run queue and exclude
  possible-but-offline CPUs from runnable-task publication.
- Add a single-deadline interruptible conditional wait, with typed block,
  interrupt, timer-admission, condition, and timeout outcomes and no slice
  polling.
- Make the generic interruptible-future adapter condition-first and preserve a
  simultaneously observed interrupt when the wrapped operation completes.
- Split prepared CFS publication into a fallible reservation which returns the
  exact task owner on failure and an allocation-free final commit. Reservation
  also excludes affinity changes until commit or cancellation.
- Keep the idle task Running while it probes the ready queue instead of
  publishing a fake Ready state, and admit an already-Running idle as the empty
  scheduler's next fallback.
- Release owned current-task handles before non-returning exit switches so
  exited task objects and kernel stacks remain reclaimable.
