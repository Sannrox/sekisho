import unittest

from scripts.tests.release import load

notes = load("notes")

CHANGELOG = (
    "# Changelog\n\n"
    "## [Unreleased]\n\n"
    "- pending\n\n"
    "## [0.3.0-rc.1] - 2026-11-01\n\n"
    "- candidate\n\n"
    "## [0.2.0] - 2026-10-30\n\n"
    "### Added\n\n"
    "- release workflow\n\n"
    "## [0.1.0] - 2026-10-12\n\n"
    "First tagged release.\n"
)


class NotesFor(unittest.TestCase):
    def test_returns_only_the_requested_section(self):
        body = notes.notes_for(CHANGELOG, "0.2.0")
        self.assertIn("release workflow", body)
        self.assertNotIn("First tagged release", body)
        self.assertNotIn("candidate", body)

    def test_last_section_runs_to_the_end(self):
        self.assertEqual(notes.notes_for(CHANGELOG, "0.1.0"), "First tagged release.")

    def test_missing_section_fails(self):
        with self.assertRaisesRegex(SystemExit, r"no ## \[9.9.9\] section"):
            notes.notes_for(CHANGELOG, "9.9.9")

    def test_empty_section_fails(self):
        with self.assertRaisesRegex(SystemExit, "empty CHANGELOG section"):
            notes.notes_for("# Changelog\n\n## [0.1.0]\n\n", "0.1.0")

    def test_final_release_never_reuses_its_release_candidate_notes(self):
        self.assertEqual(notes.notes_for(CHANGELOG, "0.3.0-rc.1"), "- candidate")
        with self.assertRaisesRegex(SystemExit, r"no ## \[0.3.0\] section"):
            notes.notes_for(CHANGELOG, "0.3.0")


if __name__ == "__main__":
    unittest.main()
