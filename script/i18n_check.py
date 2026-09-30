#!/usr/bin/env python3
"""i18n 目录一致性与覆盖率检查（CI 门禁）。

用法:
    script/i18n_check.py            # 全量检查，发现问题返回非 0
    script/i18n_check.py --summary  # 只打印统计

检查项:
    1. 代码中的每个 t!(key) 都必须存在于目录；缺 key 是错误；
    2. 同一 key 的中英文占位符集合必须一致；不一致是错误；
    3. 目录条目缺某一语言时记为待翻译（警告，可用 --strict 升级为错误）；
    4. 代码中残留的中文 UI 字面量与英文 UI 文案记为未迁移（警告/统计）；
    5. 目录中没有代码引用的条目记为孤儿（警告）。
"""

from __future__ import annotations

import json
import re
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from i18n_lib import (  # noqa: E402
    ROOT,
    candidate_text,
    classify_occurrence,
    is_han,
    scan_literals,
)

LOCALES = ROOT / "locales"
T_CALL_RE = re.compile(r"""
    i18n::t(?:_args|_in)?!\s*\(
    \s*(?:"([0-9a-f]{16})"|([A-Za-z_][A-Za-z0-9_:.]*))
    (?P<rest>[^)]*)
""", re.VERBOSE)


def load_locale(name: str) -> dict[str, str]:
    path = LOCALES / name
    return json.loads(path.read_text()) if path.exists() else {}


def placeholders(text: str) -> set[str]:
    names = set(re.findall(r"\{([A-Za-z_][A-Za-z0-9_]*)\}", text))
    if "{}" in text:
        names.add("<positional>")
    return names


def scan_code():
    """返回 (代码引用的 key 集合, 残留的中文 UI 字面量, 英文 UI 文案候选, 文本出现统计)。

    `occurrences` 记录每段文本在 UI 站点与被跳过站点（测试、日志、属性、夹具等）
    出现的次数，用于区分"应迁移"与"有意跳过"的孤儿条目。
    """
    used: set[str] = set()
    han_left: dict[str, list[str]] = defaultdict(list)
    en_left: dict[str, list[str]] = defaultdict(list)
    occurrences: dict[str, dict[str, int]] = defaultdict(lambda: {"ui": 0, "skipped": 0})
    in_block_comment = False
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        text = path.read_text(errors="ignore")
        lines = text.splitlines()
        rel = str(path.relative_to(ROOT))
        for index, line in enumerate(lines):
            if line.lstrip().startswith(("//", "/*", "*", "///")):
                continue  # 注释与文档示例不算引用
            for match in T_CALL_RE.finditer(line):
                if match.group(1):
                    used.add(match.group(1))
            literals, in_block_comment = scan_literals(line, in_block_comment)
            for raw, _col in literals:
                from i18n_lib import unescape

                value = unescape(raw)
                if not candidate_text(value):
                    continue
                if classify_occurrence(rel, lines, index, line, value) != "ui":
                    occurrences[value]["skipped"] += 1
                    continue
                occurrences[value]["ui"] += 1
                ref = f"{rel}:{index + 1}"
                if is_han(value):
                    han_left[value].append(ref)
                else:
                    en_left[value].append(ref)
    return used, han_left, en_left, occurrences


def main():
    summary_only = "--summary" in sys.argv
    strict = "--strict" in sys.argv

    zh = load_locale("zh-Hans.json")
    en = load_locale("en.json")
    used, han_left, en_left, occurrences = scan_code()

    errors: list[str] = []
    warnings: list[str] = []

    for key in sorted(used):
        if key not in zh and key not in en:
            errors.append(f"缺失 key: {key} 不在任何语言目录中")

    for key in sorted(set(zh) & set(en)):
        zh_slots = placeholders(zh[key])
        en_slots = placeholders(en[key])
        if zh_slots != en_slots:
            errors.append(
                f"占位符不一致: {key} zh={sorted(zh_slots)} en={sorted(en_slots)}"
            )

    missing_zh = sorted(k for k in en if k not in zh)
    missing_en = sorted(k for k in zh if k not in en)
    for key in missing_zh:
        warnings.append(f"缺中文翻译: {key} en={en[key][:40]!r}")
    for key in missing_en:
        warnings.append(f"缺英文翻译: {key} zh={zh[key][:40]!r}")

    referenced = used
    orphans = sorted((set(zh) | set(en)) - referenced)
    test_only_orphans = 0
    actionable_orphans = 0
    for key in orphans:
        where = zh.get(key) or en.get(key) or ""
        counts = occurrences.get(where, {"ui": 0, "skipped": 0})
        if counts["ui"] > 0:
            actionable_orphans += 1
            warnings.append(
                f"待迁移条目（UI 站点仍是字面量）: {key} {where[:40]!r} 站点数 {counts['ui']}"
            )
        else:
            test_only_orphans += 1
            warnings.append(
                f"已跳过条目（仅出现在测试/日志/属性/夹具上下文）: {key} {where[:40]!r} 站点数 {counts['skipped']}"
            )

    stats = {
        "catalog_entries": len(set(zh) | set(en)),
        "zh_entries": len(zh),
        "en_entries": len(en),
        "referenced_keys": len(referenced),
        "bilingual": len(set(zh) & set(en)),
        "missing_zh": len(missing_zh),
        "missing_en": len(missing_en),
        "orphans": len(orphans),
        "orphans_actionable": actionable_orphans,
        "orphans_skipped_context": test_only_orphans,
        "unmigrated_chinese_sites": sum(len(v) for v in han_left.values()),
        "unmigrated_chinese_entries": len(han_left),
        "untranslated_english_candidates": len(en_left),
    }
    print(json.dumps(stats, indent=2, ensure_ascii=False))

    if not summary_only:
        for message in errors:
            print(f"ERROR {message}")
        actionable = [w for w in warnings if w.startswith("待迁移条目")]
        skipped = [w for w in warnings if not w.startswith(("待迁移条目", "缺中文翻译", "缺英文翻译"))]
        for message in actionable[:40]:
            print(f"{message}")
        if len(actionable) > 40:
            print(f"WARN  ...另有 {len(actionable) - 40} 条待迁移条目")
        for message in skipped[:10]:
            print(f"{message}")
        if len(skipped) > 10:
            print(f"WARN  ...另有 {len(skipped) - 10} 条已跳过条目")
        coverage = (
            referenced and len(referenced - set(han_left)) or 0
        )
        print(
            f"\n迁移覆盖: 目录条目 {len(referenced)}/{len(set(zh) | set(en))} 已被代码引用"
        )

    if errors or (strict and (missing_zh or missing_en)):
        sys.exit(1)


if __name__ == "__main__":
    main()
