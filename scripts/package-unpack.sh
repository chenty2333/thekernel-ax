#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGES=(thekernel-axsched thekernel-axpoll)
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

allow_dirty=()
if ! git -C "$ROOT" diff --quiet \
    || ! git -C "$ROOT" diff --cached --quiet \
    || [[ -n "$(git -C "$ROOT" ls-files --others --exclude-standard)" ]]; then
    allow_dirty=(--allow-dirty)
fi

export CARGO_TARGET_DIR="$TMP/package-target"

for package in "${PACKAGES[@]}"; do
    cargo package \
        --manifest-path "$ROOT/Cargo.toml" \
        --locked \
        -p "$package" \
        "${allow_dirty[@]}"

    archive="$(find "$CARGO_TARGET_DIR/package" -maxdepth 1 -type f \
        -name "${package}-*.crate" -print -quit)"
    if [[ -z "$archive" ]]; then
        echo "package archive not found for $package" >&2
        exit 1
    fi

    unpack="$TMP/unpacked/$package"
    mkdir -p "$unpack"
    tar -xzf "$archive" -C "$unpack"
    crate_dir="$(find "$unpack" -mindepth 1 -maxdepth 1 -type d -print -quit)"

    archived_manifest="$ROOT/crates/$package/Cargo.toml.orig"
    if [[ -f "$crate_dir/Cargo.toml.orig" ]] \
        && cmp -s "$crate_dir/Cargo.toml.orig" "$archived_manifest"; then
        echo "preserved upstream Cargo.toml.orig leaked into $package archive" >&2
        exit 1
    fi

    archived_vcs="$ROOT/crates/$package/.cargo_vcs_info.json"
    if [[ -f "$crate_dir/.cargo_vcs_info.json" ]] \
        && cmp -s "$crate_dir/.cargo_vcs_info.json" "$archived_vcs"; then
        echo "preserved upstream .cargo_vcs_info.json leaked into $package archive" >&2
        exit 1
    fi

    CARGO_TARGET_DIR="$TMP/test-target/$package" \
        cargo test \
        --manifest-path "$crate_dir/Cargo.toml" \
        --all-targets \
        --locked
done

echo "package unpack tests passed for ${#PACKAGES[@]} packages"

