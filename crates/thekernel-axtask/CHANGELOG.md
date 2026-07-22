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
- Replace initial round-robin CPU placement with a bounded load-aware scan that
  respects affinity and initialized queues, and publish advisory per-CPU
  ready/running snapshots for diagnostics.
- Keep an ordinary blocking task's next wake on its affinity-allowed source
  CPU. Only an affinity-excluded source performs one bounded load-aware scan
  of initialized allowed CPUs; raw wake publication stays pinned to that
  committed owner.
- Serialize affinity, Running-to-Blocked owner publication, raw wake claiming,
  and deferred CPU handoff with a bounded per-task transaction. A wake racing
  affinity/block publication is inherited exactly once, affinity remains
  excluded through Ready-before-enqueue, and the target scheduler clears the
  wake claim before the task becomes selectable; impossible enqueue/handoff
  failures are fail-stop instead of leaving a woken Blocked task unowned.
- Preserve scheduler migration lifecycle and queue-relative fair vruntime for
  both ready-task and running-task affinity transfers.
- Add a single-deadline interruptible conditional wait, with typed block,
  interrupt, timer-admission, condition, and timeout outcomes and no slice
  polling.
- Make the generic interruptible-future adapter condition-first and preserve a
  simultaneously observed interrupt when the wrapped operation completes.
- Split prepared CFS publication into a fallible reservation which returns the
  exact task owner on failure and an allocation-free final commit. Reservation
  also excludes affinity changes until commit or cancellation; successful
  commit clears that claim under the target scheduler lock before the new task
  can run or begin blocking.
- Keep the idle task Running while it probes the ready queue instead of
  publishing a fake Ready state, and admit an already-Running idle as the empty
  scheduler's next fallback.
- Release owned current-task handles before non-returning exit switches so
  exited task objects and kernel stacks remain reclaimable.
