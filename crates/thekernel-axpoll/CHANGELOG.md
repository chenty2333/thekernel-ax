# Changelog

## 0.1.0

- Rename the maintained fork while retaining the `axpoll` library name.
- Use generic crate-owned readiness bits instead of Linux ABI constants.
- Add fixed-capacity, SMP-safe registration with registry/slot/generation
  tokens, update, cancellation, wake, close, and explicit exhaustion errors.
- Keep waker clone, drop, and callback execution outside the IRQ-safe lock.
- Add an honest per-source prepare/arm seam for bounded aggregate rollback.
- Remove the inherited `Pollable` aggregate fiction and the empty `alloc`
  feature flag.
