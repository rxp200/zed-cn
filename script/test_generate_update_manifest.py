import copy
import importlib.util
import tempfile
import unittest
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


if __name__ == "__main__":
    unittest.main()
