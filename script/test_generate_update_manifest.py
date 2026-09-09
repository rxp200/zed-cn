import copy
import importlib.util
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


class ManifestTests(unittest.TestCase):
    def setUp(self):
        def api(*arguments):
            self.assertEqual(arguments[:2], ("gh", "api"))
            tag = arguments[2].rsplit("/", 1)[1]
            return json.dumps({"ref": f"refs/tags/{tag}",
                               "object": {"type": "commit", "sha": "a" * 40}})
        self.command = patch.object(manifest, "command", side_effect=api).start()
        self.addCleanup(patch.stopall)

    def test_branch_target_commitish_is_not_source_identity(self):
        data = metadata()
        entry = release(data)
        entry["target_commitish"] = "main"
        self.assertEqual(manifest.build_manifest([entry], lambda tag: data)["releases"], [data])
        data["target_commitish"] = "c" * 40
        with self.assertRaisesRegex(ValueError, "source commit differs"):
            manifest.build_manifest([entry], lambda tag: data)

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
            item for item in entries if item["tag_name"] == tag))
        self.assertEqual(result["releases"], [linux, windows])
        with self.assertRaisesRegex(ValueError, "No completed desktop"):
            manifest.build_manifest([releases[-1]], lambda tag: empty)

    def test_feed_failure_does_not_replace_existing_output(self):
        data = metadata()
        for failure in (subprocess.CalledProcessError(1, "gh"), "not JSON",
                        json.dumps({}), json.dumps({"ref": f"refs/tags/{data['tag_name']}",
                                                   "object": {"type": "commit", "sha": "c" * 40}})):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                output = Path(temporary) / "updates.json"
                output.write_text("previous feed")
                self.command.side_effect = [json.dumps([[release(data)]]), json.dumps(data), failure]
                with patch("sys.argv", ["generate-update-manifest.py", "--output", str(output)]):
                    with self.assertRaises((ValueError, subprocess.CalledProcessError)):
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
                                         lambda tag: next(item for item in entries if item["tag_name"] == tag))
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
            manifest.build_manifest(entries, lambda tag: self.fail("Must not fetch unpublished metadata"))

    def test_bad_asset_identity_size_digest_and_url_rejected(self):
        data = metadata()
        for field, value in (("size", 4), ("digest", "sha256:" + "c" * 64),
                             ("browser_download_url", "https://example.com/payload")):
            entry = release(data)
            entry["assets"][0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                manifest.build_manifest([entry], lambda tag: data)
        entry = release(data)
        entry["assets"].pop(0)
        with self.assertRaises(ValueError):
            manifest.build_manifest([entry], lambda tag: data)

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
            self.assertEqual(manifest.release_metadata(directory, "zed-cn-v1.18.1-r1", "a" * 40), metadata())


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
