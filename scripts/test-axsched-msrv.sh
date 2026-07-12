#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

allow_dirty=()
if ! git -C "$root" diff --quiet \
    || ! git -C "$root" diff --cached --quiet \
    || [[ -n "$(git -C "$root" ls-files --others --exclude-standard)" ]]; then
    if [[ "${PACKAGE_ALLOW_DIRTY:-0}" != 1 ]]; then
        printf 'axsched MSRV packaging requires a clean release worktree; set PACKAGE_ALLOW_DIRTY=1 only for development checks\n' >&2
        exit 1
    fi
    allow_dirty=(--allow-dirty)
fi

# Cargo 1.76 predates edition 2024 and therefore cannot parse the other
# workspace members. Package with the repository toolchain, then run the exact
# registry artifact in isolation with the claimed MSRV.
CARGO_TARGET_DIR="$tmp/package-target" \
    cargo +nightly-2025-05-20 package \
        --manifest-path "$root/Cargo.toml" \
        --locked \
        --no-verify \
        -p thekernel-axsched \
        "${allow_dirty[@]}"

archive="$tmp/package-target/package/thekernel-axsched-0.1.0.crate"
mkdir -p "$tmp/unpacked"
tar -xzf "$archive" -C "$tmp/unpacked"
crate_dir="$tmp/unpacked/thekernel-axsched-0.1.0"

export CARGO_TARGET_DIR="$tmp/msrv-target"
cargo +1.76.0 test --manifest-path "$crate_dir/Cargo.toml" --all-targets --locked
cargo +1.76.0 clippy \
    --manifest-path "$crate_dir/Cargo.toml" --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
    cargo +1.76.0 doc --manifest-path "$crate_dir/Cargo.toml" --no-deps --locked
for target in riscv64gc-unknown-none-elf loongarch64-unknown-none; do
    cargo +1.76.0 check --manifest-path "$crate_dir/Cargo.toml" --target "$target" --locked
done

printf 'axsched-msrv: PASS (1.76.0, unpacked artifact)\n'
