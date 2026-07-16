# thekernel-axtlb

`thekernel-axtlb` is a `no_std`, allocation-free state machine for bounded
inter-processor interrupt reasons and synchronous TLB shootdown completion.
It deliberately does not send hardware IPIs or execute architecture-specific
TLB instructions. A kernel adapter supplies those two operations.

The first correctness profile targets every online CPU other than the issuer.
Each target owns fixed `requested_epoch`, `completed_epoch`, and pending-reason
atomics. Concurrent requests coalesce into one reason bit and the greatest
requested epoch. A caller receives `TlbGrace` only after every target has
acknowledged an equal or later epoch. Each request borrows the domain that
issued it, so safe code cannot complete it against another domain's epochs.

## Contract

- Construction allocates nothing and all storage is fixed by `MAX_CPUS`.
- IPI reasons are a machine-word bitset; there is no callback queue.
- Epochs are monotonic and never wrap. Exhaustion is reported after the page
  table writer has published its stores, so a kernel adapter must fail-stop.
- `issue_after_local_flush` is called only after the issuer has made its page
  table stores visible and completed its local invalidation.
- An IPI handler clears pending reason bits, calls `service_tlb` with a local
  full-flush function, and takes no address-space, frame, pin, or allocator
  lock.
- CPU offline first closes target admission, waits for outstanding admission
  readers, drains and acknowledges the mailbox, and only then commits offline.
- A timeout must not be converted into grace. Continuing to reclaim memory
  after a timeout is outside this crate's contract.

The crate contains no Linux ABI policy. Mapping semantics and frame/backend
retirement remain the responsibility of the consuming MM layer.
