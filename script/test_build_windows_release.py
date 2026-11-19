#!/usr/bin/env python3
"""Local source-binding regressions for build-windows-release.yml (requires PyYAML)."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/build-windows-release.yml"
PUBLISHER = ROOT / "script/publish-release-assets"
JOBS = yaml.safe_load(WORKFLOW.read_text())["jobs"]
SOURCE = "${{ needs.validate_source.outputs.source_sha }}"


class ReleaseSourceBindingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="release-source-binding-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repository = self.root / "repository"
        self.remote = self.root / "remote.git"
        self.run_command("git", "init", "--bare", str(self.remote), cwd=self.root)
        self.run_command("git", "init", str(self.repository), cwd=self.root)
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("commit", "--allow-empty", "-m", "validated source")
        self.source = self.git("rev-parse", "HEAD")
        self.git("branch", "manual-source")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "HEAD:refs/heads/manual-source")
        self.git("commit", "--allow-empty", "-m", "advanced source")
        self.advanced = self.git("rev-parse", "HEAD")
        self.git("push", "origin", "HEAD:refs/heads/manual-source")
        self.environment = dict(os.environ, SOURCE_SHA=self.source,
                                EXPECTED_SHA=self.source, RELEASE_TAG="zed-cn-v1.2.3-r1",
                                RELEASE_EVENT="workflow_dispatch", GH_REPO="fixture/repo")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text('''#!/usr/bin/env python3
import os, subprocess, sys
from pathlib import Path
with open(os.environ['GH_LOG'], 'a') as log:
    log.write(' '.join(sys.argv[1:]) + '\\n')
if os.environ.get('FAIL_CREATE'):
    sys.exit(1)
if sys.argv[1:4] != ['api', '--method', 'POST']:
    sys.exit(2)
fields = [arg for arg in sys.argv if arg.startswith(('ref=', 'sha='))]
values = dict(field.split('=', 1) for field in fields)
subprocess.run(['git', '--git-dir', os.environ['FIXTURE_REMOTE'], 'update-ref',
                values['ref'], values['sha'], '0' * 40], check=True)
''')
        gh.chmod(0o755)
        self.environment.update(PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                                GH_LOG=str(self.root / "gh.log"), FIXTURE_REMOTE=str(self.remote))

    def run_command(self, *command, cwd=None):
        return subprocess.run(command, cwd=cwd or self.repository, check=True,
                              text=True, capture_output=True).stdout.strip()

    def git(self, *arguments):
        return self.run_command("git", *arguments)

    def shell(self, script):
        return subprocess.run(["bash", "-euo", "pipefail", "-c", script],
                              cwd=self.repository, env=self.environment,
                              text=True, capture_output=True)

    def tag_guard(self):
        publisher = PUBLISHER.read_text()
        start = publisher.index("verify_release_tag() {")
        end = publisher.index("\n}\n", start) + len("\n}\n")
        return ("set -euo pipefail\n"
                "tag=\"$RELEASE_TAG\"; commit=\"$SOURCE_SHA\"; repository=\"$GH_REPO\"\n"
                + publisher[start:end]
                + "verify_release_tag create-if-missing\n")

    def test_all_downstream_checkouts_pin_and_assert_before_other_steps(self):
        checked = []
        for name, job in JOBS.items():
            if name == "validate_source":
                continue
            for index, step in enumerate(job["steps"]):
                if step.get("uses", "").startswith("actions/checkout@"):
                    checked.append(name)
                    self.assertEqual(step["with"]["ref"], SOURCE)
                    guard = job["steps"][index + 1]
                    self.assertEqual(guard["name"], "Assert validated source checkout")
                    self.assertEqual(guard["env"]["EXPECTED_SHA"], SOURCE)
                    self.assertEqual(guard["shell"], "bash")
                    # The mutable branch has advanced to B; pinned A still passes.
                    self.git("checkout", "--detach", self.source)
                    self.assertEqual(self.shell(guard["run"]).returncode, 0)
                    self.git("checkout", "--detach", self.advanced)
                    failure = self.shell(guard["run"])
                    self.assertNotEqual(failure.returncode, 0)
                    self.assertIn("does not match validated source", failure.stdout)
        self.assertEqual(set(checked), {"bundle_linux", "bundle_macos", "bundle_windows",
                                        "publish_fast_platforms", "publish_macos"})
        self.assertNotIn("source_ref", JOBS["validate_source"]["outputs"])

    def test_desktop_builds_embed_validated_custom_release_tag(self):
        for name in ("bundle_linux", "bundle_macos", "bundle_windows"):
            self.assertEqual(JOBS[name]["env"]["ZED_CUSTOM_RELEASE_TAG"],
                             "${{ needs.validate_source.outputs.release_tag }}")
            self.assertEqual(JOBS[name]["env"]["ZED_COMMIT_SHA"], SOURCE)

    def test_source_validation_runs_the_publisher_regression(self):
        step = next(step for step in JOBS["validate_source"]["steps"]
                    if step["name"] == "Test the release publisher")
        self.assertEqual(step["run"], "python3 -m unittest script.test_publish_release_assets")

    def test_source_validation_checks_license_layout(self):
        step = next(step for step in JOBS["validate_source"]["steps"]
                    if step["name"] == "Validate open-source license layout")
        self.assertEqual(step["run"], "./script/check-licenses")

    def test_release_notes_link_exact_source_and_licenses(self):
        for job in ("publish_fast_platforms", "publish_macos"):
            step = next(step for step in JOBS[job]["steps"]
                        if step["name"] == "Write release notes body")
            self.assertIn("https://github.com/$GH_REPO/tree/$SOURCE_SHA", step["run"])
            for text in (
                "## 源码与许可证",
                "blob/$SOURCE_SHA/LICENSE-GPL",
                "blob/$SOURCE_SHA/LICENSE-APACHE",
                "blob/$SOURCE_SHA/MODIFICATIONS.md",
                "各平台的产物在完成构建并通过校验后陆续发布到本 Release",
            ):
                self.assertIn(text, step["run"])

    def test_matching_lightweight_and_annotated_tags(self):
        for annotated in (False, True):
            with self.subTest(annotated=annotated):
                tag = self.environment["RELEASE_TAG"] + ("-annotated" if annotated else "-lightweight")
                self.environment["RELEASE_TAG"] = tag
                arguments = ["tag"] + (["-a", "-m", "fixture"] if annotated else [])
                self.git(*arguments, tag, self.source)
                self.git("push", "origin", "refs/tags/" + tag)
                self.assertEqual(self.shell(self.tag_guard()).returncode, 0)
        self.assertFalse((self.root / "gh.log").exists())

    def test_mismatched_tag_is_rejected_without_mutation(self):
        tag = self.environment["RELEASE_TAG"]
        self.git("tag", "-a", "-m", "wrong source", tag, self.advanced)
        self.git("push", "origin", "refs/tags/" + tag)
        before = self.git("ls-remote", "origin")
        result = self.shell(self.tag_guard())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected " + self.source, result.stdout)
        self.assertEqual(self.git("ls-remote", "origin"), before)
        self.assertFalse((self.root / "gh.log").exists())

    def test_missing_manual_tag_is_created_at_validated_source(self):
        result = self.shell(self.tag_guard())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(self.source + "\trefs/tags/" + self.environment["RELEASE_TAG"],
                      self.git("ls-remote", "origin"))
        self.assertEqual(len((self.root / "gh.log").read_text().splitlines()), 1)

    def test_missing_push_tag_and_remote_failure_are_not_creation_permission(self):
        self.environment["RELEASE_EVENT"] = "push"
        self.assertNotEqual(self.shell(self.tag_guard()).returncode, 0)
        self.environment["RELEASE_EVENT"] = "workflow_dispatch"
        self.git("remote", "set-url", "origin", str(self.root / "nonexistent"))
        self.assertNotEqual(self.shell(self.tag_guard()).returncode, 0)
        self.assertFalse((self.root / "gh.log").exists())

    def test_failed_or_racing_creation_fails_closed(self):
        self.environment["FAIL_CREATE"] = "1"
        self.assertNotEqual(self.shell(self.tag_guard()).returncode, 0)
        self.assertNotIn("refs/tags/", self.git("ls-remote", "origin"))

    def test_manual_tag_creation_retains_nonrecursive_workflow_token(self):
        for job in ("publish_fast_platforms", "publish_macos"):
            step = next(step for step in JOBS[job]["steps"]
                        if step["name"] == "Publish finished assets")
            self.assertEqual(step["env"]["GH_TOKEN"], "${{ github.token }}")
        publisher = PUBLISHER.read_text()
        self.assertIn('gh api --method POST "repos/$repository/git/refs"', publisher)

    def test_each_release_upload_rechecks_tag_and_partial_policy_is_preserved(self):
        publisher = PUBLISHER.read_text()
        lines = publisher.splitlines()
        uploads = [index for index, line in enumerate(lines) if line.startswith("gh release upload ")]
        self.assertEqual(len(uploads), 2)
        for index in uploads:
            self.assertEqual(lines[index - 1], "verify_release_tag")
        for name in ("bundle_linux", "bundle_macos", "bundle_windows", "build_static_bwrap"):
            self.assertTrue(JOBS[name]["continue-on-error"])
            self.assertFalse(JOBS[name]["strategy"]["fail-fast"])
            expected_count = 2
            self.assertEqual(len(JOBS[name]["strategy"]["matrix"]["include"]), expected_count)

    def test_macos_builds_both_architectures(self):
        self.assertEqual(JOBS["bundle_macos"]["strategy"]["matrix"]["include"], [
            {"arch": "aarch64", "target": "aarch64-apple-darwin", "runner": "macos-15"},
            {"arch": "x86_64", "target": "x86_64-apple-darwin", "runner": "macos-15-intel"},
        ])

    def test_asset_aggregation_lives_in_the_publisher(self):
        publisher = PUBLISHER.read_text()
        expected = {
            "bwrap-linux-aarch64.gz", "bwrap-linux-x86_64.gz",
            "Zed-aarch64.dmg", "Zed-x86_64.dmg", "Zed-aarch64.exe", "Zed-x86_64.exe",
            "zed-linux-aarch64.tar.gz", "zed-linux-x86_64.tar.gz",
            "zed-remote-server-linux-aarch64.gz", "zed-remote-server-linux-x86_64.gz",
            "zed-remote-server-macos-aarch64.gz", "zed-remote-server-macos-x86_64.gz",
            "zed-remote-server-windows-aarch64.zip", "zed-remote-server-windows-x86_64.zip",
        }
        for name in expected:
            self.assertIn(name, publisher)
        # One source of truth: the workflow must not keep a second asset list or
        # its own checksum generation that could drift from the publisher.
        workflow = WORKFLOW.read_text()
        self.assertNotIn("sha256sum", workflow)
        for name in expected:
            self.assertNotIn(name, workflow)
        for job, phase in (("publish_fast_platforms", "--phase partial"),
                           ("publish_macos", "--phase final")):
            step = next(step for step in JOBS[job]["steps"]
                        if step["name"] == "Publish finished assets")
            self.assertIn("./script/publish-release-assets", step["run"])
            self.assertIn(phase, step["run"])
            self.assertIn("--refresh-feed", step["run"])

    def test_phases_are_serialised_and_scan_every_platform(self):
        fast = JOBS["publish_fast_platforms"]
        macos = JOBS["publish_macos"]
        self.assertEqual(fast["needs"], ["validate_source", "bundle_linux",
                                         "build_static_bwrap", "bundle_windows"])
        self.assertEqual(macos["needs"], ["validate_source", "publish_fast_platforms",
                                          "bundle_macos"])
        for job in (fast, macos):
            self.assertEqual(job["concurrency"]["group"],
                             "publish-${{ needs.validate_source.outputs.release_tag }}")
            self.assertFalse(job["concurrency"]["cancel-in-progress"])
            self.assertEqual(job["permissions"]["contents"], "write")
            self.assertEqual(job["permissions"]["actions"], "write")
        # The phases must not overlap: a macOS artifact that finishes early may not
        # slip into the partial phase, and the append phase only needs macOS.
        fast_patterns = [step["with"]["pattern"] for step in fast["steps"]
                         if step.get("with", {}).get("pattern")]
        macos_patterns = [step["with"]["pattern"] for step in macos["steps"]
                          if step.get("with", {}).get("pattern")]
        self.assertTrue(fast_patterns and macos_patterns)
        for pattern in macos_patterns:
            self.assertNotIn(pattern, fast_patterns)
            self.assertTrue("dmg" in pattern or "macos" in pattern, pattern)
        for pattern in fast_patterns:
            self.assertNotIn("dmg", pattern)
            self.assertNotIn("macos", pattern)

if __name__ == "__main__":
    unittest.main()
