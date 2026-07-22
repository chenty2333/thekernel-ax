# Design 0001: EEVDF readiness and bounded integration

- Status: Proposed
- Version: 0.1
- Date: 2026-07-23
- Layer: generic scheduler and task/run-queue mechanisms
- Default scheduler: unchanged (`sched-cfs`)

## Decision

This change set does not add a `sched-eevdf` feature or label the existing CFS
tree as EEVDF. It adds the lifecycle and observability prerequisites that are
useful independently:

- bounded affinity/initialized-CPU-aware initial placement and forced
  wake-owner relocation;
- lock-free per-CPU ready/running load snapshots;
- explicit `New`, `Wakeup`, `Yield`, `Preempt`, and `Migrate` enqueue causes;
- explicit `Sleep`, `Exit`, and `Migrate` current-task deactivation causes;
- allocation-free CFS migration rebasing relative to the source and
  destination run-queue floors.

An opt-in EEVDF implementation is admitted only after the augmented-tree and
semantic gates below exist. A deadline-only tree, or a deadline field added to
the present CFS key, is rejected as an unsafe imitation.

## Why deadline-only ordering is incorrect

EEVDF has two independent predicates:

1. an entity is eligible only when its lag says that it is owed service; and
2. among eligible entities, the earliest virtual deadline wins.

An ineligible entity may have an earlier virtual deadline than every eligible
entity. Therefore the leftmost node of a tree sorted only by virtual deadline
is not necessarily selectable. Repeatedly skipping ineligible leftmost nodes
is an O(n) dispatch scan and makes the hot-path bound workload-dependent.

The current CFS implementation uses one allocation-free intrusive red-black
tree ordered by `(class, vruntime-or-priority, sequence)`. The
`intrusive-collections` tree used here does not expose a per-subtree augmentation
hook. `BTreeMap`, `BinaryHeap`, or a side vector would allocate at runnable
publication, violating the existing scheduler contract. None of these can be
hidden behind a feature name and called EEVDF.

## Required fair-entity state

The EEVDF fair class needs, at minimum, checked or explicitly normalized
representations of:

- weighted virtual runtime/service received;
- lag, with one documented sign convention (`lag >= 0` means eligible);
- request/slice length;
- virtual deadline for the current request;
- the virtual-time point at which an ineligible entity becomes eligible;
- stable queue sequence and intrusive-link ownership;
- sleeper/deferred-dequeue state;
- migration state sufficient to preserve lag and an active request across
  run queues with different virtual-time origins.

Real-time FIFO/RR classes remain separate and retain precedence rules. EEVDF
is not Linux `SCHED_DEADLINE`, EDF, CBS admission control, or a new userspace
scheduling policy.

The running entity is currently unlinked from the ready tree. The EEVDF design
must still account its weight and service in the run queue's virtual-time
average, and compare it with the best ready eligible entity at tick, wakeup,
yield, and preemption boundaries.

## Allocation-free augmented tree

The preferred data structure is an original, local intrusive augmented
red-black tree for fair entities:

- primary key: `(virtual_deadline, stable_sequence)`;
- node augmentation: the minimum `eligible_at` virtual time in that node's
  complete subtree;
- subtree invariant:
  `subtree_min = min(node.eligible_at, left.subtree_min, right.subtree_min)`;
- rotations, insert, remove, and key replacement recompute augmentation only
  along the affected O(log n) path;
- selection at run-queue virtual time `V` descends left only when the left
  subtree can contain `eligible_at <= V`, then tests the node, then descends
  right. The first eligible node reached is the earliest eligible deadline;
- every task owns its link and augmentation storage, so enqueue, dequeue,
  selection, rollback, and migration remain allocation-free.

An alternative two-tree design (ineligible entities ordered by `eligible_at`,
eligible entities ordered by deadline) is acceptable only if moving a wake
burst into eligibility has a strict dispatch budget and a correctness-preserving
fallback. Without that, one clock advance can cause an unbounded activation
loop.

The tree must include a test-only invariant verifier that checks ordering,
link/owner identity, red-black properties, cached subtree minima, total fair
weight, virtual-time aggregates, and the equivalence of augmented selection to
a simple full-scan reference model.

## Lifecycle semantics

### New and fork

The mechanism defines a bounded initial lag/request rule. Linux child-policy
decisions such as reset-on-fork remain in the consumer. A new entity must not
gain unbounded credit or immediately starve existing eligible work.

### Wake and sleep

`Sleep` is emitted only after the lost-wake-safe block transaction commits.
An aborted block remains a running entity and must not receive sleeper
accounting. The first EEVDF prototype must choose and document one bounded
sleeper-lag policy before code lands. Linux's current delayed-dequeue/lag-decay
behavior is a reference, but may not be copied mechanically; a different
policy must prove that repeated short sleeps cannot reset negative lag and gain
service.

Wake-owner CPU selection stays a task-layer mechanism. An ordinary sleep keeps
the task on its initialized source CPU whenever affinity still allows it. Only
an affinity-excluded source performs one bounded load-aware scan of initialized
allowed CPUs. Raw wake publication remains pinned to the owner chosen during
the block transaction, so it does not acquire multiple remote scheduler locks
or race a fresh placement decision. General wake balancing requires a remote
preemption contract and separate SMP latency evidence before it can return.

### Yield and preemption

Yield ends or advances the current request according to one explicit rule;
preemption preserves the active request. They must not be aliases. The
scheduler must compare the current entity against the earliest eligible ready
deadline without making the current entity appear twice in aggregate weight.

### Migration

Ready and running migration use `Migrate`, never `Wakeup`. A source-side hook
captures queue-relative state before ownership is released. Destination
admission reconstructs that state relative to its virtual-time origin. Any
fallible destination step happens before consuming the migration snapshot, so
rollback or retry retains exact state.

The current CFS foundation preserves relative vruntime and RR budget. EEVDF
must additionally preserve lag and active-request/deadline position. Cross-CPU
run-queue virtual times are not directly comparable absolute timestamps.

### Reweight and class changes

Nice/weight changes are transactions. They preserve accrued service debt under
the old weight, update aggregate weight exactly once, recompute request and
eligibility state, and either publish the complete new state or restore the
complete old state. Fair/RT class transitions follow the existing owner-retaining
rollback contract.

## SMP placement and stealing boundary

Initial placement and affinity-forced wake relocation scan at most the
configured CPU count, filter affinity and initialized run queues, and use an
advisory `ready + running_non_idle` score. An affinity-allowed blocking task
does not scan or migrate. The snapshot may straddle a context switch, so it is
never an ownership proof.

Idle stealing is a separate mechanism and is not part of the first EEVDF
feature. It needs all of the following before enablement:

- a bounded victim scan and imbalance threshold;
- an affinity recheck under source ownership;
- cache-hotness or minimum-residency protection;
- a nonblocking ready-task transfer with typed rollback;
- proof that two idle CPUs cannot steal the same intrusive entity;
- CPU-handoff, parameter-update, and hotplug stress tests.

EEVDF changes which task a run queue selects; stealing changes run-queue
ownership. Combining them before each is independently verifiable would make a
failure impossible to localize.

## Provenance and licensing

The implementation must be clean-room and compatible with this crate's
`GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0` publication.

| Source | Permitted use here | Boundary |
| --- | --- | --- |
| Stoica and Abdel-Wahab, *Earliest Eligible Virtual Deadline First: A Flexible and Accurate Mechanism for Proportional Share Resource Allocation*, TR-95-22 (1995) | Algorithm and fairness model | Re-derive formulas and write original Rust/data structures; do not reproduce paper text or figures. |
| [Linux EEVDF documentation](https://docs.kernel.org/scheduler/sched-eevdf.html) | Behavioral comparison, especially eligibility and sleeper lag | Documentation is a reference, not a code source. |
| Linux `kernel/sched/fair.c` | Black-box/differential oracle and failure-case checklist | Linux source is GPL-2.0-only; no transcription into the triple-licensed crate. Record tests and observations, not source-derived code. |
| [Moss](https://github.com/hexagonal-sun/moss-kernel) at `e58825aacd48d7a42486ebdc52866a2aa079aa4d` | MIT-licensed architectural comparison | Its heap-based enqueue allocates and does not satisfy this contract; no direct import. |
| [PatchworkOS](https://github.com/KaiNorberg/PatchworkOS) at `dbbdc990701ea51015be2911d15019c2880746e3` | MIT-licensed augmented-tree and invariant-testing comparison | C/x86 implementation is not a drop-in. Any copied material requires explicit attribution; prefer an original Rust implementation from this specification and tests. |

Before implementation, add a provenance note naming every source actually
consulted and retain license notices for any incorporated compatible code. A
developer who studies GPL-only implementation details must not translate
functions line by line.

## Acceptance gates for an opt-in feature

The `sched-eevdf` feature may be introduced only when all gates below pass.
It remains mutually exclusive with FIFO, RR, and CFS through `axsched`,
`axtask`, feature forwarding, and the kernel consumer.

### Correctness

- Allocation counter proves zero ready-path allocations after task creation.
- Every enqueue/dequeue/pick/rotation passes the augmented-tree verifier in
  tests.
- Augmented selection matches a full-scan rational/integer reference model for
  generated traces of new, tick, wake, sleep, yield, preempt, migrate, reweight,
  remove, and class-change operations.
- Equal-weight and mixed-weight CPU-bound traces keep service error within the
  selected request quantum after a documented warm-up.
- An earlier-deadline ineligible entity never hides a later-deadline eligible
  entity.
- Repeated short sleeps cannot erase negative lag; long sleepers converge under
  the documented decay policy without arithmetic overflow.
- Migration between deliberately divergent run-queue virtual times preserves
  service debt and active-request state across success, target rejection,
  retry, and source rollback.
- Sequence, virtual-time, weight-sum, deadline, and lag boundary tests are
  explicit; identities never wrap or silently saturate into reordered work.
- FIFO/RR precedence and time-slice behavior remain unchanged.

### SMP and lifecycle

- 1/2/4-CPU stress covers affinity changes, wake storms, concurrent parameter
  updates, remote ready/running migration, task exit, and context-switch wake
  handoff with no lost/duplicate task, foreign owner, or durable wake fault.
- A block aborted by a racing wake does not run sleep/dequeue accounting and
  restores the actual CPU identity.
- Load snapshots agree with scheduler-linked counts at quiescent test barriers;
  all direct add, reservation commit, pick, migration remove/rollback, and idle
  transitions are covered.

### Performance and rollout

- Enqueue, dequeue, reweight, and selection are O(log n); no fallback performs
  an unbounded ready-queue scan in production.
- Microbenchmarks report cycles or instructions, context switches, and p50/p99
  dispatch latency for 1, 2, 8, 64, and 1024 runnable fair entities.
- CPU-bound throughput on hardware does not regress more than 5% from CFS at
  equal switch/tick settings; any exception needs a workload-specific cause,
  an observed latency win, and an explicit approval.
- Wake-heavy and mixed-latency workloads demonstrate the intended p99 benefit
  without an unbounded context-switch increase.
- QEMU TCG is a correctness gate, not the sole performance gate. RISC-V and
  LoongArch hardware (or hardware acceleration where representative) must be
  measured separately, especially while address-space switches still perform
  broad TLB invalidation.
- The feature is opt-in for at least one release cycle. `sched-cfs` stays the
  default until both architectures pass the same SMP/lifecycle suite and the
  kernel-level benchmark receipts are reviewed.

## Implementation sequence

1. Freeze integer formulas and sleeper policy in a model-only module with
   generated differential tests.
2. Implement and verify the standalone intrusive augmented tree.
3. Add the EEVDF entity and single-run-queue scheduler behind `sched-eevdf`.
4. Wire lifecycle, class precedence, migration tokens, and feature forwarding.
5. Run SMP stress and architecture builds with stealing disabled.
6. Collect hardware performance receipts; only then evaluate default status
   and a separately feature-gated idle-stealing experiment.

Rollback is the feature selection: CFS state and EEVDF state are not shared
across a running kernel, and no persistent userspace ABI is introduced by this
mechanism.
