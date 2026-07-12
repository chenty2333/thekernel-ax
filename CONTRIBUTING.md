# Contributing

Thank you for improving `thekernel-ax`.

## Before changing code

Classify the change as a generic mechanism, downstream Linux ABI policy, or
benchmark/evaluator policy. Only the first category normally belongs here. For
resource-owning code, document the bound, ownership transfer, cleanup path,
failure behavior, and concurrency linearization point.

Preserve each crate's upstream authorship and immutable source record. Do not
replace `Cargo.toml.orig`, `.cargo_vcs_info.json`, or the archive checksums in
`VENDOR.md` with data from a convenient but different upstream checkout.

## Required checks

Run from the repository root:

```sh
cargo fmt --all --check
python3 scripts/check_registry_dependencies.py
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
scripts/package-unpack.sh
```

Add focused tests for public behavior and races. Waker callbacks and final
destructors must be assumed re-entrant; code must not invoke them while holding
an internal IRQ-safe lock.

## Patch records

Update the affected crate's `PATCHES.md` whenever maintained behavior changes.
Update `VENDOR.md` only when the immutable baseline, license record, or rebase
procedure changes. If an upstream rebase is performed, verify the registry
archive checksum before reapplying each maintained patch.

## Pull requests

Keep changes narrow. Explain the mechanism boundary, compatibility impact,
resource bound, and verification performed. Do not include generated `target/`
content or a dependency on TheKernel's root `[patch.crates-io]` table.

