#!/usr/bin/env python3
"""Prepare and validate a frozen, path-complete release-note review."""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from urllib.request import Request, urlopen

TAG_PATTERN = re.compile(r"zed-cn-v(\d+)\.(\d+)\.(\d+)-r([1-9]\d*)")
HEADINGS = ["本次更新", "功能", "改进", "错误修复", "重大变更与通知"]


def command(*arguments):
    return subprocess.run(arguments, check=True, capture_output=True, text=True).stdout.strip()


def git(*arguments):
    return command("git", *arguments)


def tag_key(tag):
    match = TAG_PATTERN.fullmatch(tag)
    if not match:
        raise ValueError(f"Invalid release tag: {tag}")
    return tuple(map(int, match.groups()))


def block(message, name):
    matches = re.findall(rf"(?ms)^{name}:\n(.*?)\n{name}-End$", message)
    if len(matches) != 1:
        raise ValueError(f"Expected exactly one {name} block")
    return matches[0]


def changed_paths(base, target):
    arguments = ["diff", "--no-renames", "--name-only", "-z", base]
    arguments += ["--cached"] if target == ":index" else [target]
    # Protected policy overlays are not application changes. Version/lockfile
    # changes remain in the review, including when advancing official Stable.
    arguments += ["--", ".", ":(exclude).rules", ":(exclude)AGENTS.md"]
    output = subprocess.run(["git", *arguments], check=True, capture_output=True, text=True).stdout
    return sorted(filter(None, output.split("\0")))


def select_previous(releases, release_tag, previous=None):
    current = tag_key(release_tag)
    eligible = [release for release in releases if not release.get("draft")
                and not release.get("prerelease")
                and release.get("published_at")
                and TAG_PATTERN.fullmatch(release.get("tag_name", ""))
                and tag_key(release["tag_name"]) < current]
    if previous:
        eligible = [release for release in eligible if release["tag_name"] == previous]
    if not eligible:
        raise ValueError("No previous published Stable Release found; check the repository or explicit baseline")
    return max(eligible, key=lambda release: tag_key(release["tag_name"]))


def validate_page(page):
    if not isinstance(page, list) or any(not isinstance(item, dict) for item in page):
        raise ValueError("GitHub Releases response must be an array of objects")
    return page


def query_releases(repository):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("Release repository must be owner/name")
    try:
        response = subprocess.run(
            ["gh", "api", "--hostname", "github.com", "--paginate",
             f"repos/{repository}/releases?per_page=100"],
            check=True, capture_output=True, text=True, timeout=120,
        ).stdout
        if not response.strip():
            raise ValueError("Empty gh response")
        releases = []
        decoder = json.JSONDecoder()
        while response.strip():
            page, end = decoder.raw_decode(response.lstrip())
            releases.extend(validate_page(page))
            response = response.lstrip()[end:]
        return releases
    except (OSError, subprocess.SubprocessError, ValueError) as error:
        # Do not print CLI output: authentication failures can contain sensitive
        # environment or credential details. The HTTPS attempt starts afresh.
        print(f"gh query unavailable ({type(error).__name__}); trying GitHub HTTPS API.", file=sys.stderr)

    headers = {"Accept": "application/vnd.github+json", "User-Agent": "zed-cn-release-review",
               "X-GitHub-Api-Version": "2022-11-28"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    releases = []
    for page_number in range(1, 1001):
        request = Request(
            f"https://api.github.com/repos/{repository}/releases?per_page=100&page={page_number}",
            headers=headers,
        )
        with urlopen(request, timeout=30) as response:
            page = validate_page(json.load(response))
        releases.extend(page)
        if len(page) < 100:
            return releases
    raise ValueError("GitHub Release pagination exceeded 1000 pages; refusing incomplete history")


def resolve_previous(release_tag, repository, previous=None):
    # Query Releases rather than Tags: failed builds with only a Tag must not
    # consume the changes that users have not received yet.
    release = select_previous(query_releases(repository), release_tag, previous)
    tag = release["tag_name"]
    git("fetch", f"https://github.com/{repository}.git",
        f"refs/tags/{tag}:refs/tags/{tag}", "--no-tags")
    return release, git("rev-parse", f"refs/tags/{tag}^{{commit}}")


def make_review(release_tag, previous_tag, previous_commit, paths):
    return {"schema_version": 1, "release_tag": release_tag,
            "previous_tag": previous_tag, "previous_commit": previous_commit,
            "target_tree": git("write-tree"),
            "items": [{"path": path, "disposition": "pending", "detail": ""} for path in paths]}


def review_block(review):
    return "Zed-CN-Release-Review:\n" + json.dumps(review, ensure_ascii=False, indent=2) + "\nZed-CN-Release-Review-End\n"


def validate(message, target, release_tag):
    notes = block(message, "Zed-CN-Release-Notes")
    if "<!--" in notes or not re.search(r"[\u4e00-\u9fff]", notes):
        raise ValueError("Release notes must contain reviewed Chinese text, not placeholders")
    for heading in HEADINGS:
        if f"## {heading}" not in notes.splitlines():
            raise ValueError(f"Missing heading: {heading}")
    review = json.loads(block(message, "Zed-CN-Release-Review"))
    if review["schema_version"] != 1 or review["release_tag"] != release_tag:
        raise ValueError("Review schema or release tag does not match")
    if tag_key(review["previous_tag"]) >= tag_key(release_tag):
        raise ValueError("Baseline must precede the new release")
    base = review["previous_commit"]
    if not re.fullmatch(r"[0-9a-f]{40}", base):
        raise ValueError("Baseline must be a full commit SHA")
    if git("rev-parse", f"refs/tags/{review['previous_tag']}^{{commit}}") != base:
        raise ValueError("Frozen baseline SHA differs from its tag")
    target_tree = git("write-tree") if target == ":index" else git("rev-parse", f"{target}^{{tree}}")
    if review["target_tree"] != target_tree:
        raise ValueError("Reviewed tree changed; review every changed path again")
    expected = changed_paths(base, target)
    items = review["items"]
    paths = [item["path"] for item in items]
    if sorted(paths) != expected or len(set(paths)) != len(paths):
        raise ValueError("Review must cover every changed path exactly once; regenerate after source changes")
    for item in items:
        detail = item["detail"].strip()
        if item["disposition"] in ("included", "merged"):
            if not detail.startswith("- ") or detail not in notes.splitlines():
                raise ValueError(f"{item['path']}: detail must quote an exact release-note bullet")
        elif item["disposition"] == "internal":
            if len(detail) < 8 or "<!--" in detail:
                raise ValueError(f"{item['path']}: explain why users need no release note")
        else:
            raise ValueError(f"{item['path']}: unresolved review item")
    return notes, review


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--message", type=Path, help="uncommitted message file; defaults to HEAD message")
    parser.add_argument("--target", default="HEAD", help="commit or :index for staged release tree")
    parser.add_argument("--release-tag", required=True)
    arguments = parser.parse_args()
    message = arguments.message.read_text() if arguments.message else git("show", "-s", "--format=%B", "HEAD")
    notes, review = validate(message, arguments.target, arguments.release_tag)
    print(f"Reviewed {review['previous_tag']} → {review['release_tag']}: {len(review['items'])} changed paths")
    print(notes)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as output:
            output.write(f"## Release notes preview\n\n{review['previous_tag']} → {review['release_tag']}\n\n{notes}\n")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"Release review failed: {error}", file=sys.stderr)
        sys.exit(1)
