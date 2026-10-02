#!/usr/bin/env python3
"""Zed CN 多语言（i18n）工具共享库。

提供 git 历史访问、字面量抽取与站点分类，被以下脚本共用：
    i18n_extract.py   抽取全仓库用户可见文本，生成/合并 locales/ 目录
    i18n_check.py     CI 校验：key 对齐、覆盖率、上游同步漂移
    i18n_codemod.py   把源码中的字面量改写为 t!("key") 调用

分类原则（与仓库 skill 中既有约定一致）：
    翻译用户可见文本：按钮、菜单、标签、tooltip、placeholder、toast、弹窗、设置项、错误提示。
    不翻译：日志、telemetry、panic/断言、测试、协议标识、URL、模型/工具载荷、内部诊断、夹具数据。
"""

from __future__ import annotations

import hashlib
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

HAN_RE = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]")

# ---------------------------------------------------------------- git 访问


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout


def try_git(*args: str) -> str | None:
    proc = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True)
    return proc.stdout if proc.returncode == 0 else None


def blob_at(rev: str, path: str) -> str | None:
    return try_git("cat-file", "blob", f"{rev}:{path}")


def is_han(text: str) -> bool:
    return bool(HAN_RE.search(text))


# ---------------------------------------------------------------- 字面量抽取


def unescape(raw: str) -> str:
    """按 Rust 规则反转义字面量内容。"""
    out = []
    i = 0
    mapping = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\", "0": "\0"}
    while i < len(raw):
        if raw[i] == "\\" and i + 1 < len(raw):
            nxt = raw[i + 1]
            # 反斜杠续行：跳过换行及其后的空白，与 Rust 的词法一致
            if nxt == "\n":
                i += 2
                while i < len(raw) and raw[i] in " \t":
                    i += 1
                continue
            out.append(mapping.get(nxt, nxt))
            i += 2
        else:
            out.append(raw[i])
            i += 1
    return "".join(out)


def scan_literals(line: str, in_block_comment: bool = False):
    """抽取一行中的字符串字面量，返回 ([(raw, column)], 块注释状态, 字符串续行状态)。

    识别 // 与 /* */ 注释。行尾以孤立反斜杠续行的字符串返回
    in_string=True，调用方必须跳过该行及其后续续行，避免把字符串内容
    当作独立字面量处理。
    """
    literals = []
    i = 0
    n = len(line)
    while i < n:
        if in_block_comment:
            end = line.find("*/", i)
            if end == -1:
                return literals, True
            in_block_comment = False
            i = end + 2
            continue
        ch = line[i]
        if ch == "/" and i + 1 < n and line[i + 1] == "/":
            break
        if ch == "/" and i + 1 < n and line[i + 1] == "*":
            in_block_comment = True
            i += 2
            continue
        if ch == '"':
            j = i + 1
            buf = []
            while j < n:
                if line[j] == "\\":
                    buf.append(line[j : j + 2])
                    j += 2
                    continue
                if line[j] == '"':
                    break
                buf.append(line[j])
                j += 1
            literals.append(("".join(buf), i))
            i = j + 1
            continue
        i += 1
    return literals, in_block_comment


def scan_logical_lines(text: str):
    """产出 (物理行号, 逻辑行文本, 是否多行字符串内部)。

    把以反斜杠续行的字符串合并到所在逻辑行，使字面量扫描看到完整文本；
    多行字符串整体标记为 unsafe，调用方直接跳过。
    """
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        start = i
        buf = lines[i]
        unsafe = False
        while buf.rstrip("\r").endswith("\\") or _ends_in_string(buf):
            unsafe = True
            i += 1
            if i >= len(lines):
                break
            buf = buf + "\n" + lines[i]
            if not lines[i].rstrip().endswith("\\") and '"' in lines[i]:
                break
        yield start, buf, unsafe
        i += 1


def _ends_in_string(line: str) -> bool:
    in_str = False
    j = 0
    while j < len(line):
        ch = line[j]
        if ch == "\\" and in_str:
            j += 2
            continue
        if ch == '"':
            in_str = not in_str
        j += 1
    return in_str


def strip_line_comment(line: str) -> str:
    literals, _ = scan_literals(line)
    # 用字面量位置信息判断注释起点不可靠，直接复用扫描器的语义：
    # 重新走一遍，遇到 // 即截断
    i = 0
    n = len(line)
    in_block = False
    while i < n:
        if in_block:
            end = line.find("*/", i)
            if end == -1:
                return line[:i]
            in_block = False
            i = end + 2
            continue
        ch = line[i]
        if ch == '"':
            j = i + 1
            while j < n:
                if line[j] == "\\":
                    j += 2
                    continue
                if line[j] == '"':
                    break
                j += 1
            i = j + 1
            continue
        if ch == "/" and i + 1 < n and line[i + 1] == "/":
            return line[:i]
        if ch == "/" and i + 1 < n and line[i + 1] == "*":
            in_block = True
            i += 2
            continue
        i += 1
    return line


def skeleton(line: str) -> str:
    """把字面量替换为 § 后的行骨架，用于中英行配对。"""

    def repl(m):
        return '"§"'

    return re.sub(r'"((?:[^"\\\n]|\\.)*)"', repl, strip_line_comment(line))


# ---------------------------------------------------------------- 站点分类

# 明确的非 UI 调用/宏：出现在这些上下文里的字面量一律不迁移
NOISE_CALL_RE = re.compile(
    r"""(?:\b(?:log|tracing|telemetry|debug|info|warn|error|trace)::(?:event|info|warn|error|debug|trace)|
        \#\[error\( | \#\[doc\s*= | \#\[serde\( | \.context\( | \.with_context\( |
        \bassert(?:_eq|_ne)?!|\bdebug_assert!|\bpanic!|\bunreachable!|\btodo!|\bunimplemented!|
        \bexpect\(|\bunwrap_or_default\(|
        \bmatches!|\bwrite!|\bwriteln!|\beprintln!|\bprintln!|\bdbg!)""",
    re.VERBOSE,
)

# 明确的 UI 调用：出现在这些上下文里的字面量默认可迁移
UI_CALL_RE = re.compile(
    r"""(?:
        Label::new\( | Button::new\( | Toggle::new\( | MenuItem::action\( |
        MenuItem::checkbox\( | MenuItem::separator\( | MenuItem::action_with_keystring\( |
        Tooltip:: | IconButton::new\( | ContextMenu::build\( | PopoverMenu::build\( |
        Notification::new\( | SelectableButton::new\( | CheckboxWithLabel::new\( |
        SettingItem:: | SettingField:: |
        \.tooltip\( | \.label\( | \.placeholder\( | \.name\( | \.title\( |
        \.message\( | \.primary_message\( | \.secondary_message\( |
        \.text\( | \.description\( | \.confirm\( | ErrorAction::link\( |
        one_line\( | two_lines\( | key_binding\( | validate\( | error_message\( |
        menu\( | submenu\( | section\( | header\( | footer\(
    )""",
    re.VERBOSE,
)

# 这些标识符/结构里的字面量不是展示文本
KEYISH_RE = re.compile(
    r"""(?:\baction\(|\bAction\(|\bkey_binding\(|\bparse\(|\bserde\(|
        \bSettingsKey\(|\bjson!\(|\bserde_json::|\bfrom_str\(|\bto_string\(\)?\s*==|
        \bPath::new\(|\bfs::|\bCommand::new\(|\benv::var\()""",
    re.VERBOSE,
)

TEST_FILE_SUFFIXES = ("_tests.rs", "_test.rs", "_bench.rs", "_benches.rs")
TEST_DIR_MARKERS = ("/tests/", "/benches/", "/examples/", "/fixtures/")

# i18n 工具链自身（含生成器与脚本），永远不作为 UI 文案
TOOLING_DIRS = ("crates/i18n/",)


def is_test_file(path: str) -> bool:
    if path.startswith(TOOLING_DIRS) or f"/{TOOLING_DIRS[0]}" in path:
        return True
    if path.endswith(TEST_FILE_SUFFIXES) or path.endswith("/test.rs"):
        return True
    if path.endswith("/visual_test_runner.rs"):
        return True
    if any(marker in path for marker in TEST_DIR_MARKERS):
        return True
    if path.endswith("/main.rs") and "benchmarks" in path:
        return True
    return False


RAW_STRING_START_RE = re.compile(r'(?:b|c|br|rb|cr|rc)?r(#*)"')
CHAR_LITERAL_RE = re.compile(r"'(?:\\.|[^\\'])'")


def mask_rust_line(line: str, in_block_comment: bool = False) -> tuple[str, bool]:
    """把字符串/字符字面量与注释替换为等长空格，用于可靠地数花括号。

    原始字符串（`r#"…"#`、`br"…"` 等）、转义与行注释、块注释都会被剔除，
    这样花括号计数不会被字符串内容干扰。
    """
    out: list[str] = []
    index = 0
    length = len(line)
    while index < length:
        if in_block_comment:
            end = line.find("*/", index)
            if end == -1:
                out.append(" " * (length - index))
                return "".join(out), True
            out.append(" " * (end + 2 - index))
            index = end + 2
            in_block_comment = False
            continue
        character = line[index]
        if character == "/" and index + 1 < length and line[index + 1] == "/":
            out.append(" " * (length - index))
            break
        if character == "/" and index + 1 < length and line[index + 1] == "*":
            end = line.find("*/", index + 2)
            if end == -1:
                out.append(" " * (length - index))
                return "".join(out), True
            out.append(" " * (end + 2 - index))
            index = end + 2
            continue
        raw_start = RAW_STRING_START_RE.match(line, index)
        if raw_start is not None:
            closing = '"' + raw_start.group(1)
            end = line.find(closing, raw_start.end())
            if end == -1:
                out.append(" " * (length - index))
                break
            out.append(" " * (end + len(closing) - index))
            index = end + len(closing)
            continue
        if character == '"':
            end = index + 1
            while end < length:
                if line[end] == "\\":
                    end += 2
                    continue
                if line[end] == '"':
                    break
                end += 1
            end = min(end, length - 1)
            out.append(" " * (end + 1 - index))
            index = end + 1
            continue
        char_literal = CHAR_LITERAL_RE.match(line, index)
        if char_literal is not None:
            out.append(" " * (char_literal.end() - index))
            index = char_literal.end()
            continue
        out.append(character)
        index += 1
    return "".join(out), in_block_comment


def compute_test_regions(lines: list[str]) -> list[bool]:
    """标记每个行索引是否位于 `#[cfg(test)]` 模块或测试函数体内。"""
    regions = [False] * len(lines)
    depth = 0
    pending = False
    test_depths: list[int] = []
    in_block_comment = False
    for index, line in enumerate(lines):
        regions[index] = bool(test_depths)
        masked, in_block_comment = mask_rust_line(line, in_block_comment)
        stripped = masked.strip()
        if re.match(r"#\[cfg\([^\]]*\btest\b", stripped) or re.match(
            r"(?:pub\s+)?mod\s+tests\b", stripped
        ):
            pending = True
        # gpui 组件预览（`impl Component`）与 `fn preview` 只是开发用的展示页，
        # 其中的示例文案不是用户可见的界面文案。
        if re.match(r"impl\s+Component\b", stripped) or re.match(r"(?:pub\s+)?fn\s+preview\w*\s*\(", stripped):
            pending = True
        delta = masked.count("{") - masked.count("}")
        if pending and delta > 0:
            test_depths.append(depth + delta)
            pending = False
            regions[index] = True
        elif pending and ";" in masked:
            pending = False
        depth += delta
        while test_depths and depth < test_depths[-1]:
            test_depths.pop()
    return regions


_test_region_cache: tuple[int, list[str], list[bool]] | None = None


def in_test_region(lines: list[str], index: int) -> bool:
    """判断某行是否位于 `#[cfg(test)]` 模块或测试函数体内。

    结果按 `lines` 对象缓存，`scan_code`/`extract` 对同一文件会反复调用。
    """
    global _test_region_cache
    if _test_region_cache is None or _test_region_cache[0] != id(lines):
        _test_region_cache = (id(lines), lines, compute_test_regions(lines))
    regions = _test_region_cache[2]
    return 0 <= index < len(regions) and regions[index]


def enclosing_call(lines: list[str], index: int) -> str | None:
    """字面量单独成行时，向上找它所属的调用行。

    UI 框架的调用经常跨多行，字符串实参单独占一行，例如

        Tooltip::for_action_in(
            "强制删除分支",
            ...
        )

    这里向上跳过以 `,`、`(`、`&&` 结尾的续行，直到找到带调用名的行，
    遇到语句边界（`;`、`{`、`}`）即放弃。
    """
    for back in range(1, 9):
        j = index - back
        if j < 0:
            return None
        prev = strip_line_comment(lines[j]).strip()
        if not prev:
            continue
        if prev.endswith((";", "{")) or prev.startswith("}"):
            return None
        if NOISE_CALL_RE.search(prev) or UI_CALL_RE.search(prev):
            return prev
        if prev.endswith((",", "(", "&&", "||", ".", "::")):
            continue
        return None
    return None


def classify_occurrence(path: str, lines: list[str], index: int, line: str, value: str) -> str:
    """把一个字面量出现分类为 ui / noise。"""
    if is_test_file(path):
        return "noise"
    code = strip_line_comment(line)
    noise = NOISE_CALL_RE.search(code)
    ui = UI_CALL_RE.search(code)
    if not noise and not ui:
        # 本行没有调用形态时，参考它所属的调用行
        enclosing = enclosing_call(lines, index)
        if enclosing is not None:
            noise = NOISE_CALL_RE.search(enclosing)
            ui = UI_CALL_RE.search(enclosing)
    if noise:
        return "noise"
    if KEYISH_RE.search(code):
        return "noise"
    if in_test_region(lines, index):
        return "noise"
    # 属性上下文（#[strum(serialize = ...)] 等）必须是字面量
    if re.search(r"#\[[^\]]*$", code[: _first_quote(code)]):
        return "noise"
    # 原始/字节字符串前缀（r"..."、br"..."）通常是正则或协议字面量，不是文案
    prefix_index = _first_quote(code) - 1
    if prefix_index >= 0 and code[prefix_index] in "rbc":
        if prefix_index == 0 or not (code[prefix_index - 1].isalnum() or code[prefix_index - 1] == "_"):
            return "noise"
    # 字面量后面跟 `=>` 说明它是 match 左值（键/模式），不是展示文本；
    # 前面跟 `=>` 则是 match 右值，通常是展示文本。
    for raw, column in scan_literals(code)[0]:
        if unescape(raw) != value:
            continue
        closing = code.find('"', column + len(raw) + 1)
        if closing != -1 and re.match(r"\s*=>", code[closing + 1 :]):
            return "noise"  # 该字面量是 match 左值（键或模式）
        break
    if re.search(r"(::|=&|\bself\.)\s*$", code[: _first_quote(code)]):
        return "noise"
    if is_han(value):
        return "ui"  # fork 只把 UI 文本替换成了中文
    if UI_CALL_RE.search(code):
        return "ui"
    # 结构体字段名白名单：这些字段承载展示文本（设置项描述、提示消息等）
    if re.match(r"[^\S\n]*(?:description|message|label|placeholder|tooltip|title|body|text|header|name)\s*:\s*$", code[: _first_quote(code)]):
        return "ui"
    return "noise"


def _first_quote(code: str) -> int:
    idx = code.find('"')
    return idx if idx != -1 else len(code)


def candidate_text(value: str) -> bool:
    """粗判文本是否可能是用户可见文案（宁滥毋缺，站点分类负责精确化）。"""
    t = value.strip()
    if is_han(t):
        return len(t) >= 1  # 单字中文标签（是/否/高/差/键/值…）同样是界面文案
    if len(t) < 2:
        return False
    words = re.findall(r"[A-Za-z]+", t)
    if len(words) < 2:
        return False
    if not (t[0].isupper() or t[0].isdigit()):
        return False
    if re.match(r"^[a-z_]*::[A-Za-z_:]+$", t):
        return False
    if re.match(r"^[a-zA-Z0-9_.\-/]+$", t):
        return False
    if t.startswith(("http://", "https://", "file://", "ssh://", "data:")):
        return False
    return True


def literal_key(seed: str) -> str:
    return hashlib.sha256(seed.strip().encode("utf-8")).hexdigest()[:16]
