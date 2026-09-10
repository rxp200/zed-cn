#!/usr/bin/env python3
"""Local source-binding regressions for build-windows-release.yml (requires PyYAML)."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/build-windows-release.yml"
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
        step = next(step for step in JOBS["publish_release"]["steps"]
                    if step["name"] == "Create or update GitHub Release")
        return step["run"].split('notes_path="release-notes.md"')[0]

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
        self.assertEqual(set(checked), {"bundle_linux", "bundle_macos", "bundle_windows", "publish_release"})
        self.assertNotIn("source_ref", JOBS["validate_source"]["outputs"])

    def test_desktop_builds_embed_validated_custom_release_tag(self):
        for name in ("bundle_linux", "bundle_macos", "bundle_windows"):
            self.assertEqual(JOBS[name]["env"]["ZED_CUSTOM_RELEASE_TAG"],
                             "${{ needs.validate_source.outputs.release_tag }}")
            self.assertEqual(JOBS[name]["env"]["ZED_COMMIT_SHA"], SOURCE)

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
        publisher = JOBS["publish_release"]["steps"][-1]
        self.assertEqual(publisher["env"]["GH_TOKEN"], "${{ github.token }}")
        self.assertIn('gh api --method POST "repos/$GH_REPO/git/refs"', publisher["run"])

    def test_each_release_upload_rechecks_tag_and_partial_policy_is_preserved(self):
        step = JOBS["publish_release"]["steps"][-1]
        lines = step["run"].splitlines()
        uploads = [index for index, line in enumerate(lines) if line.startswith("gh release upload ")]
        self.assertEqual(len(uploads), 2)
        for index in uploads:
            self.assertEqual(lines[index - 1], "verify_release_tag")
        self.assertIn("verify_release_tag\nrm -f release-artifacts/MISSING-ASSETS.txt", step["run"])
        for name in ("bundle_linux", "bundle_macos", "bundle_windows", "build_static_bwrap"):
            self.assertTrue(JOBS[name]["continue-on-error"])
            self.assertFalse(JOBS[name]["strategy"]["fail-fast"])
            self.assertEqual(len(JOBS[name]["strategy"]["matrix"]["include"]), 2)


if __name__ == "__main__":
    unittest.main()
