#!/usr/bin/env python3

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class LicenseComplianceTests(unittest.TestCase):
    def test_desktop_bundles_ship_project_and_third_party_notices(self):
        linux = (ROOT / "script/bundle-linux").read_text()
        macos = (ROOT / "script/bundle-mac").read_text()
        windows = (ROOT / "script/bundle-windows.ps1").read_text()

        required = ("LICENSE-GPL", "LICENSE-APACHE", "MODIFICATIONS.md", "THIRD-PARTY-LICENSES.md")
        for name in required:
            with self.subTest(platform="linux", name=name):
                self.assertIn(name, linux)
            with self.subTest(platform="macos", name=name):
                self.assertIn(name, macos)
            with self.subTest(platform="windows", name=name):
                self.assertIn(name, windows)

    def test_windows_stable_distribution_uses_fork_identity(self):
        bundler = (ROOT / "script/bundle-windows.ps1").read_text()
        installer = (ROOT / "crates/zed/resources/windows/zed.iss").read_text()
        self.assertIn('$appPublisher = "Zed CN contributors"', bundler)
        self.assertIn('$appPublisherUrl = "https://github.com/rxp200/zed-cn"', bundler)
        self.assertIn('AppPublisher={#AppPublisher}', installer)
        self.assertIn('Source: "{#ResourcesDir}\\licenses\\*"', installer)
        self.assertIn('LicenseFile: "script\\terms\\zed-cn-license-notice.rtf"', installer)
        self.assertNotIn('LicenseFile: "script\\terms\\terms.rtf"', installer)

    def test_modified_apache_sources_have_file_notice(self):
        modification_notice = "Modified by the Zed CN project, 2026. See MODIFICATIONS.md."
        files = (
            "crates/alacritty_terminal/src/event_loop.rs",
            "crates/gpui/src/app.rs",
            "crates/gpui/src/elements/div.rs",
            "crates/gpui/src/platform/threaded_dispatcher.rs",
            "crates/gpui/src/window.rs",
            "crates/gpui_linux/src/linux/dispatcher.rs",
            "crates/gpui_linux/src/linux/platform.rs",
            "crates/gpui_windows/src/platform.rs",
            "crates/gpui_windows/src/window.rs",
            "crates/util/src/path_list.rs",
        )
        modifications = (ROOT / "MODIFICATIONS.md").read_text()
        for name in files:
            with self.subTest(name=name):
                self.assertIn(modification_notice, (ROOT / name).read_text()[:2000])
                self.assertIn(f"`{name}`", modifications)


if __name__ == "__main__":
    unittest.main()
