import copy
import json
import io
import os
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

import release_notes as notes


class ReleaseReviewTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="zed-release-review-test-")
        self.addCleanup(self.temporary.cleanup)
        previous_directory = Path.cwd()
        os.chdir(self.temporary.name)
        self.addCleanup(os.chdir, previous_directory)
        self.git("init", "-q")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "test@example.invalid")
        Path("file.rs").write_text("old\n")
        self.git("add", ".")
        self.git("commit", "-qm", "baseline")
        self.base = self.git("rev-parse", "HEAD")
        self.previous = "zed-cn-v1.18.1-r5"
        self.current = "zed-cn-v1.18.2-r1"
        self.git("tag", self.previous)
        Path("file.rs").write_text("new\n")
        self.git("add", ".")
        self.review = notes.make_review(self.current, self.previous, self.base, ["file.rs"])
        self.prose = "\n\n".join(f"## {heading}\n\n- 修复文件显示问题。" for heading in notes.HEADINGS)

    def git(self, *arguments):
        return subprocess.check_output(["git", *arguments], text=True).strip()

    def message(self, review=None):
        return f"Release\n\nZed-CN-Release-Notes:\n{self.prose}\nZed-CN-Release-Notes-End\n\n" + notes.review_block(review or self.review)

    def resolve(self):
        self.review["items"][0].update(disposition="included", detail="- 修复文件显示问题。")

    def test_pending_blocks_release(self):
        with self.assertRaisesRegex(ValueError, "unresolved"):
            notes.validate(self.message(), ":index", self.current)

    def test_linked_note_validates_staged_and_committed_tree(self):
        self.resolve()
        notes.validate(self.message(), ":index", self.current)
        self.git("commit", "-qm", self.message())
        notes.validate(self.git("show", "-s", "--format=%B"), "HEAD", self.current)

    def test_new_source_path_invalidates_review(self):
        self.resolve()
        Path("new.rs").write_text("new\n")
        self.git("add", ".")
        with self.assertRaisesRegex(ValueError, "Reviewed tree changed"):
            notes.validate(self.message(), ":index", self.current)

    def test_duplicate_and_missing_paths_rejected(self):
        self.resolve()
        for items in ([], self.review["items"] * 2):
            review = copy.deepcopy(self.review)
            review["items"] = items
            with self.assertRaises(ValueError):
                notes.validate(self.message(review), ":index", self.current)

    def test_unlinked_bullet_rejected(self):
        self.resolve()
        self.review["items"][0]["detail"] = "- 不存在的条目。"
        with self.assertRaisesRegex(ValueError, "exact release-note bullet"):
            notes.validate(self.message(), ":index", self.current)

    def test_internal_requires_reason(self):
        self.review["items"][0].update(disposition="internal", detail="")
        with self.assertRaises(ValueError):
            notes.validate(self.message(), ":index", self.current)
        self.review["items"][0]["detail"] = "仅调整测试夹具，不改变用户可见行为。"
        notes.validate(self.message(), ":index", self.current)

    def test_frozen_tag_identity(self):
        self.resolve()
        self.git("commit", "-qm", "changed")
        self.git("tag", "-f", self.previous)
        with self.assertRaisesRegex(ValueError, "Frozen baseline"):
            notes.validate(self.message(), "HEAD", self.current)

    def test_published_selection_across_versions_and_override(self):
        releases = [{"tag_name": tag, "published_at": "2026-09-08", **flags} for tag, flags in [
            (self.previous, {}), ("zed-cn-v1.18.2-r1", {}),
            ("zed-cn-v1.18.2-r2", {"draft": True}),
            ("zed-cn-v1.18.2-r3", {"prerelease": True}),
            ("zed-cn-v1.18.2-r4", {"published_at": None})]]
        self.assertEqual(notes.select_previous(releases, "zed-cn-v1.18.2-r5")["tag_name"], "zed-cn-v1.18.2-r1")
        self.assertEqual(notes.select_previous(releases, self.current)["tag_name"], self.previous)
        self.assertEqual(notes.select_previous(releases, "zed-cn-v1.18.2-r5", self.previous)["tag_name"], self.previous)
        with self.assertRaises(ValueError):
            notes.select_previous([], self.current)

    def test_paginated_cli_resolves_and_fetches_selected_tag(self):
        pages = json.dumps([{"tag_name": self.previous, "published_at": "today"}]) + "\n[]"
        with patch.object(notes.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, pages)) as command_mock:
            with patch.object(notes, "git", side_effect=["", self.base]) as git_mock:
                release, commit = notes.resolve_previous(self.current, "rxp200/zed-cn")
        self.assertEqual(release["tag_name"], self.previous)
        self.assertEqual(commit, self.base)
        self.assertNotIn("--slurp", command_mock.call_args.args[0])
        self.assertIn(f"refs/tags/{self.previous}:refs/tags/{self.previous}", git_mock.call_args_list[0].args)

    def test_https_fallback_pagination_and_token_precedence(self):
        with patch.object(notes.subprocess, "run", side_effect=FileNotFoundError):
            with patch.dict(os.environ, {"GH_TOKEN": "first", "GITHUB_TOKEN": "second"}, clear=True):
                with patch.object(notes, "urlopen", side_effect=[io.StringIO(json.dumps([{}] * 100)), io.StringIO("[]")]) as request:
                    self.assertEqual(len(notes.query_releases("rxp200/zed-cn")), 100)
        self.assertEqual(request.call_count, 2)
        self.assertTrue(request.call_args.args[0].full_url.endswith("page=2"))
        self.assertEqual(request.call_args.args[0].get_header("Authorization"), "Bearer first")
        self.assertEqual(request.call_args.kwargs["timeout"], 30)

    def test_failed_or_invalid_gh_falls_back_anonymously(self):
        for error in [subprocess.CalledProcessError(1, "gh"), subprocess.TimeoutExpired("gh", 120), ValueError("invalid")]:
            with self.subTest(error=type(error).__name__):
                with patch.object(notes.subprocess, "run", side_effect=error):
                    with patch.dict(os.environ, {}, clear=True):
                        with patch.object(notes, "urlopen", return_value=io.StringIO("[]")) as request:
                            self.assertEqual(notes.query_releases("rxp200/zed-cn"), [])
                self.assertIsNone(request.call_args.args[0].get_header("Authorization"))

    def test_https_failure_never_guesses_baseline(self):
        with patch.object(notes.subprocess, "run", side_effect=FileNotFoundError):
            with patch.object(notes, "urlopen", side_effect=OSError("network unavailable")):
                with self.assertRaises(OSError):
                    notes.resolve_previous(self.current, "rxp200/zed-cn")
        with self.assertRaises(ValueError):
            notes.validate_page({"message": "rate limited"})

    def test_same_path_content_change_invalidates_review(self):
        self.resolve()
        Path("file.rs").write_text("changed again\n")
        self.git("add", ".")
        with self.assertRaisesRegex(ValueError, "Reviewed tree changed"):
            notes.validate(self.message(), ":index", self.current)

    def test_deleted_and_renamed_paths_are_covered(self):
        Path("file.rs").rename("renamed.rs")
        self.git("add", "-A")
        self.assertEqual(notes.changed_paths(self.base, ":index"), ["file.rs", "renamed.rs"])

    def test_wrong_release_and_placeholders_rejected(self):
        self.resolve()
        with self.assertRaises(ValueError):
            notes.validate(self.message(), ":index", "zed-cn-v1.18.2-r2")
        self.prose += "\n<!-- TODO -->"
        with self.assertRaises(ValueError):
            notes.validate(self.message(), ":index", self.current)


if __name__ == "__main__":
    unittest.main()
