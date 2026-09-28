"""Exercise nightly release boundaries against real, temporary Git histories."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts import nightly


class NightlyTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.git("init", "-b", "master")
        self.git("config", "user.name", "Nightly test")
        self.git("config", "user.email", "nightly@example.test")
        self.first = self.commit("Initial commit")

    def git(self, *args):
        return nightly.git(*args, cwd=self.root)

    def commit(self, message):
        self.git("-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def release(self, channel, suffix, published_at="2026-09-28T01:17:00Z", draft=False):
        tag = f"nightly-{channel}-{suffix}"
        self.git("tag", tag)
        return dict(tag_name=tag, published_at=published_at, draft=draft)

    def prepare(self, pages, channel="daily"):
        return nightly.prepare(channel, "owner/repo", pages, cwd=self.root)

    def test_first_release_includes_all_commits_and_escapes_subjects(self):
        second = self.commit("Fix [links] and <html> with `code`")
        head, tag, notes = self.prepare([])
        self.assertEqual(head, second)
        self.assertTrue(tag.startswith("nightly-daily-"))
        self.assertIn(self.first, notes)
        self.assertIn(f"https://github.com/owner/repo/commit/{second}", notes)
        self.assertIn(r"Fix \[links\] and &lt;html&gt; with \`code\`", notes)

    def test_same_head_skips_release(self):
        release = self.release("daily", "previous")
        _, tag, notes = self.prepare([[release]])
        self.assertEqual(tag, "")
        self.assertIn("No new commits", notes)

    def test_cadences_have_independent_baselines_across_pages(self):
        weekly = self.release("weekly", "previous")
        second = self.commit("Daily change")
        daily = self.release("daily", "previous")
        third = self.commit("Next change")
        pages = [[daily], [weekly]]
        daily_notes = self.prepare(pages)[2]
        weekly_notes = self.prepare(pages, "weekly")[2]
        self.assertNotIn("- Daily change", daily_notes)
        self.assertIn("- Next change", daily_notes)
        self.assertIn(f"- Daily change ([`{second}`]", weekly_notes)
        self.assertIn(f"- Next change ([`{third}`]", weekly_notes)

    def test_unpublished_tags_and_drafts_do_not_advance_baseline(self):
        published = self.release("daily", "previous")
        second = self.commit("Retry this change")
        self.git("tag", "nightly-daily-unpublished")
        draft = self.release("daily", "draft", draft=True)
        notes = self.prepare([[draft, published]])[2]
        self.assertIn(f"- Retry this change ([`{second}`]", notes)

    def test_latest_published_release_wins_over_api_order(self):
        old = self.release("daily", "old", "2026-09-27T01:17:00Z")
        self.commit("Already released")
        new = self.release("daily", "new")
        self.assertEqual(self.prepare([[old], [new]])[1], "")

    def test_merged_branch_commits_are_included(self):
        previous = self.release("daily", "previous")
        self.git("checkout", "-b", "feature")
        self.commit("Feature change")
        self.git("checkout", "master")
        self.commit("Main change")
        self.git("-c", "commit.gpgsign=false", "merge", "--no-ff", "feature", "-m", "Merge feature")
        notes = self.prepare([[previous]])[2]
        for subject in ["Feature change", "Main change", "Merge feature"]:
            self.assertIn(f"- {subject}", notes)

    def test_rewritten_history_fails_without_publishing(self):
        self.commit("Previous history")
        previous = self.release("daily", "previous")
        self.git("reset", "--hard", self.first)
        self.commit("Rewritten history")
        with self.assertRaises(subprocess.CalledProcessError):
            self.prepare([[previous]])

    def test_cli_writes_notes_and_workflow_outputs(self):
        releases = self.root / "releases.json"
        releases.write_text(json.dumps([]), encoding="utf-8")
        notes = self.root / "CHANGELOG.md"
        output = self.root / "output"
        subprocess.run([
            sys.executable, str(Path(nightly.__file__).resolve()),
            "--channel", "weekly", "--repository", "owner/repo",
            "--releases", str(releases), "--notes", str(notes), "--output", str(output),
        ], cwd=self.root, check=True)
        self.assertIn("changed=true\ntag=nightly-weekly-", output.read_text())
        self.assertIn(self.first, notes.read_text())


if __name__ == "__main__":
    unittest.main()
