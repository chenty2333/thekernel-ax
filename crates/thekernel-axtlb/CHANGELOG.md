# Changelog

## 0.1.0

- Add fixed-capacity IPI reason mailboxes.
- Add monotonic epoch-based synchronous TLB shootdown completion.
- Bind completion requests to the domain that issued them.
- Bind grace proofs to the same domain lifetime.
- Add explicit online, draining, and offline CPU lifecycle transitions.
- Make lifecycle admission one atomic CAS protocol and report exhaustion.
- Retain issuer admission in each live shootdown request through grace.
- Carry fixed TLB/I-cache maintenance bits in each request and acknowledge
  their epochs independently.
- Bound each maintenance service to one fixed snapshot and at most one local
  callback, leaving concurrent publication for a later reason/service pass.
- Add request-owned per-target pending and completion queries that ignore
  unrelated maintenance and newer epochs.
