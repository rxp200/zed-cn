#!/usr/bin/env python3
"""Guard the production Markdown API used by the deep explanation outline."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DeepExplanationProductionApiTests(unittest.TestCase):
    def test_parsed_markdown_accessor_is_not_feature_gated(self):
        source = (ROOT / "crates/markdown/src/markdown.rs").read_text()
        match = re.search(
            r"((?:\s*#\[[^\n]+\]\s*)*)pub fn parsed_markdown\(&self\) -> &ParsedMarkdown",
            source,
        )
        self.assertIsNotNone(match, "the outline needs a borrowed parsed Markdown snapshot")
        self.assertNotIn("cfg", match.group(1), "production outlines cannot depend on test-support")

    def test_outline_uses_the_existing_parse_without_enabling_test_support(self):
        source = (ROOT / "crates/editor/src/code_explanations.rs").read_text()
        self.assertIn("deep_explanation_outline(self.markdown.read(cx).parsed_markdown())", source)
        manifest = (ROOT / "crates/editor/Cargo.toml").read_text().split("[dev-dependencies]", 1)[0]
        entry = re.search(r"^markdown(?:\.workspace)?\s*=.*$", manifest, re.MULTILINE)
        self.assertIsNotNone(entry)
        self.assertNotIn("test-support", entry.group())

    def test_release_checks_the_api_before_platform_builds(self):
        workflow = (ROOT / ".github/workflows/build-windows-release.yml").read_text()
        validation = workflow.split("  validate_source:", 1)[1].split("  bundle_linux:", 1)[0]
        self.assertIn("run: python3 script/test_deep_explanation_production_api.py", validation)


if __name__ == "__main__":
    unittest.main()
