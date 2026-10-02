mod epub_reader;
mod excel_reader;
mod pdf_reader;

use std::{path::Path, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Font, SharedString,
    Task, WeakEntity, Window,
};
use language::File as _;
use project::{Project, ProjectPath, git_store::GitStoreEvent};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{Tooltip, prelude::*};
use util::paths::PathExt;
use workspace::{
    ItemId, ItemSettings, Pane, ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ProjectItem, SerializableItem, TabContentParams},
};
use worktree::LoadedBinaryFile;

use crate::epub_reader::EpubReader;
use crate::excel_reader::ExcelReader;
use crate::pdf_reader::PdfReader;
use persistence::DocumentViewerDb;

/// A binary document format that can be previewed natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocumentFormat {
    Pdf,
    Epub,
    Spreadsheet,
}

impl DocumentFormat {
    fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "pdf" => Some(Self::Pdf),
            "epub" => Some(Self::Epub),
            "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" => Some(Self::Spreadsheet),
            _ => None,
        }
    }

    /// Upper bound on the file size that is loaded into memory for previewing.
    ///
    /// `xlsx`/`xlsb` are parsed lazily per sheet, while `xls`/`ods` are parsed
    /// eagerly in full by calamine, so eager formats get a lower cap.
    fn max_file_size(extension: &str) -> u64 {
        const LAZY_SPREADSHEET_MAX: u64 = 256 * 1024 * 1024;
        const EAGER_SPREADSHEET_MAX: u64 = 64 * 1024 * 1024;
        const PDF_EPUB_MAX: u64 = 512 * 1024 * 1024;
        match extension.to_ascii_lowercase().as_str() {
            "xlsx" | "xlsm" | "xlsb" => LAZY_SPREADSHEET_MAX,
            "xls" | "ods" => EAGER_SPREADSHEET_MAX,
            _ => PDF_EPUB_MAX,
        }
    }
}

/// A project item holding the raw bytes of a previewable binary document.
pub struct DocumentItem {
    pub file: Arc<worktree::File>,
    pub contents: Arc<[u8]>,
    pub format: DocumentFormat,
}

impl DocumentItem {
    pub fn open(
        project: Entity<Project>,
        project_path: ProjectPath,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(extension) = project_path.path.extension().map(str::to_string) else {
            return Task::ready(Err(anyhow!("unsupported document format")));
        };
        let Some(format) = DocumentFormat::from_extension(&extension) else {
            return Task::ready(Err(anyhow!("unsupported document format")));
        };

        if let Some(entry) = project.read(cx).entry_for_path(&project_path, cx) {
            if !entry.is_file() {
                return Task::ready(Err(anyhow!("not a file")));
            }
            if entry.size > DocumentFormat::max_file_size(&extension) {
                return Task::ready(Err(anyhow!(
                    "document is too large to preview ({} bytes)",
                    entry.size
                )));
            }
        }

        let Some(worktree) = project
            .read(cx)
            .worktree_for_id(project_path.worktree_id, cx)
        else {
            return Task::ready(Err(anyhow!("worktree not found")));
        };
        let load = worktree.update(cx, |worktree, cx| {
            worktree.load_binary_file(project_path.path.as_ref(), cx)
        });
        cx.spawn(async move |cx| {
            let LoadedBinaryFile { file, content } = load.await?;
            Ok(cx.new(|_| DocumentItem {
                file,
                contents: content.into(),
                format,
            }))
        })
    }

    pub fn project_path(&self, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path().clone(),
        }
    }

    pub fn abs_path(&self, cx: &App) -> Option<std::path::PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
    }
}

impl project::ProjectItem for DocumentItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        DocumentFormat::from_extension(path.path.extension()?)?;
        Some(Self::open(project.clone(), path.clone(), cx))
    }

    fn entry_id(&self, _: &App) -> Option<project::ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

/// Which concrete reader a [`DocumentView`] shows.
enum DocumentChild {
    Pdf(Entity<PdfReader>),
    Epub(Entity<EpubReader>),
    Spreadsheet(Entity<ExcelReader>),
}

/// Workspace item that renders a binary document (PDF, EPUB or spreadsheet)
/// with a native GPUI view, in the style of the built-in image viewer.
pub struct DocumentView {
    item: Entity<DocumentItem>,
    project: Entity<Project>,
    child: DocumentChild,
}

pub enum DocumentViewEvent {
    TitleChanged,
}

impl EventEmitter<DocumentViewEvent> for DocumentView {}

impl DocumentView {
    pub fn new(
        item: Entity<DocumentItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let format = item.read(cx).format;
        let child = match format {
            DocumentFormat::Pdf => DocumentChild::Pdf(
                cx.new(|cx| PdfReader::new(item.clone(), project.clone(), window, cx)),
            ),
            DocumentFormat::Epub => DocumentChild::Epub(cx.new(|cx| {
                EpubReader::new(item.clone(), project.clone(), window, cx)
                    .with_languages(project.read(cx).languages().clone())
            })),
            DocumentFormat::Spreadsheet => DocumentChild::Spreadsheet(
                cx.new(|cx| ExcelReader::new(item.clone(), project.clone(), window, cx)),
            ),
        };

        let git_store = project.read(cx).git_store().clone();
        cx.subscribe(&git_store, |_, _, event, cx| {
            if matches!(event, GitStoreEvent::DiffBaseChanged(_)) {
                cx.emit(DocumentViewEvent::TitleChanged);
            }
        })
        .detach();

        Self {
            item,
            project,
            child,
        }
    }
}

impl Item for DocumentView {
    type Event = DocumentViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        match event {
            DocumentViewEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.item.entity_id(), self.item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.item.read(cx).abs_path(cx)?;
        let file_path = abs_path.compact().to_string_lossy().into_owned();
        Some(file_path.into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let project_path = self.item.read(cx).project_path(cx);

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .git_store()
                .read(cx)
                .display_status_for_project_path(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.item.read(cx).file.file_name(cx).to_string().into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        let show_breadcrumb = EditorSettings::get_global(cx).toolbar.breadcrumbs;
        if show_breadcrumb {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        let mut path = self.item.read(cx).file.path().to_rel_path_buf();
        if self.project.read(cx).visible_worktrees(cx).count() > 1
            && let Some(worktree) = self
                .project
                .read(cx)
                .worktree_for_id(self.item.read(cx).project_path(cx).worktree_id, cx)
        {
            path = worktree.read(cx).root_name().join(&path);
        }
        let font = ThemeSettings::get_global(cx).buffer_font.clone();
        Some((
            vec![HighlightedText {
                text: path
                    .display(self.project.read(cx).path_style(cx))
                    .to_string()
                    .into(),
                highlights: vec![],
            }],
            Some(font),
        ))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let item = self.item.clone();
        let project = self.project.clone();
        Task::ready(Some(
            cx.new(|cx| DocumentView::new(item, project, window, cx)),
        ))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.item.read(cx).file.disk_state().is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl EventEmitter<()> for DocumentView {}

impl Focusable for DocumentView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.child {
            DocumentChild::Pdf(reader) => reader.read(cx).focus_handle(cx),
            DocumentChild::Epub(reader) => reader.read(cx).focus_handle(cx),
            DocumentChild::Spreadsheet(reader) => reader.read(cx).focus_handle(cx),
        }
    }
}

impl gpui::Render for DocumentView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.child {
            DocumentChild::Pdf(reader) => reader.clone().into_any_element(),
            DocumentChild::Epub(reader) => reader.clone().into_any_element(),
            DocumentChild::Spreadsheet(reader) => reader.clone().into_any_element(),
        }
    }
}

impl ProjectItem for DocumentView {
    type Item = DocumentItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

impl SerializableItem for DocumentView {
    fn serialized_item_kind() -> &'static str {
        "DocumentView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = DocumentViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let document_path = db
                .get_document_path(item_id, workspace_id)?
                .context("No document path found")?;

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(document_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;
            let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let project_for_open = project.clone();
            let item = project
                .update(cx, |_, cx| {
                    DocumentItem::open(project_for_open, project_path, cx)
                })
                .await?;
            cx.update(|window, cx| Ok(cx.new(|cx| DocumentView::new(item, project, window, cx))))?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = DocumentViewerDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "document_views", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let document_path = self.item.read(cx).abs_path(cx)?;

        let db = DocumentViewerDb::global(cx);
        Some(cx.background_spawn(async move {
            log::debug!("Saving document at path {document_path:?}");
            db.save_document_path(item_id, workspace_id, document_path)
                .await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

/// Toolbar controls shown when a document view is the active pane item.
/// Dispatches to the controls matching the active document format.
pub struct DocumentToolbarControls {
    document_view: Option<WeakEntity<DocumentView>>,
    _subscription: Option<gpui::Subscription>,
}

impl DocumentToolbarControls {
    pub fn new() -> Self {
        Self {
            document_view: None,
            _subscription: None,
        }
    }

    fn render_pdf_controls(reader: &Entity<PdfReader>, cx: &mut Context<Self>) -> AnyElement {
        let zoom_percentage = format!("{}%", (reader.read(cx).zoom_level() * 100.0).round() as i32);
        let current_page = reader.read(cx).current_page() + 1;
        let total_pages = reader.read(cx).total_pages();
        h_flex()
            .gap_1()
            .child(
                IconButton::new("pdf-zoom-out", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(i18n::t!("290f68030501cd9c"), &pdf_reader::ZoomOut, cx)
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.zoom_out(cx));
                            }
                        }
                    }),
            )
            .child(
                h_flex()
                    .px_1()
                    .child(Label::new(zoom_percentage).size(LabelSize::Small)),
            )
            .child(
                IconButton::new("pdf-zoom-in", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| {
                        Tooltip::for_action(i18n::t!("80f8fbcfa0117633"), &pdf_reader::ZoomIn, cx)
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.zoom_in(cx));
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("pdf-fit-to-width", IconName::Maximize)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("b5d1db3cba561ce0"),
                            &pdf_reader::FitToWidth,
                            cx,
                        )
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.fit_to_width(cx));
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("pdf-previous-page", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("a163aaa22cf7ce6c"),
                            &pdf_reader::PreviousPage,
                            cx,
                        )
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.previous_page(cx));
                            }
                        }
                    }),
            )
            .child(
                Label::new(i18n::t!(
                    "f0df641234559617",
                    current = current_page,
                    total = total_pages
                ))
                .size(LabelSize::Small),
            )
            .child(
                IconButton::new("pdf-next-page", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(i18n::t!("6ead5dfdc11ef7da"), &pdf_reader::NextPage, cx)
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.next_page(cx));
                            }
                        }
                    }),
            )
            .into_any_element()
    }

    fn render_epub_controls(reader: &Entity<EpubReader>, cx: &mut Context<Self>) -> AnyElement {
        let (chapter, chapters, title, toc_visible) = {
            let reader = reader.read(cx);
            (
                reader.chapter_index() + 1,
                reader.chapter_count(),
                reader.book_title(cx),
                reader.toc_visible(),
            )
        };
        h_flex()
            .gap_1()
            .child(
                IconButton::new("epub-toggle-toc", IconName::ListCollapse)
                    .icon_size(IconSize::Small)
                    .toggle_state(toc_visible)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("a9360e0212a4d173"),
                            &epub_reader::ToggleTableOfContents,
                            cx,
                        )
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.toggle_toc(cx));
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("epub-previous-chapter", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("71033b9cb6b3bfff"),
                            &epub_reader::PreviousChapter,
                            cx,
                        )
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.previous_chapter(cx));
                            }
                        }
                    }),
            )
            .child(
                Label::new(i18n::t!(
                    "54b1fb15d12c5b79",
                    current = chapter,
                    total = chapters
                ))
                .size(LabelSize::Small),
            )
            .child(
                IconButton::new("epub-next-chapter", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("72a250b0d0a2ed44"),
                            &epub_reader::NextChapter,
                            cx,
                        )
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.next_chapter(cx));
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("epub-decrease-font", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("99f597e412b89d14"),
                            &epub_reader::DecreaseFontSize,
                            cx,
                        )
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.decrease_font_size(cx));
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("epub-increase-font", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("a4b896e9fcee581e"),
                            &epub_reader::IncreaseFontSize,
                            cx,
                        )
                    })
                    .on_click({
                        let reader = reader.downgrade();
                        move |_, _window, cx| {
                            if let Some(reader) = reader.upgrade() {
                                reader.update(cx, |reader, cx| reader.increase_font_size(cx));
                            }
                        }
                    }),
            )
            .when_some(title, |this, title| {
                this.child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .single_line(),
                )
            })
            .into_any_element()
    }
}

impl EventEmitter<workspace::ToolbarItemEvent> for DocumentToolbarControls {}

impl workspace::ToolbarItemView for DocumentToolbarControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn workspace::item::ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.document_view = None;
        self._subscription = None;

        if let Some(item) = active_pane_item.and_then(|i| i.downcast::<DocumentView>()) {
            self._subscription = Some(cx.observe(&item, |_, _, cx| {
                cx.notify();
            }));
            self.document_view = Some(item.downgrade());
            cx.notify();
            let is_spreadsheet = matches!(item.read(cx).child, DocumentChild::Spreadsheet(_));
            return if is_spreadsheet {
                ToolbarItemLocation::Hidden
            } else {
                ToolbarItemLocation::PrimaryRight
            };
        }

        ToolbarItemLocation::Hidden
    }
}

impl gpui::Render for DocumentToolbarControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.document_view.as_ref().and_then(|v| v.upgrade()) else {
            return div().into_any_element();
        };
        let child = match &view.read(cx).child {
            DocumentChild::Pdf(reader) => DocumentChild::Pdf(reader.clone()),
            DocumentChild::Epub(reader) => DocumentChild::Epub(reader.clone()),
            DocumentChild::Spreadsheet(reader) => DocumentChild::Spreadsheet(reader.clone()),
        };
        match &child {
            DocumentChild::Pdf(reader) => Self::render_pdf_controls(reader, cx),
            DocumentChild::Epub(reader) => Self::render_epub_controls(reader, cx),
            DocumentChild::Spreadsheet(_) => div().into_any_element(),
        }
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<DocumentView>(cx);
    workspace::register_serializable_item::<DocumentView>(cx);
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct DocumentViewerDb(ThreadSafeConnection);

    impl Domain for DocumentViewerDb {
        const NAME: &str = stringify!(DocumentViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
                CREATE TABLE document_views (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    document_path BLOB,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
        )];
    }

    db::static_connection!(DocumentViewerDb, [WorkspaceDb]);

    impl DocumentViewerDb {
        query! {
            pub async fn save_document_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                document_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO document_views(item_id, workspace_id, document_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_document_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT document_path
                FROM document_views
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs as _};
    use gpui::TestAppContext;
    use util::rel_path::rel_path;
    use workspace::AppState;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    async fn open_test_document(
        cx: &mut TestAppContext,
        name: &str,
        contents: Vec<u8>,
    ) -> (Entity<Project>, Entity<DocumentItem>) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/root"))
            .await
            .expect("test root should be created");
        fs.insert_file(&format!("/root/{name}"), contents).await;

        let project = Project::test(fs, [Path::new("/root")], cx).await;
        let worktree_id = cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("test project should contain a worktree")
                .read(cx)
                .id()
        });
        let item = cx
            .update(|cx| {
                DocumentItem::open(
                    project.clone(),
                    ProjectPath {
                        worktree_id,
                        path: rel_path(name).into(),
                    },
                    cx,
                )
            })
            .await
            .expect("test document should open");

        (project, item)
    }

    #[gpui::test]
    async fn test_format_detection(cx: &mut TestAppContext) {
        let _ = cx;
        assert_eq!(
            DocumentFormat::from_extension("pdf"),
            Some(DocumentFormat::Pdf)
        );
        assert_eq!(
            DocumentFormat::from_extension("EPUB"),
            Some(DocumentFormat::Epub)
        );
        assert_eq!(
            DocumentFormat::from_extension("XLSX"),
            Some(DocumentFormat::Spreadsheet)
        );
        assert_eq!(
            DocumentFormat::from_extension("ods"),
            Some(DocumentFormat::Spreadsheet)
        );
        assert_eq!(DocumentFormat::from_extension("txt"), None);
    }

    #[gpui::test]
    async fn test_open_spreadsheet_document(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) =
            open_test_document(cx, "data.xlsx", crate::excel_reader::tests::minimal_xlsx()).await;
        assert_eq!(
            cx.read(|cx| item.read(cx).format),
            DocumentFormat::Spreadsheet
        );

        let (view, cx) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        cx.run_until_parked();
        cx.run_until_parked();
        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Spreadsheet(reader) => {
                let reader = reader.read(cx);
                assert_eq!(reader.sheet_names.len(), 1);
                assert!(reader.error.is_none());
            }
            _ => panic!("expected spreadsheet reader"),
        });
    }

    #[gpui::test]
    async fn test_open_epub_document(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) = open_test_document(
            cx,
            "book.epub",
            crate::epub_reader::tests::epub_fixture(true),
        )
        .await;
        assert_eq!(cx.read(|cx| item.read(cx).format), DocumentFormat::Epub);

        let (view, cx) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Epub(reader) => {
                let reader = reader.read(cx);
                let archive = reader.archive.as_ref().expect("epub should be parsed");
                assert_eq!(archive.chapters.len(), 2);
                assert!(reader.markdown.is_some());
            }
            _ => panic!("expected epub reader"),
        });
    }

    #[gpui::test]
    async fn test_open_pdf_document(cx: &mut TestAppContext) {
        init_test(cx);
        let (_project, item) =
            open_test_document(cx, "doc.pdf", crate::pdf_reader::tests::minimal_pdf()).await;
        assert_eq!(cx.read(|cx| item.read(cx).format), DocumentFormat::Pdf);
    }

    #[gpui::test]
    async fn test_unsupported_extension_is_rejected(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/root")).await.unwrap();
        fs.insert_file("/root/notes.txt", b"hello".to_vec()).await;
        let project = Project::test(fs, [Path::new("/root")], cx).await;
        let worktree_id =
            cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
        let result = cx
            .update(|cx| {
                DocumentItem::open(
                    project,
                    ProjectPath {
                        worktree_id,
                        path: rel_path("notes.txt").into(),
                    },
                    cx,
                )
            })
            .await;
        assert!(result.is_err());
    }
}
