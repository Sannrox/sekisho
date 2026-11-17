#!/usr/bin/env python3
"""Emit the family release manifest Tenkai later admits (digests, not tags)."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import sys

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


def labels_in_range(merged: str, in_range: str) -> list[str]:
    """Labels of merged pull requests whose merge commit is in the release range.

    `merged` holds one `<merge-commit>\t<label>` line per label; `in_range`
    holds the commits of the release range, one per line. A pull request
    merged after the release commit, or before the previous final tag, is
    outside the range and never contributes a gate flag.
    """
    commits = {line.strip() for line in in_range.splitlines() if line.strip()}
    labels = set()
    for line in merged.splitlines():
        oid, sep, label = line.partition("\t")
        if sep and oid in commits and label:
            labels.add(label)
    return sorted(labels)


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


def record_attestations(path: pathlib.Path, urls: list[str]) -> None:
    """Set the manifest's attestation URLs; empty URLs are dropped."""
    manifest = json.loads(path.read_text())
    manifest["attestations"] = [url for url in urls if url]
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")


def main(argv: list[str]) -> None:
    if argv[:1] == ["record-attestations"]:
        if len(argv) < 2:
            raise SystemExit("usage: manifest.py record-attestations MANIFEST [URL...]")
        record_attestations(pathlib.Path(argv[1]), argv[2:])
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
    parser.add_argument("--merged-labels", help="file of <merge-commit>\\t<label> lines")
    parser.add_argument("--range-commits", help="file of the release range's commits")
    args = parser.parse_args(argv)
    dist = pathlib.Path(args.dist)
    labels = list(args.label)
    if args.merged_labels:
        labels.extend(
            labels_in_range(
                pathlib.Path(args.merged_labels).read_text(),
                pathlib.Path(args.range_commits).read_text(),
            )
        )
    flags = list(args.gate_flag)
    flags.extend(gate_flags_from_labels(labels))
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
