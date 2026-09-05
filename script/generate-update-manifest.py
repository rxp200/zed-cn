#!/usr/bin/env python3
"""Generate the public desktop update feed from completed release metadata."""

import argparse
import json
import re
import subprocess
from pathlib import Path

REPOSITORY = "rxp200/zed-cn"
TAG = re.compile(r"zed-cn-v(\d+)\.(\d+)\.(\d+)-r(\d+)")
ASSETS = {
    f"{prefix}{arch}{suffix}"
    for arch in ("x86_64", "aarch64")
    for prefix, suffix in (("Zed-", ".exe"), ("Zed-", ".dmg"), ("zed-linux-", ".tar.gz"))
}


def command(*arguments):
    return subprocess.check_output(arguments, text=True)


def validate_release(release):
    match = TAG.fullmatch(release["tag_name"])
    if not match or int(match.group(4)) == 0:
        raise ValueError("Invalid release tag")
    if not re.fullmatch(r"[0-9a-f]{40}", release["target_commitish"]):
        raise ValueError("Invalid source commit")
    names = set()
    for asset in release["assets"]:
        name = asset["name"]
        if name not in ASSETS or name in names:
            raise ValueError("Unexpected or duplicate desktop asset")
        names.add(name)
        expected_url = f"https://github.com/{REPOSITORY}/releases/download/{release['tag_name']}/{name}"
        if (asset["browser_download_url"] != expected_url
                or asset["state"] != "uploaded" or asset["size"] <= 0
                or not re.fullmatch(r"[0-9a-f]{64}", asset["sha256"])):
            raise ValueError("Invalid desktop asset metadata")
    return release


def release_metadata(directory, tag, commit):
    checksums = {}
    for line in (directory / "SHA256SUMS.txt").read_text().splitlines():
        checksum, name = line.split(maxsplit=1)
        checksums[name.lstrip(" *")] = checksum
    assets = []
    for name in sorted(ASSETS):
        path = directory / name
        if path.is_file():
            assets.append({
                "name": name, "state": "uploaded", "size": path.stat().st_size,
                "sha256": checksums[name],
                "browser_download_url": f"https://github.com/{REPOSITORY}/releases/download/{tag}/{name}",
            })
    return validate_release({
        "tag_name": tag, "target_commitish": commit,
        "draft": False, "prerelease": False, "assets": assets,
    })


def build_manifest(releases, load_metadata):
    result = []
    for release in releases:
        if release["draft"] or release["prerelease"] or not TAG.fullmatch(release["tag_name"]):
            continue
        uploaded = {asset["name"]: asset for asset in release["assets"]
                    if asset["state"] == "uploaded" and asset["size"] > 0}
        # This marker is uploaded last, after all binaries and checksums succeed.
        if "update-metadata.json" not in uploaded:
            continue
        metadata = validate_release(load_metadata(release["tag_name"]))
        if metadata["tag_name"] != release["tag_name"]:
            raise ValueError("Metadata tag mismatch")
        if metadata["draft"] or metadata["prerelease"]:
            raise ValueError("Unexpected unpublished metadata")
        for asset in metadata["assets"]:
            actual = uploaded.get(asset["name"])
            if actual is None or actual["size"] != asset["size"] or actual["browser_download_url"] != asset["browser_download_url"]:
                raise ValueError("Release asset differs from completed metadata")
            digest = actual.get("digest")
            if digest and digest != f"sha256:{asset['sha256']}":
                raise ValueError("Release asset digest differs from completed metadata")
        result.append(metadata)
    result.sort(key=lambda item: tuple(map(int, TAG.fullmatch(item["tag_name"]).groups())), reverse=True)
    if not result:
        raise ValueError("No completed desktop update metadata; refusing to replace the feed")
    return {"schema_version": 1, "releases": result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-directory", type=Path)
    parser.add_argument("--tag")
    parser.add_argument("--commit")
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    if arguments.release_directory:
        result = release_metadata(arguments.release_directory, arguments.tag, arguments.commit)
    else:
        pages = json.loads(command("gh", "api", "--paginate", "--slurp", f"repos/{REPOSITORY}/releases?per_page=100"))
        releases = [release for page in pages for release in page]
        result = build_manifest(releases, lambda tag: json.loads(command(
            "gh", "release", "download", tag, "--repo", REPOSITORY,
            "--pattern", "update-metadata.json", "--output", "-",
        )))
    content = json.dumps(result, ensure_ascii=False, indent=2) + "\n"
    if len(content.encode("utf-8")) > 8 * 1024 * 1024:
        raise ValueError("Update manifest exceeds the client size limit")
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(content)


if __name__ == "__main__":
    main()
