#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$ROOT"

usage() {
    cat <<'USAGE'
Usage: scripts/ci.sh [quality|release]

  quality  PR gate: formatting, dependency/provenance checks, unit tests,
           MSRV coverage, Clippy, and the axtask feature matrix.
  release  Run quality, rustdoc, package-unpack, and publish dry-runs.

The default command is quality.
USAGE
}

step() {
    local name=$1
    shift
    printf '\n==> %s\n' "$name"
    "$@"
}

stable_packages=(
    thekernel-axcbpf
    thekernel-axfault
    thekernel-axrcu
    thekernel-axpmu
    thekernel-axtlb
    thekernel-axpoll
)

axtask_features='test multitask irq preempt smp sched-eevdf task-ext irq-continuation-diagnostics irq-exit'
axtask_doc_features='multitask irq preempt smp sched-eevdf task-ext irq-continuation-diagnostics irq-exit'

quality() {
    step 'rustfmt' cargo +nightly fmt --all -- --check
    step 'registry dependency graph' python3 scripts/check_registry_dependencies.py
    step 'source provenance' scripts/check-provenance.sh
    step 'axsched MSRV artifact' scripts/test-axsched-msrv.sh

    local package
    for package in "${stable_packages[@]}"; do
        step "$package tests" \
            cargo +1.85.0 test -p "$package" --all-targets --locked
        step "$package clippy" \
            cargo +1.85.0 clippy -p "$package" --all-targets --locked -- -D warnings
    done

    step 'axtask minimal build' \
        cargo +nightly check -p thekernel-axtask --no-default-features --locked
    step 'axtask tests' \
        cargo +nightly test -p thekernel-axtask --all-targets --locked \
        --features "$axtask_features"
    step 'axtask clippy' \
        cargo +nightly clippy -p thekernel-axtask --all-targets --locked \
        --features "$axtask_features" -- -D warnings

    local scheduler
    for scheduler in sched-fifo sched-rr sched-eevdf; do
        step "axtask $scheduler configuration" \
            cargo +nightly check -p thekernel-axtask --locked \
            --features "multitask irq preempt smp task-ext $scheduler"
    done
}

release() {
    quality

    local package
    for package in "${stable_packages[@]}"; do
        step "$package rustdoc" \
            env RUSTDOCFLAGS='-D warnings' \
            cargo +1.85.0 doc -p "$package" --no-deps --locked
    done
    step 'axtask rustdoc' \
        env RUSTDOCFLAGS='-D warnings' \
        cargo +nightly doc -p thekernel-axtask --no-deps --locked \
        --features "$axtask_doc_features"

    step 'stable package artifacts' \
        env CARGO_TOOLCHAIN=1.85.0 scripts/package-unpack-original.sh
    step 'coordinated publish dry-run' \
        env AXCBPF_CARGO_TOOLCHAIN=1.85.0 CARGO_TOOLCHAIN=nightly \
        scripts/publish-dry-run.sh
    step 'coordinated package artifacts' \
        env CARGO_TOOLCHAIN=nightly scripts/package-unpack.sh
}

command=${1:-quality}
if [ "$#" -gt 0 ]; then
    shift
fi
case "$command" in
    quality)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        quality
        ;;
    release)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        release
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
