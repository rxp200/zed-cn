#!/usr/bin/env python3
"""翻译工作台：导出待翻译条目、导入翻译结果。

用法:
    script/i18n_translate.py export --lang en --limit 300 --offset 0 --out /tmp/batch.json
    script/i18n_translate.py import --lang en /tmp/batch.json
    script/i18n_translate.py export --lang zh --limit 300 --out /tmp/batch_zh.json
    script/i18n_translate.py import --lang zh /tmp/batch_zh.json

导出格式为 {"key": "源文本"}；导入时把译文写回对应语言文件。
占位符（{name} 与 {}）必须在译文中原样保留，导入时校验。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOCALES = ROOT / "locales"
FILES = {"zh": "zh-Hans.json", "en": "en.json"}


def load(target: str) -> dict:
    return json.loads((LOCALES / FILES[target]).read_text())


def save(target: str, data: dict) -> None:
    path = LOCALES / FILES[target]
    path.write_text(json.dumps(dict(sorted(data.items())), ensure_ascii=False, indent=2) + "\n")


def placeholders(text: str) -> set[str]:
    names = set(re.findall(r"\{([A-Za-z_][A-Za-z0-9_]*)\}", text))
    if "{}" in text:
        names.add("<pos>")
    return names


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("export", "import"):
        p = sub.add_parser(name)
        p.add_argument("--lang", required=True, choices=["zh", "en"])
        p.add_argument("--limit", type=int, default=300)
        p.add_argument("--offset", type=int, default=0)
        p.add_argument("--out")
        p.add_argument("--source", help="import 时的译文文件")
        p.add_argument("--crate", help="只处理某个 crate 的条目")
        p.add_argument(
            "--by-text",
            action="store_true",
            help="import 时按【源文本】而不是 key 匹配，避免译文与条目错位",
        )
    args = parser.parse_args()

    if args.command == "export":
        target = load(args.lang)
        other = load("zh" if args.lang == "en" else "en")
        missing = [k for k in other if k not in target]
        if args.crate:
            meta = json.loads((LOCALES / "meta.json").read_text())
            ctx = meta.get("context", {})
            missing = [
                k
                for k in missing
                if (ctx.get(k) or ["?"])[0].startswith(f"crates/{args.crate}/")
            ]
        batch = missing[args.offset : args.offset + args.limit]
        payload = {k: other[k] for k in batch}
        out = args.out or f"/tmp/i18n_batch_{args.lang}_{args.offset}.json"
        Path(out).write_text(json.dumps(payload, ensure_ascii=False, indent=2))
        print(f"导出 {len(payload)} 条到 {out}（总待翻译 {len(missing)}）")

    else:
        source = args.source or args.out
        if not source:
            parser.error("import 需要 --source")
        translations = json.loads(Path(source).read_text())
        target = load(args.lang)
        other = load("zh" if args.lang == "en" else "en")
        applied, rejected = 0, []
        if getattr(args, "by_text", False):
            # 按源文本匹配：译文文件形如 {"中文原文": "English"}
            by_source = {}
            for key, source_text in other.items():
                by_source.setdefault(source_text, []).append(key)
            remapped = {}
            for source_text, text in translations.items():
                matching = by_source.get(source_text)
                if not matching:
                    rejected.append((source_text[:30], "源文本不存在"))
                    continue
                for key in matching:
                    remapped[key] = text
            translations = remapped
        for key, text in translations.items():
            if key not in other:
                rejected.append((key, "源条目不存在"))
                continue
            if not isinstance(text, str) or not text.strip():
                rejected.append((key, "译文为空"))
                continue
            if placeholders(text) != placeholders(other[key]):
                rejected.append((key, f"占位符不一致: {sorted(placeholders(text))} vs {sorted(placeholders(other[key]))}"))
                continue
            target[key] = text
            applied += 1
        save(args.lang, target)
        print(f"已写入 {applied} 条；拒绝 {len(rejected)} 条")
        for key, why in rejected[:10]:
            print(f"  拒绝 {key}: {why}")


if __name__ == "__main__":
    main()
