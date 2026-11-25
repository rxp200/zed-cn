//! In-place cell writes for OOXML workbooks.
//!
//! A workbook is a ZIP of XML parts. Writing one cell edits only the affected
//! worksheet part and copies every other part (styles, theme, drawings, macros,
//! ...) through untouched. Re-serializing the whole workbook through a
//! spreadsheet library would drop formatting the reader cannot represent, so
//! the edit is applied to the raw part instead.
//!
//! Only `xlsx`/`xlsm` are writable: `xls`, `xlsb` and `ods` stay read-only
//! because their containers are not OOXML packages.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};

const WORKBOOK_PART: &str = "xl/workbook.xml";
const WORKBOOK_RELS_PART: &str = "xl/_rels/workbook.xml.rels";
const STYLES_PART: &str = "xl/styles.xml";
const CALC_CHAIN_PART: &str = "xl/calcChain.xml";
const CONTENT_TYPES_PART: &str = "[Content_Types].xml";

/// Whether a worksheet container can be written by this module.
pub fn supports_editing(extension: &str) -> bool {
    matches!(extension.to_ascii_lowercase().as_str(), "xlsx" | "xlsm")
}

/// Replaces one cell of `sheet_name` with `input` and returns the new workbook.
///
/// The cell keeps its number format (`s` attribute) so a number typed into a
/// date, currency or percent cell still displays with the sheet's formatting.
/// An empty input clears the cell.
pub fn set_cell_value(
    workbook: &[u8],
    sheet_name: &str,
    row: u32,
    column: u32,
    input: &str,
) -> Result<Vec<u8>> {
    if row == 0 || column == 0 {
        bail!("cell coordinates are one-based");
    }

    let entries = read_entries(workbook)?;
    let parts: HashMap<&str, &[u8]> = entries
        .iter()
        .map(|(name, contents)| (name.as_str(), contents.as_slice()))
        .collect();

    let workbook_xml = parts
        .get(WORKBOOK_PART)
        .ok_or_else(|| anyhow!("workbook part is missing"))?;
    let workbook_xml =
        std::str::from_utf8(workbook_xml).context("workbook part is not UTF-8 encoded XML")?;

    let workbook_rels = parts
        .get(WORKBOOK_RELS_PART)
        .map(|contents| {
            std::str::from_utf8(contents).context("workbook relationships are not UTF-8 encoded")
        })
        .transpose()?
        .unwrap_or("");

    let sheet_part = sheet_part_for(workbook_xml, workbook_rels, sheet_name)?
        .ok_or_else(|| anyhow!("worksheet {sheet_name:?} is not part of the workbook"))?;

    let sheet_xml = parts
        .get(sheet_part.as_str())
        .ok_or_else(|| anyhow!("worksheet part {sheet_part:?} is missing"))?;
    let sheet_xml =
        std::str::from_utf8(sheet_xml).context("worksheet part is not UTF-8 encoded")?;

    let date_1904 = workbook_uses_1904_epoch(workbook_xml);
    let date_style = parts
        .get(STYLES_PART)
        .map(|contents| std::str::from_utf8(contents))
        .transpose()
        .context("styles part is not UTF-8 encoded")?;

    let scan = scan_sheet(sheet_xml, row, column)?;
    let is_date_cell = date_style
        .map(|styles| {
            cell_style_is_date(
                styles,
                scan.target_cell
                    .as_ref()
                    .and_then(|cell| cell.style.as_deref()),
            )
        })
        .unwrap_or(false);

    let value = classify(input, is_date_cell, date_1904);
    if scan.sheet_data.is_none() {
        bail!("worksheet {sheet_name:?} has no sheetData element");
    }
    if scan.target_cell.is_none() && scan.has_unlabelled_rows {
        bail!("worksheet {sheet_name:?} has rows without an r attribute");
    }
    if scan.target_cell.is_none() && scan.target_row_unlabelled_cells {
        bail!("worksheet {sheet_name:?} has cells without an r attribute");
    }
    let replacement = build_edit(sheet_xml, &scan, row, column, &value);

    let mut updated_sheet = String::with_capacity(
        sheet_xml.len()
            + replacement
                .edits
                .iter()
                .map(|edit| edit.text.len())
                .sum::<usize>(),
    );
    let mut cursor = 0;
    for edit in replacement.edits {
        updated_sheet.push_str(
            sheet_xml
                .get(cursor..edit.start)
                .ok_or_else(|| anyhow!("cell edit does not align with the worksheet text"))?,
        );
        updated_sheet.push_str(&edit.text);
        cursor = edit.end;
    }
    updated_sheet.push_str(
        sheet_xml
            .get(cursor..)
            .ok_or_else(|| anyhow!("cell edit runs past the end of the worksheet"))?,
    );

    let drop_calc_chain = replacement.drop_calc_chain;
    let mut updated_entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(entries.len());
    for (name, contents) in entries {
        if name == sheet_part {
            // The rewritten sheet is used exactly once, so move it instead of
            // keeping a second full copy of the worksheet in memory.
            updated_entries.push((name, std::mem::take(&mut updated_sheet).into_bytes()));
        } else {
            updated_entries.push((name, contents));
        }
    }
    if drop_calc_chain {
        strip_calc_chain(&mut updated_entries)?;
    }
    write_entries(updated_entries)
}

/// Largest decompressed package entry the editor will rewrite.
const MAX_WORKBOOK_ENTRY_BYTES: usize = 128 * 1024 * 1024;
/// Largest total decompressed workbook the editor will rewrite.
const MAX_WORKBOOK_TOTAL_BYTES: usize = 512 * 1024 * 1024;

/// Reads one entry without trusting the size declared in the ZIP directory: a
/// tiny entry can declare gigabytes and would otherwise reserve that much
/// memory before a single byte is validated.
fn read_entry_limited(
    reader: &mut impl std::io::Read,
    limit: usize,
    name: &str,
) -> Result<Vec<u8>> {
    let mut contents = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("decompressing workbook entry {name:?}"))?;
    anyhow::ensure!(
        contents.len() <= limit,
        format!("workbook entry {name:?} is too large to rewrite")
    );
    Ok(contents)
}

fn read_entries(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).context("opening workbook package")?;
    let mut entries = Vec::with_capacity(archive.len());
    let mut total_bytes = 0usize;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .with_context(|| format!("reading workbook entry {index}"))?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_owned();
        let contents = read_entry_limited(&mut file, MAX_WORKBOOK_ENTRY_BYTES, &name)?;
        total_bytes = total_bytes.saturating_add(contents.len());
        anyhow::ensure!(
            total_bytes <= MAX_WORKBOOK_TOTAL_BYTES,
            "workbook package is too large to rewrite"
        );
        entries.push((name, contents));
    }
    Ok(entries)
}

fn write_entries(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, contents) in entries {
        writer
            .start_file(name.as_str(), options)
            .with_context(|| format!("writing workbook entry {name:?}"))?;
        writer
            .write_all(&contents)
            .with_context(|| format!("compressing workbook entry {name:?}"))?;
    }
    let cursor = writer.finish().context("finishing workbook package")?;
    Ok(cursor.into_inner())
}

/// Resolves the worksheet part for `sheet_name` through the workbook's
/// relationships, so sheet order and part file names need not agree.
fn sheet_part_for(
    workbook_xml: &str,
    workbook_rels: &str,
    sheet_name: &str,
) -> Result<Option<String>> {
    let mut relationship_id = None;
    let mut reader = Reader::from_str(workbook_xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) | Ok(Event::Empty(element))
                if element.local_name().as_ref() == b"sheet" =>
            {
                let name = attribute(&element, &reader, b"name");
                if name.as_deref() == Some(sheet_name) {
                    relationship_id = attribute(&element, &reader, b"id");
                    if relationship_id.is_some() {
                        break;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(error).context("parsing the workbook part"),
        }
    }

    let Some(relationship_id) = relationship_id else {
        return Ok(None);
    };

    let mut target = None;
    let mut reader = Reader::from_str(workbook_rels);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) | Ok(Event::Empty(element))
                if element.local_name().as_ref() == b"Relationship" =>
            {
                if attribute(&element, &reader, b"Id").as_deref() == Some(relationship_id.as_str())
                {
                    target = attribute(&element, &reader, b"Target");
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(error).context("parsing the workbook relationships"),
        }
    }

    let Some(target) = target else {
        return Ok(None);
    };
    Ok(Some(resolve_part_path("xl", &target)))
}

fn resolve_part_path(directory: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_owned();
    }
    let mut segments: Vec<&str> = directory.split('/').filter(|s| !s.is_empty()).collect();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

fn attribute(element: &BytesStart, reader: &Reader<&[u8]>, name: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.local_name().as_ref() == name)
        .and_then(|attribute| {
            attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .ok()
        })
        .map(|value| value.into_owned())
}

fn workbook_uses_1904_epoch(workbook_xml: &str) -> bool {
    let mut reader = Reader::from_str(workbook_xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) | Ok(Event::Empty(element))
                if element.local_name().as_ref() == b"workbookPr" =>
            {
                return attribute(&element, &reader, b"date1904")
                    .is_some_and(|value| matches!(value.as_str(), "1" | "true"));
            }
            Ok(Event::Eof) => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

// ---------------------------------------------------------------- cell styles

/// Built-in number formats Excel renders as dates or times.
const BUILTIN_DATE_FORMATS: &[(u32, u32)] = &[(14, 22), (27, 36), (45, 47), (50, 58)];

fn cell_style_is_date(styles_xml: &str, style_index: Option<&str>) -> bool {
    let Some(style_index) = style_index.and_then(|value| value.parse::<usize>().ok()) else {
        return false;
    };
    style_formats(styles_xml)
        .get(style_index)
        .copied()
        .unwrap_or(false)
}

/// Per `cellXfs` entry: whether its number format renders a date or time.
fn style_formats(styles_xml: &str) -> Vec<bool> {
    let mut custom_formats: HashMap<u32, String> = HashMap::new();
    let mut formats = Vec::new();
    let mut reader = Reader::from_str(styles_xml);
    let mut in_cell_xfs = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"numFmt" => {
                    if let (Some(id), Some(code)) = (
                        attribute(&element, &reader, b"numFmtId"),
                        attribute(&element, &reader, b"formatCode"),
                    ) {
                        if let Ok(id) = id.parse() {
                            custom_formats.insert(id, code);
                        }
                    }
                }
                b"cellXfs" => in_cell_xfs = true,
                b"xf" if in_cell_xfs => {
                    formats.push(xf_is_date(&element, &reader, &custom_formats))
                }
                _ => {}
            },
            Ok(Event::Empty(element)) => match element.local_name().as_ref() {
                b"numFmt" => {
                    if let (Some(id), Some(code)) = (
                        attribute(&element, &reader, b"numFmtId"),
                        attribute(&element, &reader, b"formatCode"),
                    ) {
                        if let Ok(id) = id.parse() {
                            custom_formats.insert(id, code);
                        }
                    }
                }
                b"xf" if in_cell_xfs => {
                    formats.push(xf_is_date(&element, &reader, &custom_formats))
                }
                _ => {}
            },
            Ok(Event::End(element)) => {
                if element.local_name().as_ref() == b"cellXfs" {
                    in_cell_xfs = false;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return formats,
        }
    }
    formats
}

fn xf_is_date(
    element: &BytesStart,
    reader: &Reader<&[u8]>,
    custom_formats: &HashMap<u32, String>,
) -> bool {
    let id: u32 = attribute(element, reader, b"numFmtId")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if id == 0 {
        return false;
    }
    if BUILTIN_DATE_FORMATS
        .iter()
        .any(|(first, last)| (*first..=*last).contains(&id))
    {
        return true;
    }
    custom_formats
        .get(&id)
        .is_some_and(|format| format_is_date_like(format))
}

/// Whether a custom format code contains date/time tokens.
fn format_is_date_like(format: &str) -> bool {
    let mut in_quotes = false;
    let mut in_brackets = false;
    let mut escaped = false;
    for character in format.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' | '_' | '*' => escaped = true,
            '"' => in_quotes = !in_quotes,
            '[' if !in_quotes => in_brackets = true,
            ']' if !in_quotes => in_brackets = false,
            _ if in_quotes || in_brackets => {}
            'y' | 'Y' | 'd' | 'D' | 'h' | 'H' | 's' | 'S' | 'm' | 'M' => return true,
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------------- cell values

#[derive(Clone, Debug, PartialEq)]
enum CellValue {
    Clear,
    Number(f64),
    Boolean(bool),
    Text(String),
    Formula(String),
}

fn classify(input: &str, is_date_cell: bool, date_1904: bool) -> CellValue {
    if input.is_empty() {
        return CellValue::Clear;
    }
    if let Some(formula) = input.strip_prefix('=') {
        let formula = formula.trim();
        if formula.is_empty() {
            return CellValue::Clear;
        }
        return CellValue::Formula(formula.to_owned());
    }
    let trimmed = input.trim();
    if is_date_cell && let Some(serial) = parse_datetime_serial(trimmed, date_1904) {
        return CellValue::Number(serial);
    }
    if trimmed.eq_ignore_ascii_case("true") {
        return CellValue::Boolean(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return CellValue::Boolean(false);
    }
    if let Ok(value) = trimmed.parse::<i64>() {
        return CellValue::Number(value as f64);
    }
    if let Ok(value) = trimmed.parse::<f64>()
        && value.is_finite()
    {
        return CellValue::Number(value);
    }
    CellValue::Text(input.to_owned())
}

/// Accepts the date and time spellings people type into a date-formatted cell.
fn parse_datetime_serial(input: &str, date_1904: bool) -> Option<f64> {
    if input.is_empty() {
        return None;
    }
    if let Ok(serial) = input.parse::<f64>()
        && serial.is_finite()
    {
        return Some(serial);
    }

    let (date_part, time_part) = split_date_time(input);
    let date = [
        "%Y-%m-%d",
        "%Y/%m/%d",
        "%Y.%m.%d",
        "%Y年%m月%d日",
        "%m/%d/%Y",
        "%d/%m/%Y",
    ]
    .iter()
    .find_map(|format| NaiveDate::parse_from_str(date_part, format).ok())?;

    let time = time_part
        .and_then(|time| {
            ["%H:%M:%S", "%H:%M", "%H:%M:%S%.f"]
                .iter()
                .find_map(|format| NaiveTime::parse_from_str(time, format).ok())
        })
        .unwrap_or_default();

    let datetime = NaiveDateTime::new(date, time);
    let epoch = if date_1904 {
        NaiveDate::from_ymd_opt(1904, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(1899, 12, 30)?
    };
    let seconds = datetime - epoch.and_time(NaiveTime::default());
    Some(seconds.num_seconds() as f64 / 86_400.0)
}

fn split_date_time(input: &str) -> (&str, Option<&str>) {
    for separator in [' ', 'T', '\u{3000}'] {
        if let Some((date, time)) = input.split_once(separator) {
            let time = time.trim();
            if !time.is_empty() {
                return (date.trim(), Some(time));
            }
        }
    }
    (input.trim(), None)
}

// ------------------------------------------------------------ worksheet edits

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

struct BuiltEdit {
    edits: Vec<Edit>,
    drop_calc_chain: bool,
}

struct ElementSpan {
    start: usize,
    end: usize,
    content_end: usize,
    is_empty: bool,
}

#[derive(Clone)]
struct RowSpan {
    row: u32,
    start: usize,
    end: usize,
    content_end: usize,
    is_empty: bool,
    tag: String,
}

struct CellSpan {
    start: usize,
    end: usize,
    style: Option<String>,
    has_formula: bool,
}

struct SheetScan {
    sheet_data: Option<ElementSpan>,
    rows: Vec<RowSpan>,
    target_row: Option<RowSpan>,
    target_cell: Option<CellSpan>,
    /// (start, end of `</...>`, current reference)
    dimension: Option<(usize, usize, String)>,
    prefix: String,
    /// A row without `r` cannot be given a position, so inserting around it
    /// would silently reorder or duplicate cells.
    has_unlabelled_rows: bool,
    target_row_unlabelled_cells: bool,
}

fn scan_sheet(xml: &str, row: u32, column: u32) -> Result<SheetScan> {
    let mut reader = Reader::from_str(xml);
    let mut scan = SheetScan {
        sheet_data: None,
        rows: Vec::new(),
        target_row: None,
        target_cell: None,
        dimension: None,
        prefix: String::new(),
        has_unlabelled_rows: false,
        target_row_unlabelled_cells: false,
    };
    let mut open_row: Option<RowSpan> = None;
    let mut open_cell: Option<(usize, Option<String>, bool)> = None;

    loop {
        let start = reader.buffer_position() as usize;
        let event = reader
            .read_event()
            .context("parsing the worksheet part while locating the cell")?;
        let end = reader.buffer_position() as usize;

        match event {
            Event::Start(element) => match element.local_name().as_ref() {
                b"sheetData" => {
                    scan.prefix = element_prefix(&element);
                    scan.sheet_data = Some(ElementSpan {
                        start,
                        end,
                        content_end: end,
                        is_empty: false,
                    });
                }
                b"row" => {
                    let row_number = attribute(&element, &reader, b"r")
                        .and_then(|value| value.parse::<u32>().ok());
                    scan.has_unlabelled_rows |= row_number.is_none();
                    let span = RowSpan {
                        row: row_number.unwrap_or(0),
                        start,
                        end,
                        content_end: end,
                        is_empty: false,
                        tag: if row_number == Some(row) {
                            String::from_utf8_lossy(element.as_ref()).into_owned()
                        } else {
                            String::new()
                        },
                    };
                    if row_number == Some(row) {
                        scan.target_row = Some(span.clone());
                    }
                    open_row = Some(span);
                }
                b"c" => {
                    let reference = attribute(&element, &reader, b"r");
                    let style = attribute(&element, &reader, b"s");
                    let matches = open_row.as_ref().is_some_and(|open| open.row == row)
                        && reference
                            .as_deref()
                            .and_then(parse_reference)
                            .is_some_and(|cell| cell.column == column);
                    if matches {
                        open_cell = Some((start, style, false));
                    } else if open_row.as_ref().is_some_and(|open| open.row == row)
                        && reference.is_none()
                    {
                        scan.target_row_unlabelled_cells = true;
                    }
                }
                b"f" => {
                    if let Some((_, _, has_formula)) = open_cell.as_mut() {
                        *has_formula = true;
                    }
                }
                b"dimension" => {
                    if let Some(reference) = attribute(&element, &reader, b"ref") {
                        scan.dimension = Some((start, end, reference));
                    }
                }
                _ => {}
            },
            Event::Empty(element) => match element.local_name().as_ref() {
                b"sheetData" => {
                    scan.sheet_data = Some(ElementSpan {
                        start,
                        end,
                        content_end: end,
                        is_empty: true,
                    });
                }
                b"row" => {
                    let row_number = attribute(&element, &reader, b"r")
                        .and_then(|value| value.parse::<u32>().ok());
                    scan.has_unlabelled_rows |= row_number.is_none();
                    scan.rows.push(RowSpan {
                        row: row_number.unwrap_or(0),
                        start,
                        end,
                        content_end: end,
                        is_empty: true,
                        tag: String::from_utf8_lossy(element.as_ref()).into_owned(),
                    });
                }
                b"c" => {
                    let reference = attribute(&element, &reader, b"r");
                    let matches = open_row.as_ref().is_some_and(|open| open.row == row)
                        && reference
                            .as_deref()
                            .and_then(parse_reference)
                            .is_some_and(|cell| cell.column == column);
                    if !matches
                        && open_row.as_ref().is_some_and(|open| open.row == row)
                        && reference.is_none()
                    {
                        scan.target_row_unlabelled_cells = true;
                    }
                    if matches {
                        scan.target_cell = Some(CellSpan {
                            start,
                            end,
                            style: attribute(&element, &reader, b"s"),
                            has_formula: false,
                        });
                    }
                }
                b"f" => {
                    if let Some((_, _, has_formula)) = open_cell.as_mut() {
                        *has_formula = true;
                    }
                }
                b"dimension" => {
                    if let Some(reference) = attribute(&element, &reader, b"ref") {
                        scan.dimension = Some((start, end, reference));
                    }
                }
                _ => {}
            },
            Event::End(element) => match element.local_name().as_ref() {
                b"row" => {
                    if let Some(mut span) = open_row.take() {
                        span.end = end;
                        span.content_end = start;
                        if span.row == row {
                            scan.target_row = Some(span.clone());
                        }
                        scan.rows.push(span);
                    }
                }
                b"c" => {
                    if let Some((cell_start, style, has_formula)) = open_cell.take() {
                        scan.target_cell = Some(CellSpan {
                            start: cell_start,
                            end,
                            style,
                            has_formula,
                        });
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(scan)
}

fn build_edit(xml: &str, scan: &SheetScan, row: u32, column: u32, value: &CellValue) -> BuiltEdit {
    let mut edits = Vec::new();
    let reference = format!("{}{}", column_letters(column), row);
    let cell_xml = build_cell(&scan.prefix, &reference, style_of(scan), value);
    // A replaced formula leaves a stale chain entry behind, and a new formula
    // is only picked up once Excel rebuilds the chain.
    let drop_calc_chain = matches!(value, CellValue::Formula(_))
        || (scan
            .target_cell
            .as_ref()
            .is_some_and(|cell| cell.has_formula)
            && !matches!(value, CellValue::Formula(_)));

    match (&scan.target_cell, &scan.target_row) {
        (Some(cell), _) => match &cell_xml {
            Some(replacement) => edits.push(Edit {
                start: cell.start,
                end: cell.end,
                text: replacement.clone(),
            }),
            None => edits.push(Edit {
                start: cell.start,
                end: cell.end,
                text: String::new(),
            }),
        },
        (None, Some(target_row)) => {
            if let Some(cell_xml) = cell_xml {
                if target_row.is_empty {
                    let tag = target_row
                        .tag
                        .trim_end()
                        .strip_suffix("/>")
                        .map(|tag| format!("{tag}>"))
                        .unwrap_or_else(|| target_row.tag.clone());
                    edits.push(Edit {
                        start: target_row.start,
                        end: target_row.end,
                        text: format!("{tag}{cell_xml}</{}row>", scan.prefix),
                    });
                } else {
                    let insert_at = insertion_offset_in_row(xml, target_row, column);
                    edits.push(Edit {
                        start: insert_at,
                        end: insert_at,
                        text: cell_xml,
                    });
                }
            }
        }
        (None, None) => {
            if let Some(cell_xml) = cell_xml {
                let prefix = &scan.prefix;
                let new_row = format!("<{prefix}row r=\"{row}\">{cell_xml}</{prefix}row>");
                let Some(sheet_data) = &scan.sheet_data else {
                    return BuiltEdit {
                        edits,
                        drop_calc_chain,
                    };
                };
                if sheet_data.is_empty {
                    edits.push(Edit {
                        start: sheet_data.start,
                        end: sheet_data.end,
                        text: format!("<{prefix}sheetData>{new_row}</{prefix}sheetData>"),
                    });
                } else {
                    let insert_at = insertion_offset_for_row(scan, row, sheet_data.content_end);
                    edits.push(Edit {
                        start: insert_at,
                        end: insert_at,
                        text: new_row,
                    });
                }
            }
        }
    }

    if let Some((start, end, current)) = &scan.dimension
        && let Some(expanded) = expand_dimension(current, row, column)
    {
        edits.push(Edit {
            start: *start,
            end: *end,
            text: format!("<{}dimension ref=\"{expanded}\"/>", scan.prefix),
        });
    }

    edits.sort_by_key(|edit| edit.start);
    BuiltEdit {
        edits,
        drop_calc_chain,
    }
}

fn style_of(scan: &SheetScan) -> Option<&str> {
    scan.target_cell
        .as_ref()
        .and_then(|cell| cell.style.as_deref())
}

fn insertion_offset_in_row(xml: &str, target_row: &RowSpan, column: u32) -> usize {
    let content = &xml[target_row.start..target_row.content_end];
    let mut reader = Reader::from_str(content);
    loop {
        let start = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(element)) | Ok(Event::Empty(element))
                if element.local_name().as_ref() == b"c" =>
            {
                let existing = attribute(&element, &reader, b"r")
                    .as_deref()
                    .and_then(parse_reference)
                    .map(|cell| cell.column);
                if existing.is_some_and(|existing| existing > column) {
                    return target_row.start + start;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    target_row.content_end
}

fn insertion_offset_for_row(scan: &SheetScan, row: u32, sheet_data_content_end: usize) -> usize {
    let mut later_row = None;
    for candidate in &scan.rows {
        if candidate.row > row
            && later_row.is_none_or(|current: &RowSpan| candidate.row < current.row)
        {
            later_row = Some(candidate);
        }
    }
    later_row
        .map(|candidate| candidate.start)
        .unwrap_or(sheet_data_content_end)
}

fn build_cell(
    prefix: &str,
    reference: &str,
    style: Option<&str>,
    value: &CellValue,
) -> Option<String> {
    let style_attribute = style
        .filter(|style| !style.is_empty())
        .map(|style| format!(" s=\"{style}\""))
        .unwrap_or_default();
    match value {
        CellValue::Clear => None,
        CellValue::Number(number) => Some(format!(
            "<{prefix}c r=\"{reference}\"{style_attribute}><{prefix}v>{}</{prefix}v></{prefix}c>",
            format_number(*number)
        )),
        CellValue::Boolean(flag) => Some(format!(
            "<{prefix}c r=\"{reference}\"{style_attribute} t=\"b\"><{prefix}v>{}</{prefix}v></{prefix}c>",
            u8::from(*flag)
        )),
        CellValue::Text(text) => Some(format!(
            "<{prefix}c r=\"{reference}\"{style_attribute} t=\"inlineStr\"><{prefix}is><{prefix}t xml:space=\"preserve\">{}</{prefix}t></{prefix}is></{prefix}c>",
            escape_xml(text)
        )),
        CellValue::Formula(formula) => Some(format!(
            "<{prefix}c r=\"{reference}\"{style_attribute}><{prefix}f>{}</{prefix}f></{prefix}c>",
            escape_xml(formula)
        )),
    }
}

fn format_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        (number as i64).to_string()
    } else {
        number.to_string()
    }
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn element_prefix(element: &BytesStart) -> String {
    let name = element.name();
    let Some(prefix) = name.prefix() else {
        return String::new();
    };
    if prefix.as_ref().is_empty() {
        String::new()
    } else {
        format!("{}:", String::from_utf8_lossy(prefix.as_ref()))
    }
}

fn expand_dimension(current: &str, row: u32, column: u32) -> Option<String> {
    let (first, last) = current.split_once(':').unwrap_or((current, current));
    let first = parse_reference(first)?;
    let last = parse_reference(last)?;
    let max_row = row.max(last.row);
    let max_column = column.max(last.column);
    let min_row = row.min(first.row);
    let min_column = column.min(first.column);
    let expanded = format!(
        "{}{}:{}{}",
        column_letters(min_column),
        min_row,
        column_letters(max_column),
        max_row
    );
    (expanded != current).then_some(expanded)
}

fn strip_calc_chain(entries: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    entries.retain(|(name, _)| name != CALC_CHAIN_PART);

    if let Some((_, contents)) = entries
        .iter_mut()
        .find(|(name, _)| name == CONTENT_TYPES_PART)
    {
        if let Ok(xml) = std::str::from_utf8(contents) {
            if let Some(updated) =
                remove_elements(xml, b"Override", b"PartName", &[CALC_CHAIN_PART])
            {
                *contents = updated.into_bytes();
            }
        }
    }

    if let Some((_, contents)) = entries
        .iter_mut()
        .find(|(name, _)| name == WORKBOOK_RELS_PART)
    {
        if let Ok(xml) = std::str::from_utf8(contents) {
            if let Some(updated) =
                remove_elements(xml, b"Relationship", b"Target", &["calcChain.xml"])
            {
                *contents = updated.into_bytes();
            }
        }
    }

    Ok(())
}

/// Removes every `element` whose `attribute` value contains one of `needles`.
fn remove_elements(
    xml: &str,
    element_name: &[u8],
    attribute_name: &[u8],
    needles: &[&str],
) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut spans = Vec::new();
    let mut open: Option<(usize, bool)> = None;
    loop {
        let start = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(element)) if element.local_name().as_ref() == element_name => {
                let matches = attribute(&element, &reader, attribute_name)
                    .is_some_and(|value| needles.iter().any(|needle| value.contains(needle)));
                open = Some((start, matches));
            }
            Ok(Event::End(element)) if element.local_name().as_ref() == element_name => {
                if let Some((element_start, matches)) = open.take()
                    && matches
                {
                    spans.push((element_start, reader.buffer_position() as usize));
                }
            }
            Ok(Event::Empty(element)) if element.local_name().as_ref() == element_name => {
                let matches = attribute(&element, &reader, attribute_name)
                    .is_some_and(|value| needles.iter().any(|needle| value.contains(needle)));
                if matches {
                    spans.push((start, reader.buffer_position() as usize));
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    if spans.is_empty() {
        return None;
    }
    let mut updated = xml.to_owned();
    for (start, end) in spans.into_iter().rev() {
        updated.replace_range(start..end, "");
    }
    Some(updated)
}

// --------------------------------------------------------------- coordinates

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reference {
    row: u32,
    column: u32,
}

fn parse_reference(reference: &str) -> Option<Reference> {
    let reference = reference.trim();
    let letters: String = reference
        .chars()
        .take_while(|character| character.is_ascii_alphabetic())
        .collect();
    let digits: String = reference
        .chars()
        .skip(letters.len())
        .take_while(|character| character.is_ascii_digit())
        .collect();
    if letters.is_empty() || digits.is_empty() {
        return None;
    }
    Some(Reference {
        row: digits.parse().ok()?,
        column: column_index(&letters)?,
    })
}

fn column_index(letters: &str) -> Option<u32> {
    let mut index = 0u32;
    for character in letters.chars() {
        let character = character.to_ascii_uppercase();
        if !character.is_ascii_uppercase() {
            return None;
        }
        index = index.checked_mul(26)? + (character as u32 - b'A' as u32 + 1);
    }
    (index > 0).then_some(index)
}

fn column_letters(mut column: u32) -> String {
    let mut letters = Vec::new();
    while column > 0 {
        let remainder = (column - 1) % 26;
        letters.push((b'A' + remainder as u8) as char);
        column = (column - 1) / 26;
    }
    letters.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompressed_entries_are_read_with_a_limit() {
        let mut small = Cursor::new(b"0123456789".to_vec());
        assert_eq!(
            read_entry_limited(&mut small, 10, "small").expect("within limit"),
            b"0123456789".to_vec()
        );
        // The ZIP directory can declare any size; the reader must stop at the
        // limit instead of trusting it or growing the buffer unboundedly.
        let mut oversized = Cursor::new(b"0123456789".to_vec());
        assert!(read_entry_limited(&mut oversized, 4, "oversized").is_err());
    }

    const PACKAGE_RELS_PART: &str = "_rels/.rels";

    fn entries_of(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
        read_entries(bytes)
            .expect("fixture should be a readable package")
            .into_iter()
            .collect()
    }

    fn entry_text<'a>(entries: &'a HashMap<String, Vec<u8>>, name: &str) -> &'a str {
        std::str::from_utf8(entries.get(name).expect("entry should exist")).unwrap()
    }

    const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;

    fn workbook_xml() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="应答" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#
            .to_owned()
    }

    fn workbook_rels() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#
            .to_owned()
    }

    fn styles_xml() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<numFmts count="1"><numFmt numFmtId="164" formatCode="yyyy&quot;年&quot;m&quot;月&quot;d&quot;日&quot;"/></numFmts>
<cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="164"/></cellXfs>
</styleSheet>"#
            .to_owned()
    }

    fn sheet_xml(sheet_data: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:B2"/>
<sheetData>{sheet_data}</sheetData>
</worksheet>"#
        )
    }

    fn build_package(sheet_data: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let mut entries: Vec<(String, Vec<u8>)> = vec![
            (CONTENT_TYPES_PART.into(), CONTENT_TYPES.as_bytes().to_vec()),
            (
                PACKAGE_RELS_PART.into(),
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#
                    .to_vec(),
            ),
            (WORKBOOK_PART.into(), workbook_xml().into_bytes()),
            (WORKBOOK_RELS_PART.into(), workbook_rels().into_bytes()),
            (STYLES_PART.into(), styles_xml().into_bytes()),
            (
                "xl/worksheets/sheet1.xml".into(),
                sheet_xml(sheet_data).into_bytes(),
            ),
        ];
        for (name, contents) in extra {
            entries.push(((*name).to_owned(), contents.as_bytes().to_vec()));
        }
        write_entries(entries).expect("fixture package should build")
    }

    fn build_package_with(sheet_contents: &str) -> Vec<u8> {
        let entries: Vec<(String, Vec<u8>)> = vec![
            (CONTENT_TYPES_PART.into(), CONTENT_TYPES.as_bytes().to_vec()),
            (
                PACKAGE_RELS_PART.into(),
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#
                    .to_vec(),
            ),
            (WORKBOOK_PART.into(), workbook_xml().into_bytes()),
            (WORKBOOK_RELS_PART.into(), workbook_rels().into_bytes()),
            (STYLES_PART.into(), styles_xml().into_bytes()),
            ("xl/worksheets/sheet1.xml".into(), sheet_contents.as_bytes().to_vec()),
        ];
        write_entries(entries).expect("fixture package should build")
    }

    fn write(package: &[u8], row: u32, column: u32, value: &str) -> Vec<u8> {
        set_cell_value(package, "应答", row, column, value).expect("cell write should succeed")
    }

    fn cell(package: &[u8]) -> String {
        entry_text(&entries_of(package), "xl/worksheets/sheet1.xml").to_owned()
    }

    #[test]
    fn replaces_an_existing_number_in_place() {
        let package = build_package(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>名称</t></is></c><c r="B1"><v>1</v></c></row>"#,
            &[],
        );
        let updated = cell(&write(&package, 1, 2, "7"));
        assert!(updated.contains(r#"<c r="B1"><v>7</v></c>"#), "{updated}");
        assert!(updated.contains("<t>名称</t>"));
        assert!(!updated.contains(">1<"));
    }

    #[test]
    fn writes_text_as_an_inline_string_with_escapes() {
        let package = build_package(r#"<row r="1"><c r="A1"/></row>"#, &[]);
        let updated = cell(&write(&package, 1, 1, "a & b <c>"));
        assert!(
            updated.contains(r#"<c r="A1" t="inlineStr"><is><t xml:space="preserve">a &amp; b &lt;c&gt;</t></is></c>"#),
            "{updated}"
        );
    }

    #[test]
    fn inserts_a_missing_cell_in_column_order() {
        let package = build_package(
            r#"<row r="2"><c r="A2"><v>1</v></c><c r="D2"><v>4</v></c></row>"#,
            &[],
        );
        let updated = cell(&write(&package, 2, 2, "2"));
        let b_position = updated.find(r#"<c r="B2"><v>2</v></c>"#).unwrap();
        let d_position = updated.find(r#"<c r="D2"><v>4</v></c>"#).unwrap();
        assert!(b_position < d_position, "{updated}");
    }

    #[test]
    fn inserts_a_missing_row_in_row_order() {
        let package = build_package(
            r#"<row r="1"><c r="A1"><v>1</v></c></row><row r="3"><c r="A3"><v>3</v></c></row>"#,
            &[],
        );
        let updated = cell(&write(&package, 2, 1, "two"));
        let row2 = updated.find(r#"<row r="2">"#).unwrap();
        let row3 = updated.find(r#"<row r="3">"#).unwrap();
        assert!(row2 < row3, "{updated}");
        assert!(updated.contains("two"));
    }

    #[test]
    fn expands_a_self_closing_row() {
        let package = build_package(r#"<row r="5"/>"#, &[]);
        let updated = cell(&write(&package, 5, 3, "value"));
        assert!(updated.contains(r#"<row r="5"><c r="C5""#), "{updated}");
        assert!(updated.contains("</row>"), "{updated}");
    }

    #[test]
    fn expands_an_empty_sheet_data() {
        let package = build_package("", &[]);
        let updated = cell(&write(&package, 1, 1, "first"));
        assert!(
            updated.contains(r#"<sheetData><row r="1"><c r="A1" t="inlineStr">"#),
            "{updated}"
        );
    }

    #[test]
    fn clearing_a_cell_removes_it() {
        let package = build_package(
            r#"<row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>"#,
            &[],
        );
        let updated = cell(&write(&package, 1, 1, ""));
        assert!(!updated.contains(r#"<c r="A1""#), "{updated}");
        assert!(updated.contains(r#"<c r="B1"><v>2</v></c>"#));
    }

    #[test]
    fn keeps_the_number_format_of_the_edited_cell() {
        let package = build_package(r#"<row r="1"><c r="A1" s="1"/></row>"#, &[]);
        let updated = cell(&write(&package, 1, 1, "2026-10-26"));
        // Style 1 is built-in format 14 (date); the typed date becomes a serial.
        assert!(
            updated.contains(r#"<c r="A1" s="1"><v>46321</v></c>"#),
            "{updated}"
        );
    }

    #[test]
    fn keeps_custom_date_formats_through_the_style_index() {
        let package = build_package(r#"<row r="1"><c r="B1" s="2"/></row>"#, &[]);
        let updated = cell(&write(&package, 1, 2, "2026/10/26 12:00"));
        assert!(updated.contains(r#"s="2"><v>46321.5</v>"#), "{updated}");
    }

    #[test]
    fn text_typed_into_a_date_cell_stays_text() {
        let package = build_package(r#"<row r="1"><c r="A1" s="1"/></row>"#, &[]);
        let updated = cell(&write(&package, 1, 1, "待定"));
        assert!(updated.contains(r#"t="inlineStr""#), "{updated}");
    }

    #[test]
    fn writes_booleans_and_formulas() {
        let package = build_package(r#"<row r="1"><c r="A1"/></row>"#, &[]);
        let boolean = cell(&write(&package, 1, 1, "TRUE"));
        assert!(boolean.contains(r#"t="b"><v>1</v>"#), "{boolean}");

        let formula = cell(&write(&package, 1, 1, "=SUM(B2:B9)"));
        assert!(formula.contains("<f>SUM(B2:B9)</f>"), "{formula}");
        assert!(!formula.contains("<v>"), "{formula}");
    }

    #[test]
    fn removes_calc_chain_when_a_formula_cell_is_overwritten() {
        let package = build_package(
            r#"<row r="1"><c r="A1"><f>1+1</f><v>2</v></c></row>"#,
            &[(
                CALC_CHAIN_PART,
                r#"<?xml version="1.0" encoding="UTF-8"?><calcChain xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><c r="A1" i="1"/></calcChain>"#,
            )],
        );
        let updated = write(&package, 1, 1, "5");
        let entries = entries_of(&updated);
        assert!(!entries.contains_key(CALC_CHAIN_PART));
        assert!(!entry_text(&entries, CONTENT_TYPES_PART).contains("calcChain"));
        assert!(!entry_text(&entries, WORKBOOK_RELS_PART).contains("calcChain"));
    }

    #[test]
    fn expands_the_dimension_when_the_cell_is_outside_it() {
        let package = build_package(r#"<row r="1"><c r="A1"><v>1</v></c></row>"#, &[]);
        let updated = cell(&write(&package, 4, 3, "x"));
        assert!(updated.contains(r#"<dimension ref="A1:C4"/>"#), "{updated}");
    }

    #[test]
    fn leaves_other_parts_byte_identical() {
        let package = build_package(
            r#"<row r="1"><c r="A1"><v>1</v></c></row>"#,
            &[("xl/media/image1.png", "not really a png")],
        );
        let before = entries_of(&package);
        let after = entries_of(&write(&package, 1, 1, "2"));
        for (name, contents) in &before {
            if name == "xl/worksheets/sheet1.xml" {
                continue;
            }
            assert_eq!(after.get(name), Some(contents), "part {name} changed");
        }
    }

    #[test]
    fn resolves_the_sheet_part_through_relationships() {
        let mut entries = read_entries(&build_package("", &[])).unwrap();
        for (name, contents) in entries.iter_mut() {
            if name == WORKBOOK_PART {
                *contents = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="第二张" sheetId="2" r:id="rId9"/></sheets>
</workbook>"#
                    .as_bytes()
                    .to_vec();
            } else if name == WORKBOOK_RELS_PART {
                *contents = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="/xl/worksheets/renamed.xml"/>
</Relationships>"#
                    .to_vec();
            }
        }
        entries.push((
            "xl/worksheets/renamed.xml".into(),
            sheet_xml(r#"<row r="1"><c r="A1"><v>1</v></c></row>"#).into_bytes(),
        ));
        let package = write_entries(entries).unwrap();
        let updated = set_cell_value(&package, "第二张", 1, 1, "9").unwrap();
        let after = entries_of(&updated);
        assert!(entry_text(&after, "xl/worksheets/renamed.xml").contains("<v>9</v>"));
    }

    #[test]
    fn refuses_to_write_into_an_unlabelled_row() {
        let package = build_package(r#"<row><c><v>1</v></c></row>"#, &[]);
        let error = set_cell_value(&package, "应答", 1, 1, "2").unwrap_err();
        assert!(
            error.to_string().contains("without an r attribute"),
            "{error}"
        );
    }

    #[test]
    fn refuses_to_insert_beside_an_unlabelled_cell() {
        let package = build_package(r#"<row r="1"><c><v>1</v></c></row>"#, &[]);
        let error = set_cell_value(&package, "应答", 1, 2, "2").unwrap_err();
        assert!(
            error.to_string().contains("cells without an r attribute"),
            "{error}"
        );
    }

    #[test]
    fn refuses_a_worksheet_without_sheet_data() {
        let package = build_package_with(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/></worksheet>"#,
        );
        let error = set_cell_value(&package, "应答", 1, 1, "2").unwrap_err();
        assert!(error.to_string().contains("no sheetData"), "{error}");
    }

    #[test]
    fn reports_an_unknown_sheet() {
        let package = build_package("", &[]);
        let error = set_cell_value(&package, "不存在", 1, 1, "x").unwrap_err();
        assert!(error.to_string().contains("not part of the workbook"));
    }

    #[test]
    fn only_ooxml_workbooks_are_writable() {
        assert!(supports_editing("xlsx"));
        assert!(supports_editing("XLSM"));
        for extension in ["xls", "xlsb", "ods", "csv"] {
            assert!(!supports_editing(extension));
        }
    }

    #[test]
    fn column_references_round_trip() {
        for (column, letters) in [
            (1, "A"),
            (26, "Z"),
            (27, "AA"),
            (52, "AZ"),
            (53, "BA"),
            (702, "ZZ"),
            (703, "AAA"),
        ] {
            assert_eq!(column_letters(column), letters);
            assert_eq!(
                parse_reference(&format!("{letters}12")),
                Some(Reference { row: 12, column })
            );
        }
        assert_eq!(parse_reference("2"), None);
        assert_eq!(parse_reference("A"), None);
    }

    #[test]
    fn classified_values_keep_leading_whitespace() {
        assert_eq!(
            classify("  x  ", false, false),
            CellValue::Text("  x  ".into())
        );
        assert_eq!(classify("", false, false), CellValue::Clear);
        assert_eq!(classify("12.5", false, false), CellValue::Number(12.5));
        assert_eq!(classify(" false ", false, false), CellValue::Boolean(false));
    }
}
