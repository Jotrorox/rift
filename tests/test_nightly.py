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
        return nightly.prepare(channel, pages, cwd=self.root)

    def test_first_release_tags_current_head(self):
        second = self.commit("Next change")
        head, tag = self.prepare([])
        self.assertEqual(head, second)
        self.assertTrue(tag.startswith("nightly-daily-"))
        self.assertTrue(tag.endswith(second[:12]))

    def test_same_head_skips_release(self):
        release = self.release("daily", "previous")
        _, tag = self.prepare([[release]])
        self.assertEqual(tag, "")

    def test_cadences_have_independent_baselines_across_pages(self):
        weekly = self.release("weekly", "previous")
        self.commit("Daily change")
        daily = self.release("daily", "previous")
        pages = [[daily], [weekly]]
        self.assertEqual(self.prepare(pages)[1], "")
        self.assertTrue(self.prepare(pages, "weekly")[1].startswith("nightly-weekly-"))
        third = self.commit("Next change")
        for channel in ["daily", "weekly"]:
            head, tag = self.prepare(pages, channel)
            self.assertEqual(head, third)
            self.assertTrue(tag.startswith(f"nightly-{channel}-"))

    def test_unpublished_tags_and_drafts_do_not_advance_baseline(self):
        published = self.release("daily", "previous")
        second = self.commit("Retry this change")
        self.git("tag", "nightly-daily-unpublished")
        draft = self.release("daily", "draft", draft=True)
        head, tag = self.prepare([[draft, published]])
        self.assertEqual(head, second)
        self.assertTrue(tag.endswith(second[:12]))

    def test_latest_published_release_wins_over_api_order(self):
        old = self.release("daily", "old", "2026-09-27T01:17:00Z")
        self.commit("Already released")
        new = self.release("daily", "new")
        self.assertEqual(self.prepare([[old], [new]])[1], "")

    def test_merged_branch_triggers_release(self):
        previous = self.release("daily", "previous")
        self.git("checkout", "-b", "feature")
        self.commit("Feature change")
        self.git("checkout", "master")
        self.commit("Main change")
        self.git("-c", "commit.gpgsign=false", "merge", "--no-ff", "feature", "-m", "Merge feature")
        head, tag = self.prepare([[previous]])
        self.assertEqual(head, self.git("rev-parse", "HEAD"))
        self.assertTrue(tag.endswith(head[:12]))

    def test_rewritten_history_fails_without_publishing(self):
        self.commit("Previous history")
        previous = self.release("daily", "previous")
        self.git("reset", "--hard", self.first)
        self.commit("Rewritten history")
        with self.assertRaises(subprocess.CalledProcessError):
            self.prepare([[previous]])

    def test_cli_writes_only_workflow_outputs(self):
        releases = self.root / "releases.json"
        releases.write_text(json.dumps([]), encoding="utf-8")
        output = self.root / "output"
        subprocess.run([
            sys.executable, str(Path(nightly.__file__).resolve()),
            "--channel", "weekly", "--releases", str(releases), "--output", str(output),
        ], cwd=self.root, check=True)
        self.assertIn("changed=true\ntag=nightly-weekly-", output.read_text())
        self.assertEqual({path.name for path in self.root.iterdir()},
                         {".git", "releases.json", "output"})


if __name__ == "__main__":
    unittest.main()
