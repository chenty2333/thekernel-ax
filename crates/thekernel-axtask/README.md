# thekernel-axtask

`thekernel-axtask` is the maintained generic task/run-queue mechanism used by
TheKernel. The registry package name is independent while the Rust library name
remains `axtask`:

```toml
[dependencies]
axtask = { package = "thekernel-axtask", version = "0.1.0", features = [
    "multitask",
    "sched-cfs",
] }
```

The crate owns task objects, per-CPU run queues, scheduling handoff, wait
queues, interrupt/timer wake mechanisms, and bounded deferred reclamation. It
does not own Linux PID allocation, credential/process policy, scheduling ABI
numbers, errno translation, or benchmark profiles.

Version 0.1.0 is tested with `nightly-2025-05-20` and does not claim a stable
`rust-version`, because fallible standard `Arc` allocation still uses
`allocator_api`.

The inherited `tls` feature is retained only as a compatibility sentinel for
downstream feature-forwarding manifests and is rejected with an explicit
compile error in 0.1.0. The current registry `axhal` API only offers infallible
`TlsArea::alloc()`, which can enter a null-pointer/allocator failure path before
`TaskCreateError` can report OOM. TLS task support can return in a later 0.x
release after the lower layer exposes a fallible allocation/initialization
contract and both architectures pass it.

The `irq-exit` feature is a coordinated lower-layer contract, not a dormant
performance option. TheKernel enables it unconditionally when consuming its
maintained `axhal` fork. That HAL exposes explicit per-CPU IRQ nesting and one
outermost-exit callback; ordinary preemption-guard releases inside an IRQ or an
IRQ-off task critical section only lower the guard count, while the exit
callback owns the single IRQ-off pending-reschedule check. The dispatcher holds
one task-owned preemption-disable unit
across a bounded `need_resched` drain and releases it without rescheduling, so
an IRQ-exit context switch cannot leak state to the next task or recurse on
guard release. Publications that exceed the pass budget remain pending for the
next safe point. The generic crate consumes a `crate_interface` transport
rather than naming a HAL fork; each enabling kernel must inject the matching
Layer 0 provider before linking its release set.

The release contract requires explicit task/runqueue ownership, no aliased
mutable runqueue references, no allocation/drop/wake callback inside IRQ-safe
critical sections, monotonic mechanism-only task identities, an intrusive
allocation-free exited-task FIFO, owner-CPU recycler publication, bounded
timer-backed retained-owner retries in IRQ-enabled runtimes, finite cancellable
timer/IRQ registrations,
creation-CPU timer ownership, bounded remote handoff, lost-wake-safe blocking,
and typed scheduler/lifecycle failures. Only the permanently pinned owner-CPU
recycler removes exited tasks, recycles their stacks, and runs their
destructors; public reclaim calls publish an owner-local scan request instead
of destroying tasks in a possibly migrating caller. Recycler destructors and
deferred work run outside its wait-only block session. Retained tasks use a
timer-driven bounded exponential retry from 1 through 64 ticks, never a GC
self-wake or CPU busy-spin loop. IRQ waiter registration is deliberately
separate from driver-owned source validation, enable/disable, masking, and
acknowledgement. See `PATCHES.md` and `VENDOR.md` for the maintained delta and
source identity.

`current()` returns an owned `CurrentTask` handle. Keeping that value across a
context switch is memory-safe: it owns a strong reference independently of the
per-CPU current-task slot. Callers that need a long-lived task reference may
also use `CurrentTask::clone()` to obtain `AxTaskRef` explicitly. `exit()` does
not unwind the kernel stack, so a caller which invokes it directly must release
resources requiring deterministic destruction before the call.

CPU-affinity publication requires at least one initialized run queue, and task
selection skips possible-but-offline CPUs. A mask that cannot currently run the
task is rejected before replacing the old affinity rather than being accepted
and failing later during wakeup. A prepared-publication reservation freezes its
selected affinity transaction; a concurrent affinity update receives
`ResourceBusy` and leaves the old mask unchanged until the token is committed
or cancelled.

SMP initial placement now scans the finite configured CPU set once, filters by
affinity and initialized run queues, and chooses the smallest advisory
`ready + running` load. Equal loads use a rotated deterministic tie-break.
Blocking remains source-local whenever the current initialized CPU is still
affinity-allowed. Only a task whose source CPU has become disallowed uses the
bounded load-aware selector to choose an initialized allowed wake owner. A
racing wake that aborts the block restores the actual current CPU. Raw wake
publication remains pinned to that previously committed owner instead of
selecting a new CPU inside a raw-waker path.

Affinity, Running-to-Blocked owner publication, and Blocked-to-runnable wake
publication share one bounded per-task transaction. A raw waker either claims
the exact publication, delegates it to an active affinity/block owner, or sees
an existing owner; it never spins on helper allocation or takes two runqueue
locks. A block attempt which meets another publication owner converts that
iteration into a spurious wake and repolls in task context. While a wake is
claimed, affinity updates return `ResourceBusy`, freezing the allowed target
through the Ready-before-enqueue and old-CPU handoff windows. Immediate and
deferred wakes both clear the exact claim under the target scheduler lock after
linking and before the task becomes selectable. Since valid wake publication is
capacity-free after scheduler initialization, an impossible enqueue rejection
or missing handoff owner is fail-stop rather than leaving
`Blocked + WOKEN` without an enqueue obligation.

`scheduler_load_snapshot` exposes the lock-free per-CPU ready/running
observation used by placement. It is advisory and may straddle a concurrent
context switch; queue ownership, task state, and affinity remain authoritative.
Idle stealing is intentionally absent: safe stealing still needs a bounded
remote-ready transfer policy, cache-hotness/imbalance thresholds, and stress
coverage for affinity and context-switch handoff races.

`WaitQueue::wait_timeout_until_interruptible` uses one complete deadline rather
than hidden short-slice polling. It returns condition versus timeout explicitly
and preserves separate block-session, interruption, and bounded timer-admission
errors. `future::DeadlineReservation` exposes the same one-admission mechanism
to generic consumers: each borrowed `race` future automatically disarms its
task waker while retaining an unexpired reservation for the next wait session.
A satisfied condition wins when observed with an interrupt or timeout.
The generic `future::interruptible` adapter follows the same condition-first
rule: it checks the wrapped future before consuming an interrupt and rechecks
after installing the interrupt waker. If both become ready in that race, the
future result wins and the interrupt remains pending for the next boundary.

`reserve_prepared_task` is the fallible half of two-phase CFS publication. It
claims scheduler ownership, a permanent target run queue, and a non-reused
ordering ticket before a lifecycle adapter commits externally visible state.
Failure returns `TaskEnqueueError`, which owns the exact unpublished task through
`into_task()` after all runqueue locks are gone. Dropping the reservation
cancels it; `cancel()` also returns a task owner. `publish_prepared_task` consumes
an exact reservation and performs only allocation-free final linking, so the
safe axtask adapter has no recoverable failure after external state is committed.
The successful commit clears its task-level publication claim under the same
target scheduler lock after linking and before the task becomes selectable;
this prevents a newly running task from observing its own stale reservation
when it immediately blocks.
