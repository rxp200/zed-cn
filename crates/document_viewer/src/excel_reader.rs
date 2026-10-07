use std::{
    collections::HashMap,
    io::Cursor,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use calamine::Reader as _;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, Window, div,
};
use tabular_data_preview::TableView;
use tabular_data_preview::types::{LineNumber, TableCell, TableLikeContent};
use ui::prelude::*;
use ui::table_row::TableRow;

use crate::DocumentItem;

/// Upper bound on rendered rows per sheet.
const MAX_ROWS: usize = 20_000;
/// Upper bound on rendered columns per sheet.
const MAX_COLS: usize = 256;
/// Upper bound on rendered cells per sheet, whichever limit is hit first.
const MAX_CELLS: usize = 1_000_000;

type Spreadsheet = calamine::Sheets<Cursor<SharedBytes>>;

/// `Cursor` 需要内部类型实现 `AsRef<[u8]>` 与 `Clone`，而 `Arc<Vec<u8>>` 没有这两个
/// 实现；包一层就能零拷贝地把工作簿交给 calamine。
#[derive(Clone)]
struct SharedBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// Parses and renders a spreadsheet workbook (`xlsx`, `xlsm`, `xls`, `xlsb`, `ods`)
/// natively, reusing the tabular data preview grid.
pub struct ExcelReader {
    item: Entity<DocumentItem>,
    project: Entity<project::Project>,
    workbook: Option<Arc<Mutex<Spreadsheet>>>,
    pub(crate) sheet_names: Vec<String>,
    active_sheet: usize,
    sheet_cache: HashMap<usize, Arc<SheetContent>>,
    truncation: Option<Truncation>,
    table: Entity<TableView>,
    pub(crate) error: Option<SharedString>,
    _open_task: Task<Result<()>>,
}

struct SheetContent {
    table: TableLikeContent,
    truncation: Option<Truncation>,
}

#[derive(Clone, Copy)]
struct Truncation {
    shown_rows: usize,
    total_rows: usize,
}

impl ExcelReader {
    pub fn new(
        item: Entity<DocumentItem>,
        project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| TableView::new(window, cx));

        let bytes = item.read(cx).contents.clone();
        let open_task = cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move {
                    calamine::open_workbook_auto_from_rs(Cursor::new(SharedBytes(bytes)))
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |reader, cx| match opened {
                Ok(workbook) => {
                    reader.sheet_names = workbook.sheet_names().to_vec();
                    reader.workbook = Some(Arc::new(Mutex::new(workbook)));
                    reader.load_sheet(0, cx);
                }
                Err(error) => {
                    let message = i18n::t!("868012c570c52c40");
                    reader.error = Some(format!("{message}: {error}").into());
                    reader
                        .table
                        .update(cx, |table, cx| table.set_loading(false, cx));
                    cx.notify();
                }
            })
        });

        Self {
            item,
            project,
            workbook: None,
            sheet_names: Vec::new(),
            active_sheet: 0,
            sheet_cache: HashMap::new(),
            truncation: None,
            table,
            error: None,
            _open_task: open_task,
        }
    }

    pub fn load_sheet(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(workbook) = self.workbook.clone() else {
            return;
        };
        let Some(name) = self.sheet_names.get(index).cloned() else {
            return;
        };
        self.active_sheet = index;

        if let Some(cached) = self.sheet_cache.get(&index) {
            let cached = cached.clone();
            self.truncation = cached.truncation;
            self.table.update(cx, |table, cx| {
                table.set_loading(false, cx);
                table.set_contents(cached.table.clone(), cx);
            });
            cx.notify();
            return;
        }

        self.table
            .update(cx, |table, cx| table.set_loading(true, cx));
        let task = cx.background_spawn(async move {
            let parsed = {
                let mut workbook = workbook.lock().ok()?;
                workbook.worksheet_range(&name).ok()
            };
            parsed.map(|range| sheet_to_table(&range))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |reader, cx| {
                match result {
                    Some((contents, truncation)) => {
                        reader.truncation = truncation;
                        let contents = Arc::new(SheetContent {
                            table: contents,
                            truncation,
                        });
                        reader
                            .sheet_cache
                            .insert(reader.active_sheet, contents.clone());
                        reader.table.update(cx, |table, cx| {
                            table.set_loading(false, cx);
                            table.set_contents(contents.table.clone(), cx);
                        });
                    }
                    None => {
                        reader.error = Some(i18n::t!("a269e5f69bb6e903").into());
                        reader
                            .table
                            .update(cx, |table, cx| table.set_loading(false, cx));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

impl EventEmitter<()> for ExcelReader {}

impl Focusable for ExcelReader {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.read(cx).focus_handle(cx)
    }
}

impl Render for ExcelReader {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut header = h_flex()
            .flex_shrink_0()
            .w_full()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.toolbar_background)
            .px_2()
            .gap_1()
            .overflow_x_hidden();

        if self.sheet_names.len() > 1 {
            for (index, name) in self.sheet_names.iter().enumerate() {
                let is_active = index == self.active_sheet;
                header = header.child(
                    div()
                        .id(("sheet-tab", index))
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .rounded_t_sm()
                        .when(is_active, |this| {
                            this.bg(colors.element_selected)
                                .border_b_2()
                                .border_color(colors.border_focused)
                        })
                        .when(!is_active, |this| {
                            this.hover(|style| style.bg(colors.element_hover))
                        })
                        .child(Label::new(name.clone()).size(LabelSize::Small))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.load_sheet(index, cx);
                        })),
                );
            }
        }

        if let Some(truncation) = self.truncation {
            header = header.child(div().flex_1()).child(
                Label::new(i18n::t!(
                    "a3c3a58431375a1c",
                    shown = truncation.shown_rows,
                    total = truncation.total_rows
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        }

        let body: AnyElement = if let Some(error) = self.error.clone() {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new(error).color(Color::Error))
                .into_any_element()
        } else if self.workbook.is_none() {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new(i18n::t!("ccdcdb625442cbeb")).color(Color::Muted))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .child(self.table.clone())
                .into_any_element()
        };

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("ExcelReader")
            .on_action(cx.listener(|this, _: &editor::RevealInFileManager, _, cx| {
                let path = this.item.read(cx).abs_path(cx);
                if let Some(path) = path {
                    this.project
                        .update(cx, |project, cx| project.reveal_path(&path, cx));
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .child(header)
            .child(body)
    }
}

/// Converts a calamine range into tabular preview content, applying the
/// row/column/cell caps. Returns the content plus truncation information.
fn sheet_to_table(
    range: &calamine::Range<calamine::Data>,
) -> (TableLikeContent, Option<Truncation>) {
    let total_rows = range.height();
    let total_cols = range.width();
    let shown_cols = total_cols.min(MAX_COLS);

    let mut rows = Vec::new();
    let mut cells = 0usize;
    let mut truncated = false;
    'rows: for (row_index, row) in range.rows().enumerate() {
        if row_index >= MAX_ROWS {
            truncated = true;
            break;
        }
        let mut cells_in_row = Vec::with_capacity(shown_cols);
        for col in 0..shown_cols {
            if cells >= MAX_CELLS {
                truncated = true;
                break 'rows;
            }
            cells += 1;
            let cell = row.get(col).map(cell_text);
            cells_in_row.push(match cell {
                Some(text) => TableCell::Generated(text),
                None => TableCell::Virtual,
            });
        }
        rows.push(TableRow::from_vec(cells_in_row, shown_cols));
    }
    truncated |= total_rows > rows.len() || total_cols > shown_cols;

    let headers = TableRow::from_vec(
        (0..shown_cols)
            .map(|col| TableCell::Generated(column_name(col).into()))
            .collect(),
        shown_cols,
    );
    let line_numbers = (1..=rows.len()).map(LineNumber::Line).collect();

    let content = TableLikeContent {
        number_of_cols: shown_cols,
        headers,
        rows,
        line_numbers,
    };
    let truncation = truncated.then_some(Truncation {
        shown_rows: content.rows.len(),
        total_rows,
    });
    (content, truncation)
}

/// Spreadsheet column label: `A`..`Z`, `AA`..`AZ`, ...
fn column_name(mut index: usize) -> String {
    let mut name = String::new();
    loop {
        name.insert(0, (b'A' + (index % 26) as u8) as char);
        index /= 26;
        if index == 0 {
            break;
        }
        index -= 1;
    }
    name
}

fn cell_text(data: &calamine::Data) -> SharedString {
    use calamine::Data;
    match data {
        Data::Empty => SharedString::default(),
        Data::Int(value) => value.to_string().into(),
        Data::Float(value) => {
            if value.fract() == 0.0 && value.abs() < 1e15 {
                (*value as i64).to_string().into()
            } else {
                value.to_string().into()
            }
        }
        Data::String(value) => value.clone().into(),
        Data::Bool(value) => value.to_string().into(),
        Data::DateTime(value) => value
            .as_datetime()
            .map(|datetime| {
                if datetime.time() == chrono::NaiveTime::default() {
                    datetime.format("%Y-%m-%d").to_string()
                } else {
                    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
                }
            })
            .unwrap_or_else(|| value.to_string())
            .into(),
        Data::DateTimeIso(value) | Data::DurationIso(value) => value.clone().into(),
        Data::Error(error) => error.to_string().into(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn column_names_follow_spreadsheet_convention() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(51), "AZ");
        assert_eq!(column_name(52), "BA");
    }

    #[test]
    fn cell_text_formats_values() {
        use calamine::Data;
        assert_eq!(cell_text(&Data::Empty).as_ref(), "");
        assert_eq!(cell_text(&Data::Int(42)).as_ref(), "42");
        assert_eq!(cell_text(&Data::Float(2.0)).as_ref(), "2");
        assert_eq!(cell_text(&Data::Float(0.5)).as_ref(), "0.5");
        assert_eq!(cell_text(&Data::Bool(true)).as_ref(), "true");
        assert_eq!(cell_text(&Data::String("文本".into())).as_ref(), "文本");
        assert_eq!(
            cell_text(&Data::Error(calamine::CellErrorType::Div0)).as_ref(),
            "#DIV/0!"
        );
    }

    #[test]
    fn sheet_to_table_caps_rows() {
        let mut range = calamine::Range::new((0, 0), (MAX_ROWS as u32 + 10, 2));
        range.set_value((0, 0), calamine::Data::Int(7));
        let (content, truncation) = sheet_to_table(&range);
        assert_eq!(content.rows.len(), MAX_ROWS);
        assert_eq!(content.number_of_cols, 3);
        assert!(truncation.is_some());
        let truncation = truncation.unwrap();
        assert_eq!(truncation.shown_rows, MAX_ROWS);
        assert_eq!(truncation.total_rows, MAX_ROWS + 11);
        assert_eq!(content.headers[0].display_value().unwrap().as_ref(), "A");
        assert_eq!(content.headers[2].display_value().unwrap().as_ref(), "C");
    }

    /// Builds a minimal xlsx workbook with one sheet containing two rows.
    pub(crate) fn minimal_xlsx() -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default();
        let mut add = |name: &str, contents: &str| {
            writer.start_file(name, options).unwrap();
            writer.write_all(contents.as_bytes()).unwrap();
        };
        add(
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
        );
        add(
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
        );
        add(
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="数据" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
        );
        add(
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
        );
        add(
            "xl/worksheets/sheet1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>名称</t></is></c><c r="B1" t="inlineStr"><is><t>数量</t></is></c></row>
<row r="2"><c r="A2" t="inlineStr"><is><t>苹果</t></is></c><c r="B2"><v>42</v></c></row>
</sheetData>
</worksheet>"#,
        );
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn parses_minimal_xlsx() {
        let bytes = minimal_xlsx();
        let mut workbook = calamine::open_workbook_auto_from_rs(Cursor::new(bytes))
            .expect("minimal xlsx should open");
        assert_eq!(workbook.sheet_names(), &["数据"]);
        let range = workbook.worksheet_range("数据").expect("sheet should read");
        let (content, truncation) = sheet_to_table(&range);
        assert!(truncation.is_none());
        assert_eq!(content.rows.len(), 2);
        assert_eq!(content.rows[0][0].display_value().unwrap().as_ref(), "名称");
        assert_eq!(content.rows[1][1].display_value().unwrap().as_ref(), "42");
    }
}
