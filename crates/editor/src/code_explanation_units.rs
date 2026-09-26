use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, ops::Range};

pub const MAX_INPUT_BYTES: usize = 20 * 1024;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub range: Range<usize>,
    pub owner: Range<usize>,
    pub owner_lines: usize,
    pub first_row: usize,
    pub last_row: usize,
    pub context: String,
    pub commented_rows: Vec<usize>,
}

pub fn request_code_budget(maximum_tokens: u64) -> usize {
    // Byte-based admission is deliberately conservative for tokenizers without a local counting API.
    maximum_tokens
        .saturating_sub(4096)
        .min((MAX_REQUEST_BYTES - 8192) as u64) as usize
}

pub fn fit_units_to_budget(
    snapshot: &language::BufferSnapshot,
    units: Vec<Unit>,
    budget: usize,
    estimate_tokens: impl Fn(&str) -> u64,
) -> Vec<Unit> {
    let mut result = Vec::new();
    for unit in units {
        let code = snapshot
            .text_for_range(unit.range.clone())
            .collect::<String>();
        let available = budget.saturating_sub(unit.context.len().saturating_add(256));
        if available == 0 {
            continue;
        }
        let mut segment_start = unit.range.start;
        let mut segment_first_row = unit.first_row;
        let mut segment_bytes = 0usize;
        let mut segment_rows = 0usize;
        let mut segment_tokens = 0u64;
        for line in code.split_inclusive('\n') {
            let line_tokens = estimate_tokens(line).saturating_add(12);
            if segment_bytes > 0 && segment_tokens.saturating_add(line_tokens) > available as u64 {
                let mut part = unit.clone();
                part.range = segment_start..segment_start + segment_bytes;
                part.first_row = segment_first_row;
                part.last_row = segment_first_row + segment_rows.saturating_sub(1);
                part.commented_rows = unit
                    .commented_rows
                    .iter()
                    .filter_map(|row| {
                        let absolute = unit.first_row + row;
                        (absolute >= segment_first_row
                            && absolute < segment_first_row + segment_rows)
                            .then(|| absolute - segment_first_row)
                    })
                    .collect();
                result.push(part);
                segment_start += segment_bytes;
                segment_first_row += segment_rows;
                segment_bytes = 0;
                segment_rows = 0;
                segment_tokens = 0;
            }
            if line_tokens > available as u64 {
                segment_start += line.len();
                segment_first_row += 1;
                continue;
            }
            segment_bytes += line.len();
            segment_rows += 1;
            segment_tokens = segment_tokens.saturating_add(line_tokens);
        }
        if segment_bytes > 0 {
            let mut part = unit.clone();
            part.range = segment_start..segment_start + segment_bytes;
            part.first_row = segment_first_row;
            part.last_row = segment_first_row + segment_rows.saturating_sub(1);
            part.commented_rows = unit
                .commented_rows
                .iter()
                .filter_map(|row| {
                    let absolute = unit.first_row + row;
                    (absolute >= segment_first_row && absolute < segment_first_row + segment_rows)
                        .then(|| absolute - segment_first_row)
                })
                .collect();
            result.push(part);
        }
    }
    result
}

pub fn first_non_whitespace_column(line: &str) -> usize {
    line.char_indices()
        .find_map(|(column, character)| (!character.is_whitespace()).then_some(column))
        .unwrap_or(0)
}

fn function(node: tree_sitter::Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "arrow_function"
            | "function_expression"
            | "local_function"
            | "function"
    )
}

pub fn whole_file_unit(snapshot: &language::BufferSnapshot, maximum_lines: u64) -> Option<Unit> {
    let line_count = snapshot.max_point().row as u64 + 1;
    if line_count > maximum_lines
        || snapshot
            .language()
            .is_none_or(|language| language.grammar().is_none())
        || snapshot
            .len()
            .saturating_add(line_count as usize * 12)
            .saturating_add(2048)
            > MAX_REQUEST_BYTES
    {
        return None;
    }
    let range = 0..snapshot.len();
    let mut commented_rows = Vec::new();
    let mut row = 0;
    while row <= snapshot.max_point().row {
        let units = units_at(snapshot, row);
        let next_row = units
            .iter()
            .map(|unit| unit.last_row + 1)
            .max()
            .unwrap_or(row as usize + 1);
        for unit in units {
            commented_rows.extend(
                unit.commented_rows
                    .into_iter()
                    .map(|comment| unit.first_row + comment),
            );
        }
        row = next_row.min(u32::MAX as usize) as u32;
    }
    Some(Unit {
        range: range.clone(),
        owner: range,
        owner_lines: line_count as usize,
        first_row: 0,
        last_row: snapshot.max_point().row as usize,
        context: String::new(),
        commented_rows,
    })
}

pub fn file_units(snapshot: &language::BufferSnapshot, maximum_lines: u64) -> Vec<Unit> {
    if let Some(unit) = whole_file_unit(snapshot, maximum_lines) {
        return vec![unit];
    }
    let mut units = Vec::new();
    let mut seen_ranges = HashSet::new();
    let mut row = 0;
    while row <= snapshot.max_point().row {
        let candidates = units_at(snapshot, row);
        let next = candidates
            .iter()
            .map(|unit| unit.last_row + 1)
            .max()
            .unwrap_or(row as usize + 1);
        for unit in candidates {
            if !unit.range.is_empty() && seen_ranges.insert((unit.range.start, unit.range.end)) {
                units.push(unit);
            }
        }
        row = next.min(u32::MAX as usize) as u32;
    }
    units
}

pub fn units_at(snapshot: &language::BufferSnapshot, row: u32) -> Vec<Unit> {
    let line_start = snapshot.point_to_offset(language::Point::new(row, 0));
    let line_end = snapshot.point_to_offset(language::Point::new(row, snapshot.line_len(row)));
    let line = snapshot
        .text_for_range(line_start..line_end)
        .collect::<String>();
    let content_column = first_non_whitespace_column(&line);
    if content_column == 0 && line.chars().next().is_none_or(char::is_whitespace) {
        return Vec::new();
    }
    let content_offset = line_start + content_column;
    let point = snapshot.offset_to_point(content_offset);
    let Some(mut node) = snapshot.syntax_ancestor(point..point) else {
        return Vec::new();
    };
    let mut owner = node;
    loop {
        if function(node) {
            owner = node;
            break;
        }
        let Some(parent) = node.parent() else {
            break;
        };
        if parent.parent().is_none() {
            break;
        }
        owner = parent;
        node = parent;
    }
    let owner_range = owner.byte_range();
    let owner_lines = owner
        .end_position()
        .row
        .saturating_sub(owner.start_position().row)
        + 1;
    let mut pending = vec![owner];
    let mut result = Vec::new();
    let signature_end = owner
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or(owner.start_byte());
    let context = snapshot
        .text_for_range(owner.start_byte()..signature_end)
        .collect::<String>()
        .chars()
        .take(1024)
        .collect::<String>();
    let mut visits = 0;
    while let Some(node) = pending.pop() {
        visits += 1;
        if visits > 4096 {
            break;
        }
        if node.kind().contains("comment") || !node.is_named() {
            continue;
        }
        if node.byte_range().len() > MAX_INPUT_BYTES {
            let mut cursor = node.walk();
            let children = node.named_children(&mut cursor).collect::<Vec<_>>();
            pending.extend(children.into_iter().rev());
            continue;
        }
        let first_row = node.start_position().row;
        let mut commented_rows = Vec::new();
        if node.prev_named_sibling().is_some_and(|previous| {
            previous.kind().contains("comment") && previous.end_position().row + 1 >= first_row
        }) {
            commented_rows.push(0);
        }
        let mut descendants = vec![node];
        let mut descendant_visits = 0usize;
        while let Some(child) = descendants.pop() {
            descendant_visits += 1;
            if descendant_visits > 4096 {
                break;
            }
            if child.kind().contains("comment") {
                if let Some(next) = child.next_named_sibling() {
                    if child.end_position().row + 1 >= next.start_position().row {
                        commented_rows.push(next.start_position().row.saturating_sub(first_row));
                    }
                }
            } else {
                let mut cursor = child.walk();
                descendants.extend(child.named_children(&mut cursor));
            }
        }
        result.push(Unit {
            range: node.byte_range(),
            owner: owner_range.clone(),
            owner_lines,
            first_row,
            last_row: node.end_position().row,
            context: context.clone(),
            commented_rows,
        });
    }
    result
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
    pub line: usize,
    pub explanation: String,
}

pub fn parse_annotations(
    output: &str,
    code: &str,
    commented_rows: &[usize],
) -> Result<Vec<Annotation>> {
    let output = output.trim();
    let output = output
        .strip_prefix("```json")
        .or_else(|| output.strip_prefix("```"))
        .and_then(|text| text.strip_suffix("```"))
        .unwrap_or(output)
        .trim();
    let annotations: Vec<Annotation> =
        serde_json::from_str(output).context("讲解返回格式无效，请重试或更换模型")?;
    let lines = code.lines().collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    Ok(annotations
        .into_iter()
        .filter(|annotation| {
            annotation.line > 0
                && annotation.line <= lines.len()
                && lines.get(annotation.line - 1).is_some_and(|line| {
                    let line = line.trim();
                    !line.is_empty()
                        && line.chars().any(|character| character.is_alphanumeric())
                        && !matches!(line, "else" | "else {" | "} else {" | "end")
                        && !line.starts_with("//")
                        && !(line.starts_with('#')
                            && !line.starts_with("#[")
                            && !line.starts_with("#include")
                            && !line.starts_with("#define")
                            && !line.starts_with("#if")
                            && !line.starts_with("#endif"))
                })
                && !commented_rows.contains(&(annotation.line - 1))
                && !annotation.explanation.trim().is_empty()
                && annotation.explanation.len() <= 4096
                && !annotation
                    .explanation
                    .chars()
                    .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
                && seen.insert(annotation.line)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn whole_file_is_used_only_within_line_and_request_budgets(cx: &mut gpui::App) {
        let snapshot = language::Buffer::build_snapshot_sync(
            "const FIRST: usize = 1;\nconst SECOND: usize = 2;\n".into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let unit = whole_file_unit(&snapshot, 500).unwrap();
        assert_eq!(unit.range, 0..snapshot.len());
        assert!(whole_file_unit(&snapshot, 1).is_none());

        let oversized = language::Buffer::build_snapshot_sync(
            "x".repeat(MAX_REQUEST_BYTES + 1).into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        assert!(whole_file_unit(&oversized, 500).is_none());
    }

    #[gpui::test]
    fn function_units_preserve_multiline_statements(cx: &mut gpui::App) {
        let code = "fn example() {\n let result = call(\n  1,\n  2,\n );\n}\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let units = units_at(&snapshot, 2);
        assert_eq!(units.len(), 1);
        assert_eq!(
            snapshot
                .text_for_range(units[0].range.clone())
                .collect::<String>(),
            code.trim_end()
        );
        assert_eq!(units[0].owner_lines, 6);
    }

    #[gpui::test]
    fn rows_select_the_enclosing_function_without_root_overlap(cx: &mut gpui::App) {
        let code = "fn first() {\n    let value = 1;\n}\n\nfn second() {\n    let value = 2;\n}\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );

        let first = units_at(&snapshot, 1);
        let second = units_at(&snapshot, 5);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(
            snapshot
                .text_for_range(first[0].range.clone())
                .collect::<String>(),
            "fn first() {\n    let value = 1;\n}"
        );
        assert_eq!(
            snapshot
                .text_for_range(second[0].range.clone())
                .collect::<String>(),
            "fn second() {\n    let value = 2;\n}"
        );
        assert!(units_at(&snapshot, 3).is_empty());
    }

    #[gpui::test]
    fn file_scan_includes_all_functions(cx: &mut gpui::App) {
        let code = "fn first() {\n    work();\n}\n\nfn second() {\n    work();\n}\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let units = file_units(&snapshot, 1);
        assert_eq!(units.len(), 2);
        assert!(units[0].range.end <= units[1].range.start);
    }

    #[gpui::test]
    fn whole_file_reserves_numbering_and_requires_parser(cx: &mut gpui::App) {
        let code = format!(
            "fn example() {{ /*{}*/ }}",
            "x".repeat(MAX_REQUEST_BYTES - 100)
        );
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        assert!(whole_file_unit(&snapshot, 500).is_none());
        let plain = language::Buffer::build_snapshot_sync("hello".into(), None, None, cx);
        assert!(whole_file_unit(&plain, 500).is_none());
    }

    #[gpui::test]
    fn whole_file_comments_cover_following_function(cx: &mut gpui::App) {
        let code = "// Existing explanation\nfn example() { work(); }\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        assert!(
            whole_file_unit(&snapshot, 500)
                .unwrap()
                .commented_rows
                .contains(&1)
        );
    }

    #[gpui::test]
    fn model_budget_splits_without_losing_unicode_or_line_mapping(cx: &mut gpui::App) {
        let code = format!(
            "fn example() {{\n{} }}\n",
            "    let 名称 = 123;\n".repeat(80)
        );
        let snapshot = language::Buffer::build_snapshot_sync(
            code.clone().into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let original = whole_file_unit(&snapshot, 500).unwrap();
        let parts = fit_units_to_budget(&snapshot, vec![original], 512, |text| text.len() as u64);
        assert!(parts.len() > 1);
        let restored = parts
            .iter()
            .map(|part| {
                snapshot
                    .text_for_range(part.range.clone())
                    .collect::<String>()
            })
            .collect::<String>();
        assert_eq!(restored, code);
        for part in parts {
            assert_eq!(
                snapshot.offset_to_point(part.range.start).row as usize,
                part.first_row
            );
            assert!(part.range.len() <= 256);
        }
        assert_eq!(request_code_budget(2048), 0);
        assert_eq!(request_code_budget(8192), 4096);
    }

    #[gpui::test]
    fn model_budget_skips_an_oversized_unicode_line_without_corrupting_ranges(cx: &mut gpui::App) {
        let code = format!("first();\n{}\nlast();\n", "中文".repeat(100));
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let unit = Unit {
            range: 0..snapshot.len(),
            owner: 0..snapshot.len(),
            owner_lines: 3,
            first_row: 0,
            last_row: 2,
            context: String::new(),
            commented_rows: Vec::new(),
        };

        let parts = fit_units_to_budget(&snapshot, vec![unit], 320, |text| text.len() as u64);
        let texts = parts
            .iter()
            .map(|part| {
                snapshot
                    .text_for_range(part.range.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(texts, ["first();\n", "last();\n"]);
        assert_eq!(parts[0].first_row, 0);
        assert_eq!(parts[1].first_row, 2);
    }

    #[test]
    fn indentation_column_uses_the_first_code_byte() {
        assert_eq!(first_non_whitespace_column("    value"), 4);
        assert_eq!(first_non_whitespace_column("\t\tvalue"), 2);
        assert_eq!(first_non_whitespace_column("  变量"), 2);
        assert_eq!(first_non_whitespace_column(""), 0);
        assert_eq!(first_non_whitespace_column("   "), 0);
    }

    #[gpui::test]
    fn top_level_statements_are_separate_units(cx: &mut gpui::App) {
        let code = "const FIRST: usize = 1;\nconst SECOND: usize = 2;\n";
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );

        let first = units_at(&snapshot, 0);
        let second = units_at(&snapshot, 1);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].range, second[0].range);
    }

    #[gpui::test]
    fn oversized_functions_split_without_exceeding_budget(cx: &mut gpui::App) {
        let body = "let value = 123456789;\n".repeat(1500);
        let code = format!("fn large() {{\n{body}}}\n");
        let snapshot = language::Buffer::build_snapshot_sync(
            code.into(),
            Some(language::rust_lang()),
            None,
            cx,
        );
        let units = units_at(&snapshot, 2);
        assert!(!units.is_empty());
        assert!(
            units
                .iter()
                .all(|unit| unit.range.len() <= MAX_INPUT_BYTES && unit.owner_lines > 500)
        );
    }

    #[test]
    fn structural_lines_are_not_explained() {
        let code = "fn example() {\n\n}\n);\n// comment\nlet result = call();";
        let output = (1..=6)
            .map(|line| Annotation {
                line,
                explanation: "解释".into(),
            })
            .collect::<Vec<_>>();
        let parsed =
            parse_annotations(&serde_json::to_string(&output).unwrap(), code, &[]).unwrap();
        assert_eq!(
            parsed
                .iter()
                .map(|annotation| annotation.line)
                .collect::<Vec<_>>(),
            vec![1, 6]
        );
    }

    #[test]
    fn annotations_are_validated_without_quantity_limit_and_are_comment_aware() {
        let result = parse_annotations(r#"[{"line":1,"explanation":"existing"},{"line":2,"explanation":"valid"},{"line":2,"explanation":"duplicate"},{"line":0,"explanation":"invalid"},{"line":9,"explanation":"outside"}]"#, "one\ntwo", &[0]).unwrap();
        assert_eq!(
            result,
            vec![Annotation {
                line: 2,
                explanation: "valid".into()
            }]
        );
        assert!(parse_annotations("not json", "code", &[]).is_err());

        let code = (1..=200)
            .map(|line| format!("let value_{line} = {line};"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = (1..=200)
            .map(|line| Annotation {
                line,
                explanation: format!("解释 {line}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            parse_annotations(&serde_json::to_string(&output).unwrap(), &code, &[])
                .unwrap()
                .len(),
            200
        );
    }
}
