#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
toolchain=${CARGO_TOOLCHAIN:-nightly-2025-05-20}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

run_cargo() {
    cargo "+$toolchain" "$@"
}

allow_dirty=()
if ! git -C "$root" diff --quiet \
    || ! git -C "$root" diff --cached --quiet \
    || [[ -n "$(git -C "$root" ls-files --others --exclude-standard)" ]]; then
    if [[ "${PACKAGE_ALLOW_DIRTY:-0}" != 1 ]]; then
        printf 'package-unpack requires a clean release worktree; set PACKAGE_ALLOW_DIRTY=1 only for development checks\n' >&2
        exit 1
    fi
    allow_dirty=(--allow-dirty)
fi

export CARGO_TARGET_DIR="$tmp/package-target"

# axtask depends on the two sibling 0.1.0 packages, which intentionally do not
# exist in the registry before the coordinated first release. Nightly's workspace
# packager assembles the exact release set; unpacked tests below replace its
# staging verifier and patch only to those freshly produced artifacts.
run_cargo -Z package-workspace package \
    --locked \
    --no-verify \
    -p thekernel-axsched \
    -p thekernel-axpoll \
    -p thekernel-axtask \
    "${allow_dirty[@]}"

packages=(thekernel-axsched thekernel-axpoll thekernel-axtask)
for package in "${packages[@]}"; do
    archive="$CARGO_TARGET_DIR/package/$package-0.1.0.crate"
    [[ -f "$archive" ]] || {
        printf 'package archive not found: %s\n' "$archive" >&2
        exit 1
    }

    unpack="$tmp/unpacked/$package"
    mkdir -p "$unpack"
    tar -xzf "$archive" -C "$unpack"
    crate_dir="$unpack/$package-0.1.0"

    for required in \
        Cargo.toml Cargo.toml.orig CHANGELOG.md PATCHES.md README.md VENDOR.md \
        LICENSES/Apache-2.0.txt LICENSES/GPL-3.0-or-later.txt \
        LICENSES/MulanPSL-2.0.txt; do
        [[ -f "$crate_dir/$required" ]] || {
            printf '%s packaged file missing: %s\n' "$package" "$required" >&2
            exit 1
        }
    done

    preserved="$root/crates/$package"
    if cmp -s "$crate_dir/Cargo.toml.orig" "$preserved/Cargo.toml.orig"; then
        printf 'preserved upstream Cargo.toml.orig leaked into %s archive\n' \
            "$package" >&2
        exit 1
    fi
    if [[ -f "$crate_dir/.cargo_vcs_info.json" ]] \
        && cmp -s "$crate_dir/.cargo_vcs_info.json" "$preserved/.cargo_vcs_info.json"; then
        printf 'preserved upstream VCS record leaked into %s archive\n' \
            "$package" >&2
        exit 1
    fi
    grep -Fq "name = \"$package\"" "$crate_dir/Cargo.toml"
    grep -Fq 'version = "0.1.0"' "$crate_dir/Cargo.toml"
    grep -Fq 'repository = "https://github.com/chenty2333/thekernel-ax"' \
        "$crate_dir/Cargo.toml"
    python3 - "$crate_dir/Cargo.toml" "$package" <<'PY'
import pathlib
import sys
import tomllib

manifest = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
package = sys.argv[2]
tables = []
for name in ("dependencies", "dev-dependencies", "build-dependencies"):
    tables.append(manifest.get(name, {}))
for target in manifest.get("target", {}).values():
    for name in ("dependencies", "dev-dependencies", "build-dependencies"):
        tables.append(target.get(name, {}))
for table in tables:
    for dependency, specification in table.items():
        if isinstance(specification, dict) and "path" in specification:
            raise SystemExit(
                f"{package}: packaged dependency {dependency!r} retained path "
                f"{specification['path']!r}"
            )
PY
done

axsched="$tmp/unpacked/thekernel-axsched/thekernel-axsched-0.1.0"
axpoll="$tmp/unpacked/thekernel-axpoll/thekernel-axpoll-0.1.0"
axtask="$tmp/unpacked/thekernel-axtask/thekernel-axtask-0.1.0"

# `cargo -Z package-workspace` writes the two not-yet-published sibling crates
# into axtask's release lock as crates.io packages whose checksums are the exact
# archives produced by this invocation. Verify that byte identity before using
# local artifacts for the pre-publication compile.
for sibling in thekernel-axsched thekernel-axpoll; do
    archive="$CARGO_TARGET_DIR/package/$sibling-0.1.0.crate"
    python3 - "$axtask/Cargo.lock" "$sibling" "$archive" <<'PY'
import hashlib
import pathlib
import sys
import tomllib

lock_path = pathlib.Path(sys.argv[1])
package_name = sys.argv[2]
archive_path = pathlib.Path(sys.argv[3])
lock = tomllib.loads(lock_path.read_text())
matches = [
    package
    for package in lock.get("package", [])
    if package.get("name") == package_name and package.get("version") == "0.1.0"
]
if len(matches) != 1:
    raise SystemExit(
        f"{lock_path}: expected one locked {package_name} 0.1.0, found {len(matches)}"
    )
package = matches[0]
expected_source = "registry+https://github.com/rust-lang/crates.io-index"
if package.get("source") != expected_source:
    raise SystemExit(
        f"{package_name}: release lock source is {package.get('source')!r}, "
        f"expected {expected_source!r}"
    )
archive_checksum = hashlib.sha256(archive_path.read_bytes()).hexdigest()
if package.get("checksum") != archive_checksum:
    raise SystemExit(
        f"{package_name}: release lock checksum {package.get('checksum')!r} "
        f"does not match archive {archive_checksum}"
    )
PY
done

CARGO_TARGET_DIR="$tmp/test-target/axsched" \
    run_cargo test --manifest-path "$axsched/Cargo.toml" --all-targets --locked
CARGO_TARGET_DIR="$tmp/test-target/axpoll" \
    run_cargo test --manifest-path "$axpoll/Cargo.toml" --all-targets --locked
for target in riscv64gc-unknown-none-elf loongarch64-unknown-none; do
    CARGO_TARGET_DIR="$tmp/test-target/axpoll-$target" \
        run_cargo check \
            --manifest-path "$axpoll/Cargo.toml" \
            --locked \
            --target "$target"
done

patches=(
    --config "patch.crates-io.thekernel-axsched.path=\"$axsched\""
    --config "patch.crates-io.thekernel-axpoll.path=\"$axpoll\""
)

# crates.io cannot serve the sibling 0.1.0 packages before their coordinated
# first release sequence.
# Create a pre-publication test lock in this temporary unpacked
# directory, replacing only those two checksum-verified archives with their
# exact extracted paths. Every compile below remains `--locked`.
package_lock="$tmp/axtask-package.lock"
cp "$axtask/Cargo.lock" "$package_lock"
run_cargo update \
    --manifest-path "$axtask/Cargo.toml" \
    --offline \
    -p thekernel-axsched \
    -p thekernel-axpoll \
    "${patches[@]}"

python3 - "$package_lock" "$axtask/Cargo.lock" <<'PY'
import pathlib
import sys
import tomllib

targets = {("thekernel-axsched", "0.1.0"), ("thekernel-axpoll", "0.1.0")}

def normalized(path: str):
    data = tomllib.loads(pathlib.Path(path).read_text())
    lock_version = data.pop("version", None)
    found = set()
    for package in data.get("package", []):
        identity = (package.get("name"), package.get("version"))
        if identity in targets:
            found.add(identity)
            package.pop("source", None)
            package.pop("checksum", None)
    if found != targets:
        raise SystemExit(f"{path}: missing expected sibling lock entries: {targets - found}")
    return lock_version, data

before_version, before = normalized(sys.argv[1])
after_version, after = normalized(sys.argv[2])
if before_version != after_version and (before_version, after_version) != (3, 4):
    raise SystemExit(
        f"unexpected pre-publication lock format change: {before_version} -> {after_version}"
    )
if before != after:
    raise SystemExit(
        "pre-publication artifact patch changed lock data beyond sibling source/checksum"
    )
PY

CARGO_TARGET_DIR="$tmp/test-target/axtask" \
    run_cargo test \
        --manifest-path "$axtask/Cargo.toml" \
        --all-targets \
        --locked \
        --offline \
        --features "test multitask irq preempt smp sched-cfs task-ext irq-exit" \
        "${patches[@]}"
for target in riscv64gc-unknown-none-elf loongarch64-unknown-none; do
    CARGO_TARGET_DIR="$tmp/test-target/axtask-$target" \
        run_cargo check \
            --manifest-path "$axtask/Cargo.toml" \
            --locked \
            --offline \
            --target "$target" \
            --features "multitask irq preempt smp sched-cfs task-ext irq-exit" \
            "${patches[@]}"
done
CARGO_TARGET_DIR="$tmp/test-target/axtask-minimal" \
    run_cargo check \
        --manifest-path "$axtask/Cargo.toml" \
        --no-default-features \
        --lib \
        --locked \
        --offline \
        "${patches[@]}"

tls_log="$tmp/axtask-tls-rejection.log"
if CARGO_TARGET_DIR="$tmp/test-target/axtask-tls" \
    run_cargo check \
        --manifest-path "$axtask/Cargo.toml" \
        --lib \
        --locked \
        --offline \
        --features tls \
        "${patches[@]}" >"$tls_log" 2>&1; then
    printf 'thekernel-axtask tls compatibility sentinel unexpectedly compiled\n' >&2
    exit 1
fi
grep -Fq \
    'thekernel-axtask 0.1.0 does not support TLS tasks: axhal must first expose fallible TLS allocation' \
    "$tls_log"

printf 'package-unpack: PASS (%s packages)\n' "${#packages[@]}"
