#!/usr/bin/env python3
"""Regressions for the two-phase Release publisher (script/publish-release-assets)."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

PUBLISHER = Path(__file__).resolve().parent / "publish-release-assets"
TAG = "zed-cn-dev-v1.2.3-r4"
DESKTOP = ["Zed-aarch64.exe", "Zed-x86_64.exe", "Zed-aarch64.dmg", "Zed-x86_64.dmg",
           "zed-linux-aarch64.tar.gz", "zed-linux-x86_64.tar.gz"]
LINUX_WINDOWS = ["Zed-aarch64.exe", "Zed-x86_64.exe",
                 "zed-linux-aarch64.tar.gz", "zed-linux-x86_64.tar.gz",
                 "bwrap-linux-aarch64.gz", "bwrap-linux-x86_64.gz",
                 "zed-remote-server-linux-aarch64.gz", "zed-remote-server-linux-x86_64.gz",
                 "zed-remote-server-windows-aarch64.zip", "zed-remote-server-windows-x86_64.zip"]
MACOS = ["Zed-aarch64.dmg", "Zed-x86_64.dmg",
         "zed-remote-server-macos-aarch64.gz", "zed-remote-server-macos-x86_64.gz"]

FAKE_GH = '''#!/usr/bin/env python3
import os, shutil, subprocess, sys
from pathlib import Path

store = Path(os.environ["GH_STORE"])
assets = store / "assets"
log = store / "gh.log"
argv = sys.argv[1:]
with open(log, "a") as handle:
    handle.write(" ".join(argv) + "\\n")


def fail(message, code=1):
    print(message, file=sys.stderr)
    sys.exit(code)


def value(flag):
    index = argv.index(flag)
    return argv[index + 1]


def release_exists():
    return (store / "release").is_file()


if not argv:
    fail("no arguments")
command = argv[0]
if command == "release":
    action = argv[1]
    tag = argv[2]
    if action == "view":
        if not release_exists():
            fail("release not found")
        if "--json" in argv:
            for name in sorted(p.name for p in assets.iterdir()):
                print(name)
        sys.exit(0)
    if action == "create":
        if release_exists():
            fail("release exists")
        assets.mkdir(parents=True, exist_ok=True)
        (store / "release").write_text(tag)
    elif action == "edit":
        if not release_exists():
            fail("release not found")
        assets.mkdir(parents=True, exist_ok=True)
    elif action == "delete-asset":
        name = argv[3]
        target = assets / name
        if not target.is_file():
            fail("asset not found")
        target.unlink()
        sys.exit(0)
    elif action == "upload":
        if not release_exists():
            fail("release not found")
        for path in argv[3:]:
            if path.startswith("-"):
                continue
            if value("--repo") == path:
                continue
            source = Path(path)
            if not source.is_file():
                fail(f"missing upload source {path}")
            shutil.copy2(source, assets / source.name)
        sys.exit(0)
    elif action == "download":
        if not release_exists():
            fail("release not found")
        directory = Path(value("--dir"))
        directory.mkdir(parents=True, exist_ok=True)
        pattern = value("--pattern")
        source = assets / pattern
        if not source.is_file():
            fail("asset not found")
        shutil.copy2(source, directory / pattern)
        sys.exit(0)
    else:
        fail(f"unsupported release action {action}")
    if "--notes-file" in argv:
        shutil.copy2(value("--notes-file"), store / "notes.md")
    sys.exit(0)
if command == "workflow":
    if os.environ.get("FAIL_WORKFLOW_DISPATCH"):
        fail("dispatch failed")
    sys.exit(0)
if command == "api":
    fields = [arg for arg in argv if arg.startswith(("ref=", "sha="))]
    values = dict(field.split("=", 1) for field in fields)
    if "ref" not in values or "sha" not in values:
        fail("missing ref or sha")
    subprocess.run(["git", "--git-dir", os.environ["FIXTURE_REMOTE"], "update-ref",
                    values["ref"], values["sha"], "0" * 40], check=True)
    sys.exit(0)
fail(f"unsupported command {command}")
'''


class PublishReleaseAssetsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="two-phase-release-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repository = self.root / "repository"
        self.remote = self.root / "remote.git"
        self.store = self.root / "release-store"
        self.store.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text(FAKE_GH)
        gh.chmod(0o755)

        self.run_command("git", "init", "--bare", str(self.remote), cwd=self.root)
        self.run_command("git", "init", str(self.repository), cwd=self.root)
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("checkout", "-b", "release")
        commit_message = "\n".join([
            "准备 Zed CN 1.2.3 发布",
            "",
            "Zed-CN-Release-Notes:",
            "## 本次更新",
            "",
            "测试说明。",
            "Zed-CN-Release-Notes-End",
            "",
        ])
        self.git("commit", "--allow-empty", "-m", commit_message)
        self.commit = self.git("rev-parse", "HEAD")
        self.git("tag", "-a", "-m", "fixture", TAG, self.commit)
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "refs/tags/" + TAG)

        # A minimal trusted-tree layout: the publisher shells out to the real
        # manifest generator, so the repository needs its script and the desktop
        # asset whitelist it validates against.
        (self.repository / "script").mkdir(exist_ok=True)
        shutil_scripts = Path(__file__).resolve().parent
        for name in ("publish-release-assets", "generate-update-manifest.py"):
            (self.repository / "script" / name).write_bytes((shutil_scripts / name).read_bytes())
        (self.repository / "script" / "publish-release-assets").chmod(0o755)
        # Run the copy inside the fixture repository: the publisher resolves the
        # manifest generator relative to its own tree.
        self.publisher = self.repository / "script" / "publish-release-assets"
        (self.repository / "release-notes.md").write_text("## 本次更新\n\n测试说明。\n")
        self.assets = self.repository / "release-artifacts"
        self.assets.mkdir()

        self.environment = dict(
            os.environ,
            PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
            GH_STORE=str(self.store),
            FIXTURE_REMOTE=str(self.remote),
            GH_REPO="fixture/repo",
            RELEASE_EVENT="push",
        )

    def run_command(self, *command, cwd=None):
        return subprocess.run(command, cwd=cwd or self.repository, check=True,
                              text=True, capture_output=True).stdout.strip()

    def git(self, *arguments):
        return self.run_command("git", *arguments)

    def add_assets(self, names):
        for name in names:
            (self.assets / name).write_bytes(b"fixture-" + name.encode())

    def publish(self, phase, extra=(), pending=None):
        arguments = [str(self.publisher), "--tag", TAG, "--commit", self.commit,
                     "--version", "1.2.3", "--revision", "4", "--channel", "dev",
                     "--notes-file", "release-notes.md", "--assets-dir", "release-artifacts",
                     "--phase", phase]
        if pending is not None:
            arguments += ["--pending", pending]
        arguments += list(extra)
        return subprocess.run(arguments, cwd=self.repository, env=self.environment,
                              text=True, capture_output=True)

    def published(self):
        directory = self.store / "assets"
        if not directory.is_dir():
            return []
        return sorted(path.name for path in directory.iterdir())

    def checksums(self):
        lines = (self.store / "assets" / "SHA256SUMS.txt").read_text().splitlines()
        return {line.split(maxsplit=1)[1]: line.split(maxsplit=1)[0] for line in lines}

    def upload_log(self):
        entries = []
        for line in (self.store / "gh.log").read_text().splitlines():
            if line.startswith("release upload "):
                entries.extend(arg for arg in line.split()[3:]
                               if not arg.startswith("-") and arg != "fixture/repo")
        return entries

    def test_fast_phase_publishes_only_finished_platforms(self):
        self.add_assets(LINUX_WINDOWS)
        result = self.publish("partial", pending="macOS（x86_64 / aarch64）")
        self.assertEqual(result.returncode, 0, result.stderr)

        published = self.published()
        self.assertIn("Zed-x86_64.exe", published)
        self.assertNotIn("Zed-x86_64.dmg", published)
        self.assertIn("update-metadata.json", published,
                      "the completion marker lets the feed advertise the phase")
        # A published checksum list may only describe uploaded files.
        self.assertEqual(set(self.checksums()), set(LINUX_WINDOWS))
        notes = (self.store / "notes.md").read_text()
        self.assertIn("正在构建的产物", notes)
        self.assertIn("macOS", notes)
        self.assertNotIn("本次未生成的产物", notes,
                         "a platform that has not finished yet is not a missing asset")

    def test_macos_phase_appends_without_republishing_finished_assets(self):
        self.add_assets(LINUX_WINDOWS)
        self.assertEqual(self.publish("partial").returncode, 0)
        marker_writes = [name for name in self.upload_log() if name.endswith("update-metadata.json")]
        self.assertEqual(len(marker_writes), 1)

        for name in LINUX_WINDOWS:
            (self.assets / name).unlink()
        self.add_assets(MACOS)
        uploads_before_second_phase = len(self.upload_log())
        result = self.publish("final")
        self.assertEqual(result.returncode, 0, result.stderr)

        second_phase_uploads = self.upload_log()[uploads_before_second_phase:]
        published = self.published()
        self.assertEqual(set(self.checksums()), set(LINUX_WINDOWS) | set(MACOS))
        for name in DESKTOP:
            self.assertIn(name, published)
        # Previously published binaries are fetched, not re-uploaded.
        for name in LINUX_WINDOWS:
            self.assertNotIn(name, second_phase_uploads,
                             f"{name} must not be uploaded again")
        notes = (self.store / "notes.md").read_text()
        self.assertNotIn("正在构建的产物", notes)
        self.assertNotIn("本次未生成的产物", notes)

    def test_marker_is_written_last_and_republished_after_each_phase(self):
        self.add_assets(LINUX_WINDOWS)
        self.assertEqual(self.publish("partial").returncode, 0)
        self.add_assets(MACOS)
        self.assertEqual(self.publish("final").returncode, 0)

        # Every marker upload has to be the final upload of its phase, and the
        # marker must be removed before any asset is replaced.
        lines = (self.store / "gh.log").read_text().splitlines()
        marker_deletes = [i for i, line in enumerate(lines)
                          if line.startswith("release delete-asset") and "update-metadata.json" in line]
        marker_uploads = [i for i, line in enumerate(lines)
                          if line.startswith("release upload") and "update-metadata.json" in line]
        self.assertEqual(len(marker_uploads), 2, "each phase republishes the marker last")
        self.assertEqual(len(marker_deletes), 1, "only the second phase finds a marker to replace")
        self.assertLess(marker_deletes[0], marker_uploads[1])
        for index in marker_uploads:
            self.assertEqual(lines[index].split()[2], TAG)
        self.assertLess(marker_uploads[0], marker_deletes[0])

    def test_rerun_is_idempotent(self):
        self.add_assets(LINUX_WINDOWS + MACOS)
        self.assertEqual(self.publish("final").returncode, 0)
        first = self.checksums()
        uploads_after_first = len(self.upload_log())
        result = self.publish("final")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.checksums(), first)
        repeated = self.upload_log()[uploads_after_first:]
        asset_uploads = [name for name in repeated if not name.endswith(("SHA256SUMS.txt",
                                                                       "update-metadata.json"))]
        self.assertEqual(asset_uploads, [], "a rerun must not rewrite published binaries")

    def test_replacing_published_bytes_fails_closed(self):
        self.add_assets(LINUX_WINDOWS)
        self.assertEqual(self.publish("partial").returncode, 0)
        (self.assets / "Zed-x86_64.exe").write_bytes(b"rebuilt")
        result = self.publish("final")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already published for", result.stderr)

    def test_unexpected_or_empty_asset_is_rejected(self):
        (self.assets / "Zed-arm64.dmg").write_bytes(b"fixture")
        result = self.publish("partial")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unexpected release asset", result.stderr)
        (self.assets / "Zed-arm64.dmg").unlink()
        (self.assets / "Zed-x86_64.exe").write_bytes(b"")
        result = self.publish("partial")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Empty release asset", result.stderr)

    def test_empty_first_phase_is_rejected(self):
        result = self.publish("partial")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No release assets", result.stderr)

    def test_missing_platform_is_reported_only_in_the_final_phase(self):
        self.add_assets(LINUX_WINDOWS)
        self.assertEqual(self.publish("final").returncode, 0)
        notes = (self.store / "notes.md").read_text()
        self.assertIn("本次未生成的产物", notes)
        self.assertIn("Zed-aarch64.dmg", notes)

    def test_tag_must_peel_to_the_validated_source(self):
        self.git("tag", "-a", "-m", "wrong", TAG + "-wrong", "HEAD")
        self.add_assets(LINUX_WINDOWS)
        arguments = [str(self.publisher), "--tag", TAG + "-wrong", "--commit", "0" * 40,
                     "--version", "1.2.3", "--revision", "4", "--channel", "dev",
                     "--notes-file", "release-notes.md", "--assets-dir", "release-artifacts",
                     "--phase", "partial"]
        result = subprocess.run(arguments, cwd=self.repository, env=self.environment,
                                text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected " + "0" * 40, result.stdout)
        self.assertEqual(self.published(), [], "a mismatched tag must not publish anything")

    def test_manual_dispatch_creates_a_missing_tag_before_publishing(self):
        self.git("push", "origin", ":refs/tags/" + TAG)
        self.add_assets(LINUX_WINDOWS)
        self.environment["RELEASE_EVENT"] = "workflow_dispatch"
        result = self.publish("partial")
        self.assertEqual(result.returncode, 0, result.stderr)
        tags = self.git("ls-remote", "origin", "refs/tags/" + TAG)
        self.assertIn(self.commit, tags)

    def test_feed_refresh_runs_after_publishing(self):
        self.add_assets(LINUX_WINDOWS)
        result = self.publish("partial", extra=["--refresh-feed"])
        self.assertEqual(result.returncode, 0, result.stderr)
        dispatches = [line for line in (self.store / "gh.log").read_text().splitlines()
                      if line.startswith("workflow run publish-update-manifest.yml")]
        self.assertEqual(len(dispatches), 1)

    def test_failed_feed_refresh_keeps_the_published_release_usable(self):
        self.add_assets(LINUX_WINDOWS)
        self.environment["FAIL_WORKFLOW_DISPATCH"] = "1"
        result = self.publish("partial", extra=["--refresh-feed"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Could not refresh the update feed", result.stdout + result.stderr)
        self.assertIn("update-metadata.json", self.published())


if __name__ == "__main__":
    unittest.main()
