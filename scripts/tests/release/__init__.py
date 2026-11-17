"""Tests for scripts/release/ and the release workflows."""

import importlib.util
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[3]
RELEASE = ROOT / "scripts" / "release"


def load(name: str):
    """Import scripts/release/<name>.py, which is a script and not a package."""
    spec = importlib.util.spec_from_file_location(f"release_{name}", RELEASE / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
