//! Read-only view for Word (OOXML) documents.
//!
//! A `.docx` file is a ZIP container, like an EPUB book, so this reader fetches
//! one archive entry at a time through `Project::read_office_entry` and never
//! keeps the container in memory. The main document part is converted into a
//! bounded HTML fragment that the existing Markdown pipeline renders, which
//! keeps fonts, headings, lists, tables, links and images consistent with the
//! EPUB reader.

use std::{
    collections::HashMap,
    io::Cursor,
    mem,
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, ImageSource, Pixels, Point,
    ScrollHandle, SharedString, Task, Window, actions, div, px,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownOptions, MarkdownStyle};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use ui::prelude::*;
use ui::utils::WithRemSize;
use util::ResultExt as _;

use crate::DocumentItem;
use crate::epub_reader::{image_format_for_path, resolve_zip_path, zip_dir};

actions!(
    word_reader,
    [
        /// Increase the reading font size.
        IncreaseFontSize,
        /// Decrease the reading font size.
        DecreaseFontSize,
        /// Reset the reading font size.
        ResetFontSize,
        /// Scroll one page down.
        ScrollPageDown,
        /// Scroll one page up.
        ScrollPageUp,
        /// Scroll down by one line.
        ScrollDown,
        /// Scroll up by one line.
        ScrollUp,
        /// Scroll to the start of the document.
        ScrollToStart,
        /// Scroll to the end of the document.
        ScrollToEnd
    ]
);

const DEFAULT_FONT_SIZE: f32 = 16.0;
const MIN_FONT_SIZE: f32 = 10.0;
const MAX_FONT_SIZE: f32 = 32.0;
const FONT_SIZE_STEP: f32 = 1.0;
const SCROLL_LINE: f32 = 48.0;

/// Upper bound for the decompressed main document part converted to HTML.
const MAX_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;
/// Upper bound for a single auxiliary part (styles, numbering, relationships).
const MAX_PART_BYTES: u64 = 16 * 1024 * 1024;
/// Upper bound for the generated HTML fragment.
const MAX_HTML_BYTES: usize = 24 * 1024 * 1024;
const MAX_BLOCKS: usize = 50_000;
/// Image budgets mirror the EPUB reader: bounded counts and byte totals per
/// document, so a media-heavy file cannot grow the renderer without bound.
const MAX_IMAGES: usize = 128;
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_DECODED_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

const HEADINGS: [&str; 6] = ["h1", "h2", "h3", "h4", "h5", "h6"];

pub struct WordReader {
    item: Entity<DocumentItem>,
    project: Entity<project::Project>,
    focus_handle: FocusHandle,
    languages: Option<Arc<language::LanguageRegistry>>,
    pub(crate) error: Option<SharedString>,
    pub(crate) markdown: Option<Entity<Markdown>>,
    images: Rc<HashMap<String, ImageSource>>,
    progress: Option<(u64, u64)>,
    loading_resource: Option<String>,
    truncated: bool,
    scroll_handle: ScrollHandle,
    font_size: f32,
    image_cache: Entity<gpui::RetainAllImageCache>,
    _load_task: Option<Task<Result<()>>>,
}

impl WordReader {
    pub fn new(
        item: Entity<DocumentItem>,
        project: Entity<project::Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|reader, cx| {
            reader._load_task = None;
            for image in reader.images.values() {
                image.remove_asset(cx);
            }
        })
        .detach();

        let mut reader = Self {
            item,
            project,
            focus_handle: cx.focus_handle(),
            languages: None,
            error: None,
            markdown: None,
            images: Rc::new(HashMap::new()),
            progress: None,
            loading_resource: None,
            truncated: false,
            scroll_handle: ScrollHandle::new(),
            font_size: DEFAULT_FONT_SIZE,
            image_cache: gpui::RetainAllImageCache::new(cx),
            _load_task: None,
        };
        let source = reader.new_source(cx);
        reader.start_load(source, None, cx);
        reader
    }

    fn new_source(&self, cx: &App) -> Arc<WordSource> {
        Arc::new(WordSource {
            project: self.project.downgrade(),
            path: self.item.read(cx).project_path(cx),
            snapshot: Mutex::new(None),
            limiter: Arc::new(async_lock::Semaphore::new(1)),
        })
    }

    fn start_load(
        &mut self,
        source: Arc<WordSource>,
        restore_offset: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = load_document(&source, &this, cx).await;
            this.update(cx, |reader, cx| {
                reader.loading_resource = None;
                reader.progress = None;
                match loaded {
                    Ok((html, images, truncated)) => {
                        reader.images = Rc::new(images);
                        reader.truncated = truncated;
                        let languages = reader.languages.clone();
                        reader.markdown = Some(cx.new(|cx| {
                            Markdown::new_with_options(
                                html.into(),
                                languages,
                                None,
                                MarkdownOptions {
                                    parse_html: true,
                                    ..Default::default()
                                },
                                cx,
                            )
                        }));
                        // A reload restores the reader's place; the initial load
                        // starts at the top.
                        if let Some(offset) = restore_offset {
                            reader.scroll_handle.set_offset(offset);
                        }
                    }
                    Err(error) => {
                        log::warn!("failed to read Word document: {error:#}");
                        reader.error = Some(i18n::t!("59b305a1af94171e", error = error).into());
                    }
                }
                cx.notify();
            })
        }));
    }

    /// Re-reads the document after the file changed on disk. A fresh source
    /// drops the cached size and modification time, and the reader keeps its
    /// scroll position and font size.
    pub fn reload(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let restore_offset = Some(self.scroll_handle.offset());
        self.error = None;
        self.markdown = None;
        self.progress = None;
        self.loading_resource = None;
        for image in self.images.values() {
            image.remove_asset(cx);
        }
        self.images = Rc::new(HashMap::new());
        self.image_cache = gpui::RetainAllImageCache::new(cx);
        let source = self.new_source(cx);
        self.start_load(source, restore_offset, cx);
        cx.notify();
    }

    pub fn with_languages(mut self, languages: Arc<language::LanguageRegistry>) -> Self {
        self.languages = Some(languages);
        self
    }

    pub fn increase_font_size(&mut self, cx: &mut Context<Self>) {
        self.font_size = (self.font_size + FONT_SIZE_STEP).min(MAX_FONT_SIZE);
        cx.notify();
    }

    pub fn decrease_font_size(&mut self, cx: &mut Context<Self>) {
        self.font_size = (self.font_size - FONT_SIZE_STEP).max(MIN_FONT_SIZE);
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let item = self.item.clone();
        let source = Arc::new(WordSource {
            project: project.downgrade(),
            path: item.read(cx).project_path(cx),
            snapshot: Mutex::new(None),
            limiter: Arc::new(async_lock::Semaphore::new(1)),
        });
        self.error = None;
        self.markdown = None;
        self.truncated = false;
        self.progress = None;
        self.loading_resource = None;
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = load_document(&source, &this, cx).await;
            this.update(cx, |reader, cx| {
                reader.loading_resource = None;
                reader.progress = None;
                match loaded {
                    Ok((html, images, truncated)) => {
                        reader.images = Rc::new(images);
                        reader.truncated = truncated;
                        let languages = reader.languages.clone();
                        reader.markdown = Some(cx.new(|cx| {
                            Markdown::new_with_options(
                                html.into(),
                                languages,
                                None,
                                MarkdownOptions {
                                    parse_html: true,
                                    ..Default::default()
                                },
                                cx,
                            )
                        }));
                    }
                    Err(error) => {
                        log::warn!("failed to read Word document: {error:#}");
                        reader.error = Some(i18n::t!("59b305a1af94171e", error = error).into());
                    }
                }
                cx.notify();
            })
        }));
        cx.notify();
    }
}

impl EventEmitter<()> for WordReader {}

impl Focusable for WordReader {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WordReader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let content: AnyElement = if let Some(error) = self.error.clone() {
            div()
                .size_full()
                .flex()
                .flex_col()
                .gap_3()
                .items_center()
                .justify_center()
                .child(Label::new(error).color(Color::Error))
                .child(
                    ui::Button::new("word-retry", i18n::t!("b8784c8dd5636ff2"))
                        .on_click(cx.listener(|reader, _, _, cx| reader.retry(cx))),
                )
                .into_any_element()
        } else if let Some(markdown) = self.markdown.clone() {
            let images = self.images.clone();
            let reader = cx.entity().downgrade();
            let mut style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
            style.base_text_style.font_size = px(self.font_size).into();
            let markdown_element = MarkdownElement::new(markdown, style)
                .image_resolver(move |url, _cx| {
                    images.get(url.split('#').next().unwrap_or(url)).cloned()
                })
                .on_url_click(move |url, _window, cx| {
                    if url.starts_with("http://") || url.starts_with("https://") {
                        cx.open_url(&url);
                    } else {
                        reader
                            .update(cx, |reader, cx| {
                                reader.scroll_handle.set_offset(gpui::point(px(0.), px(0.)));
                                cx.notify();
                            })
                            .log_err();
                    }
                });
            div()
                .id("word-content-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll_handle)
                .child(
                    div()
                        .w_full()
                        .max_w(px(880.))
                        .mx_auto()
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .when(self.truncated, |element| {
                            element.child(
                                Label::new(i18n::t!("9995c30406b70f02")).color(Color::Warning),
                            )
                        })
                        .child(markdown_element),
                )
                .into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .flex_col()
                .gap_3()
                .items_center()
                .justify_center()
                .child(ui::SpinnerLabel::new())
                .child(Label::new(i18n::t!("d71a0adaa59e490a")).color(Color::Muted))
                .when_some(self.loading_resource.clone(), |element, resource| {
                    element.child(
                        Label::new(resource)
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                })
                .when_some(self.progress, |element, (loaded, total)| {
                    element
                        .child(div().w(px(240.)).child(ui::ProgressBar::new(
                            "word-load-progress",
                            loaded as f32,
                            total.max(1) as f32,
                            cx,
                        )))
                        .child(Label::new(format!(
                            "{:.0}%",
                            loaded as f64 / total.max(1) as f64 * 100.0
                        )))
                })
                .into_any_element()
        };

        div()
            .image_cache(self.image_cache.clone())
            .key_context("WordReader")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _, cx| {
                this.increase_font_size(cx);
            }))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, _, cx| {
                this.decrease_font_size(cx);
            }))
            .on_action(cx.listener(|this, _: &ResetFontSize, _, cx| {
                this.font_size = DEFAULT_FONT_SIZE;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollPageDown, _, cx| {
                let height = this.scroll_handle.bounds().size.height;
                this.scroll_handle
                    .set_offset(this.scroll_handle.offset() + gpui::point(px(0.), height * 0.9));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollPageUp, _, cx| {
                let height = this.scroll_handle.bounds().size.height;
                this.scroll_handle
                    .set_offset(this.scroll_handle.offset() - gpui::point(px(0.), height * 0.9));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollDown, _, cx| {
                this.scroll_handle
                    .set_offset(this.scroll_handle.offset() + gpui::point(px(0.), px(SCROLL_LINE)));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollUp, _, cx| {
                this.scroll_handle
                    .set_offset(this.scroll_handle.offset() - gpui::point(px(0.), px(SCROLL_LINE)));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollToStart, _, cx| {
                this.scroll_handle.set_offset(gpui::point(px(0.), px(0.)));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollToEnd, _, cx| {
                this.scroll_handle.scroll_to_bottom();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &editor::RevealInFileManager, _, cx| {
                let path = this.item.read(cx).abs_path(cx);
                if let Some(path) = path {
                    this.project
                        .update(cx, |project, cx| project.reveal_path(&path, cx));
                }
            }))
            .size_full()
            .bg(colors.editor_background)
            .child(
                WithRemSize::new(px(self.font_size))
                    .size_full()
                    .child(content),
            )
    }
}

/// Reads archive entries of the open Word document one chunk at a time.
///
/// The container is never transferred or retained in full: each request asks
/// the project for a range of one entry, and every entry has its own size cap.
struct WordSource {
    project: gpui::WeakEntity<project::Project>,
    path: project::ProjectPath,
    snapshot: Mutex<Option<(u64, rpc::proto::Timestamp)>>,
    limiter: Arc<async_lock::Semaphore>,
}

impl WordSource {
    async fn read(
        &self,
        entry: &str,
        max_bytes: u64,
        reader: Option<&gpui::WeakEntity<WordReader>>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<Vec<u8>> {
        let _permit = self.limiter.acquire_arc().await;
        let mut bytes = Vec::new();
        let mut total = None;
        loop {
            let snapshot = *self
                .snapshot
                .lock()
                .map_err(|_| anyhow::anyhow!("document snapshot lock poisoned"))?;
            let response = self
                .project
                .update(cx, |project, cx| {
                    project.read_office_entry(
                        self.path.clone(),
                        entry.to_owned(),
                        bytes.len() as u64,
                        snapshot,
                        cx,
                    )
                })?
                .await?;
            let file = response.file.context("missing office document metadata")?;
            anyhow::ensure!(
                file.worktree_id == self.path.worktree_id.to_proto()
                    && file.path == self.path.path.as_unix_str()
                    && response.total_size <= project::epub::OFFICE_FILE_LIMIT,
                "invalid office document metadata"
            );
            let mtime = file
                .mtime
                .context("missing office document modification time")?;
            if let Some((size, expected_mtime)) = snapshot {
                anyhow::ensure!(
                    size == response.total_size && expected_mtime == mtime,
                    i18n::t!("d757df97219fc135")
                );
            } else {
                *self
                    .snapshot
                    .lock()
                    .map_err(|_| anyhow::anyhow!("document snapshot lock poisoned"))? =
                    Some((response.total_size, mtime));
            }
            anyhow::ensure!(
                response.entry_size <= project::epub::OFFICE_ENTRY_LIMIT
                    && response.content.len() <= project::epub::OFFICE_CHUNK_SIZE
                    && bytes.len() as u64 + response.content.len() as u64 <= response.entry_size
                    && total.is_none_or(|total| total == response.entry_size),
                "invalid office document entry chunk"
            );
            anyhow::ensure!(
                response.entry_size <= max_bytes,
                i18n::t!("11275775e3eaa2cc")
            );
            if total.is_none() {
                bytes.try_reserve_exact(response.entry_size as usize)?;
                total = Some(response.entry_size);
            }
            anyhow::ensure!(
                !response.content.is_empty() || bytes.len() as u64 == response.entry_size,
                "empty office document entry chunk"
            );
            bytes.extend_from_slice(&response.content);
            if let Some(reader) = reader {
                reader.update(cx, |reader, cx| {
                    reader.loading_resource = Some(entry_label(entry));
                    reader.progress = Some((bytes.len() as u64, response.entry_size));
                    cx.notify();
                })?;
            }
            if bytes.len() as u64 == response.entry_size {
                return Ok(bytes);
            }
        }
    }
}

fn entry_label(entry: &str) -> String {
    entry.rsplit('/').next().unwrap_or(entry).to_owned()
}

/// Reads the parts of the container that describe the document and returns the
/// rendered HTML fragment, the images it references and whether it was capped.
async fn load_document(
    source: &WordSource,
    reader: &gpui::WeakEntity<WordReader>,
    cx: &mut gpui::AsyncApp,
) -> Result<(String, HashMap<String, ImageSource>, bool)> {
    // An entry-less request only establishes the file snapshot, so every later
    // chunk is validated against the same size and modification time.
    source.read("", 0, None, cx).await?;
    let root_rels = source
        .read("_rels/.rels", MAX_PART_BYTES, Some(reader), cx)
        .await?;
    let root_rels = String::from_utf8(root_rels)?;
    let main_part = parse_main_document_part(&root_rels)
        .context("office document has no main document part")?;
    let part_dir = zip_dir(&main_part);
    let main_file = main_part
        .rsplit('/')
        .next()
        .unwrap_or(&main_part)
        .to_owned();

    let rels_path = part_path(&part_dir, &format!("_rels/{main_file}.rels"));
    let styles_path = part_path(&part_dir, "styles.xml");
    let numbering_path = part_path(&part_dir, "numbering.xml");
    let rels = read_part(source, &rels_path, reader, cx).await?;
    let styles = read_part(source, &styles_path, reader, cx).await?;
    let numbering = read_part(source, &numbering_path, reader, cx).await?;

    let document = source
        .read(&main_part, MAX_DOCUMENT_BYTES, Some(reader), cx)
        .await?;
    let document = String::from_utf8(document)?;
    let flow = build_flow(
        &document,
        styles.as_deref(),
        numbering.as_deref(),
        rels.as_deref(),
        &part_dir,
    )?;

    let mut images = HashMap::new();
    let mut compressed = 0usize;
    let mut decoded = 0u64;
    let mut skipped_images = false;
    for path in &flow.images {
        let bytes = source.read(path, MAX_IMAGE_BYTES, Some(reader), cx).await?;
        compressed = compressed.saturating_add(bytes.len());
        if compressed > MAX_IMAGE_TOTAL_BYTES {
            log::info!("stopped loading Word document images at the compressed budget");
            skipped_images = true;
            break;
        }
        let dimensions = if path.to_ascii_lowercase().ends_with(".svg") {
            let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default())?;
            (
                tree.size().width().ceil() as u32,
                tree.size().height().ceil() as u32,
            )
        } else {
            image::ImageReader::new(Cursor::new(&bytes))
                .with_guessed_format()?
                .into_dimensions()?
        };
        decoded = decoded.saturating_add(u64::from(dimensions.0) * u64::from(dimensions.1) * 4);
        if decoded > MAX_DECODED_IMAGE_BYTES {
            log::info!("stopped loading Word document images at the decoded budget");
            skipped_images = true;
            break;
        }
        if let Some(format) = image_format_for_path(path) {
            images.insert(
                path.clone(),
                ImageSource::Image(Arc::new(gpui::Image::from_bytes(format, bytes))),
            );
        }
    }

    Ok((flow.html, images, flow.truncated || skipped_images))
}

fn part_path(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

/// Reads an optional part; a missing styles or numbering part only means the
/// document relies on defaults, so the failure is recorded and degrades.
async fn read_part(
    source: &WordSource,
    entry: &str,
    reader: &gpui::WeakEntity<WordReader>,
    cx: &mut gpui::AsyncApp,
) -> Result<Option<String>> {
    match source.read(entry, MAX_PART_BYTES, Some(reader), cx).await {
        Ok(bytes) => Ok(Some(String::from_utf8(bytes)?)),
        Err(error) => {
            log::debug!("optional office document part {entry} unavailable: {error:#}");
            Ok(None)
        }
    }
}

/// Resolves the main document part from the package relationships.
fn parse_main_document_part(xml: &str) -> Option<String> {
    parse_relationships(xml)
        .into_values()
        .find(|relationship| relationship.kind.ends_with("/officeDocument"))
        .map(|relationship| relationship.target.trim_start_matches('/').to_owned())
}

#[derive(Clone, Debug)]
struct Relationship {
    kind: String,
    target: String,
    external: bool,
}

fn parse_relationships(xml: &str) -> HashMap<String, Relationship> {
    let mut reader = Reader::from_str(xml);
    let mut relationships = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let element = match &event {
                    Event::Start(element) | Event::Empty(element) => element,
                    _ => continue,
                };
                if element.local_name().as_ref() != b"Relationship" {
                    continue;
                }
                let Some(id) = attribute_value(element, &reader, b"Id") else {
                    continue;
                };
                let Some(target) = attribute_value(element, &reader, b"Target") else {
                    continue;
                };
                relationships.insert(
                    id,
                    Relationship {
                        kind: attribute_value(element, &reader, b"Type").unwrap_or_default(),
                        target,
                        external: attribute_value(element, &reader, b"TargetMode")
                            .is_some_and(|mode| mode == "External"),
                    },
                );
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    relationships
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ListKind {
    Bullet,
    Number,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct RunStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    code: bool,
}

enum Piece {
    Text {
        text: String,
        style: RunStyle,
        link: Option<String>,
    },
    Break,
    Image(String),
}

#[derive(Default)]
struct Paragraph {
    style_id: String,
    outline: Option<u8>,
    level: u8,
    number_id: Option<u32>,
    runs: Vec<Piece>,
}

impl Paragraph {
    fn number(&self) -> Option<(u8, u32)> {
        self.number_id.map(|number_id| (self.level, number_id))
    }

    /// Paragraphs whose style is a heading become `<h1>`…`<h6>`; the outline
    /// level stored on the paragraph or inherited from its style wins over the
    /// style name, because localized Word templates name them differently.
    fn heading_level(&self, styles: &HashMap<String, ResolvedStyle>) -> Option<u8> {
        let resolved = styles.get(&self.style_id);
        if let Some(outline) = self
            .outline
            .or_else(|| resolved.and_then(|style| style.outline))
        {
            return Some((outline + 1).min(6));
        }
        let name = resolved
            .map(|style| style.name.as_str())
            .unwrap_or_default();
        heading_from_name(name).or_else(|| heading_from_name(&self.style_id))
    }
}

fn heading_from_name(name: &str) -> Option<u8> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.eq_ignore_ascii_case("title") {
        return Some(1);
    }
    let lower = name.to_ascii_lowercase();
    let digits = lower
        .strip_prefix("heading")
        .or_else(|| lower.strip_prefix("标题"))?;
    digits
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|level| (1..=9).contains(level))
        .map(|level| level.min(6))
}

#[derive(Default, Clone)]
struct ResolvedStyle {
    name: String,
    outline: Option<u8>,
    bold: bool,
    italic: bool,
}

#[derive(Default, Clone)]
struct RawStyle {
    name: String,
    based_on: Option<String>,
    outline: Option<u8>,
    bold: bool,
    italic: bool,
}

/// Parses `styles.xml` and resolves `basedOn` chains, so a heading that only
/// inherits its outline level still renders as a heading.
fn parse_styles(xml: &str) -> HashMap<String, ResolvedStyle> {
    let mut reader = Reader::from_str(xml);
    let mut raw: HashMap<String, RawStyle> = HashMap::new();
    let mut current: Option<(String, RawStyle)> = None;
    let mut in_run_props = false;
    let mut in_paragraph_props = false;
    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let (element, is_empty) = match &event {
                    Event::Start(element) => (element, false),
                    Event::Empty(element) => (element, true),
                    _ => continue,
                };
                if element.local_name().as_ref() == b"style" {
                    if let Some(id) = attribute_value(element, &reader, b"styleId") {
                        current = Some((id, RawStyle::default()));
                    }
                    continue;
                }
                let Some((_, style)) = current.as_mut() else {
                    continue;
                };
                match element.local_name().as_ref() {
                    b"name" => {
                        style.name = attribute_value(element, &reader, b"val").unwrap_or_default()
                    }
                    b"basedOn" => style.based_on = attribute_value(element, &reader, b"val"),
                    b"rPr" if !is_empty => in_run_props = true,
                    b"pPr" if !is_empty => in_paragraph_props = true,
                    b"b" if in_run_props => style.bold = on_off(element, &reader),
                    b"i" if in_run_props => style.italic = on_off(element, &reader),
                    b"outlineLvl" if in_paragraph_props => {
                        style.outline = attribute_value(element, &reader, b"val")
                            .and_then(|value| value.parse().ok())
                    }
                    _ => {}
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"style" => {
                    if let Some((id, style)) = current.take() {
                        raw.insert(id, style);
                    }
                }
                b"rPr" => in_run_props = false,
                b"pPr" => in_paragraph_props = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    let mut resolved = HashMap::new();
    for id in raw.keys() {
        let mut name = String::new();
        let mut outline = None;
        let mut bold = false;
        let mut italic = false;
        let mut cursor = Some(id.clone());
        let mut depth = 0;
        while let Some(current_id) = cursor {
            let Some(style) = raw.get(&current_id) else {
                break;
            };
            if depth == 0 {
                name = style.name.clone();
            }
            if outline.is_none() {
                outline = style.outline;
            }
            bold |= style.bold;
            italic |= style.italic;
            cursor = style.based_on.clone();
            depth += 1;
            if depth > 8 {
                break;
            }
        }
        resolved.insert(
            id.clone(),
            ResolvedStyle {
                name,
                outline,
                bold,
                italic,
            },
        );
    }
    resolved
}

/// Maps `(numId, ilvl)` to the list kind declared in `numbering.xml`.
fn parse_numbering(xml: &str) -> HashMap<(u32, u8), ListKind> {
    let mut reader = Reader::from_str(xml);
    let mut abstracts: HashMap<u32, HashMap<u8, ListKind>> = HashMap::new();
    let mut numbers: HashMap<u32, u32> = HashMap::new();
    let mut current_abstract = None;
    let mut current_level = None;
    let mut current_kind = None;
    let mut pending_number = None;
    loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let element = match &event {
                    Event::Start(element) | Event::Empty(element) => element,
                    _ => continue,
                };
                match element.local_name().as_ref() {
                    b"abstractNum" => {
                        current_abstract = attribute_value(element, &reader, b"abstractNumId")
                            .and_then(|value| value.parse().ok())
                    }
                    b"lvl" => {
                        current_level = attribute_value(element, &reader, b"ilvl")
                            .and_then(|value| value.parse().ok());
                        current_kind = None;
                    }
                    b"numFmt" => {
                        current_kind = attribute_value(element, &reader, b"val").map(|value| {
                            if value == "bullet" || value == "none" {
                                ListKind::Bullet
                            } else {
                                ListKind::Number
                            }
                        })
                    }
                    b"num" => {
                        pending_number = attribute_value(element, &reader, b"numId")
                            .and_then(|value| value.parse().ok())
                    }
                    b"abstractNumId" => {
                        if let (Some(number), Some(abstract_id)) = (
                            pending_number,
                            attribute_value(element, &reader, b"val")
                                .and_then(|value| value.parse().ok()),
                        ) {
                            numbers.insert(number, abstract_id);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"lvl" => {
                    if let (Some(abstract_id), Some(level), Some(kind)) =
                        (current_abstract, current_level, current_kind)
                    {
                        abstracts
                            .entry(abstract_id)
                            .or_default()
                            .insert(level, kind);
                    }
                    current_level = None;
                    current_kind = None;
                }
                b"abstractNum" => current_abstract = None,
                b"num" => pending_number = None,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    let mut result = HashMap::new();
    for (number, abstract_id) in numbers {
        if let Some(levels) = abstracts.get(&abstract_id) {
            for (level, kind) in levels {
                result.insert((number, *level), *kind);
            }
        }
    }
    result
}

struct WordFlow {
    html: String,
    images: Vec<String>,
    truncated: bool,
}

/// Streaming converter from the main document part to an HTML fragment.
///
/// This is a reading view rather than a layout engine: it keeps structure
/// (headings, paragraphs, run formatting, lists, tables, images and links) and
/// deliberately drops page layout, text boxes, field results and embedded
/// objects. Everything it keeps is bounded by the caps above.
struct DocumentParser {
    styles: HashMap<String, ResolvedStyle>,
    numbering: HashMap<(u32, u8), ListKind>,
    rels: HashMap<String, Relationship>,
    part_dir: String,
    flow: FlowBuilder,
    paragraph: Paragraph,
    in_paragraph: bool,
    in_paragraph_props: bool,
    in_numbering_props: bool,
    in_run: bool,
    in_run_props: bool,
    in_text: bool,
    in_instruction: bool,
    deleted_depth: usize,
    fallback_depth: usize,
    text: String,
    run_style: RunStyle,
    run_style_id: Option<String>,
    current_link: Option<String>,
    pending_image: Option<String>,
    grid_span: Option<u32>,
}

impl DocumentParser {
    fn start_element(&mut self, element: &BytesStart, is_empty: bool, reader: &Reader<&[u8]>) {
        match element.local_name().as_ref() {
            b"p" => {
                self.paragraph = Paragraph::default();
                if is_empty {
                    self.finish_paragraph();
                } else {
                    self.in_paragraph = true;
                }
            }
            b"pPr" if !is_empty => self.in_paragraph_props = true,
            b"rPr" if !is_empty => self.in_run_props = true,
            b"numPr" if !is_empty => self.in_numbering_props = true,
            b"pStyle" if self.in_paragraph_props => {
                self.paragraph.style_id =
                    attribute_value(element, reader, b"val").unwrap_or_default();
            }
            b"outlineLvl" if self.in_paragraph_props => {
                self.paragraph.outline =
                    attribute_value(element, reader, b"val").and_then(|value| value.parse().ok());
            }
            b"ilvl" if self.in_numbering_props => {
                self.paragraph.level = attribute_value(element, reader, b"val")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
            }
            b"numId" if self.in_numbering_props => {
                self.paragraph.number_id =
                    attribute_value(element, reader, b"val").and_then(|value| value.parse().ok());
            }
            b"r" if self.in_paragraph && !is_empty => {
                self.in_run = true;
                self.in_run_props = false;
                self.run_style = RunStyle::default();
                self.run_style_id = None;
            }
            b"rStyle" if self.in_run_props && self.in_run => {
                self.run_style_id = attribute_value(element, reader, b"val");
            }
            b"b" if self.in_run_props && self.in_run => {
                self.run_style.bold = on_off(element, reader);
            }
            b"i" if self.in_run_props && self.in_run => {
                self.run_style.italic = on_off(element, reader);
            }
            b"strike" | b"dstrike" if self.in_run_props && self.in_run => {
                self.run_style.strike = on_off(element, reader);
            }
            b"u" if self.in_run_props && self.in_run => {
                self.run_style.underline =
                    attribute_value(element, reader, b"val").is_none_or(|value| value != "none");
            }
            b"rFonts" if self.in_run_props && self.in_run => {
                if is_monospace(element, reader) {
                    self.run_style.code = true;
                }
            }
            b"t" if self.in_run && !is_empty => {
                self.in_text = true;
                self.text.clear();
            }
            b"tab" if self.in_run && self.deleted_depth == 0 => {
                self.push_run_text("\u{a0}\u{a0}");
            }
            b"noBreakHyphen" if self.in_run && self.deleted_depth == 0 => {
                self.push_run_text("-");
            }
            b"br" | b"cr" if self.in_run && self.deleted_depth == 0 => {
                self.paragraph.runs.push(Piece::Break);
            }
            b"instrText" => self.in_instruction = true,
            b"del" | b"moveFrom" => self.deleted_depth += 1,
            b"Fallback" => self.fallback_depth += 1,
            b"blip" if self.fallback_depth == 0 => {
                if let Some(id) = attribute_value(element, reader, b"embed") {
                    self.pending_image = resolve_image(&self.rels, &self.part_dir, &id);
                }
            }
            b"imagedata" if self.fallback_depth == 0 => {
                if let Some(id) = attribute_value(element, reader, b"id") {
                    self.pending_image = resolve_image(&self.rels, &self.part_dir, &id);
                }
            }
            b"drawing" | b"pict" if !is_empty => self.pending_image = None,
            b"hyperlink" if !is_empty => {
                self.current_link = resolve_link(&self.rels, element, reader);
            }
            b"tbl" => {
                self.flow.close_lists();
                self.flow.push("<table>");
            }
            b"tr" => self.flow.push("<tr>"),
            b"tc" => {
                self.flow.begin_cell();
                self.grid_span = None;
                if is_empty {
                    self.flow.end_cell(None);
                }
            }
            b"gridSpan" => {
                self.grid_span =
                    attribute_value(element, reader, b"val").and_then(|value| value.parse().ok());
            }
            _ => {}
        }
    }

    fn end_element(&mut self, name: &[u8]) {
        match name {
            b"pPr" => self.in_paragraph_props = false,
            b"rPr" => self.in_run_props = false,
            b"numPr" => self.in_numbering_props = false,
            b"r" => self.in_run = false,
            b"t" => {
                self.in_text = false;
                if self.deleted_depth == 0 && !self.text.is_empty() {
                    let text = mem::take(&mut self.text);
                    let style = merged_run_style(
                        self.run_style,
                        self.run_style_id.as_deref(),
                        &self.styles,
                    );
                    self.paragraph.runs.push(Piece::Text {
                        text,
                        style,
                        link: self.current_link.clone(),
                    });
                } else {
                    self.text.clear();
                }
            }
            b"instrText" => self.in_instruction = false,
            b"del" | b"moveFrom" => self.deleted_depth = self.deleted_depth.saturating_sub(1),
            b"Fallback" => self.fallback_depth = self.fallback_depth.saturating_sub(1),
            b"hyperlink" => self.current_link = None,
            b"drawing" | b"pict" => {
                if let Some(path) = self.pending_image.take() {
                    self.paragraph.runs.push(Piece::Image(path));
                }
            }
            b"tc" => {
                let span = self.grid_span.take();
                self.flow.end_cell(span);
            }
            b"tr" => self.flow.push("</tr>"),
            b"tbl" => self.flow.push("</table>"),
            b"p" => {
                self.in_paragraph = false;
                self.finish_paragraph();
            }
            _ => {}
        }
    }

    fn push_text(&mut self, content: &str) {
        if self.in_text && !self.in_instruction && self.deleted_depth == 0 {
            self.text.push_str(content);
        }
    }

    fn push_run_text(&mut self, text: &str) {
        self.paragraph.runs.push(Piece::Text {
            text: text.to_owned(),
            style: self.run_style,
            link: self.current_link.clone(),
        });
    }

    fn finish_paragraph(&mut self) {
        let paragraph = mem::take(&mut self.paragraph);
        self.flow
            .finish_paragraph(paragraph, &self.styles, &self.numbering);
    }
}

/// Converts the main document part into an HTML fragment.
fn build_flow(
    document_xml: &str,
    styles_xml: Option<&str>,
    numbering_xml: Option<&str>,
    rels_xml: Option<&str>,
    part_dir: &str,
) -> Result<WordFlow> {
    let mut parser = DocumentParser {
        styles: styles_xml.map(parse_styles).unwrap_or_default(),
        numbering: numbering_xml.map(parse_numbering).unwrap_or_default(),
        rels: rels_xml.map(parse_relationships).unwrap_or_default(),
        part_dir: part_dir.to_owned(),
        flow: FlowBuilder::default(),
        paragraph: Paragraph::default(),
        in_paragraph: false,
        in_paragraph_props: false,
        in_numbering_props: false,
        in_run: false,
        in_run_props: false,
        in_text: false,
        in_instruction: false,
        deleted_depth: 0,
        fallback_depth: 0,
        text: String::new(),
        run_style: RunStyle::default(),
        run_style_id: None,
        current_link: None,
        pending_image: None,
        grid_span: None,
    };

    let mut reader = Reader::from_str(document_xml);
    while let Ok(event) = reader.read_event() {
        match &event {
            Event::Start(element) => parser.start_element(element, false, &reader),
            Event::Empty(element) => parser.start_element(element, true, &reader),
            Event::End(element) => {
                let name = element.local_name();
                parser.end_element(name.as_ref());
            }
            Event::Text(element) => {
                if let Ok(content) = element.xml_content(XmlVersion::Implicit1_0) {
                    parser.push_text(&content);
                }
            }
            Event::CData(element) => {
                if let Ok(content) = element.decode() {
                    parser.push_text(&content);
                }
            }
            Event::GeneralRef(element) => match element.resolve_char_ref() {
                Ok(Some(character)) => {
                    let mut buffer = [0u8; 4];
                    parser.push_text(character.encode_utf8(&mut buffer));
                }
                _ => {
                    if let Ok(name) = element.decode() {
                        let replacement = match name.as_ref() {
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            "quot" => "\"",
                            "apos" => "'",
                            _ => "",
                        };
                        parser.push_text(replacement);
                    }
                }
            },
            Event::Eof => break,
            _ => {}
        }
        if parser.flow.truncated {
            break;
        }
    }

    parser.flow.close_lists();
    Ok(WordFlow {
        html: parser.flow.html,
        images: parser.flow.referenced_images,
        truncated: parser.flow.truncated,
    })
}

fn merged_run_style(
    mut style: RunStyle,
    style_id: Option<&str>,
    styles: &HashMap<String, ResolvedStyle>,
) -> RunStyle {
    if let Some(resolved) = style_id.and_then(|id| styles.get(id)) {
        style.bold |= resolved.bold;
        style.italic |= resolved.italic;
    }
    style
}

fn resolve_image(rels: &HashMap<String, Relationship>, part_dir: &str, id: &str) -> Option<String> {
    let path = resolve_internal_target(rels, part_dir, id)?;
    image_format_for_path(&path).map(|_| path)
}

fn resolve_link(
    rels: &HashMap<String, Relationship>,
    element: &BytesStart,
    reader: &Reader<&[u8]>,
) -> Option<String> {
    let relationship = attribute_value(element, reader, b"id")
        .and_then(|id| rels.get(&id))
        .filter(|relationship| relationship.external)?;
    let target = relationship.target.trim();
    if target.starts_with("http://") || target.starts_with("https://") {
        Some(target.to_owned())
    } else {
        None
    }
}

fn resolve_internal_target(
    rels: &HashMap<String, Relationship>,
    part_dir: &str,
    id: &str,
) -> Option<String> {
    let relationship = rels.get(id)?;
    if relationship.external {
        return None;
    }
    if let Some(absolute) = relationship.target.strip_prefix('/') {
        return Some(resolve_zip_path("", absolute));
    }
    Some(resolve_zip_path(part_dir, &relationship.target))
}

fn attribute_value(element: &BytesStart, reader: &Reader<&[u8]>, name: &[u8]) -> Option<String> {
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

fn on_off(element: &BytesStart, reader: &Reader<&[u8]>) -> bool {
    match attribute_value(element, reader, b"val") {
        Some(value) => !matches!(value.as_str(), "0" | "false" | "off"),
        None => true,
    }
}

fn is_monospace(element: &BytesStart, reader: &Reader<&[u8]>) -> bool {
    ["ascii", "hAnsi", "cs"]
        .iter()
        .filter_map(|name| attribute_value(element, reader, name.as_bytes()))
        .any(|font| {
            let font = font.to_ascii_lowercase();
            font.contains("consolas")
                || font.contains("courier")
                || font.contains("mono")
                || font.contains("menlo")
        })
}

/// Word marks deleted revisions with `w:del`/`w:moveFrom` wrappers; their runs
/// are still `<w:r>` elements, so the depth counter suppresses the text.
#[derive(Default)]
struct FlowBuilder {
    html: String,
    written: usize,
    blocks: usize,
    truncated: bool,
    lists: Vec<OpenList>,
    cell_buffers: Vec<String>,
    referenced_images: Vec<String>,
}

struct OpenList {
    level: u8,
    kind: ListKind,
    item_open: bool,
}

impl FlowBuilder {
    fn push(&mut self, text: &str) {
        if self.written.saturating_add(text.len()) > MAX_HTML_BYTES {
            self.truncated = true;
            return;
        }
        self.written += text.len();
        match self.cell_buffers.last_mut() {
            Some(buffer) => buffer.push_str(text),
            None => self.html.push_str(text),
        }
    }

    fn is_full(&self) -> bool {
        self.written >= MAX_HTML_BYTES || self.blocks >= MAX_BLOCKS
    }

    fn begin_cell(&mut self) {
        self.cell_buffers.push(String::new());
    }

    fn end_cell(&mut self, colspan: Option<u32>) {
        let Some(buffer) = self.cell_buffers.pop() else {
            return;
        };
        let open = match colspan.filter(|span| *span > 1) {
            Some(span) => format!("<td colspan=\"{span}\">"),
            None => "<td>".to_owned(),
        };
        self.push(&open);
        self.push(&buffer);
        self.push("</td>");
    }

    fn finish_paragraph(
        &mut self,
        paragraph: Paragraph,
        styles: &HashMap<String, ResolvedStyle>,
        numbering: &HashMap<(u32, u8), ListKind>,
    ) {
        if self.is_full() {
            self.truncated = true;
            return;
        }
        self.blocks += 1;
        let number = paragraph.number().map(|(level, number_id)| {
            let kind = numbering
                .get(&(number_id, level))
                .copied()
                .unwrap_or(ListKind::Bullet);
            (level, kind)
        });

        if !self.cell_buffers.is_empty() {
            let first = self
                .cell_buffers
                .last()
                .is_some_and(|buffer| buffer.is_empty());
            if !first {
                self.push("<br/>");
            }
            self.push_runs(&paragraph.runs);
            return;
        }

        match number {
            Some((level, kind)) => self.open_list(level, kind),
            None => self.close_lists(),
        }
        if number.is_none() {
            let tag = paragraph
                .heading_level(styles)
                .map(|level| HEADINGS[usize::from(level - 1)])
                .unwrap_or("p");
            self.push(&format!("<{tag}>"));
            self.push_runs(&paragraph.runs);
            self.push(&format!("</{tag}>"));
        } else {
            self.push_runs(&paragraph.runs);
        }
    }

    fn open_list(&mut self, level: u8, kind: ListKind) {
        while let Some(list) = self.lists.pop() {
            if list.level <= level {
                self.lists.push(list);
                break;
            }
            if list.item_open {
                self.push("</li>");
            }
            self.push(close_list_tag(list.kind));
        }
        let same_level_other_kind = self
            .lists
            .last()
            .is_some_and(|list| list.level == level && list.kind != kind);
        if same_level_other_kind {
            if let Some(list) = self.lists.pop() {
                if list.item_open {
                    self.push("</li>");
                }
                self.push(close_list_tag(list.kind));
            }
        }
        let same_level_open_item = self
            .lists
            .last()
            .is_some_and(|list| list.level == level && list.item_open);
        if same_level_open_item {
            self.push("</li>");
            if let Some(list) = self.lists.last_mut() {
                list.item_open = false;
            }
        }
        if self.lists.last().map(|list| list.level) != Some(level) {
            self.push(open_list_tag(kind));
            self.lists.push(OpenList {
                level,
                kind,
                item_open: false,
            });
        }
        self.push("<li>");
        if let Some(list) = self.lists.last_mut() {
            list.item_open = true;
        }
    }

    fn close_lists(&mut self) {
        while let Some(list) = self.lists.pop() {
            if list.item_open {
                self.push("</li>");
            }
            self.push(close_list_tag(list.kind));
        }
    }

    fn push_runs(&mut self, runs: &[Piece]) {
        let mut open_link: Option<String> = None;
        for piece in runs {
            if self.truncated {
                return;
            }
            match piece {
                Piece::Text { text, style, link } => {
                    if open_link.as_deref() != link.as_deref() {
                        if open_link.is_some() {
                            self.push("</a>");
                        }
                        if let Some(link) = link {
                            let mut escaped = String::with_capacity(link.len());
                            escape_attribute_into(&mut escaped, link);
                            self.push(&format!("<a href=\"{escaped}\">"));
                        }
                        open_link = link.clone();
                    }
                    let mut prefix = String::new();
                    let mut closers: Vec<&str> = Vec::new();
                    if style.bold {
                        prefix.push_str("<strong>");
                        closers.push("</strong>");
                    }
                    if style.italic {
                        prefix.push_str("<em>");
                        closers.push("</em>");
                    }
                    if style.underline {
                        prefix.push_str("<u>");
                        closers.push("</u>");
                    }
                    if style.strike {
                        prefix.push_str("<s>");
                        closers.push("</s>");
                    }
                    if style.code {
                        prefix.push_str("<code>");
                        closers.push("</code>");
                    }
                    self.push(&prefix);
                    let mut escaped = String::with_capacity(text.len());
                    escape_html_into(&mut escaped, text);
                    self.push(&escaped);
                    for closer in closers.into_iter().rev() {
                        self.push(closer);
                    }
                }
                Piece::Break => self.push("<br/>"),
                Piece::Image(path) => {
                    if let Some(path) = self.reference_image(path) {
                        let mut escaped = String::with_capacity(path.len());
                        escape_attribute_into(&mut escaped, &path);
                        self.push(&format!("<img src=\"{escaped}\" alt=\"\"/>"));
                    }
                }
            }
        }
        if open_link.is_some() {
            self.push("</a>");
        }
    }

    fn reference_image(&mut self, path: &str) -> Option<String> {
        if self
            .referenced_images
            .iter()
            .any(|existing| existing == path)
        {
            return Some(path.to_owned());
        }
        if self.referenced_images.len() >= MAX_IMAGES {
            return None;
        }
        self.referenced_images.push(path.to_owned());
        Some(path.to_owned())
    }
}

fn open_list_tag(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Bullet => "<ul>",
        ListKind::Number => "<ol>",
    }
}

fn close_list_tag(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Bullet => "</ul>",
        ListKind::Number => "</ol>",
    }
}

fn escape_html_into(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(character),
        }
    }
}

fn escape_attribute_into(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::io::Write as _;

    fn rels(entries: &[(&str, &str, bool)]) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        );
        for (id, target, external) in entries {
            let mode = if *external {
                " TargetMode=\"External\""
            } else {
                ""
            };
            let _ = write!(
                xml,
                "<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"{target}\"{mode}/>"
            );
        }
        xml.push_str("</Relationships>");
        xml
    }

    fn styles() -> String {
        String::from(
            "<?xml version=\"1.0\"?><w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
             <w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:pPr><w:outlineLvl w:val=\"0\"/></w:pPr></w:style>\
             <w:style w:type=\"paragraph\" w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/><w:basedOn w:val=\"Heading1\"/><w:pPr><w:outlineLvl w:val=\"1\"/></w:pPr></w:style>\
             <w:style w:type=\"character\" w:styleId=\"Strong\"><w:name w:val=\"Strong\"/><w:rPr><w:b/></w:rPr></w:style>\
             </w:styles>",
        )
    }

    fn numbering() -> String {
        String::from(
            "<?xml version=\"1.0\"?><w:numbering xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
             <w:abstractNum w:abstractNumId=\"0\">\
             <w:lvl w:ilvl=\"0\"><w:numFmt w:val=\"bullet\"/></w:lvl>\
             <w:lvl w:ilvl=\"1\"><w:numFmt w:val=\"decimal\"/></w:lvl>\
             </w:abstractNum>\
             <w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num>\
             </w:numbering>",
        )
    }

    /// Minimal valid Word container used by the integration test: a package
    /// relationship pointing at the main part, plus styles so a heading can be
    /// recognized.
    pub(crate) fn docx_fixture() -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let parts = [
            (
                "_rels/.rels",
                "<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>".to_owned(),
            ),
            (
                "word/document.xml",
                body("<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:r><w:t>Report</w:t></w:r></w:p><w:p><w:r><w:t>Body</w:t></w:r></w:p>"),
            ),
            ("word/styles.xml", styles()),
        ];
        for (name, contents) in parts {
            archive
                .start_file(name, zip::write::FileOptions::default())
                .expect("entry");
            archive.write_all(contents.as_bytes()).expect("write");
        }
        archive.finish().expect("finish").into_inner()
    }

    fn body(content: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><w:body>{content}</w:body></w:document>"
        )
    }

    #[test]
    fn resolves_the_main_document_part() {
        let xml = "<?xml version=\"1.0\"?><Relationships>\
            <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
            </Relationships>";
        assert_eq!(
            parse_main_document_part(xml).as_deref(),
            Some("word/document.xml")
        );
    }

    #[test]
    fn renders_headings_runs_and_escaped_text() {
        let document = body(
            "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:r><w:t>Chapter</w:t></w:r></w:p>\
             <w:p><w:r><w:rPr><w:b/></w:rPr><w:t>bold &amp; </w:t></w:r><w:r><w:rPr><w:rStyle w:val=\"Strong\"/></w:rPr><w:t>strong</w:t></w:r></w:p>\
             <w:p><w:r><w:t>a</w:t></w:r><w:r><w:t>b</w:t></w:r></w:p>",
        );
        let flow = build_flow(&document, Some(&styles()), None, None, "word").expect("flow");
        assert!(flow.html.contains("<h1>Chapter</h1>"), "{}", flow.html);
        assert!(
            flow.html
                .contains("<p><strong>bold &amp; </strong><strong>strong</strong></p>"),
            "{}",
            flow.html
        );
        assert!(flow.html.contains("<p>ab</p>"), "{}", flow.html);
        assert!(!flow.truncated);
    }

    #[test]
    fn inherits_heading_levels_and_handles_lists() {
        let document = body(
            "<w:p><w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr><w:r><w:t>Sub</w:t></w:r></w:p>\
             <w:p><w:pPr><w:numPr><w:ilvl w:val=\"0\"/><w:numId w:val=\"1\"/></w:numPr></w:pPr><w:r><w:t>bullet</w:t></w:r></w:p>\
             <w:p><w:pPr><w:numPr><w:ilvl w:val=\"1\"/><w:numId w:val=\"1\"/></w:numPr></w:pPr><w:r><w:t>nested</w:t></w:r></w:p>\
             <w:p><w:pPr><w:numPr><w:ilvl w:val=\"0\"/><w:numId w:val=\"1\"/></w:numPr></w:pPr><w:r><w:t>second</w:t></w:r></w:p>\
             <w:p><w:r><w:t>after</w:t></w:r></w:p>",
        );
        let flow =
            build_flow(&document, Some(&styles()), Some(&numbering()), None, "word").expect("flow");
        assert!(flow.html.contains("<h2>Sub</h2>"), "{}", flow.html);
        assert!(
            flow.html.contains(
                "<ul><li>bullet<ol><li>nested</li></ol></li><li>second</li></ul><p>after</p>"
            ),
            "{}",
            flow.html
        );
    }

    #[test]
    fn renders_tables_images_and_links() {
        let relationships = rels(&[
            ("rId4", "media/image1.png", false),
            ("rId5", "https://example.com/page", true),
        ]);
        let document = body(
            "<w:p><w:hyperlink r:id=\"rId5\"><w:r><w:t>link</w:t></w:r></w:hyperlink></w:p>\
             <w:p><w:r><w:drawing><wp:inline><a:graphic><a:blip r:embed=\"rId4\"/></a:graphic></wp:inline></w:drawing></w:r></w:p>\
             <w:tbl><w:tr><w:tc><w:tcPr><w:gridSpan w:val=\"2\"/></w:tcPr><w:p><w:r><w:t>cell</w:t></w:r></w:p><w:p><w:r><w:t>more</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
        );
        let flow = build_flow(&document, None, None, Some(&relationships), "word").expect("flow");
        assert!(
            flow.html
                .contains("<a href=\"https://example.com/page\">link</a>"),
            "{}",
            flow.html
        );
        assert!(
            flow.html
                .contains("<img src=\"word/media/image1.png\" alt=\"\"/>"),
            "{}",
            flow.html
        );
        assert_eq!(flow.images, vec!["word/media/image1.png".to_owned()]);
        assert!(
            flow.html
                .contains("<table><tr><td colspan=\"2\">cell<br/>more</td></tr></table>"),
            "{}",
            flow.html
        );
    }

    #[test]
    fn drops_deleted_revisions_and_field_instructions() {
        let document = body(
            "<w:p><w:r><w:t>kept</w:t></w:r><w:del><w:r><w:delText>gone</w:delText></w:r></w:del>\
             <w:r><w:instrText> PAGE </w:instrText></w:r></w:p>",
        );
        let flow = build_flow(&document, None, None, None, "word").expect("flow");
        assert!(flow.html.contains("<p>kept</p>"), "{}", flow.html);
        assert!(!flow.html.contains("gone"), "{}", flow.html);
        assert!(!flow.html.contains("PAGE"), "{}", flow.html);
    }

    #[test]
    fn keeps_table_cells_that_are_not_images() {
        let relationships = rels(&[("rId9", "media/chart.bin", false)]);
        let document =
            body("<w:p><w:r><w:drawing><a:blip r:embed=\"rId9\"/></w:drawing></w:r></w:p>");
        let flow = build_flow(&document, None, None, Some(&relationships), "word").expect("flow");
        assert!(!flow.html.contains("<img"), "{}", flow.html);
        assert!(flow.images.is_empty());
    }
}
