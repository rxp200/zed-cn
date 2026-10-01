import copy
import importlib.util
import io
import json
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

spec = importlib.util.spec_from_file_location("manifest", Path(__file__).with_name("generate-update-manifest.py"))
manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manifest)


def metadata(revision=1):
    tag = f"zed-cn-v1.18.1-r{revision}"
    return {
        "tag_name": tag, "target_commitish": "a" * 40,
        "draft": False, "prerelease": False,
        "assets": [{"name": "Zed-x86_64.exe", "state": "uploaded", "size": 3,
                    "sha256": "b" * 64,
                    "browser_download_url": f"https://github.com/rxp200/zed-cn/releases/download/{tag}/Zed-x86_64.exe"}],
    }


def release(data):
    result = copy.deepcopy(data)
    result["assets"].append({"name": "update-metadata.json", "state": "uploaded", "size": 100})
    return result


def embedded_assets(release):
    """The released assets as embedded in the release object.

    Production reads the authoritative per-release endpoint; unit tests inject
    this loader to exercise the manifest rules without network access.
    """
    return {asset["name"]: asset for asset in release["assets"]
            if asset["state"] == "uploaded" and asset["size"] > 0}


class ManifestTests(unittest.TestCase):
    def setUp(self):
        def api(*arguments):
            self.assertEqual(arguments[:2], ("gh", "api"))
            endpoint = arguments[-1]
            if endpoint.endswith("/assets?per_page=100"):
                release_id = int(endpoint.rsplit("/", 2)[1])
                entry = next(entry for entry in self.listed_releases
                             if entry["id"] == release_id)
                return json.dumps([entry["assets"]])
            tag = endpoint.rsplit("/", 1)[1]
            return json.dumps({"ref": f"refs/tags/{tag}",
                               "object": {"type": "commit", "sha": "a" * 40}})
        self.listed_releases = []
        self.command = patch.object(manifest, "command", side_effect=api).start()
        self.addCleanup(patch.stopall)

    def test_branch_target_commitish_is_not_source_identity(self):
        data = metadata()
        entry = release(data)
        entry["target_commitish"] = "main"
        self.assertEqual(manifest.build_manifest([entry], lambda tag: data, embedded_assets)["releases"], [data])
        data["target_commitish"] = "c" * 40
        with patch("sys.stderr", new_callable=io.StringIO) as warnings:
            with self.assertRaisesRegex(ValueError, "No completed desktop"):
                manifest.build_manifest([entry], lambda tag: data, embedded_assets)
        self.assertIn("source commit differs", warnings.getvalue())

    def test_zero_desktop_skipped_with_historical_platform_fallback(self):
        windows, linux, empty = metadata(1), metadata(2), metadata(3)
        linux["assets"][0]["name"] = "zed-linux-aarch64.tar.gz"
        linux["assets"][0]["browser_download_url"] = (
            f"https://github.com/{manifest.REPOSITORY}/releases/download/"
            f"{linux['tag_name']}/zed-linux-aarch64.tar.gz")
        empty["assets"] = []
        entries = [windows, linux, empty]
        releases = [release(item) for item in entries]
        releases[-1]["assets"].append({"name": "zed-remote-server-linux-x86_64.gz",
                                      "state": "uploaded", "size": 3})
        result = manifest.build_manifest(releases, lambda tag: next(
            item for item in entries if item["tag_name"] == tag), embedded_assets)
        self.assertEqual(result["releases"], [linux, windows])
        with self.assertRaisesRegex(ValueError, "No completed desktop"):
            manifest.build_manifest([releases[-1]], lambda tag: empty, embedded_assets)

    def test_retired_intel_macos_history_is_preserved_without_new_asset(self):
        intel, arm = metadata(1), metadata(2)
        for entry, name in ((intel, "Zed-x86_64.dmg"), (arm, "Zed-aarch64.dmg")):
            entry["assets"][0]["name"] = name
            entry["assets"][0]["browser_download_url"] = (
                f"https://github.com/{manifest.REPOSITORY}/releases/download/"
                f"{entry['tag_name']}/{name}")
        entries = [intel, arm]
        result = manifest.build_manifest([release(entry) for entry in entries], lambda tag: next(
            entry for entry in entries if entry["tag_name"] == tag), embedded_assets)
        self.assertEqual(result["releases"], [arm, intel])
        self.assertEqual([asset["name"] for asset in result["releases"][0]["assets"]],
                         ["Zed-aarch64.dmg"])

    def test_feed_failure_does_not_replace_existing_output(self):
        data = metadata()
        for failure in (subprocess.CalledProcessError(1, "gh"), "not JSON",
                        json.dumps({}), json.dumps({"ref": f"refs/tags/{data['tag_name']}",
                                                   "object": {"type": "commit", "sha": "c" * 40}})):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                output = Path(temporary) / "updates.json"
                output.write_text("previous feed")
                entry = release(data)
                entry["id"] = 1
                self.listed_releases = [entry]
                self.command.side_effect = [json.dumps([[entry]]),
                                            json.dumps([entry["assets"]]),
                                            json.dumps(data), failure]
                with patch("sys.argv", ["generate-update-manifest.py", "--output", str(output)]):
                    with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                        manifest.main()
                self.assertEqual(output.read_text(), "previous feed")

    def test_failed_history_does_not_block_valid_new_release(self):
        old, new = metadata(1), metadata(6)
        for failure in (subprocess.CalledProcessError(1, "gh"),
                        subprocess.TimeoutExpired("gh", 120),
                        json.JSONDecodeError("bad JSON", "", 0)):
            def load(tag):
                if tag == old["tag_name"]:
                    raise failure
                return new
            with self.subTest(failure=failure), patch("sys.stderr", new_callable=io.StringIO) as warnings:
                result = manifest.build_manifest([release(old), release(new)], load, embedded_assets)
                self.assertEqual(result["releases"], [new])
                self.assertIn("Skipping " + old["tag_name"], warnings.getvalue())

    def test_legacy_marker_uses_release_title_and_notes_for_history(self):
        data = metadata()
        entry = release(data)
        entry["name"] = "Zed CN 1.18.1 r1"
        entry["body"] = "## 历史版本说明"

        result = manifest.build_manifest([entry], lambda tag: copy.deepcopy(data), embedded_assets)

        self.assertEqual(result["releases"][0]["title"], entry["name"])
        self.assertEqual(result["releases"][0]["release_notes"], entry["body"])

    def test_marker_notes_remain_authoritative_when_present(self):
        data = metadata()
        data["title"] = "已审核标题"
        data["release_notes"] = "已审核说明"
        entry = release(data)
        entry["name"] = "Release 标题"
        entry["body"] = "Release 说明"

        result = manifest.build_manifest([entry], lambda tag: copy.deepcopy(data), embedded_assets)

        self.assertEqual(result["releases"][0]["title"], data["title"])
        self.assertEqual(result["releases"][0]["release_notes"], data["release_notes"])

    def test_invalid_new_release_preserves_valid_history(self):
        old, new = metadata(1), metadata(6)
        for failure in ("source", "digest", "size", "url", "tag", "schema"):
            entry, marker = release(new), copy.deepcopy(new)
            if failure == "source":
                marker["target_commitish"] = "c" * 40
            elif failure == "digest":
                entry["assets"][0]["digest"] = "sha256:" + "c" * 64
            elif failure == "size":
                entry["assets"][0]["size"] = 4
            elif failure == "url":
                marker["assets"][0]["browser_download_url"] = "https://example.com/payload"
            elif failure == "tag":
                marker["tag_name"] = old["tag_name"]
            else:
                marker = {}
            with self.subTest(failure=failure), patch("sys.stderr", new_callable=io.StringIO) as warnings:
                result = manifest.build_manifest([entry, release(old)],
                    lambda tag: marker if tag == new["tag_name"] else old, embedded_assets)
                self.assertEqual(result["releases"], [old])
                self.assertIn("Skipping " + new["tag_name"], warnings.getvalue())

    def test_tag_lookup_failure_is_isolated_to_one_release(self):
        old, new = metadata(1), metadata(6)
        with patch.object(manifest, "resolve_tag_commit", side_effect=[
                subprocess.CalledProcessError(1, "gh"), "a" * 40]):
            result = manifest.build_manifest([release(old), release(new)],
                lambda tag: old if tag == old["tag_name"] else new, embedded_assets)
        self.assertEqual(result["releases"], [new])

    def test_release_listing_failure_preserves_existing_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "updates.json"
            output.write_text("previous feed")
            self.command.side_effect = subprocess.CalledProcessError(1, "gh")
            with patch("sys.argv", ["generate-update-manifest.py", "--output", str(output)]):
                with self.assertRaises(subprocess.CalledProcessError):
                    manifest.main()
            self.assertEqual(output.read_text(), "previous feed")

    def test_server_only_metadata_generation_remains_allowed(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            name = "zed-remote-server-linux-x86_64.gz"
            (directory / name).write_bytes(b"abc")
            (directory / "SHA256SUMS.txt").write_text("b" * 64 + "  " + name + "\n")
            self.assertEqual(manifest.release_metadata(
                directory, metadata()["tag_name"], "a" * 40)["assets"], [])

    def test_history_sorted_numerically_and_partial_platforms_preserved(self):
        entries = [metadata(2), metadata(10)]
        result = manifest.build_manifest([release(item) for item in entries],
                                         lambda tag: next(item for item in entries if item["tag_name"] == tag),
                                         embedded_assets)
        self.assertEqual([item["tag_name"] for item in result["releases"]],
                         ["zed-cn-v1.18.1-r10", "zed-cn-v1.18.1-r2"])
        self.assertEqual(len(result["releases"][0]["assets"]), 1)

    def test_draft_prerelease_and_incomplete_are_excluded(self):
        data = metadata()
        entries = [release(data) for _ in range(3)]
        entries[0]["draft"] = True
        entries[1]["prerelease"] = True
        entries[2]["assets"].pop()
        with self.assertRaises(ValueError):
            manifest.build_manifest(entries, lambda tag: self.fail("Must not fetch unpublished metadata"),
                                embedded_assets)

    def test_bad_asset_identity_size_digest_and_url_rejected(self):
        data = metadata()
        for field, value in (("size", 4), ("digest", "sha256:" + "c" * 64),
                             ("browser_download_url", "https://example.com/payload")):
            entry = release(data)
            entry["assets"][0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                manifest.build_manifest([entry], lambda tag: data, embedded_assets)
        entry = release(data)
        entry["assets"].pop(0)
        with self.assertRaises(ValueError):
            manifest.build_manifest([entry], lambda tag: data, embedded_assets)

    def test_metadata_validation(self):
        for field, value in (("tag_name", "../tag"), ("target_commitish", "main")):
            data = metadata()
            data[field] = value
            with self.assertRaises(ValueError):
                manifest.validate_release(data)
        for field, value in (("name", "../payload"), ("sha256", "bad"), ("size", 0)):
            data = metadata()
            data["assets"][0][field] = value
            with self.assertRaises(ValueError):
                manifest.validate_release(data)

    def test_generation_only_includes_present_desktop_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "Zed-x86_64.exe").write_bytes(b"abc")
            (directory / "SHA256SUMS.txt").write_text("b" * 64 + "  Zed-x86_64.exe\n")
            expected = metadata()
            expected["title"] = "Zed CN 1.18.1 r1"
            expected["release_notes"] = "# 本次更新\n\n- 自定义功能\n"
            self.assertEqual(
                manifest.release_metadata(
                    directory,
                    "zed-cn-v1.18.1-r1",
                    "a" * 40,
                    expected["title"],
                    expected["release_notes"],
                ),
                expected,
            )


class CommandRetryTests(unittest.TestCase):
    def test_retry_recovers_and_discards_failed_partial_output(self):
        error = subprocess.CalledProcessError(1, "gh", output="partial JSON")
        with patch.object(manifest.subprocess, "check_output", side_effect=[
                error, subprocess.TimeoutExpired("gh", 120), "complete JSON"]) as run, \
                patch.object(manifest.time, "sleep") as sleep, \
                patch("sys.stderr", new_callable=io.StringIO) as warnings:
            self.assertEqual(manifest.command("gh", "api", "endpoint"), "complete JSON")
            self.assertEqual(run.call_count, 3)
            run.assert_called_with(("gh", "api", "endpoint"), text=True, timeout=120)
            self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4])
            self.assertIn("retry 2/3", warnings.getvalue())

    def test_retries_are_bounded_for_exit_failure_and_timeout(self):
        for error in (subprocess.CalledProcessError(1, "gh"),
                      subprocess.TimeoutExpired("gh", 120)):
            with self.subTest(error=error), \
                    patch.object(manifest.subprocess, "check_output", side_effect=error) as run, \
                    patch.object(manifest.time, "sleep") as sleep:
                with self.assertRaises(type(error)):
                    manifest.command("gh", "api", "endpoint")
                self.assertEqual(run.call_count, 4)
                self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4, 8])

    def test_success_and_missing_executable_do_not_retry(self):
        with patch.object(manifest.subprocess, "check_output", return_value="ok") as run, \
                patch.object(manifest.time, "sleep") as sleep:
            self.assertEqual(manifest.command("gh", "api", "endpoint"), "ok")
            run.assert_called_once()
            sleep.assert_not_called()
        with patch.object(manifest.subprocess, "check_output", side_effect=FileNotFoundError) as run, \
                patch.object(manifest.time, "sleep") as sleep:
            with self.assertRaises(FileNotFoundError):
                manifest.command("gh", "api", "endpoint")
            run.assert_called_once()
            sleep.assert_not_called()


class ResolveTagCommitTests(unittest.TestCase):
    tag = "zed-cn-v1.18.1-r1"

    def reference(self, kind="commit", sha="a" * 40):
        return {"ref": f"refs/tags/{self.tag}", "object": {"type": kind, "sha": sha}}

    def test_lightweight_and_nested_annotated_tags(self):
        for depth in (0, 1, 2, 8):
            objects = [f"{index + 1:040x}" for index in range(depth)]
            responses = [self.reference("tag", objects[0]) if depth else self.reference()]
            for index, sha in enumerate(objects):
                target = {"type": "tag", "sha": objects[index + 1]} if index + 1 < depth else {
                    "type": "commit", "sha": "a" * 40}
                responses.append({"sha": sha, "object": target})
            with self.subTest(depth=depth), patch.object(manifest, "command", side_effect=[
                    json.dumps(item) for item in responses]) as command:
                self.assertEqual(manifest.resolve_tag_commit(self.tag), "a" * 40)
                self.assertEqual(command.call_count, depth + 1)
                self.assertEqual(command.call_args_list[0].args, (
                    "gh", "api", f"repos/{manifest.REPOSITORY}/git/ref/tags/{self.tag}"))
                for call, sha in zip(command.call_args_list[1:], objects):
                    self.assertEqual(call.args, ("gh", "api", f"repos/{manifest.REPOSITORY}/git/tags/{sha}"))

    def test_missing_malformed_and_noncommit_targets(self):
        responses = [None, [], {}, {"ref": "refs/tags/other"},
                     {"ref": f"refs/tags/{self.tag}"},
                     self.reference("tree"), self.reference(sha="bad"),
                     self.reference(sha=None)]
        for response in responses:
            with self.subTest(response=response), patch.object(manifest, "command", return_value=json.dumps(response)):
                with self.assertRaises(ValueError):
                    manifest.resolve_tag_commit(self.tag)
        for response in (None, {}, {"sha": "b" * 40}, {"sha": "a" * 40, "object": []}):
            with self.subTest(annotated=response), patch.object(manifest, "command", side_effect=[
                    json.dumps(self.reference("tag")), json.dumps(response)]):
                with self.assertRaises(ValueError):
                    manifest.resolve_tag_commit(self.tag)

    def test_api_errors_and_invalid_json_propagate(self):
        for depth in (0, 1):
            for failure in (subprocess.CalledProcessError(1, "gh"), "not JSON"):
                responses = [json.dumps(self.reference("tag"))] * depth + [failure]
                with self.subTest(depth=depth, failure=failure), patch.object(manifest, "command", side_effect=responses):
                    with self.assertRaises((subprocess.CalledProcessError, ValueError)):
                        manifest.resolve_tag_commit(self.tag)

    def test_cycles_and_excessive_depth_are_bounded(self):
        with patch.object(manifest, "command", side_effect=[json.dumps(self.reference("tag")),
                json.dumps({"sha": "a" * 40, "object": {"type": "tag", "sha": "a" * 40}})]) as command:
            with self.assertRaisesRegex(ValueError, "cycle or depth"):
                manifest.resolve_tag_commit(self.tag)
            self.assertEqual(command.call_count, 2)
        responses = [self.reference("tag", f"{1:040x}")]
        responses.extend({"sha": f"{index:040x}", "object": {"type": "tag", "sha": f"{index + 1:040x}"}}
                         for index in range(1, 9))
        with patch.object(manifest, "command", side_effect=[json.dumps(item) for item in responses]) as command:
            with self.assertRaisesRegex(ValueError, "cycle or depth"):
                manifest.resolve_tag_commit(self.tag)
            self.assertEqual(command.call_count, 9)


if __name__ == "__main__":
    unittest.main()


class AssetListingTests(unittest.TestCase):
    """The releases list embedded ``assets`` array must not decide completeness.

    A published release once reported an empty embedded asset array for a long
    time while every asset was present, so the newest revision silently vanished
    from the update feed. The per-release asset endpoint is authoritative.
    """

    tag = "zed-cn-v1.20.2-r12"
    commit = "b" * 40

    def entry(self):
        data = metadata(1)
        data["tag_name"] = self.tag
        data["target_commitish"] = self.commit
        data["assets"] = [dict(data["assets"][0], browser_download_url=(
            f"https://github.com/{manifest.REPOSITORY}/releases/download/"
            f"{self.tag}/Zed-x86_64.exe"))]
        return data

    def assets(self, entry, marker=True):
        assets = [dict(asset, browser_download_url=(
            f"https://github.com/{manifest.REPOSITORY}/releases/download/"
            f"{self.tag}/{asset['name']}")) for asset in entry["assets"]]
        if marker:
            assets.append({"name": manifest.MARKER_ASSET, "state": "uploaded", "size": 100})
        return assets

    def build(self, embedded, assets):
        data = self.entry()
        release_entry = release(data)
        release_entry["id"] = 7
        release_entry["assets"] = embedded
        errors = io.StringIO()

        def api(*arguments):
            self.assertEqual(arguments[:2], ("gh", "api"))
            endpoint = arguments[-1]
            if endpoint.endswith("/assets?per_page=100"):
                return json.dumps([assets])
            if endpoint.endswith("releases?per_page=100"):
                return json.dumps([[release_entry]])
            return json.dumps({"ref": f"refs/tags/{self.tag}",
                               "object": {"type": "commit", "sha": self.commit}})

        with patch.object(manifest, "command", side_effect=api), \
                patch("sys.stderr", new_callable=io.StringIO) as warnings:
            try:
                result = manifest.build_manifest(
                    [release_entry], lambda tag: data, manifest.uploaded_assets)
                entries = result["releases"]
            except ValueError:
                # An empty feed is refused; the caller inspects the warnings.
                entries = None
        return entries, warnings.getvalue()

    def test_empty_embedded_asset_list_does_not_drop_the_release(self):
        entries, _ = self.build(embedded=[], assets=self.assets(self.entry()))
        self.assertEqual([entry["tag_name"] for entry in entries], [self.tag])

    def test_empty_embedded_asset_list_is_reported(self):
        _, warnings = self.build(embedded=[], assets=self.assets(self.entry()))
        self.assertIn("using the per-release asset listing", warnings)

    def test_missing_completion_marker_warns_instead_of_skipping_silently(self):
        # An incomplete release is skipped loudly, and refusing to publish an
        # empty feed still surfaces the reason instead of succeeding silently.
        entries, warnings = self.build(
            embedded=[], assets=self.assets(self.entry(), marker=False))
        self.assertIsNone(entries)
        self.assertIn(f"WARNING: Skipping {self.tag}", warnings)
        self.assertIn(manifest.MARKER_ASSET, warnings)
