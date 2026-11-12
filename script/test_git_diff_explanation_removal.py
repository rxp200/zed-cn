"""Source guards for removed Git AI hooks; GPUI tests cover normal diff behavior."""

import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class GitDiffExplanationRemovalTests(unittest.TestCase):
    def test_git_views_have_no_explanation_module_or_hooks(self):
        self.assertFalse((ROOT / "crates/git_ui/src/diff_explanations.rs").exists())
        for name in ("git_ui", "diff_multibuffer", "commit_view"):
            source = (ROOT / f"crates/git_ui/src/{name}.rs").read_text()
            for obsolete in (
                "mod diff_explanations;",
                "DiffExplanationController",
                "schedule_explanations",
                "render_explanation_controls",
                "explanation_controller",
                "DiffFileInput",
                "DiffHunkInput",
            ):
                with self.subTest(view=name, obsolete=obsolete):
                    self.assertNotIn(obsolete, source)

    def test_git_only_title_is_removed_but_editor_settings_remain(self):
        for locale in ("en", "zh-Hans"):
            catalog = json.loads((ROOT / f"locales/{locale}.json").read_text())
            self.assertNotIn("089c5937ddac41ed", catalog)
            for retained in ("16ece1acc00be44a", "769a2f0db1f64f03", "0f16310584adb487"):
                self.assertIn(retained, catalog)
        defaults = (ROOT / "assets/settings/default.json").read_text()
        self.assertIn('"code_explanations": {', defaults)
        settings_page = (ROOT / "crates/settings_ui/src/page_data.rs").read_text()
        self.assertIn("code_explanations_section()", settings_page)
        self.assertIn('json_path: Some("code_explanations.enabled")', settings_page)

    def test_editor_annotations_deep_explanations_and_scans_remain(self):
        source = (ROOT / "crates/editor/src/code_explanations.rs").read_text()
        for retained in (
            "pub struct CodeExplanationSettings",
            "pub(crate) fn schedule(",
            "virtual_annotations_do_not_change_buffer",
            "deep_request_accepts_default_detail_and_streams",
            "sqlite_round_trip_and_eviction",
            "selected_scan_paths_include_files_and_recursive_directories",
        ):
            self.assertIn(retained, source)
        documentation = (ROOT / "docs/src/code-explanations.md").read_text()
        self.assertNotIn("## Git 差异讲解", documentation)
        self.assertIn("深入讲解选中代码", documentation)
        self.assertIn("扫描当前项目并预生成讲解", documentation)


if __name__ == "__main__":
    unittest.main()
