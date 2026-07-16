# thekernel-axtlb

`thekernel-axtlb` is a `no_std`, allocation-free state machine for bounded
inter-processor interrupt reasons and synchronous CPU-maintenance shootdown.
It tracks full-TLB invalidation and instruction-stream synchronization as two
explicit maintenance classes. It deliberately does not send hardware IPIs or
execute architecture-specific maintenance instructions; a kernel adapter
supplies those operations.

The first correctness profile targets every online CPU other than the issuer.
Each target owns fixed requested/completed epoch pairs for TLB and I-cache
maintenance plus one pending-reason bitset. Concurrent requests coalesce into
one reason bit and the greatest requested epoch for each class. A caller
receives `ShootdownGrace` only after every target has acknowledged every class
carried by that request. Each request borrows the domain that issued it, so safe
code cannot complete it against another domain's epochs.

## Contract

- Construction allocates nothing and all storage is fixed by `MAX_CPUS`.
- IPI reasons are a machine-word bitset; there is no callback queue.
- Epochs are monotonic and never wrap. Every issue error is reported after the
  caller has published page-table or executable-data stores and may follow
  partial mailbox publication, so a kernel adapter must fail-stop.
- `issue_after_local_maintenance` is called only after the issuer has made its
  page-table or executable-data stores visible and completed every matching
  local operation. `issue_after_local_flush` is the TLB-only convenience API.
- An IPI handler clears pending reason bits and calls `service_maintenance`.
  Its callback must execute every requested bit and take no address-space,
  frame, pin, allocator, or mailbox lock. If both bits are present, it executes
  the full TLB invalidation before instruction-stream synchronization.
- CPU offline first closes target admission, waits for outstanding admission
  readers, drains and acknowledges the mailbox, and only then commits offline.
- A live request retains its issuer admission through grace; target mailboxes
  retain their own pending epoch/reason until service.
- CPU state and admission count share one atomic lifecycle word, so offline
  cannot miss a reader between a state check and a separate counter increment.
- Both a request and its grace remain borrowed from the issuing domain.
- A timeout must not be converted into grace. Continuing to reclaim memory
  after a timeout is outside this crate's contract.

The crate contains no Linux ABI policy. Mapping semantics, executable
publication policy, and frame/backend retirement remain the responsibility of
the consuming MM layer.
