"""package.sh dry runs against this checkout."""

import json
import os
import pathlib
import re
import subprocess
import tempfile
import unittest

from scripts.tests.release import RELEASE, ROOT

SCRIPT = RELEASE / "package.sh"


def crate_version() -> str:
    text = (ROOT / "Cargo.toml").read_text()
    return re.search(r'(?ms)^\[package\][^\[]*?^version\s*=\s*"([^"]+)"', text).group(1)


class Package(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix=f"pkg{os.getpid()}-")
        self.addCleanup(tmp.cleanup)
        self.tmp = pathlib.Path(tmp.name)
        self.sha = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "HEAD"], check=True, capture_output=True, text=True
        ).stdout.strip()
        self.tag = f"v{crate_version()}"

    def package(self, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(SCRIPT), "--out", str(self.tmp / "dist"), "--skip-build", "--skip-test",
             "--git-sha", self.sha, *args],
            env=env or os.environ, capture_output=True, text=True,
        )

    def test_dry_run_writes_manifest_sbom_and_checksums_without_latest(self):
        result = self.package("--skip-github")
        self.assertEqual(result.returncode, 0, result.stderr)
        dist = self.tmp / "dist"
        manifest = json.loads((dist / "sekisho-release.json").read_text())
        self.assertEqual(manifest["schema"], "tenkai.family-release-manifest.v1")
        self.assertEqual(manifest["component"], "sekisho")
        self.assertEqual(manifest["git_sha"], self.sha)
        self.assertEqual(manifest["images"], [])
        self.assertNotIn("latest", json.dumps(manifest))
        self.assertTrue((dist / "sekisho.cdx.json").is_file())
        self.assertTrue((dist / "SHA256SUMS").is_file())

    def test_tagged_run_writes_notes_from_changelog(self):
        result = self.package("--skip-github", "--tag", self.tag)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.tmp / "dist" / "RELEASE_NOTES.md").read_text().strip())

    def test_refuses_a_tag_that_does_not_match_the_crate_version(self):
        result = self.package("--skip-github", "--tag", "v999.0.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match crate version", result.stderr)

    def test_push_image_needs_a_tagged_image_build(self):
        result = self.package("--skip-github", "--push-image", "ghcr.io/example/sekisho")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--push-image needs --with-images and --tag", result.stderr)

    def test_tagged_run_fails_closed_when_gate_flags_cannot_be_read(self):
        # Empty gate flags would read as a release that passed no gates.
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        (bin_dir / "gh").write_text("#!/usr/bin/env bash\necho 'HTTP 403' >&2\nexit 1\n")
        (bin_dir / "gh").chmod(0o755)
        env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"}
        result = self.package("--tag", self.tag, env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot list merged pull requests", result.stderr)
        self.assertFalse((self.tmp / "dist" / "sekisho-release.json").exists())


if __name__ == "__main__":
    unittest.main()
