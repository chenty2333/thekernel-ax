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

## Registration lifecycle

`PollSet<const CAPACITY: usize = 64>` has a compile-time capacity and never
grows or silently evicts a waiter. Registration returns an opaque token:

```rust
use axpoll::{PollSet, RegisterError, RegistrationToken, UpdateError};
use core::task::Context;

fn arm_wait(
    waiters: &PollSet<8>,
    registration: &mut Option<RegistrationToken>,
    context: &Context<'_>,
) -> Result<(), RegisterError> {
    if let Some(token) = *registration {
        match waiters.update(token, context.waker()) {
            Ok(()) => return Ok(()),
            Err(UpdateError::InvalidToken) => *registration = None,
            Err(UpdateError::Closed) => return Err(RegisterError::Closed),
        }
    }

    *registration = Some(waiters.register(context.waker())?);
    Ok(())
}
```

Keep the token while a wait is pending and call `cancel(token)` when the wait
completes or its future is dropped. Reusing a slot increments its generation,
so stale tokens cannot cancel a later waiter. Tokens from another `PollSet` are
also rejected.

Every `register` call creates an independent token and consumes one slot, even
when two registrations use equivalent wakers. This is necessary because the
same executor waker may represent separate waits or event interests; cancelling
one must not cancel the other. A logical waiter that is polled again retains its
token and uses `update(token, waker)` rather than calling `register` again.

A registration presented to a full set gets `RegisterError::Full`; no old
waiter is replaced or spuriously woken. `update` changes a live registration's
waker without changing its token.

`wake()` is a one-shot drain and leaves the registry open. `close()` atomically
marks it closed, drains and wakes current waiters, and rejects later
registrations. Dropping the set closes it. Registration, cancellation, wake,
and close races are lock-linearized, while RawWaker clone, destruction, and
wake callbacks run outside the IRQ-safe lock and may re-enter the registry.
The package enables `kspin/smp` explicitly, so this guarantee does not depend on
feature unification in a larger kernel workspace.

## Generic events, not Linux constants

`IoEvents` uses neutral names and crate-owned bit values:

| Inherited name | Generic name |
| --- | --- |
| `IN` | `READABLE` |
| `PRI` | `PRIORITY` |
| `OUT` | `WRITABLE` |
| `ERR` | `ERROR` |
| `HUP` | `HANGUP` |
| `NVAL` | `INVALID` |
| `RDNORM` / `RDBAND` | `READ_NORMAL` / `READ_BAND` |
| `WRNORM` / `WRBAND` | `WRITE_NORMAL` / `WRITE_BAND` |
| `MSG` / `REMOVE` / `RDHUP` | `MESSAGE` / `REMOVED` / `READ_HANGUP` |
| `ALWAYS_POLL` | `ALWAYS` |

The crate no longer depends on `linux-raw-sys`. A Linux ABI adapter should map
Linux `POLL*` input bits into these events and map readiness back at the syscall
boundary; raw Linux values must not be passed through as `IoEvents::from_bits`.

This `0.1.0` package intentionally does not define a `Pollable` trait. A single
poll operation may fan in to several readiness sources, and one token cannot
represent atomic cancellation of that group. Downstream object/fd layers must
own their bounded composite-registration contract, including interest changes,
partial-registration rollback, waker updates, and drop cancellation. The core
crate claims only the per-source `PollSet` lifecycle documented above.

See [`VENDOR.md`](VENDOR.md) for the immutable upstream source record and
[`PATCHES.md`](PATCHES.md) for the maintained delta.
