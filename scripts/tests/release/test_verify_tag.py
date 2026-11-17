"""verify-tag.sh against real annotated, signed, and lightweight tags in a scratch repository."""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

from scripts.tests.release import RELEASE

SCRIPT = RELEASE / "verify-tag.sh"


@unittest.skipUnless(shutil.which("gpg"), "gpg is required")
class VerifyTag(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Short base path: gpg-agent socket paths have a length limit.
        cls.tmp = pathlib.Path(tempfile.mkdtemp(prefix=f"vt{os.getpid()}-", dir="/tmp"))
        gnupg = cls.tmp / "g"
        gnupg.mkdir(mode=0o700)
        (gnupg / "gpg.conf").write_text("pinentry-mode loopback\n")
        (gnupg / "gpg-agent.conf").write_text("allow-loopback-pinentry\n")
        cls.env = {**os.environ, "GNUPGHOME": str(gnupg)}
        subprocess.run(
            ["gpg", "--batch", "--pinentry-mode", "loopback", "--passphrase", "",
             "--quick-generate-key", "Release test <release-test@example.invalid>",
             "ed25519", "default", "never"],
            env=cls.env, check=True, capture_output=True,
        )
        listing = subprocess.run(
            ["gpg", "--batch", "--list-secret-keys", "--with-colons"],
            env=cls.env, check=True, capture_output=True, text=True,
        ).stdout
        cls.fpr = next(line.split(":")[9] for line in listing.splitlines() if line.startswith("fpr:"))
        key = cls.tmp / "key.asc"
        key.write_bytes(subprocess.run(
            ["gpg", "--batch", "--armor", "--export", cls.fpr],
            env=cls.env, check=True, capture_output=True,
        ).stdout)
        cls.fingerprints = cls.tmp / "fingerprints"
        cls.work = cls.tmp / "w"
        cls.env.update(
            RELEASE_TAG_KEY=str(key),
            RELEASE_TAG_FINGERPRINTS=str(cls.fingerprints),
            RELEASE_ROOT=str(cls.work),
        )
        subprocess.run(["git", "init", "-q", str(cls.work)], env=cls.env, check=True)
        for k, v in {
            "user.name": "Release test", "user.email": "release-test@example.invalid",
            "commit.gpgsign": "false", "tag.gpgSign": "false",
            "user.signingkey": cls.fpr, "gpg.program": shutil.which("gpg"),
        }.items():
            cls.git("config", k, v)
        cls.first = cls.commit("0.1.0")
        cls.git("tag", "-a", "v0.0.9", "-m", "unsigned", cls.first)
        cls.git("tag", "-s", "v0.1.0", "-m", "signed", cls.first)
        cls.git("tag", "-s", "v0.2.0", "-m", "wrong version", cls.first)
        cls.rc = cls.commit("0.2.0-rc.1")
        cls.git("tag", "-s", "v0.2.0-rc.1", "-m", "rc", cls.rc)
        cls.git("tag", "-s", "v0.2.1", "-m", "final on rc", cls.rc)
        cls.git("tag", "-s", "v0.2.0-beta.x", "-m", "bad suffix", cls.rc)
        cls.git("checkout", "-q", cls.first)

    @classmethod
    def git(cls, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(cls.work), *args], env=cls.env, check=True, capture_output=True, text=True
        ).stdout.strip()

    @classmethod
    def commit(cls, version: str) -> str:
        (cls.work / "Cargo.toml").write_text(f'[package]\nname = "sekisho"\nversion = "{version}"\n')
        cls.git("add", "Cargo.toml")
        cls.git("commit", "-q", "-m", version)
        return cls.git("rev-parse", "HEAD")

    @classmethod
    def tearDownClass(cls):
        subprocess.run(["gpgconf", "--kill", "gpg-agent"], env=cls.env, capture_output=True)
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        self.fingerprints.write_text(self.fpr + "\n")

    def verify(self, tag: str, sha: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(SCRIPT), "verify", tag, sha], cwd=self.work, env=self.env, capture_output=True, text=True
        )

    def assertRefused(self, tag: str, sha: str, message: str):
        result = self.verify(tag, sha)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stderr)

    def test_accepts_a_signed_tag_matching_the_crate_version(self):
        result = self.verify("v0.1.0", self.first)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_refuses_a_lightweight_tag(self):
        self.git("tag", "-f", "v0.4.0", self.first)
        self.assertRefused("v0.4.0", self.first, "is lightweight")

    def test_refuses_an_unsigned_tag(self):
        self.assertRefused("v0.0.9", self.first, "unsigned")

    def test_refuses_an_unknown_key(self):
        self.fingerprints.write_text("AEDA618886BF0331105B9CFCE4DC1F4A579505FB\n")
        self.assertRefused("v0.1.0", self.first, "unknown key")

    def test_refuses_a_tag_that_does_not_match_the_crate_version(self):
        self.assertRefused("v0.2.0", self.first, "does not match crate version 0.1.0")

    def test_release_candidate_tag_matches_an_rc_crate_version_only(self):
        self.git("checkout", "-q", self.rc)
        try:
            self.assertEqual(self.verify("v0.2.0-rc.1", self.rc).returncode, 0)
            self.assertRefused("v0.2.1", self.rc, "does not match crate version 0.2.0-rc.1")
            self.assertRefused("v0.2.0-beta.x", self.rc, "invalid release tag")
        finally:
            self.git("checkout", "-q", self.first)

    def test_refuses_a_tag_for_another_commit(self):
        self.assertRefused("v0.1.0", self.rc, f"resolves to {self.first}, expected {self.rc}")


if __name__ == "__main__":
    unittest.main()
