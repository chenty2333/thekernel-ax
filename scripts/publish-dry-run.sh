#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
coordinated_toolchain=${CARGO_TOOLCHAIN:-nightly-2025-05-20}
# Keep this exact compiler aligned with `rust-version = "1.85"` in axcbpf's
# manifest. The maintained-fork release set still requires its pinned nightly.
axcbpf_toolchain=${AXCBPF_CARGO_TOOLCHAIN:-1.85.0}

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

publish_dry_run() {
    local toolchain=$1
    local package=$2

    cargo "+$toolchain" publish \
        --dry-run \
        --locked \
        --registry crates-io \
        -p "$package" \
        "${allow_dirty[@]}"
}

# axcbpf is an independent original mechanism. Verify it at its declared MSRV
# before any downstream Linux-ABI seccomp adapter can consume its registry
# release.
publish_dry_run "$axcbpf_toolchain" thekernel-axcbpf

for package in thekernel-axsched thekernel-axpoll; do
    publish_dry_run "$coordinated_toolchain" "$package"
done

if [[ "${AXTASK_REGISTRY_READY:-0}" == 1 ]]; then
    publish_dry_run "$coordinated_toolchain" thekernel-axtask
    printf 'publish-dry-run: PASS (4 packages)\n'
else
    printf '%s\n' \
        'publish-dry-run: thekernel-axcbpf and coordinated leaf packages PASS; thekernel-axtask is registry-blocked until both 0.1.0 sibling packages are published' \
        'publish-dry-run: package-unpack remains the checksum-bound pre-publication axtask gate; rerun with AXTASK_REGISTRY_READY=1 after the leaf release'
fi
