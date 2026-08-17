# EEVDF implementation provenance

The EEVDF implementation in `thekernel-axsched` is an original Rust
implementation written for TheKernel's allocation-free scheduler contract.

Sources actually consulted:

- Ion Stoica and Hussein Abdel-Wahab, *Earliest Eligible Virtual Deadline
  First: A Flexible and Accurate Mechanism for Proportional Share Resource
  Allocation*, Technical Report TR-95-22, revised January 1996,
  <https://people.eecs.berkeley.edu/~istoica/papers/eevdf-tr-95.pdf>. We use the
  published proportional-share model, virtual eligible time/deadline
  definitions, fairness invariants, and the requirement for logarithmic
  dynamic operations.
- Linux kernel documentation, *EEVDF Scheduler*,
  <https://docs.kernel.org/scheduler/sched-eevdf.html>. We use it only as a
  behavioral comparison for positive/negative lag, eligibility, preemption,
  and sleeper-lag concerns.
- TheKernel's existing `thekernel-axsched` CFS and RT mechanisms, plus
  `thekernel-axtask` run-queue lifecycle code. These define the local
  ownership, reservation, migration, wake, block, RT-precedence, and
  allocation-free publication contracts into which EEVDF is integrated.
- `docs/design/0001-eevdf-readiness.md`, which records the project-specific
  augmented-tree, verification, rollout, and idle-stealing boundaries.

The implementation does not consult or translate Linux `kernel/sched/fair.c`.
Linux scheduler source remains a GPL-2.0-only black-box comparison boundary,
not an implementation source for this triple-licensed crate. No source code is
copied from the report, Linux, Moss, or PatchworkOS.
