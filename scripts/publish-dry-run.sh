#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
toolchain=${CARGO_TOOLCHAIN:-nightly-2025-05-20}

allow_dirty=()
if ! git -C "$root" diff --quiet \
    || ! git -C "$root" diff --cached --quiet \
    || [[ -n "$(git -C "$root" ls-files --others --exclude-standard)" ]]; then
    if [[ "${PACKAGE_ALLOW_DIRTY:-0}" != 1 ]]; then
        printf 'publish dry-run requires a clean release worktree; set PACKAGE_ALLOW_DIRTY=1 only for development checks\n' >&2
        exit 1
    fi
    allow_dirty=(--allow-dirty)
fi

for package in thekernel-axsched thekernel-axpoll; do
    cargo "+$toolchain" publish \
        --dry-run \
        --locked \
        --registry crates-io \
        -p "$package" \
        "${allow_dirty[@]}"
done

if [[ "${AXTASK_REGISTRY_READY:-0}" == 1 ]]; then
    cargo "+$toolchain" publish \
        --dry-run \
        --locked \
        --registry crates-io \
        -p thekernel-axtask \
        "${allow_dirty[@]}"
    printf 'publish-dry-run: PASS (3 packages)\n'
else
    printf '%s\n' \
        'publish-dry-run: leaf packages PASS; thekernel-axtask is registry-blocked until both 0.1.0 sibling packages are published' \
        'publish-dry-run: package-unpack remains the checksum-bound pre-publication axtask gate; rerun with AXTASK_REGISTRY_READY=1 after the leaf release'
fi
