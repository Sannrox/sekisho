#!/usr/bin/env python3
"""Emit the family release manifest Tenkai later admits (digests, not tags)."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import sys
import tempfile

SCHEMA = "tenkai.family-release-manifest.v1"
SKIP_NAMES = {
    "SHA256SUMS",
    "RELEASE_NOTES.md",
    "sekisho-release.json",
    "sekisho.cdx.json",
}
GATE_LABELS = {
    "risk:breaking": "breaking",
    "risk:migration": "migration",
    "risk:realm": "realm",
    "gate:break": "breaking",
    "gate:mig": "migration",
    "gate:realm": "realm",
}


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def gate_flags_from_labels(labels: list[str]) -> list[str]:
    flags: set[str] = set()
    for raw in labels:
        name = raw.strip().lower()
        if name in GATE_LABELS:
            flags.add(GATE_LABELS[name])
            continue
        if name.startswith("risk:breaking") or name == "breaking":
            flags.add("breaking")
        elif name.startswith("risk:migration") or name == "migration":
            flags.add("migration")
        elif "realm" in name:
            flags.add("realm")
    return sorted(flags)


def binaries_from_dir(dist: pathlib.Path) -> list[dict[str, str]]:
    items = []
    for path in sorted(dist.iterdir()):
        if not path.is_file():
            continue
        if path.name in SKIP_NAMES:
            continue
        if path.suffix == ".cdx.json" or path.name.endswith(".intoto.jsonl"):
            continue
        if path.name.endswith("-image.tar"):
            continue
        items.append({"name": path.name, "digest": sha256_file(path)})
    return items


def parse_image(raw: str) -> dict[str, str]:
    if "=" not in raw:
        raise SystemExit(f"image must be name=digest, got {raw}")
    name, digest = raw.split("=", 1)
    if not name or not digest.startswith("sha256:"):
        raise SystemExit(f"image must be name=sha256:..., got {raw}")
    if name.endswith(":latest") or name.endswith("/latest"):
        raise SystemExit("images must not use a latest tag")
    return {"name": name, "digest": digest}


def build_manifest(
    *,
    component: str,
    version: str,
    git_sha: str,
    tag: str | None,
    dist: pathlib.Path,
    images: list[dict[str, str]],
    attestations: list[str],
    gate_flags: list[str],
) -> dict:
    if not re.fullmatch(r"[0-9a-f]{40}", git_sha):
        raise SystemExit("git_sha must be a full lowercase SHA")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(-(rc|beta)\.[0-9]+)?", version):
        raise SystemExit(f"invalid version: {version}")
    for image in images:
        if image["name"].endswith(":latest"):
            raise SystemExit("images must not use a latest tag")
    return {
        "schema": SCHEMA,
        "component": component,
        "version": version,
        "git_sha": git_sha,
        "tag": tag,
        "binaries": binaries_from_dir(dist),
        "images": images,
        "attestations": attestations,
        "gate_flags": gate_flags,
    }


def self_test() -> None:
    labels = gate_flags_from_labels(
        ["risk:breaking", "area:ops", "GATE:mig", "risk:realm", "docs"]
    )
    assert labels == ["breaking", "migration", "realm"], labels
    assert gate_flags_from_labels([]) == []
    try:
        build_manifest(
            component="sekisho",
            version="0.1.0",
            git_sha="deadbeef",
            tag="v0.1.0",
            dist=pathlib.Path("."),
            images=[],
            attestations=[],
            gate_flags=[],
        )
    except SystemExit:
        pass
    else:
        raise SystemExit("expected short git_sha to fail")
    try:
        parse_image("sekisho:latest=sha256:" + "a" * 64)
    except SystemExit as exc:
        assert "latest" in str(exc)
    else:
        raise SystemExit("expected latest image tag to fail")
    with tempfile.TemporaryDirectory() as tmp:
        dist = pathlib.Path(tmp)
        (dist / "sekisho-0.0.0-x86_64-unknown-linux-gnu").write_bytes(b"bin")
        (dist / "sekisho-0.0.0-image.tar").write_bytes(b"tar")
        (dist / "SHA256SUMS").write_text("skip\n")
        manifest = build_manifest(
            component="sekisho",
            version="0.0.0",
            git_sha="a" * 40,
            tag="v0.0.0",
            dist=dist,
            images=[
                {
                    "name": "sekisho:v0.0.0",
                    "digest": "sha256:" + "b" * 64,
                }
            ],
            attestations=["https://github.com/Sannrox/sekisho/attestations/1"],
            gate_flags=["breaking"],
        )
        assert manifest["schema"] == SCHEMA
        assert len(manifest["binaries"]) == 1
        assert manifest["binaries"][0]["name"] == "sekisho-0.0.0-x86_64-unknown-linux-gnu"
        assert manifest["binaries"][0]["digest"].startswith("sha256:")
        assert manifest["images"][0]["name"] == "sekisho:v0.0.0"
        dumped = json.dumps(manifest)
        assert "latest" not in dumped
    print("release-manifest self-test passed")


def main(argv: list[str]) -> None:
    if argv == ["--self-test"]:
        self_test()
        return
    parser = argparse.ArgumentParser()
    parser.add_argument("--component", default="sekisho")
    parser.add_argument("--version", required=True)
    parser.add_argument("--git-sha", required=True)
    parser.add_argument("--tag", default="")
    parser.add_argument("--dist", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--attestation", action="append", default=[])
    parser.add_argument("--gate-flag", action="append", default=[])
    parser.add_argument("--label", action="append", default=[])
    parser.add_argument("--image", action="append", default=[])
    args = parser.parse_args(argv)
    dist = pathlib.Path(args.dist)
    flags = list(args.gate_flag)
    flags.extend(gate_flags_from_labels(args.label))
    images = [parse_image(item) for item in args.image]
    manifest = build_manifest(
        component=args.component,
        version=args.version,
        git_sha=args.git_sha,
        tag=args.tag or None,
        dist=dist,
        images=images,
        attestations=args.attestation,
        gate_flags=sorted(set(flags)),
    )
    out = pathlib.Path(args.out)
    out.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(out)


if __name__ == "__main__":
    main(sys.argv[1:])
