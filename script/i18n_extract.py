#!/usr/bin/env python3
"""抽取 Zed CN 的用户可见文本，生成统一的多语言目录。

用法:
    script/i18n_extract.py                  # 全量抽取（增量，带缓存）
    script/i18n_extract.py <file> ...       # 只抽取指定文件
    script/i18n_extract.py --rebuild        # 忽略缓存全量重建

产物（locales/ 为唯一事实来源，翻译在此编辑）:
    zh-Hans.json   简体中文（默认语言）
    en.json        英文原文（优先从翻译前的历史 blob 恢复）
    meta.json      条目上下文（文件:行号、UI/噪声分类、来源方式）

条目 key 取规范化中文（无中文时退化为英文）的 SHA-256 前 16 位，
保证同一段文本全仓库共用一个条目，并让上游同步造成的漂移可被检测。
"""

from __future__ import annotations

import difflib
import re
import json
import subprocess
import sys
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from i18n_lib import (  # noqa: E402
    ROOT,
    blob_at,
    candidate_text,
    classify_occurrence,
    git,
    is_han,
    literal_key,
    scan_literals,
    skeleton,
    unescape,
)

LOCALES_DIR = ROOT / "locales"
CACHE_DIR = LOCALES_DIR / ".cache"
BLOB_LIST_CACHE = CACHE_DIR / "blobs.json"


def tracked_blobs() -> dict[str, str]:
    """一次性拿到所有 .rs 文件的 blob sha，用于增量缓存。"""
    if BLOB_LIST_CACHE.exists():
        cached = json.loads(BLOB_LIST_CACHE.read_text())
        if cached.get("rev") == current_rev():
            return cached["blobs"]
    out = git("ls-files", "-s", "crates")
    blobs = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 4 and parts[3].endswith(".rs"):
            blobs[parts[3]] = parts[1]
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    BLOB_LIST_CACHE.write_text(
        json.dumps({"rev": current_rev(), "blobs": blobs})
    )
    return blobs


def current_rev() -> str:
    return git("rev-parse", "HEAD").strip()


def rev_history_with_paths(path: str):
    """返回 [(rev, path_at_rev)]，新→旧，跟踪重命名。"""
    out = git("log", "--follow", "--name-status", "--format=@@@%H", "--", path)
    mapping = []
    cur = None
    for line in out.splitlines():
        if line.startswith("@@@"):
            cur = line[3:]
        elif line.strip() and cur:
            parts = line.split("\t")
            status = parts[0]
            if status.startswith("R") and len(parts) >= 3:
                mapping.append((cur, parts[2]))
            elif status[:1] in ("A", "M", "D") and len(parts) >= 2:
                mapping.append((cur, parts[1]))
    return mapping


def find_translation_baseline(path: str):
    """在历史中找出最后一个英文版本（翻译提交的父版本）。"""
    revs = rev_history_with_paths(path)
    if not revs:
        return None, None
    newest = blob_at(revs[0][0], revs[0][1])
    if newest is None or not is_han(newest):
        return None, None
    lo, hi, ans = 0, len(revs) - 1, 0
    while lo <= hi:
        mid = (lo + hi) // 2
        text = blob_at(revs[mid][0], revs[mid][1])
        if text is not None and is_han(text):
            ans, lo = mid, mid + 1
        else:
            hi = mid - 1
    if ans + 1 >= len(revs):
        return None, None
    base_rev, base_path = revs[ans + 1]
    return blob_at(base_rev, base_path), base_rev


def pair_baseline(baseline: str, current: str) -> dict[str, str]:
    """序列对齐 diff，只在 replace 块内按行配对，得到 {中文: 英文}。"""
    pairs = {}
    base_lines = baseline.splitlines()
    cur_lines = current.splitlines()
    matcher = difflib.SequenceMatcher(a=base_lines, b=cur_lines, autojunk=False)
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag != "replace":
            continue
        old_block = base_lines[i1:i2]
        new_block = cur_lines[j1:j2]
        if len(old_block) != len(new_block):
            continue
        for bl, cl in zip(old_block, new_block):
            if skeleton(bl) != skeleton(cl):
                continue
            base_lits = [unescape(raw) for raw, _ in scan_literals(bl)[0]]
            cur_lits = [unescape(raw) for raw, _ in scan_literals(cl)[0]]
            if len(base_lits) != len(cur_lits):
                continue
            for en, zh in zip(base_lits, cur_lits):
                if is_han(zh) and not is_han(en) and zh != en:
                    pairs[zh] = en
    return pairs


def process_file(path: str, force: bool = False, from_ref: str | None = None):
    """扫描单文件，返回该文件的字面量出现（分类在聚合阶段做，便于调整规则）。"""
    blob_sha = tracked_blobs().get(path) if from_ref is None else None
    cache_file = CACHE_DIR / "files" / f"{'{' + from_ref + '}' if from_ref else blob_sha or 'wt'}.json"
    if not force and cache_file.exists():
        return json.loads(cache_file.read_text())

    if from_ref:
        text = blob_at(from_ref, path) or ""
    else:
        text = (ROOT / path).read_text(errors="ignore")
    if not text:
        return {"pairs": {}, "sites": []}
    baseline, _rev = find_translation_baseline(path)
    pairs = pair_baseline(baseline, text) if baseline else {}

    lines = text.splitlines()
    sites = []
    in_block_comment = False
    for index, line in enumerate(lines):
        literals, in_block_comment = scan_literals(line, in_block_comment)
        for raw, _col in literals:
            value = unescape(raw)
            if not candidate_text(value):
                continue
            sites.append(
                {
                    "line": index + 1,
                    "value": value,
                    "en": pairs.get(value, ""),
                    "crate": path.split("/")[1] if path.startswith("crates/") else "",
                }
            )
    result = {"pairs": pairs, "sites": sites}
    if from_ref is None:
        cache_file.parent.mkdir(parents=True, exist_ok=True)
        cache_file.write_text(json.dumps(result, ensure_ascii=False))
    return result


def main():
    argv = sys.argv[1:]
    force = "--rebuild" in argv
    from_ref = None
    if "--from-ref" in argv:
        ref_at = argv.index("--from-ref")
        from_ref = argv[ref_at + 1]
        argv = argv[:ref_at] + argv[ref_at + 2 :]
    args = [a for a in argv if not a.startswith("--")]
    if args:
        files = [str(Path(a).resolve().relative_to(ROOT)) for a in args]
    elif from_ref:
        out = git("ls-tree", "-r", "--name-only", from_ref, "--", "crates")
        files = [l for l in out.splitlines() if l.endswith(".rs")]
        blobs = {}
    else:
        blobs = tracked_blobs()
        files = sorted(blobs)

    with ThreadPoolExecutor(max_workers=16) as pool:
        results = list(pool.map(lambda p: process_file(p, force, from_ref), files))

    if from_ref:
        globals()["tracked_blobs"] = lambda: {}
    catalog: dict[str, dict] = {}
    context: dict[str, list] = defaultdict(list)
    ui_sites: dict[str, int] = defaultdict(int)

    for path, result in zip(files, results):
        if from_ref:
            lines = (blob_at(from_ref, path) or "").splitlines()
        else:
            lines = (ROOT / path).read_text(errors="ignore").splitlines()
        for index, site in enumerate(result["sites"]):
            if classify_occurrence(path, lines, site["line"] - 1, lines[site["line"] - 1], site["value"]) != "ui":
                continue
            value = site["value"]
            en = site.get("en", "")
            key = literal_key(value)
            entry = catalog.setdefault(
                key, {"zh": "", "en": "", "crate": site.get("crate", "")}
            )
            if is_han(value):
                entry["zh"] = value
                if en and not entry["en"]:
                    entry["en"] = en
            elif not entry["en"]:
                entry["en"] = value
            if not entry["crate"]:
                entry["crate"] = site.get("crate", "")
            ui_sites[key] += 1
            ref = f"{path}:{site['line']}"
            if ref not in context[key]:
                context[key].append(ref)

    # JSON 资源（keymap / 默认设置）
    for path in sorted(
        str(p.relative_to(ROOT)) for p in (ROOT / "assets").rglob("*.json")
    ):
        text = (ROOT / path).read_text(errors="ignore")
        key_re = re.compile(r'"((?:[^"\\])*)"\s*:')
        for lineno, line in enumerate(text.splitlines(), 1):
            keys = key_re.findall(line)
            for raw in re.findall(r'"((?:[^"\\])*)"', line):
                value = raw
                if not is_han(value) or value in keys:
                    continue
                key = literal_key(value)
                entry = catalog.setdefault(key, {"zh": "", "en": "", "crate": "assets"})
                entry["zh"] = value
                if not entry["crate"]:
                    entry["crate"] = "assets"
                ui_sites[key] += 1
                ref = f"{path}:{lineno}"
                if ref not in context[key]:
                    context[key].append(ref)

    LOCALES_DIR.mkdir(exist_ok=True)
    write_locale("zh-Hans.json", {k: v["zh"] for k, v in catalog.items() if v["zh"]})
    write_locale("en.json", {k: v["en"] for k, v in catalog.items() if v["en"]})
    (LOCALES_DIR / "meta.json").write_text(
        json.dumps(
            {
                "format_version": 1,
                "entries": catalog,
                "context": dict(context),
                "ui_sites": dict(ui_sites),
            },
            ensure_ascii=False,
            indent=2,
        )
        + "\n"
    )

    stats = {
        "files_scanned": len(files),
        "entries": len(catalog),
        "with_en": sum(1 for v in catalog.values() if v["en"]),
        "with_zh": sum(1 for v in catalog.values() if v["zh"]),
        "bilingual": sum(1 for v in catalog.values() if v["en"] and v["zh"]),
        "zh_only": sum(1 for v in catalog.values() if v["zh"] and not v["en"]),
        "en_only": sum(1 for v in catalog.values() if v["en"] and not v["zh"]),
        "ui_sites": sum(ui_sites.values()),
        "ui_entries": len(ui_sites),
    }
    print(json.dumps(stats, indent=2))




def write_locale(name: str, mapping: dict):
    ordered = dict(sorted(mapping.items()))
    (LOCALES_DIR / name).write_text(
        json.dumps(ordered, ensure_ascii=False, indent=2) + "\n"
    )


if __name__ == "__main__":
    main()
