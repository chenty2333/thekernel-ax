#!/usr/bin/env python3
"""Reject workspace patches and non-registry external dependencies."""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "Cargo.toml"


def fail(message: str) -> None:
    print(f"registry dependency check failed: {message}", file=sys.stderr)
    raise SystemExit(1)


with MANIFEST.open("rb") as manifest_file:
    root_manifest = tomllib.load(manifest_file)

for forbidden_table in ("patch", "replace"):
    if forbidden_table in root_manifest:
        fail(f"root Cargo.toml contains [{forbidden_table}]")

result = subprocess.run(
    [
        "cargo",
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--manifest-path",
        str(MANIFEST),
    ],
    cwd=ROOT,
    check=True,
    text=True,
    stdout=subprocess.PIPE,
)
metadata = json.loads(result.stdout)
workspace_ids = set(metadata["workspace_members"])
workspace_packages = {
    package["id"]: package
    for package in metadata["packages"]
    if package["id"] in workspace_ids
}
workspace_roots = {
    Path(package["manifest_path"]).resolve().parent
    for package in workspace_packages.values()
}

errors: list[str] = []
for package in workspace_packages.values():
    for dependency in package["dependencies"]:
        source = dependency.get("source")
        path = dependency.get("path")
        if source is not None and source.startswith("registry+"):
            continue
        if source is None and path is not None and Path(path).resolve() in workspace_roots:
            continue

        detail = source or path or "missing source"
        errors.append(f"{package['name']} -> {dependency['name']}: {detail}")

if errors:
    fail("non-registry dependency found:\n  " + "\n  ".join(errors))

print(
    f"registry dependency check passed for {len(workspace_packages)} workspace packages"
)

