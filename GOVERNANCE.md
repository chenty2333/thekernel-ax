# Governance

## Mission

`thekernel-ax` maintains bounded, reusable operating-system mechanisms that are
useful to TheKernel and can also stand on their own. It does not absorb Linux
ABI policy merely because TheKernel is the first consumer.

## Maintainers and decisions

The repository maintainers review changes and publish releases through
<https://github.com/chenty2333/thekernel-ax>. Routine changes are accepted by
maintainer review. Changes to public contracts, unsafe-code invariants,
licensing, provenance, or crate ownership require an explicit rationale in the
pull request and maintainer agreement before merge.

If consensus is not immediate, the safer and more reversible behavior remains
the default while evidence is gathered. A benchmark result alone is not enough
to weaken semantics or resource bounds.

## Ownership boundary

This repository may own:

- scheduler data structures and generic scheduling policy mechanisms;
- bounded registration, cancellation, and wakeup mechanics;
- resource ownership and lifecycle contracts needed by those mechanisms;
- tests and diagnostics that validate those generic contracts.

It must not own:

- Linux syscall numbers or userspace argument decoding;
- Linux file-descriptor, signal, errno, or process policy;
- OSComp program-name special cases or evaluator output conventions;
- unbounded caches, registrations, pins, or hidden busy-wait policy.

Linux-visible translation belongs in a downstream ABI-support crate. Benchmark
profiles belong in their runner or evaluator layer.

## Compatibility and releases

The package names and versions are independent from upstream. Public API
changes follow semantic versioning after the initial `0.x` development series.
Every release must preserve its source record, pass registry-only dependency
checks, and pass tests from the unpacked package artifact.

Upstream work remains welcome. A change may be proposed upstream when its
contract is generic and its tests do not depend on TheKernel policy. Until such
a change is accepted and released, this repository records and maintains the
delta explicitly rather than hiding it in a workspace patch table.

