# thekernel-axsched

[![Crates.io](https://img.shields.io/crates/v/thekernel-axsched)](https://crates.io/crates/thekernel-axsched)
[![Docs.rs](https://docs.rs/thekernel-axsched/badge.svg)](https://docs.rs/thekernel-axsched)
[![CI](https://github.com/chenty2333/thekernel-ax/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/chenty2333/thekernel-ax/actions/workflows/ci.yml)

Maintained scheduler algorithms in a unified `no_std` interface. This package
is a fork of upstream `axsched`; its package name is independent while the Rust
library name remains `axsched`.

Currently supported algorithms:

- [`FifoScheduler`]: FIFO (First-In-First-Out) scheduling (cooperative).
- [`RRScheduler`]: round-robin scheduling (preemptive).
- [`CFScheduler`]: fair scheduling plus FIFO/RR real-time classes (preemptive).

[`FifoScheduler`]: https://docs.rs/thekernel-axsched/latest/axsched/struct.FifoScheduler.html
[`RRScheduler`]: https://docs.rs/thekernel-axsched/latest/axsched/struct.RRScheduler.html
[`CFScheduler`]: https://docs.rs/thekernel-axsched/latest/axsched/struct.CFScheduler.html

## Dependency and import

```toml
[dependencies]
axsched = { package = "thekernel-axsched", version = "0.1.0" }
```

```rust
use std::sync::Arc;
use axsched::{BaseScheduler, FifoScheduler, FifoTask};

let mut scheduler = FifoScheduler::new();
scheduler.init();

for i in 0..10 {
    scheduler.add_task(Arc::new(FifoTask::new(i))).unwrap();
}

for i in 0..10 {
    let next = scheduler.pick_next_task().unwrap();
    assert_eq!(*next.inner(), i);
    scheduler.put_prev_task(next, false).unwrap();
}
```

See [`VENDOR.md`](VENDOR.md) for the immutable upstream source record and
[`PATCHES.md`](PATCHES.md) for the maintained delta.

## Generic boundary

`CfsTaskParams` describes scheduler mechanism only. Real-time priority uses the
full nonzero `u8` domain; an ABI adapter may expose a narrower range. Process
lifecycle policy such as Linux `SCHED_RESET_ON_FORK` is intentionally not
stored in scheduler tasks and must be applied by the downstream task/ABI layer
when it creates a child.

The fair-child vruntime seeding helper is policy-neutral: callers decide which
task-creation operations should use it. Removing a task that belongs to another
scheduler instance is safe and returns `SchedulerError::ForeignQueue`; an
unqueued task returns `Ok(None)`.

## Queue ownership and failure contract

Every intrusive task wrapper has one generation-independent queue-owner state.
Enqueue atomically claims it, duplicate publication returns `AlreadyQueued`,
and another scheduler cannot unlink it. Pick/remove transfers ownership back to
the caller, while dropping a scheduler drains its queue and releases surviving
task references. Queue operations return typed errors instead of silently
ignoring a foreign link.

`CFSTask::configure` only updates an unqueued/running task and uses a short
configuration claim so an enqueue cannot race the parameter transaction. A
ready task is updated with `CFScheduler::set_task_params`, which removes,
reconfigures, and reinserts it under the scheduler's exclusive borrow without
mutating an intrusive-tree key in place.

Round-robin counters use saturating unsigned arithmetic. A zero const time
slice is representable but every enqueue rejects it with
`InvalidTimeSlice`; `usize::MAX` is valid and cannot truncate through a signed
counter. CFS fair and real-time tie-break sequences are rebased
allocation-free before exhaustion while preserving ready order.
