//! In-place editing of delimited-text table cells.
//!
//! The grid itself is source-agnostic: it renders whatever [`crate::types::TableLikeContent`]
//! it is handed and reports committed values through
//! [`crate::TableEdited`]. This module holds the delimited-text half of editing: how a value
//! is quoted, where in the source buffer a cell lives, and how the replacement is applied to
//! the editor's buffer so undo, dirty state and saving keep working as usual.

use std::ops::Range;

use anyhow::Result;
use editor::{Editor, MultiBufferOffset};
use gpui::{App, Entity};
use text::BufferSnapshot;
use ui::table_row::TableRow;

use crate::types::{AnyColumn, TableCell};

/// A single in-place edit expressed as a buffer replacement.
pub(crate) struct CellEditTarget {
    /// Byte range in the source buffer to replace. Empty for a cell the row does not
    /// currently contain, in which case the value is appended after the last real field.
    pub(crate) range: Range<usize>,
    pub(crate) replacement: String,
}

/// Quotes a value for a delimited-text field when it would otherwise be ambiguous.
pub(crate) fn escape_delimited_value(value: &str, delimiter: char) -> String {
    let needs_quoting = value
        .chars()
        .any(|character| character == delimiter || character == '"' || character == '\n' || character == '\r');
    if !needs_quoting {
        return value.to_owned();
    }

    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        if character == '"' {
            escaped.push('"');
        }
        escaped.push(character);
    }
    escaped.push('"');
    escaped
}

/// Locates the buffer range a cell edit has to replace.
///
/// `Virtual` cells are the padding a short row gets from the parser: they have no source
/// span, so a value written into one has to be appended after the row's last real field to
/// keep the implicit padding intact.
pub(crate) fn cell_edit_target(
    row: &TableRow<TableCell>,
    column: AnyColumn,
    value: &str,
    delimiter: char,
    snapshot: &BufferSnapshot,
) -> Option<CellEditTarget> {
    match row.get(column)? {
        TableCell::Real { position, .. } => Some(CellEditTarget {
            range: snapshot.offset_for_anchor(&position.start)
                ..snapshot.offset_for_anchor(&position.end),
            replacement: escape_delimited_value(value, delimiter),
        }),
        TableCell::Virtual => {
            let insert_at = row.as_slice().iter().rev().find_map(|cell| match cell {
                TableCell::Real { position, .. } => Some(snapshot.offset_for_anchor(&position.end)),
                TableCell::Generated(_) | TableCell::Virtual => None,
            })?;
            Some(CellEditTarget {
                range: insert_at..insert_at,
                replacement: format!("{delimiter}{}", escape_delimited_value(value, delimiter)),
            })
        }
        TableCell::Generated(_) => None,
    }
}

/// Applies a planned cell edit as one buffer transaction.
pub(crate) fn apply_cell_edit(
    editor: &Entity<Editor>,
    target: CellEditTarget,
    cx: &mut App,
) -> Result<()> {
    editor.update(cx, |editor, cx| {
        if editor.read_only(cx) {
            anyhow::bail!("the buffer is read-only");
        }
        editor.edit(
            [(
                MultiBufferOffset(target.range.start)..MultiBufferOffset(target.range.end),
                target.replacement,
            )],
            cx,
        );
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::from_buffer_with_delimiter;
    use text::{Buffer, BufferId, ReplicaId};

    fn snapshot(text: &str) -> BufferSnapshot {
        Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), text.to_owned())
            .snapshot()
            .clone()
    }

    fn edit_row(
        text: &str,
        data_row: usize,
        column: usize,
        value: &str,
        delimiter: char,
    ) -> String {
        let buffer = snapshot(text);
        let contents = from_buffer_with_delimiter(&buffer, delimiter);
        let row = contents.rows.get(data_row).expect("row should exist");
        let target = cell_edit_target(row, AnyColumn(column), value, delimiter, &buffer)
            .expect("cell should be editable");
        let mut edited = text.to_owned();
        edited.replace_range(target.range, &target.replacement);
        edited
    }

    #[test]
    fn quotes_only_when_required() {
        assert_eq!(escape_delimited_value("plain", ','), "plain");
        assert_eq!(escape_delimited_value("", ','), "");
        assert_eq!(escape_delimited_value("a,b", ','), "\"a,b\"");
        assert_eq!(escape_delimited_value("a\"b", ','), "\"a\"\"b\"");
        assert_eq!(escape_delimited_value("two\nlines", ','), "\"two\nlines\"");
        assert_eq!(escape_delimited_value("two\r\nlines", ','), "\"two\r\nlines\"");
        assert_eq!(escape_delimited_value("a|b", '|'), "\"a|b\"");
        assert_eq!(escape_delimited_value("a,b", '|'), "a,b");
    }

    #[test]
    fn replaces_a_plain_field() {
        assert_eq!(edit_row("a,b\n1,2\n", 0, 1, "changed", ','), "a,b\n1,changed\n");
    }

    #[test]
    fn replaces_a_quoted_field_without_keeping_stale_quotes() {
        assert_eq!(edit_row("a,b\n1,\"x,y\"\n", 0, 1, "plain", ','), "a,b\n1,plain\n");
        assert_eq!(
            edit_row("a,b\n1,plain\n", 0, 1, "x,y", ','),
            "a,b\n1,\"x,y\"\n"
        );
    }

    #[test]
    fn escapes_embedded_quotes() {
        assert_eq!(
            edit_row("a,b\n1,2\n", 0, 1, "say \"hi\"", ','),
            "a,b\n1,\"say \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn replaces_a_multiline_field_in_place() {
        assert_eq!(
            edit_row("a,b\n1,\"two\nlines\"\n", 0, 1, "one", ','),
            "a,b\n1,one\n"
        );
        assert_eq!(
            edit_row("a,b\n1,2\n", 0, 1, "two\nlines", ','),
            "a,b\n1,\"two\nlines\"\n"
        );
    }

    #[test]
    fn appends_a_missing_field() {
        assert_eq!(edit_row("a,b,c\n1,2\n", 0, 2, "3", ','), "a,b,c\n1,2,3\n");
    }

    #[test]
    fn appends_an_empty_missing_field() {
        assert_eq!(edit_row("a,b,c\n1,\n", 0, 2, "3", ','), "a,b,c\n1,,3\n");
    }

    #[test]
    fn round_trips_edited_values_through_the_parser() {
        for (delimiter, text, value) in [
            (',', "a,b\n1,2\n", "x,y"),
            (',', "a,b\n1,2\n", "multi\nline"),
            (',', "a,b\n1,2\n", "say \"hi\""),
            ('\t', "a\tb\n1\t2\n", "x\ty"),
            (';', "a;b\n1;2\n", "x;y"),
            ('|', "a|b\n1|2\n", "x|y"),
        ] {
            let edited = edit_row(text, 0, 1, value, delimiter);
            let buffer = snapshot(&edited);
            let contents = from_buffer_with_delimiter(&buffer, delimiter);
            let round_tripped = contents.rows[0][1]
                .display_value()
                .expect("edited cell should have a value");
            assert_eq!(round_tripped.as_ref(), value, "edited text: {edited:?}");
        }
    }
}
