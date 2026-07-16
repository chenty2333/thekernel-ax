#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
toolchain=${CARGO_TOOLCHAIN:-1.85.0}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

packages=("$@")
if [[ ${#packages[@]} -eq 0 ]]; then
    packages=(thekernel-axfault thekernel-axtlb)
fi

allow_dirty=()
if ! git -C "$root" diff --quiet \
    || ! git -C "$root" diff --cached --quiet \
    || [[ -n "$(git -C "$root" ls-files --others --exclude-standard)" ]]; then
    if [[ "${PACKAGE_ALLOW_DIRTY:-0}" != 1 ]]; then
        printf 'original package unpack requires a clean release worktree; set PACKAGE_ALLOW_DIRTY=1 only for development checks\n' >&2
        exit 1
    fi
    allow_dirty=(--allow-dirty)
fi

export CARGO_TARGET_DIR="$tmp/package-target"

for package in "${packages[@]}"; do
    case "$package" in
        thekernel-axfault | thekernel-axtlb) ;;
        *)
            printf 'unsupported original package: %s\n' "$package" >&2
            exit 2
            ;;
    esac

    manifest="$root/crates/$package/Cargo.toml"
    version=$(python3 - "$manifest" <<'PY'
import pathlib
import sys
import tomllib

manifest = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
print(manifest["package"]["version"])
PY
    )

    cargo "+$toolchain" package \
        --locked \
        -p "$package" \
        "${allow_dirty[@]}"

    archive="$CARGO_TARGET_DIR/package/$package-$version.crate"
    [[ -f "$archive" ]] || {
        printf 'package archive not found: %s\n' "$archive" >&2
        exit 1
    }

    unpack="$tmp/unpacked/$package"
    mkdir -p "$unpack"
    tar -xzf "$archive" -C "$unpack"
    crate_dir="$unpack/$package-$version"

    for required in \
        Cargo.toml Cargo.toml.orig CHANGELOG.md README.md \
        LICENSES/Apache-2.0.txt src/lib.rs; do
        [[ -f "$crate_dir/$required" ]] || {
            printf '%s packaged file missing: %s\n' "$package" "$required" >&2
            exit 1
        }
    done

    python3 - "$crate_dir/Cargo.toml" "$package" "$version" <<'PY'
import pathlib
import sys
import tomllib

manifest = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
package = sys.argv[2]
version = sys.argv[3]
metadata = manifest["package"]
expected = {
    "name": package,
    "version": version,
    "license": "Apache-2.0",
    "repository": "https://github.com/chenty2333/thekernel-ax",
}
for key, value in expected.items():
    if metadata.get(key) != value:
        raise SystemExit(
            f"{package}: packaged {key} is {metadata.get(key)!r}, expected {value!r}"
        )

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

    expected_license=c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4
    actual_license=$(sha256sum "$crate_dir/LICENSES/Apache-2.0.txt" | awk '{print $1}')
    if [[ "$actual_license" != "$expected_license" ]]; then
        printf '%s packaged Apache-2.0 checksum mismatch\n' "$package" >&2
        exit 1
    fi

    CARGO_TARGET_DIR="$tmp/test-target/$package" \
        cargo "+$toolchain" test \
            --manifest-path "$crate_dir/Cargo.toml" \
            --all-targets \
            --locked \
            --offline
done

printf 'original-package-unpack: PASS (%s packages)\n' "${#packages[@]}"
