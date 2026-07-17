#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

cargo +nightly-2025-05-20 fmt --all -- --check
python3 scripts/check_registry_dependencies.py
scripts/check-provenance.sh

scripts/test-axsched-msrv.sh

cargo +1.85.0 test -p thekernel-axfault --all-targets --locked
cargo +1.85.0 test -p thekernel-axfault --doc --locked
cargo +1.85.0 clippy -p thekernel-axfault --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axfault --no-deps --locked

cargo +1.85.0 test -p thekernel-axtlb --all-targets --locked
cargo +1.85.0 test -p thekernel-axtlb --doc --locked
cargo +1.85.0 clippy -p thekernel-axtlb --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axtlb --no-deps --locked

cargo +1.85.0 test -p thekernel-axpoll --all-targets --locked
cargo +1.85.0 clippy -p thekernel-axpoll --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axpoll --no-deps --locked

nightly_features='multitask irq preempt smp sched-cfs task-ext'
diagnostic_features="$nightly_features irq-continuation-diagnostics"
nightly_test_features='test multitask irq preempt smp sched-cfs irq-continuation-diagnostics'
cargo +nightly-2025-05-20 check -p thekernel-axtask --no-default-features --locked
cargo +nightly-2025-05-20 test \
    -p thekernel-axtask --all-targets --locked --features "$nightly_test_features"
cargo +nightly-2025-05-20 clippy \
    -p thekernel-axtask --all-targets --locked --features "$nightly_test_features" \
    -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +nightly-2025-05-20 doc \
        -p thekernel-axtask --no-deps --locked --features "$diagnostic_features"

for scheduler in sched-fifo sched-rr sched-cfs; do
    cargo +nightly-2025-05-20 check \
        -p thekernel-axtask \
        --locked \
        --features "multitask irq preempt smp task-ext $scheduler"
done

for target in riscv64gc-unknown-none-elf loongarch64-unknown-none; do
    cargo +1.85.0 check -p thekernel-axfault --locked --target "$target"
    cargo +1.85.0 check -p thekernel-axtlb --locked --target "$target"
    cargo +1.85.0 check -p thekernel-axpoll --locked --target "$target"
    cargo +nightly-2025-05-20 check \
        -p thekernel-axtask \
        --locked \
        --target "$target" \
        --features "$diagnostic_features"
done

CARGO_TOOLCHAIN=1.85.0 scripts/package-unpack-original.sh
CARGO_TOOLCHAIN=nightly-2025-05-20 scripts/publish-dry-run.sh
CARGO_TOOLCHAIN=nightly-2025-05-20 scripts/package-unpack.sh
printf 'workspace-ci: PASS\n'
