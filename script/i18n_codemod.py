#!/usr/bin/env python3
"""把源码中的用户可见字面量改写为 i18n::t!(\"key\") 调用。

用法:
    script/i18n_codemod.py <file-or-crate-or-dir> ...   # 只改指定目标
    script/i18n_codemod.py --all                        # 全仓库
    script/i18n_codemod.py --dry-run <files>            # 只预览 diff 统计
    script/i18n_codemod.py --tests <files>              # 额外改写测试中的断言

改写规则:
    1. 只改写分类为 ui 且已存在于目录中的字面量（按规范化文本哈希定位 key）；
    2. `format!(模板, 名=值)` 改写为 `i18n::t!("key", 名=值)`；
       `format!(模板, 值)` 改写为 `i18n::t_args!("key", 值)`；
    3. 跳过 match 左值、字段名、常量初始化、宏键名等非展示位置；
    4. 测试模块默认跳过，--tests 下只改写与目录文案完全一致的断言字面量。
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from i18n_lib import (  # noqa: E402
    ROOT,
    candidate_text,
    classify_occurrence,
    is_han,
    literal_key,
    scan_literals,
    strip_line_comment,
    unescape,
)

CATALOG_ZH = ROOT / "locales" / "zh-Hans.json"
CATALOG_EN = ROOT / "locales" / "en.json"


def load_catalog() -> dict[str, str]:
    """{规范化文本: key}，中英文都索引，便于上游恢复英文后仍能命中。"""
    index: dict[str, str] = {}
    for path in (CATALOG_ZH, CATALOG_EN):
        if not path.exists():
            continue
        data = json.loads(path.read_text())
        for key, text in data.items():
            index.setdefault(text.strip(), key)
    return index


# 模板可能出现在 format! 或 anyhow 系列宏、context 包装里；
# 这些消息都是用户可见文案，且目录里已有译文。
# 真正的格式占位符：{} / {name} / {name:spec}；
# 排除 ${...} 模板变量与 JSON 花括号等纯文本花括号。
FORMAT_PLACEHOLDER_RE = re.compile(r"(?<!\$)\{(?:[A-Za-z_][A-Za-z0-9_]*)?(?::[^}]*)?\}")

FORMAT_CALL_RE = re.compile(
    r"\b(?:format|anyhow|bail|ensure|panic|assert|assert_eq|assert_ne)!\s*\(|"
    r"(?:anyhow::(?:anyhow|bail|ensure)!)\s*\(|"
    r"\.(?:context|with_context)\s*\(\s*(?:\|\|\s*)?"
)
CONST_RE = re.compile(r"^\s*(?:pub\s+)?(?:const|static)\s")
MATCH_KEY_RE = re.compile(r"^\s*=>")


def rewrite_line(
    path: Path, lines: list[str], index_line: int, line: str, index: dict, in_tests: bool,
    new_entries: dict | None = None,
) -> tuple[str, int, list[str]]:
    if new_entries is None:
        new_entries = {}
    """返回 (新行, 改写数, 备注)。"""
    notes: list[str] = []
    code = strip_line_comment(line)
    if CONST_RE.match(line):
        return line, 0, notes
    if "const " in code.split("=")[0] or "static " in code.split("=")[0]:
        return line, 0, notes

    literals, _ = scan_literals(line)
    if not literals:
        return line, 0, notes

    in_test_region = "test" in str(path)
    replacements = []
    for raw, col in literals:
        value = unescape(raw)
        if not candidate_text(value):
            continue
        key = index.get(value.strip())
        discovered = None
        if key is None and is_han(value):
            # 新的中文 UI 文本：先记下，只有确认改写时才入目录
            discovered = literal_key(value)
            key = discovered
        if key is None:
            continue
        # 带真实占位符但本行没有 format! 类调用 —— 实参可能在后续行或被隐式捕获，
        # 交给人工处理；纯文本花括号（${...}、JSON）不影响。
        if FORMAT_PLACEHOLDER_RE.search(value) and not FORMAT_CALL_RE.search(line):
            notes.append(f"跳过多行占位符: {value[:40]}")
            continue
        kind = classify_occurrence(str(path), lines, index_line, line, value)
        if kind != "ui" and not (in_tests and is_han(value)):
            continue
        span = find_literal_span(line, col)
        if span is None:
            continue
        start, end = span
        # 原始/字节字符串前缀（r"..."、br"..."）不迁移
        if start > 0 and line[start - 1] in "rbc" and (
            start - 1 == 0 or not (line[start - 2].isalnum() or line[start - 2] == "_")
        ):
            continue
        before = line[:start].rstrip()
        after = line[end:].lstrip()
        # match 左值、路径、赋值右侧的类型转换等位置。
        # 注意：`=>` 之后是 match 右值（通常是 UI 文本），只有出现在字面量【后面】
        # 的 `=>` 才表示该字面量是 match 左值键。
        if before.endswith(("::", "=&")) or before.endswith("as ") or before.endswith("move"):
            continue
        # 属性上下文（#[strum(serialize = ...)] 等）必须是字面量
        if re.search(r"#\[[^\]]*$", before):
            notes.append("跳过属性字面量")
            continue
        if MATCH_KEY_RE.match(after):
            continue
        if before.endswith(("&", "as", "move")) and "format!" not in code:
            continue
        if discovered is not None:
            new_entries[discovered] = value
            index[value.strip()] = discovered
        replacements.append((start, end, key, value, after))

    if not replacements:
        return line, 0, notes

    # format!(模板, ...) 特例：模板必须是第一个实参
    fmt_match = FORMAT_CALL_RE.search(line)
    rebuilt = line
    count = 0
    for start, end, key, value, after in reversed(replacements):
        original = line[start:end]
        if fmt_match and fmt_match.start() < start:
            args_text, consumed = extract_args_text(line[end:])
            args = [a.strip() for a in split_top_level(args_text)] if args_text.strip() else []
            named_args = (
                all(re.match(r"^[A-Za-z_][A-Za-z0-9_]*\s*=", a) for a in args) if args else False
            )
            positional_placeholders = "{}" in value
            named_placeholders = re.findall(r"\{([A-Za-z_][A-Za-z0-9_]*)\}", value)
            if re.search(r"\{[A-Za-z_][A-Za-z0-9_]*:", value):
                notes.append(f"跳过带格式说明符的占位符: {value[:40]}")
                continue
            if args and named_args and positional_placeholders and named_placeholders:
                new_text = 'i18n::t_mix!("{}"; ; {})'.format(key, ", ".join(args))
            elif args and named_args:
                if positional_placeholders:
                    notes.append(f"跳过位置/命名混用: {value[:40]}")
                    continue
                new_text = 'i18n::t!("{}", {})'.format(key, ", ".join(args))
            elif args and positional_placeholders and named_placeholders:
                # 位置实参 + 模板中的 {name} 隐式捕获
                implicit = [f"{n} = {n}" for n in named_placeholders]
                new_text = 'i18n::t_mix!("{}"; {}; {})'.format(
                    key, ", ".join(args), ", ".join(implicit)
                )
            elif args:
                if named_placeholders:
                    notes.append(f"跳过位置/命名混用: {value[:40]}")
                    continue
                new_text = 'i18n::t_args!("{}", {})'.format(key, ", ".join(args))
            else:
                names = re.findall(r"\{([A-Za-z_][A-Za-z0-9_]*)\}", value)
                if names:
                    new_text = 'i18n::t!("{}", {})'.format(
                        key, ", ".join(f"{n} = {n}" for n in names)
                    )
                elif "{}" in value:
                    notes.append(f"跳过无实参位置占位符: {value[:40]}")
                    continue
                else:
                    new_text = 'i18n::t!("{}")'.format(key)
            match_text = fmt_match.group(0).lstrip()
            prefix_between = line[fmt_match.end() : start].strip()
            if match_text.startswith(("format!", "format_args!")) and prefix_between == "":
                rebuilt = rebuilt[: fmt_match.start()] + new_text + rebuilt[end + consumed :]
            else:
                rebuilt = rebuilt[:start] + new_text + rebuilt[end + consumed - 1 :]
            notes.append(f"format! -> {new_text[:44]}")
        else:
            rebuilt = rebuilt[:start] + 'i18n::t!("{}")'.format(key) + rebuilt[end:]
        count += 1
    return rebuilt, count, notes


def split_top_level(text: str) -> list[str]:
    """按顶层逗号拆分（忽略字符串、括号、泛型内的逗号）。"""
    parts, current = [], []
    depth = 0
    in_str = False
    i = 0
    while i < len(text):
        ch = text[i]
        if in_str:
            current.append(ch)
            if ch == "\\" and i + 1 < len(text):
                current.append(text[i + 1])
                i += 2
                continue
            if ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
            current.append(ch)
        elif ch in "([{<":
            depth += 1
            current.append(ch)
        elif ch in ")]}>":
            depth -= 1
            current.append(ch)
        elif ch == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(ch)
        i += 1
    if current:
        parts.append("".join(current))
    return parts


def extract_args_text(text: str) -> tuple[str, int]:
    """返回 (实参文本, 从 text 起点消费的字符数)。

    text 以 `, ...` 或 `)` 开头：第一个处于深度 0 的 `)` 即调用结束。
    实参跨行时返回的文本包含换行，调用方据此跳过。
    """
    depth = 0
    for i, ch in enumerate(text):
        if ch == "(":
            depth += 1
        elif ch == ")":
            if depth == 0:
                return text[1:i], i + 1
            depth -= 1
    return text[1:], len(text)


def extract_format_args(text: str) -> list[str] | None:
    """从 `, arg1, arg2) 的剩余文本` 拆出顶层逗号分隔的参数。"""
    args_text = extract_args_text(text)
    inner = args_text[1:-1].strip() if args_text.endswith(")") else ""
    if inner == "":
        return None
    parts = []
    depth = 0
    current = []
    in_str = False
    i = 0
    while i < len(inner):
        ch = inner[i]
        if in_str:
            current.append(ch)
            if ch == "\\":
                if i + 1 < len(inner):
                    current.append(inner[i + 1])
                    i += 2
                    continue
            elif ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
            current.append(ch)
        elif ch in "([{":
            depth += 1
            current.append(ch)
        elif ch in ")]}":
            depth -= 1
            current.append(ch)
        elif ch == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(ch)
        i += 1
    if current:
        parts.append("".join(current))
    return [p.strip() for p in parts if p.strip()]


def find_literal_span(line: str, col: int) -> tuple[int, int] | None:
    """col 是 scan_literals 给出的开引号位置，返回包含首尾引号的区间。"""
    if col >= len(line) or line[col] != '"':
        return None
    i = col + 1
    in_escape = False
    while i < len(line):
        ch = line[i]
        if in_escape:
            in_escape = False
            i += 1
            continue
        if ch == "\\":
            in_escape = True
            i += 1
            continue
        if ch == '"':
            return col, i + 1
        i += 1
    return None




MULTILINE_FORMAT_RE = None  # 运行时编译，避免顶层依赖


def migrate_multiline_format(path: Path, index: dict, new_entries: dict, dry_run: bool) -> int:
    """迁移跨行 `format!(模板, ...)` 调用。

    模板常单独占一行、实参在后续行，行级规则无法安全处理；这里按调用整体替换。
    """
    import re as _re

    pattern = _re.compile(
        r'(?:format!|anyhow!|anyhow::anyhow!|bail!|anyhow::bail!|ensure!|anyhow::ensure!)'
        r'\s*\(\s*"((?:[^"\\\n]|\\.)*)"(\s*,|\s*\))',
        _re.S,
    )
    text = path.read_text(errors="ignore")
    replacements = []
    for match in pattern.finditer(text):
        raw = match.group(1)
        value = unescape(raw)
        if not is_han(value):
            continue
        key = index.get(value.strip())
        if key is None:
            key = literal_key(value)
            new_entries[key] = value
            index[value.strip()] = key
        # 找到 format!( 对应的右括号，取实参文本
        open_paren = text.index("(", match.start())
        depth = 0
        i = open_paren
        while i < len(text):
            ch = text[i]
            if ch == '"':
                i += 1
                while i < len(text) and text[i] != '"':
                    i += 2 if text[i] == "\\" else 1
            elif ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        if i >= len(text):
            continue
        template_end = match.end()
        args_rest = text[template_end:i].strip()
        if args_rest.startswith(","):
            args_rest = args_rest[1:].strip()
        args = [a.strip() for a in _split_top(args_rest)] if args_rest else []
        if _re.search(r"\{[A-Za-z_][A-Za-z0-9_]*:", value):
            continue  # 带格式说明符，人工处理
        named = [a for a in args if _re.match(r"^[A-Za-z_][A-Za-z0-9_]*\s*=", a)]
        positional = [a for a in args if not _re.match(r"^[A-Za-z_][A-Za-z0-9_]*\s*=", a)]
        names = _re.findall(r"\{([A-Za-z_][A-Za-z0-9_]*)\}", value)
        has_positional = "{}" in value
        has_named = bool(names)
        if named and positional and has_positional and has_named:
            new_text = 'i18n::t_mix!("{}"; {}; {})'.format(key, ", ".join(positional), ", ".join(named))
        elif named and positional and has_positional:
            new_text = 'i18n::t_mix!("{}"; {}; {})'.format(key, ", ".join(positional), ", ".join(named))
        elif named and not positional and not has_positional:
            new_text = 'i18n::t!("{}", {})'.format(key, ", ".join(named))
        elif positional and not named and has_named:
            implicit = [f"{n} = {n}" for n in names]
            new_text = 'i18n::t_mix!("{}"; {}; {})'.format(
                key, ", ".join(positional), ", ".join(implicit)
            )
        elif positional and not named and not has_named:
            new_text = 'i18n::t_args!("{}", {})'.format(key, ", ".join(positional))
        elif names and not args:
            new_text = 'i18n::t!("{}", {})'.format(key, ", ".join(f"{n} = {n}" for n in names))
        elif not args and not names and "{}" not in value:
            new_text = 'i18n::t!("{}")'.format(key)
        else:
            continue
        macro_name = match.group(0).lstrip()
        literal_start = match.start(1) - 1
        if macro_name.startswith(("format!", "format_args!")):
            replacements.append((match.start(), i + 1, new_text))
        else:
            # anyhow!/bail!/ensure! 等包装宏：保留宏名、前置实参与调用右括号
            replacements.append((literal_start, i, new_text))

    if not replacements:
        return 0
    if dry_run:
        return len(replacements)
    out = text
    for start, end, new_text in reversed(replacements):
        out = out[:start] + new_text + out[end:]
    path.write_text(out)
    return len(replacements)


def _split_top(text: str):
    parts, current = [], []
    depth = 0
    in_str = False
    i = 0
    while i < len(text):
        ch = text[i]
        if in_str:
            current.append(ch)
            if ch == "\\":
                if i + 1 < len(text):
                    current.append(text[i + 1])
                    i += 2
                    continue
            elif ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
            current.append(ch)
        elif ch in "([{":
            depth += 1
            current.append(ch)
        elif ch in ")]}":
            depth -= 1
            current.append(ch)
        elif ch == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(ch)
        i += 1
    if current:
        parts.append("".join(current))
    return parts


def _contains_newline_top_level(text: str) -> bool:
    depth = 0
    in_str = False
    i = 0
    while i < len(text):
        ch = text[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
        elif ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == "\n" and depth == 0:
            return True
        i += 1
    return False





def literal_span(lines: list[str], start_index: int, open_col: int):
    """从 open_col 处的开引号出发，返回 (结束行号, 闭引号后一列)。

    支持跨物理行的反斜杠续行字符串；扫描时跳过转义字符。
    """
    line_index = start_index
    i = open_col + 1
    while line_index < len(lines):
        line = lines[line_index]
        while i < len(line):
            ch = line[i]
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                return line_index, i + 1
            i += 1
        line_index += 1
        i = 0
    return None


def migrate_multiline_strings(path: Path, index: dict, new_entries: dict, dry_run: bool) -> int:
    """迁移以反斜杠续行的多行字符串字面量。

    行级规则只能看到续行字符串的第一段，这里定位完整的引号区间后整体替换为
    i18n::t!("key")。
    """
    from i18n_lib import scan_logical_lines

    text = path.read_text(errors="ignore")
    lines = text.splitlines(keepends=True)
    plain_lines = text.splitlines()
    replacements = []
    for start_index, logical, unsafe in scan_logical_lines(text):
        if not unsafe:
            continue
        literals, _ = scan_literals(logical)
        if len(literals) != 1:
            continue
        raw, column = literals[0]
        value = unescape(raw)
        # 目录中已有的 UI 文案（中文或英文）都可以迁移
        key = index.get(value.strip())
        if key is None:
            continue
        if classify_occurrence(str(path), plain_lines, start_index, plain_lines[start_index], value) != "ui":
            continue
        span = literal_span(plain_lines, start_index, column)
        if span is None:
            continue
        end_index, close_col = span
        replacements.append((start_index, column, end_index, close_col, key))

    if not replacements:
        return 0
    if dry_run:
        return len(replacements)
    for start_index, open_col, end_index, close_col, key in reversed(replacements):
        prefix = lines[start_index][:open_col]
        suffix = lines[end_index][close_col:]
        lines[start_index : end_index + 1] = [prefix + 'i18n::t!("{}")'.format(key) + suffix]
    path.write_text("".join(lines))
    return len(replacements)


def process_file(path: Path, index: dict, in_tests: bool, dry_run: bool, new_entries: dict | None = None):
    if new_entries is None:
        new_entries = {}
    text = path.read_text(errors="ignore")
    lines = text.splitlines(keepends=True)
    changed = []
    total = 0
    for i, line in enumerate(lines):
        bare = line.rstrip("\n")
        new_line, count, notes = rewrite_line(path, lines, i, bare, index, in_tests, new_entries)
        if count:
            total += count
            changed.append((i + 1, bare, new_line, notes))
    if not changed:
        return 0, []
    if not dry_run:
        new_lines = [l for l in lines]
        for lineno, old, new, _ in changed:
            new_lines[lineno - 1] = new + ("\n" if lines[lineno - 1].endswith("\n") else "")
        path.write_text("".join(new_lines), errors="ignore")
    return total, changed


def main():
    args = sys.argv[1:]
    dry_run = "--dry-run" in args
    include_tests = "--tests" in args
    do_all = "--all" in args
    args = [a for a in args if not a.startswith("--")]
    index = load_catalog()
    if not index:
        print("目录为空，请先运行 script/i18n_extract.py")
        sys.exit(1)

    targets: list[Path] = []
    if do_all:
        targets = sorted((ROOT / "crates").rglob("*.rs"))
    else:
        for arg in args:
            p = (ROOT / arg).resolve()
            if p.is_dir():
                targets.extend(sorted(p.rglob("*.rs")))
            else:
                targets.append(p)
    targets = [t for t in targets if t.exists()]

    grand_total = 0
    files_changed = 0
    new_entries: dict[str, str] = {}
    multiline = "--multiline" in sys.argv
    for target in targets:
        total, changed = process_file(target, index, include_tests, dry_run, new_entries)
        if multiline:
            total += migrate_multiline_format(target, index, new_entries, dry_run)
            total += migrate_multiline_strings(target, index, new_entries, dry_run)
        if total:
            files_changed += 1
            grand_total += total
            rel = target.relative_to(ROOT)
            if dry_run or include_tests:
                for lineno, old, new, notes in changed[:6]:
                    print(f"{rel}:{lineno}\n  - {old.strip()[:110]}\n  + {new.strip()[:110]}")
                if len(changed) > 6:
                    print(f"  ...({len(changed)} 行)")
    print(f"\n{'[dry-run] ' if dry_run else ''}改写文件 {files_changed} 个，共 {grand_total} 处")
    if new_entries:
        if not dry_run:
            path = ROOT / "locales" / "zh-Hans.json"
            data = json.loads(path.read_text())
            for key, text in new_entries.items():
                data.setdefault(key, text)
            path.write_text(json.dumps(dict(sorted(data.items())), ensure_ascii=False, indent=2) + "\n")
        print(f"新增目录条目（待补英文）: {len(new_entries)} 条")


if __name__ == "__main__":
    main()
