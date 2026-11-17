"""changelog-draft.sh in a scratch clone with final and release-candidate tags."""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

from scripts.tests.release import ROOT

SCRIPT = ROOT / "scripts" / "changelog-draft.sh"


class ChangelogDraft(unittest.TestCase):
    def test_starts_at_the_last_final_tag_not_a_release_candidate(self):
        with tempfile.TemporaryDirectory(prefix=f"cd{os.getpid()}-") as tmp:
            tmp = pathlib.Path(tmp)
            env = {
                **os.environ,
                "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
                "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
                "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
            }
            work = tmp / "work"

            def git(*args: str) -> None:
                subprocess.run(["git", "-C", str(work), *args], env=env, check=True, capture_output=True)

            subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(tmp / "origin.git")], env=env, check=True)
            subprocess.run(["git", "clone", "-q", str(tmp / "origin.git"), str(work)], env=env, check=True, capture_output=True)
            (work / "scripts").mkdir()
            shutil.copy(SCRIPT, work / "scripts" / SCRIPT.name)
            for subject, tag in (("first", "v0.1.0"), ("in the candidate", "v0.2.0-rc.1"), ("after the candidate", None)):
                git("commit", "-q", "--allow-empty", "-m", subject)
                if tag:
                    git("tag", "-a", tag, "-m", tag)
            git("push", "-q", "--follow-tags", "origin", "HEAD:main")
            result = subprocess.run(
                ["bash", str(work / "scripts" / SCRIPT.name)], env=env, capture_output=True, text=True
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            ["## Unreleased (since v0.1.0)", "", "- after the candidate", "- in the candidate"],
        )


if __name__ == "__main__":
    unittest.main()
