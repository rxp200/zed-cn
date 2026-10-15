use std::{
    collections::HashMap,
    io::Cursor,
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, ImageFormat, ImageSource,
    ScrollHandle, SharedString, Task, Window, actions, div, px,
};
use language::File as _;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownOptions, MarkdownStyle};
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::Event;
#[cfg(test)]
use std::io::Read as _;
use ui::prelude::*;
use ui::utils::WithRemSize;
use util::ResultExt as _;

use crate::DocumentItem;

actions!(
    epub_reader,
    [
        /// Go to the next chapter.
        NextChapter,
        /// Go to the previous chapter.
        PreviousChapter,
        /// Toggle the table of contents sidebar.
        ToggleTableOfContents,
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
        /// Scroll to the start of the chapter.
        ScrollToStart,
        /// Scroll to the end of the chapter.
        ScrollToEnd
    ]
);

const DEFAULT_FONT_SIZE: f32 = 16.0;
const MIN_FONT_SIZE: f32 = 10.0;
const MAX_FONT_SIZE: f32 = 32.0;
const FONT_SIZE_STEP: f32 = 1.0;
const SCROLL_LINE: f32 = 48.0;

/// One chapter of an EPUB book, in reading (spine) order.
#[derive(Debug, Clone)]
pub struct EpubChapter {
    /// Path of the chapter document inside the ZIP archive.
    pub path: String,
}

/// A table-of-contents entry.
#[derive(Debug, Clone)]
pub struct EpubTocEntry {
    pub title: String,
    /// Link target as written in the TOC document, before resolution.
    pub href: Option<String>,
    /// Index of the chapter this entry points to, if it could be resolved.
    pub chapter_index: Option<usize>,
    pub depth: usize,
}

/// Parsed structural data of an EPUB archive.
#[derive(Debug)]
pub struct EpubArchive {
    pub title: Option<String>,
    pub author: Option<String>,
    pub chapters: Vec<EpubChapter>,
    pub toc: Vec<EpubTocEntry>,
}

#[cfg(test)]
type SharedArchive = Arc<Mutex<zip::ZipArchive<Cursor<Arc<[u8]>>>>>;

pub struct EpubReader {
    item: Entity<DocumentItem>,
    project: Entity<project::Project>,
    focus_handle: FocusHandle,
    languages: Option<Arc<language::LanguageRegistry>>,
    pub(crate) archive: Option<Arc<EpubArchive>>,
    error: Option<SharedString>,
    chapter_index: usize,
    pub(crate) markdown: Option<Entity<Markdown>>,
    source: Option<Arc<EpubSource>>,
    images: Rc<HashMap<String, ImageSource>>,
    progress: Option<(u64, u64)>,
    loading_resource: Option<String>,
    scroll_handle: ScrollHandle,
    font_size: f32,
    show_toc: bool,
    image_cache: Entity<gpui::RetainAllImageCache>,
    _load_task: Option<Task<Result<()>>>,
}

impl EpubReader {
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
        let source = Arc::new(EpubSource {
            project: project.downgrade(),
            path: item.read(cx).project_path(cx),
            snapshot: Mutex::new(None),
            limiter: Arc::new(async_lock::Semaphore::new(1)),
        });
        let parse_task = cx.spawn({
            let source = source.clone();
            async move |this, cx| {
                let parsed = load_structure(&source, cx).await;
                this.update(cx, |reader, cx| match parsed {
                    Ok(archive) => {
                        reader.archive = Some(Arc::new(archive));
                        reader.load_chapter(0, cx);
                        cx.notify();
                    }
                    Err(error) => {
                        reader.error = Some(i18n::t!("5e36c31539810b81", error = error).into());
                        cx.notify();
                    }
                })
            }
        });

        Self {
            item,
            project,
            focus_handle: cx.focus_handle(),
            languages: None,
            archive: None,
            error: None,
            chapter_index: 0,
            markdown: None,
            source: Some(source),
            images: Rc::new(HashMap::new()),
            progress: None,
            loading_resource: None,
            scroll_handle: ScrollHandle::new(),
            font_size: DEFAULT_FONT_SIZE,
            show_toc: false,
            image_cache: gpui::RetainAllImageCache::new(cx),
            _load_task: Some(parse_task),
        }
    }

    pub fn with_languages(mut self, languages: Arc<language::LanguageRegistry>) -> Self {
        self.languages = Some(languages);
        self
    }

    pub fn chapter_index(&self) -> usize {
        self.chapter_index
    }

    pub fn chapter_count(&self) -> usize {
        self.archive.as_ref().map_or(0, |a| a.chapters.len())
    }

    pub fn book_title(&self, cx: &App) -> Option<SharedString> {
        self.archive
            .as_ref()
            .and_then(|a| a.title.clone())
            .map(SharedString::from)
            .or_else(|| {
                Some(SharedString::from(
                    self.item.read(cx).file.file_name(cx).to_string(),
                ))
            })
    }

    pub fn toc_visible(&self) -> bool {
        self.show_toc
    }

    pub fn toggle_toc(&mut self, cx: &mut Context<Self>) {
        self.show_toc = !self.show_toc;
        cx.notify();
    }

    pub fn next_chapter(&mut self, cx: &mut Context<Self>) {
        let count = self.chapter_count();
        if self.chapter_index + 1 < count {
            self.load_chapter(self.chapter_index + 1, cx);
        }
    }

    pub fn previous_chapter(&mut self, cx: &mut Context<Self>) {
        if self.chapter_index > 0 {
            self.load_chapter(self.chapter_index - 1, cx);
        }
    }

    pub fn increase_font_size(&mut self, cx: &mut Context<Self>) {
        self.font_size = (self.font_size + FONT_SIZE_STEP).min(MAX_FONT_SIZE);
        cx.notify();
    }

    pub fn decrease_font_size(&mut self, cx: &mut Context<Self>) {
        self.font_size = (self.font_size - FONT_SIZE_STEP).max(MIN_FONT_SIZE);
        cx.notify();
    }

    pub fn load_chapter(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.clone() else {
            return;
        };
        if index >= archive.chapters.len() {
            return;
        }
        self.chapter_index = index;
        self.scroll_handle.set_offset(gpui::point(px(0.), px(0.)));

        let Some(source) = self.source.clone() else {
            return;
        };
        self._load_task = None;
        self.markdown = None;
        for image in self.images.values() {
            image.remove_asset(cx);
        }
        self.images = Rc::new(HashMap::new());
        self.image_cache = gpui::RetainAllImageCache::new(cx);
        self.error = None;
        self.progress = None;
        self.loading_resource = Some(chapter_path_label(&archive.chapters[index].path));
        let chapter_path = archive.chapters[index].path.clone();
        let languages = self.languages.clone();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = load_chapter_content(&source, &chapter_path, &this, cx).await;
            this.update(cx, |reader, cx| {
                reader.loading_resource = None;
                match loaded {
                    Ok((body, images)) => {
                        reader.images = Rc::new(images);
                        reader.markdown = Some(cx.new(|cx| {
                            Markdown::new_with_options(
                                body.into(),
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
                    Err(error) => reader.error = Some(i18n::t!("5e36c31539810b81", error = error).into()),
                }
                cx.notify();
            })
        }));
        cx.notify();
    }

    /// Resolves a URL found in a chapter against the chapter's location in the archive.
    fn resolve_chapter_url(&self, url: &str) -> Option<usize> {
        let archive = self.archive.as_ref()?;
        let chapter = archive.chapters.get(self.chapter_index)?;
        let chapter_dir = zip_dir(&chapter.path);
        let path = resolve_zip_path(&chapter_dir, url.split('#').next().unwrap_or(url));
        archive.chapters.iter().position(|c| c.path == path)
    }

    fn handle_url_click(&mut self, url: SharedString, cx: &mut Context<Self>) {
        if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
            cx.open_url(&url);
            return;
        }
        if let Some(chapter) = self.resolve_chapter_url(&url) {
            self.load_chapter(chapter, cx);
        }
    }

    fn render_toc(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let archive = self.archive.clone();
        let mut list = div().flex_col().gap_1().p_2();
        if let Some(archive) = archive {
            if archive.title.is_some() || archive.author.is_some() {
                let mut book_header = div().flex_col().px_2().pb_2().gap_1();
                if let Some(title) = &archive.title {
                    book_header = book_header.child(Label::new(title.clone()).single_line());
                }
                if let Some(author) = &archive.author {
                    book_header = book_header.child(
                        Label::new(author.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line(),
                    );
                }
                list = list.child(book_header);
            }
            if archive.toc.is_empty() {
                list = list.child(
                    Label::new(i18n::t!("73d3c38babbd1f6a"))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                );
            }
            for (index, entry) in archive.toc.iter().enumerate() {
                let is_current = entry.chapter_index == Some(self.chapter_index);
                let chapter_index = entry.chapter_index;
                let depth = entry.depth;
                list = list.child(
                    div()
                        .id(("toc-entry", index))
                        .pl(px(8.0 + depth as f32 * 12.0))
                        .py_1()
                        .pr_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(is_current, |this| {
                            this.bg(cx.theme().colors().element_selected)
                        })
                        .when(!is_current, |this| {
                            this.hover(|style| style.bg(cx.theme().colors().element_hover))
                        })
                        .child(
                            Label::new(entry.title.clone())
                                .size(LabelSize::Small)
                                .when(is_current, |this| this.color(Color::Accent)),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if let Some(chapter_index) = chapter_index {
                                this.load_chapter(chapter_index, cx);
                            }
                        })),
                );
            }
        }
        div()
            .w(px(280.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(
                div()
                    .id("epub-toc-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(list),
            )
    }
}

impl EventEmitter<()> for EpubReader {}

impl Focusable for EpubReader {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EpubReader {
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
                .when(self.archive.is_some(), |element| {
                    element.child(
                        ui::Button::new("epub-retry", i18n::t!("b8784c8dd5636ff2")).on_click(
                            cx.listener(|reader, _, _, cx| {
                                reader.load_chapter(reader.chapter_index, cx)
                            }),
                        ),
                    )
                })
                .into_any_element()
        } else if let Some(markdown) = self.markdown.clone() {
            let chapter_dir = self
                .archive
                .as_ref()
                .and_then(|archive| archive.chapters.get(self.chapter_index))
                .map(|chapter| zip_dir(&chapter.path))
                .unwrap_or_default();
            let images = self.images.clone();
            let reader = cx.entity().downgrade();
            let mut style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
            style.base_text_style.font_size = px(self.font_size).into();
            let markdown_element = MarkdownElement::new(markdown, style)
                .image_resolver(move |url, _cx| {
                    images
                        .get(&resolve_zip_path(
                            &chapter_dir,
                            url.split('#').next().unwrap_or(url),
                        ))
                        .cloned()
                })
                .on_url_click(move |url, _window, cx| {
                    reader
                        .update(cx, |reader, cx| reader.handle_url_click(url, cx))
                        .log_err();
                });
            div()
                .id("epub-content-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll_handle)
                .child(
                    div()
                        .w_full()
                        .max_w(px(880.))
                        .mx_auto()
                        .p_4()
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
                .child(Label::new(i18n::t!("b6c983f80bc7ee9e")).color(Color::Muted))
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
                            "epub-load-progress",
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
            .key_context("EpubReader")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &NextChapter, _, cx| this.next_chapter(cx)))
            .on_action(cx.listener(|this, _: &PreviousChapter, _, cx| {
                this.previous_chapter(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTableOfContents, _, cx| {
                this.toggle_toc(cx);
            }))
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
                h_flex()
                    .size_full()
                    .when(self.show_toc, |this| this.child(self.render_toc(cx)))
                    .child(
                        div().flex_1().min_w_0().h_full().child(
                            WithRemSize::new(px(self.font_size))
                                .size_full()
                                .child(content),
                        ),
                    ),
            )
    }
}

fn chapter_path_label(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

struct EpubSource {
    project: gpui::WeakEntity<project::Project>,
    path: project::ProjectPath,
    snapshot: Mutex<Option<(u64, rpc::proto::Timestamp)>>,
    limiter: Arc<async_lock::Semaphore>,
}

impl EpubSource {
    async fn read(
        &self,
        entry: &str,
        reader: Option<&gpui::WeakEntity<EpubReader>>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<Vec<u8>> {
        use anyhow::Context as _;
        let _permit = self.limiter.acquire_arc().await;
        let mut bytes = Vec::new();
        let mut total = None;
        loop {
            let snapshot = *self
                .snapshot
                .lock()
                .map_err(|_| anyhow::anyhow!("EPUB snapshot lock poisoned"))?;
            let response = self
                .project
                .update(cx, |project, cx| {
                    project.read_epub_entry(
                        self.path.clone(),
                        entry.to_owned(),
                        bytes.len() as u64,
                        snapshot,
                        cx,
                    )
                })?
                .await?;
            let file = response.file.context("missing EPUB metadata")?;
            anyhow::ensure!(
                file.worktree_id == self.path.worktree_id.to_proto()
                    && file.path == self.path.path.as_unix_str()
                    && response.total_size <= 512 * 1024 * 1024,
                "invalid EPUB metadata"
            );
            let mtime = file.mtime.context("missing EPUB modification time")?;
            if let Some((size, expected_mtime)) = snapshot {
                anyhow::ensure!(
                    size == response.total_size && expected_mtime == mtime,
                    i18n::t!("289cab9d66de19ce")
                );
            } else {
                *self
                    .snapshot
                    .lock()
                    .map_err(|_| anyhow::anyhow!("EPUB snapshot lock poisoned"))? =
                    Some((response.total_size, mtime));
            }
            anyhow::ensure!(
                response.entry_size <= project::epub::EPUB_ENTRY_LIMIT
                    && response.content.len() <= project::epub::EPUB_CHUNK_SIZE
                    && bytes.len() as u64 + response.content.len() as u64 <= response.entry_size
                    && total.is_none_or(|total| total == response.entry_size),
                "invalid EPUB entry chunk"
            );
            if total.is_none() {
                bytes.try_reserve_exact(response.entry_size as usize)?;
                total = Some(response.entry_size);
            }
            anyhow::ensure!(
                !response.content.is_empty() || bytes.len() as u64 == response.entry_size,
                "empty EPUB entry chunk"
            );
            bytes.extend_from_slice(&response.content);
            if let Some(reader) = reader {
                reader.update(cx, |reader, cx| {
                    reader.loading_resource = Some(chapter_path_label(entry));
                    reader.progress = Some((bytes.len() as u64, response.entry_size));
                    cx.notify();
                })?;
            }
            if bytes.len() as u64 == response.entry_size {
                return Ok(bytes);
            }
        }
    }

    async fn text(&self, entry: &str, cx: &mut gpui::AsyncApp) -> Result<String> {
        Ok(String::from_utf8(self.read(entry, None, cx).await?)?)
    }
}

async fn load_structure(source: &EpubSource, cx: &mut gpui::AsyncApp) -> Result<EpubArchive> {
    use anyhow::Context as _;
    source.read("", None, cx).await?;
    let container = source.text("META-INF/container.xml", cx).await?;
    let opf_path = parse_container(&container).context("invalid EPUB container")?;
    let opf = source.text(&opf_path, cx).await?;
    let parsed = parse_opf(&opf);
    let opf_dir = zip_dir(&opf_path);
    let chapters = parsed
        .spine
        .iter()
        .filter_map(|id| parsed.manifest.get(id))
        .filter(|item| item.media_type.contains("html"))
        .map(|item| EpubChapter {
            path: resolve_zip_path(&opf_dir, &item.href),
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(!chapters.is_empty(), i18n::t!("66947e52b572e014"));
    let mut toc = Vec::new();
    let mut toc_dir = opf_dir.clone();
    if let Some(nav) = parsed
        .manifest
        .values()
        .find(|item| item.properties.iter().any(|property| property == "nav"))
    {
        let path = resolve_zip_path(&opf_dir, &nav.href);
        toc = parse_nav(&source.text(&path, cx).await?);
        toc_dir = zip_dir(&path);
    }
    if toc.is_empty() {
        let ncx = parsed
            .ncx_id
            .as_ref()
            .and_then(|id| parsed.manifest.get(id))
            .or_else(|| {
                parsed
                    .manifest
                    .values()
                    .find(|item| item.media_type == "application/x-dtbncx+xml")
            });
        if let Some(ncx) = ncx {
            let path = resolve_zip_path(&opf_dir, &ncx.href);
            toc = parse_ncx(&source.text(&path, cx).await?);
            toc_dir = zip_dir(&path);
        }
    }
    for entry in &mut toc {
        if let Some(href) = &entry.href {
            let path = resolve_zip_path(&toc_dir, href.split('#').next().unwrap_or(href));
            entry.chapter_index = chapters.iter().position(|chapter| chapter.path == path);
        }
    }
    Ok(EpubArchive {
        title: parsed.title,
        author: parsed.author,
        chapters,
        toc,
    })
}

fn chapter_image_paths(body: &str, directory: &str) -> Vec<String> {
    let mut reader = Reader::from_str(body);
    let mut paths = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element))
                if element.local_name().as_ref() == b"img" =>
            {
                for attribute in element.attributes().flatten() {
                    if attribute.key.as_ref() == b"src" {
                        if let Ok(value) = attribute
                            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        {
                            if !value.contains(':') {
                                let path = resolve_zip_path(
                                    directory,
                                    value.split('#').next().unwrap_or(&value),
                                );
                                if image_format_for_path(&path).is_some()
                                    && !paths.contains(&path)
                                    && paths.len() < 128
                                {
                                    paths.push(path);
                                }
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    paths
}

async fn load_chapter_content(
    source: &EpubSource,
    chapter_path: &str,
    reader: &gpui::WeakEntity<EpubReader>,
    cx: &mut gpui::AsyncApp,
) -> Result<(String, HashMap<String, ImageSource>)> {
    let bytes = source.read(chapter_path, Some(reader), cx).await?;
    let body = extract_body(&String::from_utf8(bytes)?);
    let paths = chapter_image_paths(&body, &zip_dir(chapter_path));
    let mut images = HashMap::new();
    let mut compressed_bytes = 0usize;
    let mut decoded_bytes = 0u64;
    for path in paths {
        let bytes = source.read(&path, Some(reader), cx).await?;
        compressed_bytes += bytes.len();
        anyhow::ensure!(
            compressed_bytes <= 16 * 1024 * 1024,
            i18n::t!("2514e8f9aac8cc7c")
        );
        let dimensions = if path.to_ascii_lowercase().ends_with(".svg") {
            let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default())?;
            (tree.size().width().ceil() as u32, tree.size().height().ceil() as u32)
        } else {
            image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?.into_dimensions()?
        };
        decoded_bytes =
            decoded_bytes.saturating_add(u64::from(dimensions.0) * u64::from(dimensions.1) * 4);
        anyhow::ensure!(
            decoded_bytes <= 64 * 1024 * 1024,
            i18n::t!("c528a431deef8f33")
        );
        if let Some(format) = image_format_for_path(&path) {
            images.insert(
                path,
                ImageSource::Image(Arc::new(gpui::Image::from_bytes(format, bytes))),
            );
        }
    }
    Ok((body, images))
}

#[cfg(test)]
fn open_zip(bytes: Arc<[u8]>) -> Result<zip::ZipArchive<Cursor<Arc<[u8]>>>, SharedString> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| SharedString::from(e.to_string()))
}

/// Resolves an image URL inside a chapter to a GPUI image source.
#[cfg(test)]
fn resolve_epub_image(
    zip: Option<&SharedArchive>,
    chapter_dir: &str,
    url: &str,
) -> Option<ImageSource> {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("data:") {
        return None;
    }
    let path = resolve_zip_path(chapter_dir, url.split('#').next().unwrap_or(url));
    let format = image_format_for_path(&path)?;
    let bytes = {
        let mut archive = zip?.lock().ok()?;
        let mut entry = archive.by_name(&path).ok()?;
        let mut contents = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut contents).ok()?;
        contents
    };
    let image = gpui::Image::from_bytes(format, bytes);
    Some(ImageSource::Image(Arc::new(image)))
}

fn image_format_for_path(path: &str) -> Option<ImageFormat> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::Webp),
        "svg" => Some(ImageFormat::Svg),
        "bmp" => Some(ImageFormat::Bmp),
        "ico" => Some(ImageFormat::Ico),
        "tiff" | "tif" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

/// Returns the directory portion of a ZIP entry path.
fn zip_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}

/// Resolves a possibly relative href against a directory inside the archive,
/// normalizing `.` and `..` segments. ZIP paths never start with `/`.
fn resolve_zip_path(base_dir: &str, href: &str) -> String {
    let mut segments: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for segment in href.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    segments.join("/")
}

/// Extracts the inner content of the `<body>` element of an XHTML document.
fn extract_body(document: &str) -> String {
    let lower = document.to_ascii_lowercase();
    let Some(body_start) = lower.find("<body") else {
        return document.to_string();
    };
    let Some(content_start) = lower[body_start..].find('>').map(|i| body_start + i + 1) else {
        return document.to_string();
    };
    let Some(body_end) = lower[content_start..].rfind("</body>") else {
        return document[content_start..].to_string();
    };
    document[content_start..content_start + body_end].to_string()
}

/// Parses the structural data (metadata, spine, table of contents) of an EPUB archive.
#[cfg(test)]
pub(crate) fn parse_epub(bytes: &[u8]) -> anyhow::Result<EpubArchive> {
    use anyhow::Context as _;
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("opening epub archive")?;

    let container =
        read_zip_entry(&mut archive, "META-INF/container.xml").context("reading container.xml")?;
    let opf_path = parse_container(&container).context("parsing container.xml")?;

    let opf = read_zip_entry(&mut archive, &opf_path).context("reading package document")?;
    let parsed = parse_opf(&opf);

    let opf_dir = zip_dir(&opf_path);
    let chapters = parsed
        .spine
        .iter()
        .filter_map(|idref| parsed.manifest.get(idref))
        .filter(|item| item.media_type.contains("html"))
        .map(|item| EpubChapter {
            path: resolve_zip_path(&opf_dir, &item.href),
        })
        .collect::<Vec<_>>();

    let mut toc = Vec::new();
    // TOC hrefs are relative to the document that contains them.
    let mut toc_base_dir = opf_dir.clone();
    if let Some(nav_item) = parsed
        .manifest
        .values()
        .find(|item| item.properties.iter().any(|p| p == "nav"))
    {
        let nav_path = resolve_zip_path(&opf_dir, &nav_item.href);
        if let Ok(nav) = read_zip_entry(&mut archive, &nav_path) {
            toc = parse_nav(&nav);
            toc_base_dir = zip_dir(&nav_path);
        }
    }
    if toc.is_empty() {
        let ncx_href = parsed
            .ncx_id
            .as_ref()
            .and_then(|id| parsed.manifest.get(id))
            .or_else(|| {
                parsed
                    .manifest
                    .values()
                    .find(|item| item.media_type == "application/x-dtbncx+xml")
            })
            .map(|item| item.href.clone());
        if let Some(ncx_href) = ncx_href {
            let ncx_path = resolve_zip_path(&opf_dir, &ncx_href);
            if let Ok(ncx) = read_zip_entry(&mut archive, &ncx_path) {
                toc = parse_ncx(&ncx);
                toc_base_dir = zip_dir(&ncx_path);
            }
        }
    }

    // Resolve TOC hrefs to chapter indices.
    for entry in &mut toc {
        if let Some(href) = &entry.href {
            let path = resolve_zip_path(&toc_base_dir, href.split('#').next().unwrap_or(href));
            entry.chapter_index = chapters.iter().position(|c| c.path == path);
        }
    }

    if chapters.is_empty() {
        anyhow::bail!("epub archive contains no chapters");
    }

    Ok(EpubArchive {
        title: parsed.title,
        author: parsed.author,
        chapters,
        toc,
    })
}

#[cfg(test)]
fn read_zip_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    path: &str,
) -> std::io::Result<String> {
    let mut entry = archive.by_name(path)?;
    let mut contents = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut contents)?;
    Ok(String::from_utf8_lossy(&contents).into_owned())
}

/// Finds the path of the OPF package document in `META-INF/container.xml`.
fn parse_container(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(element)) | Ok(Event::Start(element)) => {
                if element.local_name().as_ref() == b"rootfile" {
                    for attribute in element.attributes().flatten() {
                        if attribute.key.local_name().as_ref() == b"full-path" {
                            return attribute
                                .decoded_and_normalized_value(
                                    XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )
                                .ok()
                                .map(|value| value.into_owned());
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

#[derive(Default)]
struct ManifestItem {
    href: String,
    media_type: String,
    properties: Vec<String>,
}

#[derive(Default)]
struct ParsedOpf {
    title: Option<String>,
    author: Option<String>,
    manifest: HashMap<String, ManifestItem>,
    spine: Vec<String>,
    ncx_id: Option<String>,
}

fn parse_opf_item(
    element: &quick_xml::events::BytesStart,
    reader: &Reader<&[u8]>,
    parsed: &mut ParsedOpf,
) {
    let mut id = None;
    let mut idref = None;
    let mut href = None;
    let mut media_type = None;
    let mut properties = Vec::new();
    for attribute in element.attributes().flatten() {
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map(|value| value.into_owned())
            .unwrap_or_default();
        match attribute.key.local_name().as_ref() {
            b"id" => id = Some(value),
            b"idref" => idref = Some(value),
            b"href" => href = Some(value),
            b"media-type" => media_type = Some(value),
            b"properties" => properties = value.split_whitespace().map(str::to_string).collect(),
            _ => {}
        }
    }
    if let (Some(id), Some(href)) = (id, href) {
        parsed.manifest.insert(
            id,
            ManifestItem {
                href,
                media_type: media_type.unwrap_or_default(),
                properties,
            },
        );
    }
    if let Some(idref) = idref {
        parsed.spine.push(idref);
    }
}

fn parse_opf(xml: &str) -> ParsedOpf {
    let mut parsed = ParsedOpf::default();
    let mut reader = Reader::from_str(xml);
    let mut in_metadata = false;
    let mut current_text_element: Option<&'static str> = None;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"metadata" => in_metadata = true,
                b"title" if in_metadata => {
                    current_text_element = Some("title");
                    text.clear();
                }
                b"creator" if in_metadata => {
                    current_text_element = Some("creator");
                    text.clear();
                }
                b"spine" => {
                    for attribute in element.attributes().flatten() {
                        if attribute.key.local_name().as_ref() == b"toc" {
                            parsed.ncx_id = attribute
                                .decoded_and_normalized_value(
                                    XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )
                                .ok()
                                .map(|value| value.into_owned());
                        }
                    }
                }
                b"item" | b"itemref" => parse_opf_item(&element, &reader, &mut parsed),
                _ => {}
            },
            Ok(Event::Empty(element))
                if matches!(element.local_name().as_ref(), b"item" | b"itemref") =>
            {
                parse_opf_item(&element, &reader, &mut parsed);
            }
            Ok(Event::Text(event)) => {
                if current_text_element.is_some()
                    && let Ok(content) = event.xml_content(XmlVersion::Implicit1_0)
                {
                    text.push_str(&content);
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"metadata" => in_metadata = false,
                b"title" if current_text_element == Some("title") => {
                    parsed.title = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                    current_text_element = None;
                }
                b"creator" if current_text_element == Some("creator") => {
                    if parsed.author.is_none() {
                        parsed.author = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                    }
                    current_text_element = None;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    parsed
}

/// Parses an EPUB 3 navigation document (`properties="nav"`).
fn parse_nav(xhtml: &str) -> Vec<EpubTocEntry> {
    let mut entries = Vec::new();
    let mut reader = Reader::from_str(xhtml);
    let mut nav_depth = 0usize;
    let mut in_nav = false;
    let mut current_href: Option<String> = None;
    let mut current_text = String::new();
    let mut in_anchor = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"nav" => {
                    // Only follow the table-of-contents nav, ignoring landmarks etc.
                    let is_toc = element.attributes().flatten().any(|attribute| {
                        attribute.key.local_name().as_ref() == b"type"
                            && attribute
                                .value
                                .split(|b: &u8| b.is_ascii_whitespace())
                                .any(|v| v == b"toc")
                    });
                    if is_toc || !in_nav {
                        in_nav = true;
                        nav_depth = 0;
                    }
                }
                b"ol" | b"ul" if in_nav => nav_depth += 1,
                b"a" if in_nav => {
                    in_anchor = true;
                    current_text.clear();
                    current_href = None;
                    for attribute in element.attributes().flatten() {
                        if attribute.key.local_name().as_ref() == b"href" {
                            current_href = attribute
                                .decoded_and_normalized_value(
                                    XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )
                                .ok()
                                .map(|value| value.into_owned());
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Text(event)) if in_anchor => {
                if let Ok(content) = event.xml_content(XmlVersion::Implicit1_0) {
                    current_text.push_str(&content);
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"nav" => in_nav = false,
                b"ol" | b"ul" if in_nav => nav_depth = nav_depth.saturating_sub(1),
                b"a" if in_anchor => {
                    in_anchor = false;
                    let title = current_text.trim().to_string();
                    if !title.is_empty() {
                        entries.push(EpubTocEntry {
                            title,
                            href: current_href.clone(),
                            chapter_index: None,
                            depth: nav_depth.saturating_sub(1),
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    entries
}

struct PendingNcxEntry {
    title: String,
    src: Option<String>,
    depth: usize,
}

fn set_ncx_content(
    element: &quick_xml::events::BytesStart,
    reader: &Reader<&[u8]>,
    pending: &mut Option<PendingNcxEntry>,
) {
    let Some(entry) = pending.as_mut() else {
        return;
    };
    for attribute in element.attributes().flatten() {
        if attribute.key.local_name().as_ref() == b"src" {
            entry.src = attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .ok()
                .map(|value| value.into_owned());
        }
    }
}

/// Parses an EPUB 2 NCX document.
fn parse_ncx(xml: &str) -> Vec<EpubTocEntry> {
    let mut entries = Vec::new();
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut in_label = false;
    let mut pending: Option<PendingNcxEntry> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"content" => set_ncx_content(&element, &reader, &mut pending),
                b"navPoint" => {
                    // A nested navPoint starts after its parent's label and
                    // content, so the parent entry is complete now.
                    if let Some(entry) = pending.take()
                        && !entry.title.trim().is_empty()
                    {
                        entries.push(EpubTocEntry {
                            title: entry.title.trim().to_string(),
                            href: entry.src,
                            chapter_index: None,
                            depth: entry.depth,
                        });
                    }
                    pending = Some(PendingNcxEntry {
                        title: String::new(),
                        src: None,
                        depth,
                    });
                    depth += 1;
                }
                b"text" if pending.is_some() => in_label = true,
                _ => {}
            },
            Ok(Event::Empty(element)) if element.local_name().as_ref() == b"content" => {
                set_ncx_content(&element, &reader, &mut pending);
            }
            Ok(Event::Text(event)) if in_label => {
                if let Ok(content) = event.xml_content(XmlVersion::Implicit1_0)
                    && let Some(entry) = pending.as_mut()
                {
                    entry.title.push_str(&content);
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"text" => in_label = false,
                b"navPoint" => {
                    if let Some(entry) = pending.take()
                        && !entry.title.trim().is_empty()
                    {
                        entries.push(EpubTocEntry {
                            title: entry.title.trim().to_string(),
                            href: entry.src,
                            chapter_index: None,
                            depth: entry.depth,
                        });
                    }
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    entries
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn resolves_zip_paths() {
        assert_eq!(
            resolve_zip_path("OEBPS", "chapter1.xhtml"),
            "OEBPS/chapter1.xhtml"
        );
        assert_eq!(
            resolve_zip_path("OEBPS/text", "../images/a.png"),
            "OEBPS/images/a.png"
        );
        assert_eq!(resolve_zip_path("", "chapter1.xhtml"), "chapter1.xhtml");
        assert_eq!(resolve_zip_path("OEBPS", "./c.xhtml"), "OEBPS/c.xhtml");
        assert_eq!(resolve_zip_path("a/b", "../../c.xhtml"), "c.xhtml");
    }

    #[test]
    fn extracts_body_content() {
        let document = r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body><p>Hello</p></body></html>"#;
        assert_eq!(extract_body(document), "<p>Hello</p>");
        assert_eq!(extract_body("<p>No body</p>"), "<p>No body</p>");
    }

    pub(crate) fn epub_fixture(use_nav: bool) -> Vec<u8> {
        fn add(writer: &mut zip::ZipWriter<Cursor<Vec<u8>>>, name: &str, contents: &str) {
            let options = zip::write::FileOptions::default();
            writer.start_file(name, options).unwrap();
            writer.write_all(contents.as_bytes()).unwrap();
        }
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default();
        macro_rules! add {
            ($($args:tt)*) => {
                add(&mut writer, $($args)*)
            };
        }
        add!(
            "META-INF/container.xml",
            r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        );
        let (manifest_extra, spine_extra) = if use_nav {
            (
                r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#,
                "",
            )
        } else {
            (
                r#"<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>"#,
                r#" toc="ncx""#,
            )
        };
        add!(
            "OEBPS/content.opf",
            &format!(
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>测试书</dc:title>
    <dc:creator>作者甲</dc:creator>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/pixel.png" media-type="image/png"/>
    {manifest_extra}
  </manifest>
  <spine{spine_extra}>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#
            ),
        );
        add!(
            "OEBPS/ch1.xhtml",
            r#"<html><body><h1>第一章</h1><p>正文<img src="images/pixel.png"/></p><p><a href="text/ch2.xhtml">下一章</a></p></body></html>"#,
        );
        add!(
            "OEBPS/text/ch2.xhtml",
            r#"<html><body><h1>第二章</h1><p>结束</p></body></html>"#,
        );
        // 1x1 transparent PNG
        let pixel: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        writer
            .start_file("OEBPS/images/pixel.png", options)
            .unwrap();
        writer.write_all(pixel).unwrap();
        if use_nav {
            add!(
                "OEBPS/nav.xhtml",
                r#"<html><body><nav epub:type="toc" xmlns:epub="http://www.idpf.org/2007/ops"><ol><li><a href="ch1.xhtml">第一章</a><ol><li><a href="text/ch2.xhtml">第二章</a></li></ol></li></ol></nav></body></html>"#,
            );
        } else {
            add!(
                "OEBPS/toc.ncx",
                r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><navMap>
<navPoint id="n1"><navLabel><text>第一章</text></navLabel><content src="ch1.xhtml"/>
<navPoint id="n2"><navLabel><text>第二节</text></navLabel><content src="text/ch2.xhtml"/></navPoint>
</navPoint>
</navMap></ncx>"#,
            );
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn parses_epub2_structure() {
        let bytes = epub_fixture(false);
        let archive = parse_epub(&bytes).expect("epub2 fixture should parse");
        assert_eq!(archive.title.as_deref(), Some("测试书"));
        assert_eq!(archive.author.as_deref(), Some("作者甲"));
        assert_eq!(
            archive
                .chapters
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            vec!["OEBPS/ch1.xhtml", "OEBPS/text/ch2.xhtml"]
        );
        assert_eq!(archive.toc.len(), 2);
        assert_eq!(archive.toc[0].title, "第一章");
        assert_eq!(archive.toc[0].chapter_index, Some(0));
        assert_eq!(archive.toc[0].depth, 0);
        assert_eq!(archive.toc[1].title, "第二节");
        assert_eq!(archive.toc[1].chapter_index, Some(1));
        assert_eq!(archive.toc[1].depth, 1);
    }

    #[test]
    fn parses_epub3_structure() {
        let bytes = epub_fixture(true);
        let archive = parse_epub(&bytes).expect("epub3 fixture should parse");
        assert_eq!(archive.chapters.len(), 2);
        assert_eq!(archive.toc.len(), 2);
        assert_eq!(archive.toc[0].title, "第一章");
        assert_eq!(archive.toc[0].chapter_index, Some(0));
        assert_eq!(archive.toc[1].title, "第二章");
        assert_eq!(archive.toc[1].chapter_index, Some(1));
        assert_eq!(archive.toc[1].depth, 1);
    }

    #[test]
    fn resolves_images_from_archive() {
        let bytes: Arc<[u8]> = epub_fixture(false).into();
        let zip = Arc::new(Mutex::new(open_zip(bytes).unwrap()));
        let image = resolve_epub_image(Some(&zip), "OEBPS", "images/pixel.png");
        assert!(image.is_some());
        assert!(resolve_epub_image(Some(&zip), "OEBPS", "missing.png").is_none());
        assert!(resolve_epub_image(Some(&zip), "OEBPS", "images/pixel.txt").is_none());
        assert!(resolve_epub_image(Some(&zip), "OEBPS", "https://example.com/a.png").is_none());
    }

    #[test]
    fn chapter_images_are_local_deduplicated_and_bounded() {
        let body = r#"<body><img src="images/a.png"/><img src="images/a.png"/><img src="https://example.com/a.png"/><img src="data:image/png;base64,x"/></body>"#;
        assert_eq!(
            chapter_image_paths(body, "OEBPS"),
            vec!["OEBPS/images/a.png"]
        );
        let body = (0..200)
            .map(|index| format!("<img src=\"{index}.png\"/>"))
            .collect::<String>();
        assert_eq!(
            chapter_image_paths(&format!("<body>{body}</body>"), "").len(),
            128
        );
    }

    #[test]
    fn rejects_non_epub_data() {
        assert!(parse_epub(b"not a zip").is_err());
    }
}
