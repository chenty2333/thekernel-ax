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
