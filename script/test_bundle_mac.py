#!/usr/bin/env python3
"""Exercise macOS packaging order without requiring Apple's build tools."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("bundle-mac").read_text()


class BundleMacTests(unittest.TestCase):
    def run_packaging(self, *, profile="release", local_install=False, sentry=False, fail_bundle=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "crates/zed"
            crate.mkdir(parents=True)
            manifest = "[package.metadata.bundle-stable]\n"
            (crate / "Cargo.toml").write_text(manifest)
            binaries = root / f"target/aarch64-apple-darwin/{profile}"
            binaries.mkdir(parents=True)
            for binary in ("zed", "cli", "remote_server"):
                (binaries / binary).write_text("original\n")
            # The final Remote Server archive still uses the release path in debug mode.
            release = root / "target/aarch64-apple-darwin/release"
            release.mkdir(exist_ok=True)
            (release / "remote_server").touch(exist_ok=True)
            prelude = r'''
set -euo pipefail
build_flag=--release
target_triple=aarch64-apple-darwin
arch_suffix=aarch64
channel=stable
dsymutil() { echo "dsym $2" >> events; cp "$2" "$2.dwarf"; }
strip() { echo "strip $2" >> events; printf 'stripped\n' > "$2"; }
sentry-cli() { echo sentry >> events; }
cargo() {
    test "${CARGO_BUNDLE_SKIP_BUILD:-}" = true
    echo bundle >> "$ROOT/events"
    if [[ "$FAIL_BUNDLE" = true ]]; then return 1; fi
    local app="$ROOT/target/app with spaces/Zed.app"
    mkdir -p "$app/Contents/MacOS"
    cp "$ROOT/target/$target_triple/$target_dir/zed" "$app/Contents/MacOS/zed"
    echo "$app"
}
sign_app_binaries() {
    cmp "target/$target_triple/$target_dir/zed" "$app_path/Contents/MacOS/zed"
    cmp "target/$target_triple/$target_dir/cli" "$app_path/Contents/MacOS/cli"
    echo sign >> events
}
sign_binary() { :; }
'''
            environment = dict(os.environ, ROOT=str(root), target_dir=profile,
                               local_install=str(local_install).lower(),
                               FAIL_BUNDLE=str(fail_bundle).lower(),
                               SENTRY_AUTH_TOKEN="test" if sentry else "")
            result = subprocess.run(
                ["bash", "-c", prelude + SCRIPT[SCRIPT.index("debug_symbols_extracted=false"):]],
                cwd=root, env=environment, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1 if fail_bundle else 0, result.stderr)
            self.assertEqual((crate / "Cargo.toml").read_text(), manifest)
            self.assertFalse((crate / "Cargo.toml.backup").exists())
            events = (root / "events").read_text().splitlines()
            should_extract = sentry and not local_install or profile == "release" and not local_install
            self.assertEqual(sum(event.startswith("dsym ") for event in events), 2 if should_extract else 0)
            for index, event in enumerate(events):
                if event.startswith(("dsym ", "strip ")) or event == "sentry":
                    self.assertLess(index, events.index("bundle"))
            self.assertEqual(events.count("bundle"), 1)
            self.assertEqual("sign" in events, not fail_bundle)
            if profile == "release" and not local_install:
                self.assertEqual((binaries / "zed").read_text(), "stripped\n")
                self.assertEqual((binaries / "zed.dwarf").read_text(), "original\n")

    def test_release_without_sentry(self):
        self.run_packaging()

    def test_sentry_extracts_once(self):
        self.run_packaging(sentry=True)

    def test_local_install_does_not_strip(self):
        self.run_packaging(local_install=True)

    def test_debug_does_not_strip(self):
        self.run_packaging(profile="debug")

    def test_bundle_failure_restores_manifest(self):
        self.run_packaging(fail_bundle=True)


if __name__ == "__main__":
    unittest.main()
