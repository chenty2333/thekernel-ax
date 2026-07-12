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

Every CFS enqueue claims that owner before it snapshots class/nice/real-time
priority or changes vruntime, child-seed state, or an RR time slice. Duplicate,
foreign, and sequence-exhausted submissions therefore leave all task state
unchanged. The claimed parameter tuple is used for the complete enqueue, so a
concurrent configuration cannot classify one task with two parameter epochs.

`CFSTask::configure` only updates an unqueued/running task and uses a short
configuration claim so an enqueue cannot race the parameter transaction. A
ready task is updated with `CFScheduler::set_task_params`, which removes,
reconfigures, and reinserts it under the scheduler's exclusive borrow without
mutating an intrusive-tree key in place. If target ordering admission fails,
the transaction restores the exact old parameter tuple, vruntime, seed, RR
slice, ready key, owner, and scheduler floor without allocating or requesting a
second sequence. Configuration, fair-child vruntime seeding, and runtime
priority updates return `SchedulerError`; unsupported schedulers, invalid
values, incompatible classes, busy tasks, and foreign queues are therefore
distinguishable.

Class, nice value, and real-time priority are published in one atomic snapshot.
One `sched_params()` read therefore cannot observe a torn combination such as a
new real-time class with the previous class's zero priority. Scheduler
operations additionally claim or serialize task ownership before using that
snapshot to modify class-specific state.

`CFScheduler::reserve_new_task` claims one unpublished task and a unique
scheduler-local ordering sequence before a caller commits external lifecycle
state. Dropping or explicitly cancelling the token returns task ownership
without publication. `commit_reserved_task` is safe and allocation-free for the
owning scheduler; a wrong scheduler or inconsistent private owner returns
`CfsReservationCommitError`, which retains the complete token for retry or
cancellation.

The const-generic `RRScheduler` counters use saturating unsigned arithmetic. A
zero const time slice is representable but every enqueue rejects it with
`InvalidTimeSlice`; `usize::MAX` is valid and cannot truncate through a signed
counter. CFS fair, real-time-front, and real-time-back tie-break sequences are
strictly monotonic and never wrapped, rebased, or reused. Cancellation may leave
a harmless gap. Once a direction reaches its finite `isize` domain, new
admission returns `SequenceExhausted` without mutation, while reservations that
already own earlier sequences remain committable.
