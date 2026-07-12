# thekernel-axpoll

[![Crates.io](https://img.shields.io/crates/v/thekernel-axpoll)](https://crates.io/crates/thekernel-axpoll)
[![Docs.rs](https://docs.rs/thekernel-axpoll/badge.svg)](https://docs.rs/thekernel-axpoll)
[![CI](https://github.com/chenty2333/thekernel-ax/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/chenty2333/thekernel-ax/actions/workflows/ci.yml)

Bounded I/O readiness registration and wakeup primitives for `no_std` systems.
This package is a maintained fork of upstream `axpoll`; its package name is
independent while the Rust library name remains `axpoll`.

```toml
[dependencies]
axpoll = { package = "thekernel-axpoll", version = "0.1.0" }
```

`PollSet` stores at most 64 distinct wakers in the initial extraction
checkpoint, deduplicates equivalent registrations, and wakes registered tasks
outside its IRQ-safe lock. The next maintained API checkpoint makes capacity,
cancellation, closure, and stale-token behavior explicit.

`IoEvents` currently mirrors the inherited event surface. Linux ABI bit-value
translation is being separated from the generic core and belongs in a caller
adapter.

See [`VENDOR.md`](VENDOR.md) for the immutable upstream source record and
[`PATCHES.md`](PATCHES.md) for the maintained delta.

