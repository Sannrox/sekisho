#!/usr/bin/env python3
"""Write a CycloneDX 1.5 SBOM from `cargo metadata` (locked crate)."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


def cargo_metadata(root: Path) -> dict:
    raw = subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=root,
        text=True,
    )
    return json.loads(raw)


def bom_from_metadata(meta: dict) -> dict:
    workspace = set(meta.get("workspace_members") or [])
    packages = meta.get("packages") or []
    root_pkg = None
    components = []
    for pkg in packages:
        pid = pkg.get("id")
        name = pkg.get("name")
        version = pkg.get("version")
        if pid in workspace and name == "sekisho":
            root_pkg = pkg
        purl = f"pkg:cargo/{name}@{version}"
        component = {
            "type": "library",
            "bom-ref": purl,
            "name": name,
            "version": version,
            "purl": purl,
        }
        if pkg.get("license"):
            component["licenses"] = [{"license": {"id": pkg["license"]}}]
        if pid not in workspace:
            components.append(component)
    if root_pkg is None:
        raise SystemExit("cargo metadata did not include package sekisho")
    root_purl = f"pkg:cargo/sekisho@{root_pkg['version']}"
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "bom-ref": root_purl,
                "name": "sekisho",
                "version": root_pkg["version"],
                "purl": root_purl,
            }
        },
        "components": sorted(components, key=lambda item: item["bom-ref"]),
    }


def self_test() -> None:
    meta = {
        "workspace_members": ["sekisho 0.0.0 (path+file:///workspace)"],
        "packages": [
            {
                "id": "sekisho 0.0.0 (path+file:///workspace)",
                "name": "sekisho",
                "version": "0.0.0",
                "license": "Apache-2.0",
            },
            {
                "id": "libc 0.2.0 (registry+https://github.com/rust-lang/crates.io-index)",
                "name": "libc",
                "version": "0.2.0",
                "license": "MIT OR Apache-2.0",
            },
        ],
    }
    bom = bom_from_metadata(meta)
    assert bom["bomFormat"] == "CycloneDX"
    assert bom["metadata"]["component"]["version"] == "0.0.0"
    assert bom["metadata"]["component"]["type"] == "application"
    assert bom["components"][0]["name"] == "libc"
    print("release-sbom self-test passed")


def main(argv: list[str]) -> None:
    if argv == ["--self-test"]:
        self_test()
        return
    if len(argv) != 1:
        raise SystemExit("usage: release-sbom.py OUT")
    root = Path(__file__).resolve().parent.parent
    bom = bom_from_metadata(cargo_metadata(root))
    out = Path(argv[0])
    out.write_text(json.dumps(bom, indent=2) + "\n")
    print(out)


if __name__ == "__main__":
    main(sys.argv[1:])
