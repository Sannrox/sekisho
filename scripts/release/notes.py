#!/usr/bin/env python3
"""Print the CHANGELOG.md body for one version heading (Keep a Changelog)."""

from __future__ import annotations

import pathlib
import re
import sys


def notes_for(text: str, version: str) -> str:
    matches = list(re.finditer(r"^## \[([^\]]+)\][^\n]*\n", text, re.M))
    for i, match in enumerate(matches):
        if match.group(1) == version:
            start = match.end()
            end = matches[i + 1].start() if i + 1 < len(matches) else len(text)
            body = text[start:end].strip()
            if not body:
                raise SystemExit(f"empty CHANGELOG section for {version}")
            return body
    raise SystemExit(f"CHANGELOG.md has no ## [{version}] section")


def main(argv: list[str]) -> None:
    if len(argv) != 1:
        raise SystemExit("usage: notes.py VERSION")
    version = argv[0]
    text = pathlib.Path("CHANGELOG.md").read_text()
    print(notes_for(text, version))


if __name__ == "__main__":
    main(sys.argv[1:])
