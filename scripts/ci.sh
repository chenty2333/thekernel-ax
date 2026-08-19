#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

cargo +nightly fmt --all -- --check
python3 scripts/check_registry_dependencies.py
scripts/check-provenance.sh

scripts/test-axsched-msrv.sh

cargo +1.85.0 test -p thekernel-axcbpf --all-targets --locked
cargo +1.85.0 test -p thekernel-axcbpf --doc --locked
cargo +1.85.0 clippy -p thekernel-axcbpf --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axcbpf --no-deps --locked

cargo +1.85.0 test -p thekernel-axfault --all-targets --locked
cargo +1.85.0 test -p thekernel-axfault --doc --locked
cargo +1.85.0 clippy -p thekernel-axfault --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axfault --no-deps --locked

cargo +1.85.0 test -p thekernel-axpmu --all-targets --locked
cargo +1.85.0 test -p thekernel-axpmu --doc --locked
cargo +1.85.0 clippy -p thekernel-axpmu --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axpmu --no-deps --locked

cargo +1.85.0 test -p thekernel-axtlb --all-targets --locked
cargo +1.85.0 test -p thekernel-axtlb --doc --locked
cargo +1.85.0 clippy -p thekernel-axtlb --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axtlb --no-deps --locked

cargo +1.85.0 test -p thekernel-axpoll --all-targets --locked
cargo +1.85.0 clippy -p thekernel-axpoll --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.85.0 doc -p thekernel-axpoll --no-deps --locked

nightly_features='multitask irq preempt smp sched-eevdf task-ext'
diagnostic_features="$nightly_features irq-continuation-diagnostics irq-exit"
nightly_test_features='test multitask irq preempt smp sched-eevdf task-ext irq-continuation-diagnostics irq-exit'
cargo +nightly check -p thekernel-axtask --no-default-features --locked
cargo +nightly test \
    -p thekernel-axtask --all-targets --locked --features "$nightly_test_features"
cargo +nightly clippy \
    -p thekernel-axtask --all-targets --locked --features "$nightly_test_features" \
    -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +nightly doc \
        -p thekernel-axtask --no-deps --locked --features "$diagnostic_features"

for scheduler in sched-fifo sched-rr sched-eevdf; do
    cargo +nightly check \
        -p thekernel-axtask \
        --locked \
        --features "multitask irq preempt smp task-ext $scheduler"
done

CARGO_TOOLCHAIN=1.85.0 scripts/package-unpack-original.sh
AXCBPF_CARGO_TOOLCHAIN=1.85.0 \
    CARGO_TOOLCHAIN=nightly \
    scripts/publish-dry-run.sh
CARGO_TOOLCHAIN=nightly scripts/package-unpack.sh
printf 'workspace-ci: PASS\n'
