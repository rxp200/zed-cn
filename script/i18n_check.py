#!/usr/bin/env python3
"""i18n 目录一致性与覆盖率检查（CI 门禁）。

用法:
    script/i18n_check.py            # 全量检查，发现问题返回非 0
    script/i18n_check.py --summary  # 只打印统计

检查项:
    1. 代码中的每个 t!(key) 都必须存在于目录；缺 key 是错误；
    2. 同一 key 的中英文占位符（含位置数量与格式说明符）必须一致；不一致是错误；
    3. 每个调用点必须提供模板需要的占位符；缺实参、多传实参或使用
       `interpolate` 无法渲染的 Debug 说明符都是错误；
    4. 目录条目缺某一语言时记为待翻译（警告，可用 --strict 升级为错误）；
    5. 代码中残留的中文 UI 字面量与英文 UI 文案记为未迁移（警告/统计）；
    6. 目录中没有代码引用的条目记为孤儿（警告）。
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


def template_slots(text: str) -> tuple[int, set[str], tuple[str, ...]]:
    """返回 (位置占位符个数, 命名占位符集合, 格式说明符元组)。

    与 `i18n::interpolate` 的解析保持一致：`${...}` 是模板变量而非占位符，
    `{{`/`}}` 是转义花括号。
    """
    text = re.sub(r"\$\{[^{}]*\}", "", text)
    text = text.replace("{{", "\x00").replace("}}", "\x01")
    positionals = 0
    named: set[str] = set()
    specs: list[str] = []
    for match in re.finditer(r"\{([^{}]*)\}", text):
        name, separator, spec = match.group(1).partition(":")
        if name == "":
            positionals += 1
        elif re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
            named.add(name)
        else:
            continue
        specs.append(spec if separator else "")
    return positionals, named, tuple(specs)


def display_format_supported(spec: str) -> bool:
    """`i18n::interpolate` 只能渲染 Display 说明符（`#`、`.N` 及其组合）。"""
    remainder = spec[1:] if spec.startswith("#") else spec
    return remainder == "" or re.fullmatch(r"\.\d+", remainder) is not None


MACRO_RE = re.compile(r"\bi18n::(t|t_args|t_mix|t_in)!\s*\(")


def find_macro_end(text: str, open_paren: int) -> int | None:
    depth = 0
    index = open_paren
    in_string = False
    while index < len(text):
        character = text[index]
        if in_string:
            if character == "\\":
                index += 2
                continue
            if character == '"':
                in_string = False
            index += 1
            continue
        if character == '"':
            in_string = True
        elif character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return None


def split_top_level(text: str, separator: str = ",") -> list[str]:
    parts: list[str] = []
    current: list[str] = []
    depth = 0
    in_string = False
    index = 0
    while index < len(text):
        character = text[index]
        if in_string:
            current.append(character)
            if character == "\\":
                current.append(text[index + 1] if index + 1 < len(text) else "")
                index += 2
                continue
            if character == '"':
                in_string = False
            index += 1
            continue
        if character == '"':
            in_string = True
            current.append(character)
        elif character in "([{":
            depth += 1
            current.append(character)
        elif character in ")]}":
            depth -= 1
            current.append(character)
        elif character == separator and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(character)
        index += 1
    parts.append("".join(current))
    return parts


def split_args(text: str) -> list[str]:
    return [part.strip() for part in split_top_level(text) if part.strip()]


def parse_args(text: str) -> tuple[list[str], dict[str, str]]:
    positional: list[str] = []
    named: dict[str, str] = {}
    for argument in split_args(text.replace("=>", "=")):
        match = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)\s*=(?!=)", argument)
        if match:
            named[match.group(1)] = argument
        else:
            positional.append(argument)
    return positional, named


def scan_call_sites() -> list[dict]:
    """解析每个 i18n 宏调用的 key 与实参，供占位符一致性校验使用。"""
    sites: list[dict] = []
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        text = path.read_text(errors="ignore")
        lines = text.splitlines()
        relative = str(path.relative_to(ROOT))
        for match in MACRO_RE.finditer(text):
            line_index = text.count("\n", 0, match.start())
            if lines[line_index].lstrip().startswith(("//", "/*", "*")):
                continue
            open_paren = match.end() - 1
            end = find_macro_end(text, open_paren)
            if end is None:
                continue
            inner = text[open_paren + 1 : end]
            key_match = re.search(r'"([0-9a-f]{16})"', inner)
            if not key_match:
                continue
            macro = match.group(1)
            body = inner[key_match.end() :]
            if macro == "t_mix":
                stripped = body.strip()
                rest = stripped[1:] if stripped.startswith(";") else stripped
                groups = split_top_level(rest, ";")
                positional_text = groups[0] if groups else ""
                named_text = groups[1] if len(groups) > 1 else ""
                positional = split_args(positional_text) if positional_text.strip() else []
                _, named = parse_args(named_text)
            elif macro == "t_args":
                positional, named = split_args(body), {}
            else:
                positional, named = parse_args(body)
            sites.append(
                {
                    "path": relative,
                    "line": line_index + 1,
                    "macro": macro,
                    "key": key_match.group(1),
                    "positional": len(positional),
                    "named": set(named),
                }
            )
    return sites


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
        in_block_comment = False
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
        zh_slots = template_slots(zh[key])
        en_slots = template_slots(en[key])
        if zh_slots != en_slots:
            errors.append(f"占位符不一致: {key} zh={zh_slots} en={en_slots}")

    call_sites = scan_call_sites()
    for site in call_sites:
        template = zh.get(site["key"]) or en.get(site["key"])
        located = f"{site['path']}:{site['line']}"
        if template is None:
            errors.append(f"缺失 key: {site['key']}（{located}）不在任何语言目录中")
            continue
        want_positional, want_named, specs = template_slots(template)
        unsupported = [spec for spec in specs if not display_format_supported(spec)]
        has_specs = any(spec for spec in specs)
        has_arguments = site["positional"] > 0 or site["named"]
        if has_arguments:
            if site["positional"] != want_positional or site["named"] != want_named:
                errors.append(
                    f"占位符不匹配: {site['key']}（{located}）"
                    f" 需要 位置={want_positional} 命名={sorted(want_named)}，"
                    f"实际 位置={site['positional']} 命名={sorted(site['named'])}"
                )
            if unsupported:
                errors.append(
                    f"不支持的格式说明符: {site['key']}（{located}）{unsupported}"
                )
        elif unsupported:
            errors.append(f"不支持的格式说明符且未传实参: {site['key']}（{located}）{unsupported}")
        elif has_specs:
            errors.append(f"模板含格式说明符但未传实参: {site['key']}（{located}）")
        elif want_positional or want_named:
            warnings.append(f"占位符待确认: {site['key']}（{located}）模板含占位符但未传实参")

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
        "call_sites_checked": len(call_sites),
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
        actionable = [w for w in warnings if w.startswith(("待迁移条目", "占位符待确认"))]
        skipped = [
            w
            for w in warnings
            if not w.startswith(("待迁移条目", "缺中文翻译", "缺英文翻译", "占位符待确认"))
        ]
        for message in actionable[:40]:
            print(f"{message}")
        if len(actionable) > 40:
            print(f"WARN  ...另有 {len(actionable) - 40} 条待处理条目")
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
