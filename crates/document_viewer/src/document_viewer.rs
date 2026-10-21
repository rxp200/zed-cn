mod cad_dxf;
mod cad_reader;
mod cad_scene;
mod epub_reader;
mod excel_reader;
mod model_mesh;
mod model_reader;
mod model_section;
mod pdf_reader;
mod step_reader;
mod word_reader;

use std::{path::Path, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Font, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use language::File as _;
use project::{Project, ProjectPath, git_store::GitStoreEvent};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use util::paths::PathExt;
use util::size::format_file_size;
use workspace::{
    ItemId, ItemSettings, Pane, ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ProjectItem, SerializableItem, TabContentParams},
};
use worktree::LoadedBinaryFile;

use editor::Editor;

use crate::cad_reader::CadReader;
use crate::cad_scene::CadScene;
use crate::epub_reader::EpubReader;
use crate::excel_reader::ExcelReader;
use crate::pdf_reader::PdfReader;
use crate::word_reader::WordReader;
use persistence::DocumentViewerDb;

/// A binary document format that can be previewed natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocumentFormat {
    Pdf,
    Epub,
    Spreadsheet,
    Model,
    Cad,
    Word,
}

impl DocumentFormat {
    fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "stl" | "obj" | "ply" => Some(Self::Model),
            "step" | "stp" => Some(Self::Model),
            "pdf" => Some(Self::Pdf),
            "epub" => Some(Self::Epub),
            "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" => Some(Self::Spreadsheet),
            "dxf" => Some(Self::Cad),
            "docx" | "docm" | "dotx" | "dotm" => Some(Self::Word),
            _ => None,
        }
    }

    /// Upper bound on the file size that is loaded into memory for previewing.
    ///
    /// `xlsx`/`xlsb` are parsed lazily per sheet, while `xls`/`ods` are parsed
    /// eagerly in full by calamine, so eager formats get a lower cap. Word
    /// documents are read entry by entry like EPUB books and never enter memory
    /// as a whole, so only their container needs a file-level cap.
    fn max_file_size(extension: &str) -> u64 {
        match extension.to_ascii_lowercase().as_str() {
            "docx" | "docm" | "dotx" | "dotm" => project::epub::OFFICE_FILE_LIMIT,
            _ => project::document_file_size_limit(extension).unwrap_or(0),
        }
    }
}

/// How far a previewable document has progressed. It is kept next to the bytes
/// so an open tab can show transfer progress instead of blocking on the read.
#[derive(Clone, Debug, PartialEq)]
enum DocumentLoadState {
    /// The tab is already open and the bytes are still in transit.
    Loading {
        transferred: u64,
        total: Option<u64>,
    },
    Ready,
    Failed(SharedString),
}

/// Events emitted by [`DocumentItem`] while it loads.
pub enum DocumentItemEvent {
    /// The bytes are available and a reader can be created.
    Ready,
    /// The bytes changed on disk and an existing reader must reload in place.
    Reloaded,
    /// The document could not be read.
    Failed,
}

/// A project item holding the raw bytes of a previewable binary document.
///
/// `Arc<Vec<u8>>`（而不是 `Arc<[u8]>`）是为了让打开文件时的 `Vec<u8>` 能直接
/// 包进 `Arc`，不需要再复制一份；PDF 渲染器也能共享同一块缓冲区。
pub struct DocumentItem {
    pub file: Arc<worktree::File>,
    pub contents: Arc<Vec<u8>>,
    pub format: DocumentFormat,
    pub model: Option<Arc<model_mesh::ModelMesh>>,
    /// Flattened drawing geometry, which replaces the raw bytes once parsed.
    pub cad: Option<Arc<CadScene>>,
    state: DocumentLoadState,
    load_task: Option<Task<()>>,
    /// Route-aware project handle used to re-read the document on change, so
    /// reloads follow the same local/remote path as the initial open.
    project: Entity<Project>,
    /// Incremented per reload so a slow load cannot overwrite newer bytes.
    reload_generation: u64,
    _worktree_subscription: Option<Subscription>,
}

impl EventEmitter<DocumentItemEvent> for DocumentItem {}

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
        log::info!(
            "[open-debug] DocumentItem::open {:?} format={format:?}",
            project_path.path.as_unix_str()
        );

        let entry = project.read(cx).entry_for_path(&project_path, cx);
        if let Some(entry) = &entry {
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

        let file = Arc::new(worktree::File {
            is_local: worktree.read(cx).is_local(),
            is_private: entry.as_ref().is_some_and(|entry| entry.is_private),
            disk_state: entry
                .as_ref()
                .and_then(|entry| {
                    entry.mtime.map(|mtime| language::DiskState::Present {
                        mtime,
                        size: entry.size,
                    })
                })
                .unwrap_or(language::DiskState::New),
            entry_id: entry.as_ref().map(|entry| entry.id),
            path: project_path.path.clone(),
            worktree,
        });

        // EPUB and Word documents read only the archive entries they show, so
        // they never wait for the whole file and report their own progress
        // from their readers.
        if matches!(format, DocumentFormat::Epub | DocumentFormat::Word) {
            let item = cx.new(|_| Self {
                file,
                contents: Arc::new(Vec::new()),
                format,
                model: None,
                cad: None,
                state: DocumentLoadState::Ready,
                load_task: None,
                project,
                reload_generation: 0,
                _worktree_subscription: None,
            });
            Self::follow_worktree_changes(&item, cx);
            return Task::ready(Ok(item));
        }

        // Formats that need the whole file ask how the server would transfer it
        // before opening the tab, so unsupported remote servers keep showing
        // their existing "cannot preview" notice instead of an empty tab.
        if let Err(error) = project.read(cx).validate_document_file(&project_path, cx) {
            return Task::ready(Err(error));
        }

        let total = entry
            .as_ref()
            .map(|entry| entry.size)
            .filter(|size| *size > 0);
        let item = cx.new(|_| Self {
            file,
            contents: Arc::new(Vec::new()),
            format,
            model: None,
            cad: None,
            state: DocumentLoadState::Loading {
                transferred: 0,
                total,
            },
            load_task: None,
            project: project.clone(),
            reload_generation: 0,
            _worktree_subscription: None,
        });
        Self::follow_worktree_changes(&item, cx);

        let progress_item = item.downgrade();
        let progress: Option<project::DocumentLoadProgress> = Some(Box::new(
            move |cx: &mut App, transferred: u64, total: u64| {
                if let Some(item) = progress_item.upgrade() {
                    item.update(cx, |item, cx| {
                        item.state = DocumentLoadState::Loading {
                            transferred,
                            total: Some(total),
                        };
                        cx.notify();
                    });
                }
            },
        ));
        let load = project.update(cx, |project, cx| {
            project.load_document_file(project_path, progress, cx)
        });
        let load = match load {
            Ok(load) => Some(load),
            Err(error) => {
                item.update(cx, |item, _| {
                    item.state = DocumentLoadState::Failed(error.to_string().into());
                });
                None
            }
        };
        if let Some(load) = load {
            item.update(cx, |item, cx| {
                item.load_task = Some(cx.spawn(async move |this, cx| {
                    let result = match load.await {
                        Ok(LoadedBinaryFile { file, content }) => {
                            let extension = extension.clone();
                            cx.background_spawn(async move {
                                let (content, model, cad) =
                                    parse_document_payload(format, extension, content).await?;
                                anyhow::Ok((file, content, model, cad))
                            })
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    this.update(cx, |item, cx| {
                        let event = match result {
                            Ok((file, content, model, cad)) => {
                                item.file = file;
                                item.contents = Arc::new(content);
                                item.model = model;
                                item.cad = cad;
                                item.state = DocumentLoadState::Ready;
                                DocumentItemEvent::Ready
                            }
                            Err(error) => {
                                log::warn!("failed to load document preview: {error:#}");
                                item.state = DocumentLoadState::Failed(error.to_string().into());
                                DocumentItemEvent::Failed
                            }
                        };
                        cx.notify();
                        cx.emit(event);
                    })
                    .log_err();
                }));
            });
        }

        Task::ready(Ok(item))
    }

    /// Watches the owning worktree so an open preview follows the file when it
    /// is replaced on disk instead of staying on the bytes it was opened with.
    fn follow_worktree_changes(item: &Entity<Self>, cx: &mut App) {
        item.update(cx, |item, cx| {
            let worktree = item.file.worktree.clone();
            item._worktree_subscription = Some(
                cx.subscribe(&worktree, |item: &mut Self, worktree, event, cx| {
                    item.on_worktree_event(&worktree, event, cx)
                }),
            );
        });
    }

    fn on_worktree_event(
        &mut self,
        worktree: &Entity<worktree::Worktree>,
        event: &worktree::Event,
        cx: &mut Context<Self>,
    ) {
        let worktree::Event::UpdatedEntries(changes) = event else {
            return;
        };
        let is_relevant = changes.iter().any(|(path, entry_id, _)| {
            Some(*entry_id) == self.file.entry_id || path.as_ref() == self.file.path.as_ref()
        });
        if !is_relevant {
            return;
        }

        let snapshot = worktree.read(cx).snapshot();
        let entry = self
            .file
            .entry_id
            .and_then(|entry_id| snapshot.entry_for_id(entry_id))
            .or_else(|| snapshot.entry_for_path(self.file.path.as_ref()));
        let new_file = match entry {
            Some(entry) => worktree::File {
                disk_state: match entry.mtime {
                    Some(mtime) => language::DiskState::Present {
                        mtime,
                        size: entry.size,
                    },
                    None => self.file.disk_state,
                },
                is_local: worktree.read(cx).is_local(),
                entry_id: Some(entry.id),
                path: entry.path.clone(),
                worktree: worktree.clone(),
                is_private: entry.is_private,
            },
            None => worktree::File {
                disk_state: language::DiskState::Deleted,
                is_local: worktree.read(cx).is_local(),
                entry_id: self.file.entry_id,
                path: self.file.path.clone(),
                is_private: self.file.is_private,
                worktree: worktree.clone(),
            },
        };

        if new_file == *self.file {
            return;
        }
        // A deleted document keeps its last loaded copy; a re-created one
        // becomes present again and reloads through this same path.
        let became_present = matches!(new_file.disk_state, language::DiskState::Present { .. });
        self.file = Arc::new(new_file);
        if became_present {
            self.reload(cx);
        }
        cx.notify();
    }

    /// Re-reads a document whose bytes changed on disk. Readers reload in place
    /// so the tab keeps its scroll position, zoom or camera.
    fn reload(&mut self, cx: &mut Context<Self>) {
        match self.format {
            // EPUB and Word read the archive entry by entry inside their own
            // readers, so the item only has to tell them the file changed.
            DocumentFormat::Epub | DocumentFormat::Word => {
                cx.emit(DocumentItemEvent::Reloaded);
            }
            _ => self.reload_bytes(cx),
        }
    }

    fn reload_bytes(&mut self, cx: &mut Context<Self>) {
        let path = self.project_path(cx);
        let Some(extension) = path.path.extension().map(str::to_string) else {
            return;
        };
        if let Some(entry) = self.project.read(cx).entry_for_path(&path, cx)
            && entry.size > DocumentFormat::max_file_size(&extension)
        {
            log::warn!(
                "document grew past the preview limit, keeping the loaded copy: {:?} ({} bytes)",
                path.path.as_unix_str(),
                entry.size
            );
            return;
        }

        let load = self
            .project
            .update(cx, |project, cx| project.load_document_file(path, None, cx));
        let load = match load {
            Ok(load) => load,
            Err(error) => {
                log::warn!("failed to reload document preview: {error:#}");
                return;
            }
        };

        self.reload_generation += 1;
        let generation = self.reload_generation;
        let format = self.format;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = match load.await {
                Ok(loaded) => loaded,
                Err(error) => {
                    log::warn!("failed to reload document preview: {error:#}");
                    return;
                }
            };
            let (file, content) = (loaded.file, loaded.content);
            let parsed = cx
                .background_spawn(parse_document_payload(format, extension, content))
                .await;
            this.update(cx, |item, cx| {
                if item.reload_generation != generation {
                    return;
                }
                match parsed {
                    Ok((content, model, cad)) => {
                        item.file = file;
                        item.contents = Arc::new(content);
                        item.model = model;
                        item.cad = cad;
                        item.state = DocumentLoadState::Ready;
                        cx.emit(DocumentItemEvent::Reloaded);
                        cx.notify();
                    }
                    // A failed re-parse keeps the previously rendered copy
                    // instead of replacing a usable preview with an error page.
                    Err(error) => log::warn!("failed to parse reloaded document: {error:#}"),
                }
            })
            .log_err();
        }));
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

    pub fn host_path(&self, cx: &App) -> std::path::PathBuf {
        self.file.worktree.read(cx).absolutize(self.file.path())
    }
}

impl project::ProjectItem for DocumentItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        DocumentFormat::from_extension(path.path.extension()?)?;
        log::info!(
            "[open-debug] DocumentItem::try_open {:?}",
            path.path.as_unix_str()
        );
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

/// Parses a loaded document into the representation its reader renders. Formats
/// that render the bytes directly keep them; models and drawings are flattened
/// so their file bytes do not stay resident.
async fn parse_document_payload(
    format: DocumentFormat,
    extension: String,
    content: Vec<u8>,
) -> Result<(
    Vec<u8>,
    Option<Arc<model_mesh::ModelMesh>>,
    Option<Arc<CadScene>>,
)> {
    match format {
        DocumentFormat::Model => {
            log::info!(
                "[open-debug] parsing model {extension} ({} bytes)",
                content.len()
            );
            let mesh = model_mesh::ModelMesh::parse(&extension, &content)?;
            log::info!(
                "[open-debug] model parsed: {} triangles",
                mesh.triangles.len()
            );
            Ok((Vec::new(), Some(Arc::new(mesh)), None))
        }
        // The drawing only needs its bytes until it has been flattened into the
        // render scene, which is smaller than the file and drops everything the
        // reader cannot draw.
        DocumentFormat::Cad => {
            log::info!("[open-debug] parsing drawing ({} bytes)", content.len());
            let scene = cad_dxf::parse(&content)?;
            log::info!(
                "[open-debug] drawing parsed: {} paths, {} fills, {} texts",
                scene.paths.len(),
                scene.fills.len(),
                scene.texts.len()
            );
            Ok((Vec::new(), None, Some(Arc::new(scene))))
        }
        _ => {
            log::info!("[open-debug] document loaded ({} bytes)", content.len());
            Ok((content, None, None))
        }
    }
}

/// Which concrete reader a [`DocumentView`] shows. `Loading` covers both a
/// document whose bytes are still in transit and one that failed to load.
enum DocumentChild {
    Loading,
    Pdf(Entity<PdfReader>),
    Epub(Entity<EpubReader>),
    Spreadsheet(Entity<ExcelReader>),
    Model(Entity<model_reader::ModelReader>),
    Cad(Entity<CadReader>),
    Word(Entity<WordReader>),
}

/// Workspace item that renders a binary document (PDF, EPUB or spreadsheet)
/// with a native GPUI view, in the style of the built-in image viewer.
pub struct DocumentView {
    item: Entity<DocumentItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    child: DocumentChild,
    _subscriptions: Vec<Subscription>,
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
        log::info!("[open-debug] DocumentView::new format={format:?}");

        let subscriptions = vec![cx.subscribe_in(
            &item,
            window,
            |this, _item, event, window, cx| match event {
                DocumentItemEvent::Ready => {
                    if matches!(this.child, DocumentChild::Loading) {
                        this.child =
                            Self::build_child(this.item.clone(), this.project.clone(), window, cx);
                    }
                }
                DocumentItemEvent::Reloaded => {
                    if matches!(this.child, DocumentChild::Loading) {
                        this.child =
                            Self::build_child(this.item.clone(), this.project.clone(), window, cx);
                    } else {
                        this.reload_child(window, cx);
                    }
                }
                DocumentItemEvent::Failed => cx.notify(),
            },
        )];

        let child = match item.read(cx).state {
            DocumentLoadState::Ready => {
                Self::build_child(item.clone(), project.clone(), window, cx)
            }
            DocumentLoadState::Loading { .. } | DocumentLoadState::Failed(_) => {
                DocumentChild::Loading
            }
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
            focus_handle: cx.focus_handle(),
            child,
            _subscriptions: subscriptions,
        }
    }

    /// Reloads the open reader in place. Each reader keeps its own view state
    /// (scroll position, zoom, camera), so the tab stays where the user left it.
    fn reload_child(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.child {
            DocumentChild::Loading => {}
            DocumentChild::Pdf(reader) => reader.update(cx, |reader, cx| reader.reload(window, cx)),
            DocumentChild::Epub(reader) => {
                reader.update(cx, |reader, cx| reader.reload(window, cx))
            }
            DocumentChild::Spreadsheet(reader) => {
                reader.update(cx, |reader, cx| reader.reload(window, cx))
            }
            DocumentChild::Model(reader) => {
                reader.update(cx, |reader, cx| reader.reload(window, cx))
            }
            DocumentChild::Cad(reader) => reader.update(cx, |reader, cx| reader.reload(window, cx)),
            DocumentChild::Word(reader) => {
                reader.update(cx, |reader, cx| reader.reload(window, cx))
            }
        }
    }

    fn build_child(
        item: Entity<DocumentItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> DocumentChild {
        match item.read(cx).format {
            DocumentFormat::Model => {
                DocumentChild::Model(cx.new(|cx| model_reader::ModelReader::new(item, window, cx)))
            }
            DocumentFormat::Pdf => {
                DocumentChild::Pdf(cx.new(|cx| PdfReader::new(item, project, window, cx)))
            }
            DocumentFormat::Epub => DocumentChild::Epub(cx.new(|cx| {
                EpubReader::new(item, project.clone(), window, cx)
                    .with_languages(project.read(cx).languages().clone())
            })),
            DocumentFormat::Spreadsheet => {
                DocumentChild::Spreadsheet(cx.new(|cx| ExcelReader::new(item, project, window, cx)))
            }
            DocumentFormat::Cad => {
                DocumentChild::Cad(cx.new(|cx| CadReader::new(item, window, cx)))
            }
            DocumentFormat::Word => DocumentChild::Word(cx.new(|cx| {
                WordReader::new(item, project.clone(), window, cx)
                    .with_languages(project.read(cx).languages().clone())
            })),
        }
    }

    /// Renders the state of a document whose bytes are still being read, or the
    /// error that stopped the read.
    fn render_pending(&self, cx: &mut Context<Self>) -> AnyElement {
        let item = self.item.read(cx);
        let path = item.host_path(cx).display().to_string();
        match &item.state {
            DocumentLoadState::Failed(error) => v_flex()
                .size_full()
                .gap_2()
                .items_center()
                .justify_center()
                .child(
                    Label::new(i18n::t!("49fceb3a4d998915", error = error.clone()))
                        .color(Color::Error),
                )
                .child(Label::new(path).color(Color::Muted).single_line())
                .debug_selector(|| "document-load-error".to_string())
                .into_any_element(),
            state => {
                let (transferred, total) = match state {
                    DocumentLoadState::Loading { transferred, total } => (*transferred, *total),
                    _ => (0, None),
                };
                let title = if item.file.is_local {
                    i18n::t!("de0df9167630114a")
                } else {
                    i18n::t!("dd6f7f92c4d6e776")
                };
                v_flex()
                    .size_full()
                    .gap_3()
                    .items_center()
                    .justify_center()
                    .child(ui::SpinnerLabel::new())
                    .child(Label::new(title).size(LabelSize::Large))
                    .child(Label::new(path).color(Color::Muted).single_line())
                    .when_some(total, |element, total| {
                        element.child(
                            v_flex()
                                .items_center()
                                .gap_1()
                                .debug_selector(|| "document-load-progress".to_string())
                                .child(div().w(px(240.)).child(ui::ProgressBar::new(
                                    "document-load-progress-bar",
                                    transferred as f32,
                                    total.max(1) as f32,
                                    cx,
                                )))
                                .child(Label::new(format!(
                                    "{:.0}%",
                                    transferred as f64 / total.max(1) as f64 * 100.0
                                )))
                                .child(
                                    Label::new(format!(
                                        "{} / {}",
                                        format_file_size(transferred, false),
                                        format_file_size(total, false)
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        )
                    })
                    .into_any_element()
            }
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
        let abs_path = self.item.read(cx).host_path(cx);
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
        let path = self.item.read(cx).host_path(cx);
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
            DocumentChild::Loading => self.focus_handle.clone(),
            DocumentChild::Pdf(reader) => reader.read(cx).focus_handle(cx),
            DocumentChild::Epub(reader) => reader.read(cx).focus_handle(cx),
            DocumentChild::Spreadsheet(reader) => reader.read(cx).focus_handle(cx),
            DocumentChild::Model(reader) => reader.read(cx).focus_handle(cx),
            DocumentChild::Cad(reader) => reader.read(cx).focus_handle(cx),
            DocumentChild::Word(reader) => reader.read(cx).focus_handle(cx),
        }
    }
}

impl gpui::Render for DocumentView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.child {
            DocumentChild::Loading => self.render_pending(cx),
            DocumentChild::Pdf(reader) => reader.clone().into_any_element(),
            DocumentChild::Epub(reader) => reader.clone().into_any_element(),
            DocumentChild::Spreadsheet(reader) => reader.clone().into_any_element(),
            DocumentChild::Model(reader) => reader.clone().into_any_element(),
            DocumentChild::Cad(reader) => reader.clone().into_any_element(),
            DocumentChild::Word(reader) => reader.clone().into_any_element(),
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
        let document_path = self.item.read(cx).host_path(cx);

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
    fn render_word_controls(reader: &Entity<WordReader>) -> AnyElement {
        h_flex()
            .gap_1()
            .child(
                IconButton::new("word-decrease-font", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("99f597e412b89d14"),
                            &word_reader::DecreaseFontSize,
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
                IconButton::new("word-increase-font", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            i18n::t!("a4b896e9fcee581e"),
                            &word_reader::IncreaseFontSize,
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
            let hides_toolbar = matches!(
                item.read(cx).child,
                DocumentChild::Spreadsheet(_)
                    | DocumentChild::Model(_)
                    | DocumentChild::Cad(_)
                    | DocumentChild::Loading
            );
            return if hides_toolbar {
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
            DocumentChild::Loading => DocumentChild::Loading,
            DocumentChild::Pdf(reader) => DocumentChild::Pdf(reader.clone()),
            DocumentChild::Epub(reader) => DocumentChild::Epub(reader.clone()),
            DocumentChild::Spreadsheet(reader) => DocumentChild::Spreadsheet(reader.clone()),
            DocumentChild::Model(reader) => DocumentChild::Model(reader.clone()),
            DocumentChild::Cad(reader) => DocumentChild::Cad(reader.clone()),
            DocumentChild::Word(reader) => DocumentChild::Word(reader.clone()),
        };
        match &child {
            DocumentChild::Pdf(reader) => Self::render_pdf_controls(reader, cx),
            DocumentChild::Epub(reader) => Self::render_epub_controls(reader, cx),
            DocumentChild::Word(reader) => Self::render_word_controls(reader),
            DocumentChild::Loading
            | DocumentChild::Spreadsheet(_)
            | DocumentChild::Model(_)
            | DocumentChild::Cad(_) => div().into_any_element(),
        }
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<DocumentView>(cx);
    workspace::register_serializable_item::<DocumentView>(cx);
    // The pane handle is only available inside the workspace, so opening a
    // drawing in a text editor is a workspace action rather than a button that
    // builds the editor itself.
    cx.observe_new(|workspace: &mut Workspace, _window, _| {
        workspace.register_action(|workspace, _: &cad_reader::OpenAsText, window, cx| {
            let Some(view) = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<DocumentView>(cx))
            else {
                return;
            };
            let (project, path) = view.read_with(cx, |view, cx| {
                (view.project.clone(), view.item.read(cx).project_path(cx))
            });
            let pane = workspace.active_pane().clone();
            let load = project.update(cx, |project, cx| project.open_buffer(path, cx));
            cx.spawn_in(window, async move |_, cx| {
                let buffer = load.await?;
                pane.update_in(cx, |pane, window, cx| {
                    let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
                    pane.add_item(Box::new(editor), true, true, None, window, cx);
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        });
    })
    .detach();
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
    use postage::stream::Stream as _;
    use util::rel_path::rel_path;
    use workspace::AppState;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    /// Builds a test project containing one previewable document.
    async fn test_document_project(
        cx: &mut TestAppContext,
        name: &str,
        contents: Vec<u8>,
    ) -> (Entity<Project>, ProjectPath) {
        let (_, project, project_path) = test_document_project_with_fs(cx, name, contents).await;
        (project, project_path)
    }

    /// Same as [`test_document_project`], but also hands back the fake filesystem
    /// so a test can rewrite the file and rescan the worktree.
    async fn test_document_project_with_fs(
        cx: &mut TestAppContext,
        name: &str,
        contents: Vec<u8>,
    ) -> (Arc<FakeFs>, Entity<Project>, ProjectPath) {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/root"))
            .await
            .expect("test root should be created");
        fs.insert_file(&format!("/root/{name}"), contents).await;

        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        let worktree_id = cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("test project should contain a worktree")
                .read(cx)
                .id()
        });
        (
            fs,
            project,
            ProjectPath {
                worktree_id,
                path: rel_path(name).into(),
            },
        )
    }

    /// Rewrites a file and rescans its worktree entry, which is how the real
    /// filesystem watcher reports an external change.
    async fn rewrite_test_file(
        cx: &mut TestAppContext,
        fs: &FakeFs,
        project: &Entity<Project>,
        name: &str,
        contents: Vec<u8>,
    ) {
        fs.insert_file(&format!("/root/{name}"), contents).await;
        rescan_test_file(cx, project, name).await;
    }

    /// Rescans one worktree entry after its file was changed outside the editor.
    async fn rescan_test_file(cx: &mut TestAppContext, project: &Entity<Project>, name: &str) {
        let worktree = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().expect("test worktree")
        });
        worktree
            .read_with(cx, |worktree, _| {
                worktree
                    .as_local()
                    .expect("test worktree is local")
                    .manually_refresh_entries_for_paths(vec![rel_path(name).into()])
            })
            .recv()
            .await;
        cx.run_until_parked();
    }

    /// Builds an image that can be pushed into the window's sprite atlas.
    fn test_render_image() -> Arc<gpui::RenderImage> {
        let frame = image::Frame::new(image::ImageBuffer::from_pixel(
            2,
            2,
            image::Rgba([10u8, 20, 30, 255]),
        ));
        Arc::new(gpui::RenderImage::new(smallvec::SmallVec::from_elem(
            frame, 1,
        )))
    }

    /// Opens a document and waits for its bytes, which is what most tests want.
    async fn open_test_document(
        cx: &mut TestAppContext,
        name: &str,
        contents: Vec<u8>,
    ) -> (Entity<Project>, Entity<DocumentItem>) {
        let (project, project_path) = test_document_project(cx, name, contents).await;
        let item = cx
            .update(|cx| DocumentItem::open(project.clone(), project_path, cx))
            .await
            .expect("test document should open");
        cx.run_until_parked();
        item.read_with(cx, |item, _| {
            assert!(
                matches!(&item.state, DocumentLoadState::Ready),
                "test document should finish loading, got {:?}",
                item.state
            );
        });

        (project, item)
    }

    #[gpui::test]
    async fn test_open_dxf_document_and_release_its_bytes(cx: &mut TestAppContext) {
        init_test(cx);
        let bytes =
            crate::cad_dxf::minimal_dxf("0\nLINE\n8\nWALLS\n10\n0.0\n20\n0.0\n11\n10.0\n21\n5.0\n");
        let (project, item) = open_test_document(cx, "plan.dxf", bytes).await;
        item.read_with(cx, |item, _| {
            assert_eq!(item.format, DocumentFormat::Cad);
            assert!(
                item.contents.is_empty(),
                "the parsed drawing must not keep the file bytes resident"
            );
            let scene = item.cad.as_ref().expect("drawing scene");
            assert_eq!(scene.paths.len(), 1);
            assert_eq!(
                scene.layers.last().expect("walls layer").name.as_ref(),
                "WALLS"
            );
        });
        let (view, window) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        window.run_until_parked();
        view.read_with(&mut *window, |view, _| {
            assert!(matches!(view.child, DocumentChild::Cad(_)));
        });
        let viewport = window.debug_bounds("cad-viewport").expect("cad viewport");
        assert!(viewport.size.width > gpui::px(100.0), "{viewport:?}");
        assert!(viewport.size.height > gpui::px(60.0), "{viewport:?}");
        assert!(window.debug_bounds("cad-toolbar").is_some());
        assert!(window.debug_bounds("cad-status").is_some());
    }

    #[gpui::test]
    async fn test_dxf_is_claimed_but_dwg_is_not(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, path) = test_document_project(cx, "plan.dwg", b"AC1032".to_vec()).await;
        assert!(
            cx.update(|cx| {
                <DocumentItem as project::ProjectItem>::try_open(&project, &path, cx)
            })
            .is_none(),
            "DWG must not be claimed by the drawing viewer"
        );

        // A DWG that was renamed to .dxf explains itself instead of failing
        // silently inside the parsing stage.
        let (project, project_path) =
            test_document_project(cx, "renamed.dxf", b"AC1032\x00\x00\x00\x00".to_vec()).await;
        let item = cx
            .update(|cx| DocumentItem::open(project, project_path, cx))
            .await
            .expect("the tab opens first");
        cx.run_until_parked();
        item.read_with(cx, |item, _| {
            assert!(
                matches!(&item.state, DocumentLoadState::Failed(message)
                    if message.contains(&i18n::t!("4a57c500c76df00e"))),
                "expected the DWG explanation, got {:?}",
                item.state
            );
        });
    }

    #[gpui::test]
    async fn test_step_documents_open_the_model_reader(cx: &mut TestAppContext) {
        init_test(cx);
        assert_eq!(
            DocumentFormat::from_extension("step"),
            Some(DocumentFormat::Model)
        );
        assert_eq!(
            DocumentFormat::from_extension("STP"),
            Some(DocumentFormat::Model)
        );
        let step =
            crate::step_reader::tests::cube_step(crate::step_reader::tests::millimetre_unit());
        let (project, item) = open_test_document(cx, "part.step", step.into_bytes()).await;
        item.read_with(cx, |item, _| {
            assert_eq!(item.format, DocumentFormat::Model);
            let mesh = item.model.as_ref().expect("STEP mesh");
            assert_eq!(mesh.step_solids, 1);
            assert_eq!(mesh.triangles.len(), 12);
            assert_eq!(
                mesh.step_unit,
                Some(crate::step_reader::StepUnit::Millimetre)
            );
            assert!(
                !mesh.topology_reliable,
                "tessellated STEP surfaces must not claim an exact topology check"
            );
        });
        let (view, window) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        window.run_until_parked();
        assert!(window.debug_bounds("model-viewport").is_some());
        assert!(window.debug_bounds("model-panel").is_some());
        window.update(|_, cx| {
            assert!(matches!(view.read(cx).child, DocumentChild::Model(_)));
        });
    }

    #[gpui::test]
    async fn test_model_viewport_has_height(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) = open_test_document(
            cx,
            "triangle.obj",
            b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3".to_vec(),
        )
        .await;
        let (_view, window) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        window.run_until_parked();
        let bounds = window.debug_bounds("model-viewport").expect("viewport");
        assert!(bounds.size.width > gpui::px(100.0), "{bounds:?}");
        assert!(bounds.size.height > gpui::px(100.0), "{bounds:?}");
        let panel = window.debug_bounds("model-panel").expect("panel");
        assert_eq!(panel.size.width, gpui::px(300.0));
        assert!(panel.size.height > bounds.size.height);
        let toolbar = window.debug_bounds("model-toolbar").expect("toolbar");
        assert!(toolbar.size.height >= gpui::px(40.0), "{toolbar:?}");
        let geometry = window
            .debug_bounds("model-geometry-header")
            .expect("geometry");
        let topology = window
            .debug_bounds("model-topology-header")
            .expect("topology");
        assert!(geometry.origin.x > panel.origin.x);
        assert!(topology.origin.y > geometry.origin.y + geometry.size.height);
    }

    #[gpui::test]
    async fn test_model_view_cube_selects_presets(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) = open_test_document(
            cx,
            "triangle.obj",
            b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3".to_vec(),
        )
        .await;
        let (view, window) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        let reader = window.update(|_, cx| match &view.read(cx).child {
            DocumentChild::Model(reader) => reader.clone(),
            _ => panic!("model documents should open the model reader"),
        });
        window.run_until_parked();
        let viewport = window.debug_bounds("model-viewport").expect("viewport");
        let cube = window.debug_bounds("model-view-cube").expect("view cube");
        assert_eq!(cube.size.width, cube.size.height);
        assert!(cube.origin.x > viewport.origin.x + viewport.size.width * 0.75);
        assert!(cube.origin.y < viewport.origin.y + viewport.size.height * 0.25);
        for (selector, expected) in [
            ("model-view-cube-top", i18n::t!("d5cdfcf7ff75338f")),
            ("model-view-cube-front", i18n::t!("a617590202898821")),
            ("model-view-cube-right", i18n::t!("883361d5d682a157")),
        ] {
            let region = window.debug_bounds(selector).expect("view cube region");
            assert!(
                cube.contains(&region.center()),
                "{selector} must be inside the cube"
            );
            window.simulate_click(region.center(), gpui::Modifiers::none());
            window.run_until_parked();
            assert_eq!(
                window.update(|_, cx| reader.read(cx).preset_label()),
                expected,
                "{selector} must select its preset"
            );
        }
    }

    #[gpui::test(iterations = 20)]
    async fn test_model_views_share_mesh_and_release_on_last_close(cx: &mut TestAppContext) {
        init_test(cx);
        let bytes = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3".to_vec();
        let (project, item) = open_test_document(cx, "triangle.obj", bytes).await;
        let weak_mesh = item.read_with(cx, |item, _| {
            assert!(item.contents.is_empty());
            Arc::downgrade(item.model.as_ref().expect("mesh"))
        });
        let (view, window) = cx.add_window_view(|window, cx| {
            DocumentView::new(item.clone(), project.clone(), window, cx)
        });
        let second = window.update(|window, cx| {
            cx.new(|cx| DocumentView::new(item.clone(), project.clone(), window, cx))
        });
        drop(item);
        drop(second);
        assert!(weak_mesh.upgrade().is_some());
        window.update(|window, cx| {
            window.replace_root(cx, |_, _| gpui::Empty);
        });
        drop(view);
        window.run_until_parked();
        let _arena_clear = window.update(|window, cx| window.draw(cx));
        window.run_until_parked();
        assert!(
            weak_mesh.upgrade().is_none(),
            "no retained geometry after last view closes"
        );
    }

    #[gpui::test]
    async fn test_remote_document_path_is_not_a_local_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) = open_test_document(
            cx,
            "book.epub",
            crate::epub_reader::tests::epub_fixture(true),
        )
        .await;
        item.update(cx, |item, _| {
            let file = &item.file;
            item.file = Arc::new(worktree::File {
                worktree: file.worktree.clone(),
                path: file.path.clone(),
                disk_state: file.disk_state,
                entry_id: file.entry_id,
                is_local: false,
                is_private: file.is_private,
            });
        });
        item.read_with(cx, |item, cx| {
            assert!(
                item.abs_path(cx).is_none(),
                "remote paths must not become local file operations"
            );
            assert_eq!(item.host_path(cx), Path::new("/root/book.epub"));
        });
        let (view, cx) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.tab_tooltip_text(cx).as_deref(),
                Some("/root/book.epub")
            );
        });
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
        assert_eq!(
            DocumentFormat::from_extension("DXF"),
            Some(DocumentFormat::Cad)
        );
        assert_eq!(
            DocumentFormat::from_extension("DOCX"),
            Some(DocumentFormat::Word)
        );
        assert_eq!(
            DocumentFormat::from_extension("docm"),
            Some(DocumentFormat::Word)
        );
        assert_eq!(
            DocumentFormat::from_extension("dotx"),
            Some(DocumentFormat::Word)
        );
        assert_eq!(
            DocumentFormat::from_extension("doc"),
            None,
            "the legacy binary Word format is not previewable and must not be claimed"
        );
        assert_eq!(
            DocumentFormat::from_extension("dwg"),
            None,
            "DWG is not previewable and must not be claimed"
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

    /// A one-triangle OBJ and its two-triangle counterpart, so a reload can be
    /// observed through the parsed mesh rather than the raw bytes.
    fn one_triangle_obj() -> Vec<u8> {
        b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n".to_vec()
    }

    fn two_triangle_obj() -> Vec<u8> {
        b"v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nf 1 2 3\nf 2 4 3\n".to_vec()
    }

    /// A workbook rewritten by another application must refresh the open tab
    /// instead of leaving the bytes it was opened with.
    #[gpui::test]
    async fn test_open_spreadsheet_reloads_when_the_file_changes_on_disk(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project, project_path) = test_document_project_with_fs(
            cx,
            "data.xlsx",
            crate::excel_reader::tests::minimal_xlsx(),
        )
        .await;
        let item = cx
            .update(|cx| DocumentItem::open(project.clone(), project_path, cx))
            .await
            .expect("spreadsheet should open");

        let (view, cx) = cx.add_window_view(|window, cx| {
            DocumentView::new(item.clone(), project.clone(), window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Spreadsheet(reader) => {
                assert_eq!(
                    reader.read(cx).cell_text_for_test(1, 1).as_deref(),
                    Some("42"),
                    "the first read must show the original value"
                );
            }
            _ => panic!("expected spreadsheet reader"),
        });

        rewrite_test_file(
            cx,
            &fs,
            &project,
            "data.xlsx",
            crate::excel_reader::tests::minimal_xlsx_with_value(7),
        )
        .await;

        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Spreadsheet(reader) => {
                assert_eq!(
                    reader.read(cx).cell_text_for_test(1, 1).as_deref(),
                    Some("7"),
                    "an externally rewritten workbook must reload in place"
                );
            }
            _ => panic!("expected spreadsheet reader"),
        });
    }

    /// A regenerated 3D model must refresh the open tab while keeping the
    /// viewer's camera, which is what makes a live STL preview usable.
    #[gpui::test]
    async fn test_open_model_reloads_in_place_and_keeps_the_camera(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project, project_path) =
            test_document_project_with_fs(cx, "shape.obj", one_triangle_obj()).await;
        let item = cx
            .update(|cx| DocumentItem::open(project.clone(), project_path, cx))
            .await
            .expect("model should open");

        let (view, cx) = cx.add_window_view(|window, cx| {
            DocumentView::new(item.clone(), project.clone(), window, cx)
        });
        cx.run_until_parked();

        let reader = view.read_with(cx, |view, _| match &view.child {
            DocumentChild::Model(reader) => reader.clone(),
            _ => panic!("expected model reader"),
        });
        reader.update(cx, |reader, _| {
            reader.set_camera_for_test(1.25, -0.5, 2.5);
        });
        let camera_before = reader.read_with(cx, |reader, _| reader.camera_state_for_test());

        rewrite_test_file(cx, &fs, &project, "shape.obj", two_triangle_obj()).await;

        assert_eq!(
            item.read_with(cx, |item, _| item
                .model
                .as_ref()
                .expect("parsed model")
                .triangles
                .len()),
            2,
            "the rewritten model must replace the parsed mesh"
        );
        let camera_after = reader.read_with(cx, |reader, _| reader.camera_state_for_test());
        assert_eq!(
            camera_before, camera_after,
            "reloading a model must keep the viewer's camera"
        );
    }

    /// An EPUB rewritten by another application must re-parse its structure
    /// instead of keeping the archive it was opened with.
    #[gpui::test]
    async fn test_open_epub_reloads_in_place_when_the_file_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project, project_path) = test_document_project_with_fs(
            cx,
            "book.epub",
            crate::epub_reader::tests::epub_fixture(false),
        )
        .await;
        let item = cx
            .update(|cx| DocumentItem::open(project.clone(), project_path, cx))
            .await
            .expect("epub should open");
        let (view, cx) = cx.add_window_view(|window, cx| {
            DocumentView::new(item.clone(), project.clone(), window, cx)
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Epub(reader) => {
                let archive = reader.read(cx).archive.clone().expect("epub archive");
                assert_eq!(archive.toc[1].title, "第二节");
            }
            _ => panic!("expected epub reader"),
        });

        // The EPUB 3 fixture carries a nav document, so the table of contents
        // is parsed from a different file than the EPUB 2 fixture's NCX.
        rewrite_test_file(
            cx,
            &fs,
            &project,
            "book.epub",
            crate::epub_reader::tests::epub_fixture(true),
        )
        .await;

        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Epub(reader) => {
                let archive = reader
                    .read(cx)
                    .archive
                    .clone()
                    .expect("reloaded epub archive");
                assert_eq!(
                    archive.toc[1].title, "第二章",
                    "an externally rewritten book must re-parse its structure"
                );
            }
            _ => panic!("expected epub reader"),
        });
    }

    /// A rewritten Word document must be read again, which replaces its
    /// rendered Markdown instead of leaving the previous render on screen.
    #[gpui::test]
    async fn test_open_word_reloads_in_place_when_the_file_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project, project_path) = test_document_project_with_fs(
            cx,
            "report.docx",
            crate::word_reader::tests::docx_fixture(),
        )
        .await;
        let item = cx
            .update(|cx| DocumentItem::open(project.clone(), project_path, cx))
            .await
            .expect("word document should open");
        let (view, cx) = cx.add_window_view(|window, cx| {
            DocumentView::new(item.clone(), project.clone(), window, cx)
        });
        cx.run_until_parked();

        let markdown_before = view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Word(reader) => reader
                .read(cx)
                .markdown
                .clone()
                .expect("rendered document")
                .entity_id(),
            _ => panic!("expected word reader"),
        });

        rewrite_test_file(
            cx,
            &fs,
            &project,
            "report.docx",
            crate::word_reader::tests::docx_fixture(),
        )
        .await;

        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Word(reader) => {
                let reader = reader.read(cx);
                assert!(reader.error.is_none(), "reloaded document must not error");
                let markdown_after = reader.markdown.clone().expect("re-rendered document");
                assert_ne!(
                    markdown_before,
                    markdown_after.entity_id(),
                    "a rewritten document must be rendered again"
                );
            }
            _ => panic!("expected word reader"),
        });
    }

    /// A document removed while its tab is open keeps its last loaded copy;
    /// only a re-created file triggers another reload.
    #[gpui::test]
    async fn test_deleted_document_keeps_its_loaded_copy(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project, project_path) = test_document_project_with_fs(
            cx,
            "data.xlsx",
            crate::excel_reader::tests::minimal_xlsx(),
        )
        .await;
        let item = cx
            .update(|cx| DocumentItem::open(project.clone(), project_path, cx))
            .await
            .expect("spreadsheet should open");
        cx.run_until_parked();
        let contents_before = item.read_with(cx, |item, _| item.contents.clone());
        assert!(!contents_before.is_empty());

        fs.remove_file(Path::new("/root/data.xlsx"), Default::default())
            .await
            .expect("test file should be removed");
        rescan_test_file(cx, &project, "data.xlsx").await;

        assert_eq!(
            item.read_with(cx, |item, _| item.contents.clone()),
            contents_before,
            "removing a document must not drop the loaded preview"
        );
        item.read_with(cx, |item, _| {
            assert!(matches!(item.state, DocumentLoadState::Ready));
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
        assert!(
            cx.read(|cx| item.read(cx).contents.is_empty()),
            "opening a tab must not load the book"
        );

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
    async fn test_open_word_document(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) =
            open_test_document(cx, "report.docx", crate::word_reader::tests::docx_fixture()).await;
        assert_eq!(cx.read(|cx| item.read(cx).format), DocumentFormat::Word);
        assert!(
            cx.read(|cx| item.read(cx).contents.is_empty()),
            "opening the tab must not load the container"
        );

        let (view, cx) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| match &view.child {
            DocumentChild::Word(reader) => {
                let reader = reader.read(cx);
                assert!(reader.error.is_none(), "the document should be parsed");
                assert!(reader.markdown.is_some(), "the document flow should render");
            }
            _ => panic!("expected a word reader"),
        });
    }

    #[gpui::test(iterations = 20)]
    async fn test_invalid_epub_still_opens_a_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) = open_test_document(cx, "broken.epub", b"not a ZIP".to_vec()).await;
        assert!(cx.read(|cx| item.read(cx).contents.is_empty()));
        let (view, window_cx) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        view.read_with(window_cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx).as_ref(), "broken.epub");
        });
        window_cx.run_until_parked();
        view.read_with(window_cx, |view, cx| match &view.child {
            DocumentChild::Epub(reader) => assert!(reader.read(cx).archive.is_none()),
            _ => panic!("expected EPUB"),
        });
    }

    #[gpui::test(iterations = 20)]
    async fn test_epub_reader_drop_cancels_pending_load(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) = open_test_document(
            cx,
            "book.epub",
            crate::epub_reader::tests::epub_fixture(true),
        )
        .await;
        let (view, window_cx) = cx.add_window_view(|window, cx| {
            DocumentView::new(item.clone(), project.clone(), window, cx)
        });
        let reader = view.read_with(window_cx, |view, _| match &view.child {
            DocumentChild::Epub(reader) => reader.clone(),
            _ => panic!("expected EPUB"),
        });
        let weak = reader.downgrade();
        view.update_in(window_cx, |view, window, cx| {
            view.child =
                DocumentChild::Epub(cx.new(|cx| EpubReader::new(item, project, window, cx)));
        });
        drop(reader);
        assert!(weak.upgrade().is_none());
        window_cx.run_until_parked();
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

    /// Opening a document must hand back an item straight away instead of
    /// blocking until every byte was read.
    #[gpui::test(iterations = 8)]
    async fn test_document_open_reports_loading_before_its_bytes_arrive(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, project_path) =
            test_document_project(cx, "doc.pdf", crate::pdf_reader::tests::minimal_pdf()).await;

        let item = cx
            .update(|cx| DocumentItem::open(project.clone(), project_path, cx))
            .await
            .expect("opening a document must not wait for its bytes");
        cx.update(|cx| {
            assert!(
                matches!(&item.read(cx).state, DocumentLoadState::Loading { .. }),
                "the item must exist while the bytes are still in transit"
            );
        });

        cx.run_until_parked();
        cx.update(|cx| {
            let item = item.read(cx);
            assert!(
                matches!(&item.state, DocumentLoadState::Ready),
                "the item must finish loading"
            );
            assert!(
                !item.contents.is_empty(),
                "the bytes must be kept once read"
            );
        });
    }

    /// The open tab must show how much of the file has been transferred.
    #[gpui::test]
    async fn test_document_loading_tab_shows_transfer_progress(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, loaded) =
            open_test_document(cx, "doc.pdf", crate::pdf_reader::tests::minimal_pdf()).await;
        let file = loaded.read_with(cx, |item, _| item.file.clone());
        let bytes = crate::pdf_reader::tests::minimal_pdf();
        let item = cx.new(|_| DocumentItem {
            file,
            contents: Arc::new(Vec::new()),
            format: DocumentFormat::Pdf,
            model: None,
            cad: None,
            state: DocumentLoadState::Loading {
                transferred: 5,
                total: Some(10),
            },
            load_task: None,
            project: project.clone(),
            reload_generation: 0,
            _worktree_subscription: None,
        });

        let (view, cx) =
            cx.add_window_view(|window, cx| DocumentView::new(item.clone(), project, window, cx));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        view.read_with(&mut *cx, |view, _| {
            assert!(matches!(view.child, DocumentChild::Loading));
        });
        assert!(
            cx.debug_bounds("document-load-progress").is_some(),
            "the loading tab must show how much of the file was transferred"
        );

        item.update(&mut *cx, |item, cx| {
            item.contents = Arc::new(bytes);
            item.state = DocumentLoadState::Ready;
            cx.emit(DocumentItemEvent::Ready);
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        view.read_with(&mut *cx, |view, _| {
            assert!(
                matches!(view.child, DocumentChild::Pdf(_)),
                "the reader must replace the loading state"
            );
        });
        assert!(
            cx.debug_bounds("document-load-progress").is_none(),
            "the progress indicator must disappear once the bytes are read"
        );
    }

    /// A failed transfer is reported in the already-open tab rather than
    /// leaving the progress indicator spinning forever.
    #[gpui::test]
    async fn test_document_load_failure_is_reported_in_the_open_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, loaded) =
            open_test_document(cx, "doc.pdf", crate::pdf_reader::tests::minimal_pdf()).await;
        let file = loaded.read_with(cx, |item, _| item.file.clone());
        let failed = cx.new(|_| DocumentItem {
            file,
            contents: Arc::new(Vec::new()),
            format: DocumentFormat::Pdf,
            model: None,
            cad: None,
            state: DocumentLoadState::Failed("connection lost".into()),
            load_task: None,
            project: project.clone(),
            reload_generation: 0,
            _worktree_subscription: None,
        });
        let (view, cx) =
            cx.add_window_view(|window, cx| DocumentView::new(failed, project, window, cx));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        view.read_with(&mut *cx, |view, _| {
            assert!(matches!(view.child, DocumentChild::Loading));
        });
        assert!(
            cx.debug_bounds("document-load-error").is_some(),
            "a failed document must report the error in its tab"
        );
        assert!(
            cx.debug_bounds("document-load-progress").is_none(),
            "a failed document must not render transfer progress"
        );
    }

    /// Rendered pages live in the window's sprite atlas as well as in the page
    /// cache, so releasing the reader has to drop both.
    #[gpui::test]
    async fn test_pdf_pages_leave_the_atlas_when_the_reader_is_released(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) =
            open_test_document(cx, "doc.pdf", crate::pdf_reader::tests::minimal_pdf()).await;
        let (view, cx) =
            cx.add_window_view(|window, cx| DocumentView::new(item, project, window, cx));
        let reader = view.read_with(&mut *cx, |view, _| match &view.child {
            DocumentChild::Pdf(reader) => reader.clone(),
            _ => panic!("expected a PDF reader"),
        });
        let page = test_render_image();
        reader.update(&mut *cx, |reader, _| {
            reader.set_page_sizes_for_test(vec![(200.0, 100.0)]);
            reader.insert_page_for_test(0, page.clone(), 1.0, 1024, 1);
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(
            cx.update(|window, _| window.has_image_atlas_entry(&page)),
            "a cached page must be uploaded to the sprite atlas when painted"
        );

        cx.update(|window, cx| {
            window.replace_root(cx, |_, _| gpui::Empty);
        });
        drop(reader);
        drop(view);
        cx.cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(
            !cx.update(|window, _| window.has_image_atlas_entry(&page)),
            "closing the tab must drop its rendered pages from the sprite atlas"
        );
    }

    /// Evicting a page from the cache must free its atlas entry too, otherwise
    /// scrolling a large document keeps every page it ever rendered.
    #[gpui::test]
    async fn test_evicted_pdf_pages_leave_the_atlas(cx: &mut TestAppContext) {
        init_test(cx);
        let (project, item) =
            open_test_document(cx, "doc.pdf", crate::pdf_reader::tests::minimal_pdf()).await;
        let evicted = test_render_image();
        let kept = test_render_image();
        let (paint_evicted, paint_kept) = (evicted.clone(), kept.clone());
        let (_view, cx) =
            cx.add_window_view(move |_window, _cx| PaintImages(vec![paint_evicted, paint_kept]));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(cx.update(|window, _| window.has_image_atlas_entry(&evicted)));
        assert!(cx.update(|window, _| window.has_image_atlas_entry(&kept)));

        let reader = cx.update(|window, cx| cx.new(|cx| PdfReader::new(item, project, window, cx)));
        let fillers: Vec<Arc<gpui::RenderImage>> = (0..12).map(|_| test_render_image()).collect();
        reader.update(&mut *cx, {
            let evicted = evicted.clone();
            let kept = kept.clone();
            move |reader, _| {
                reader.insert_page_for_test(1, evicted, 1.0, 1024, 0);
                reader.insert_page_for_test(5, kept, 1.0, 1024, 1000);
                for (index, image) in fillers.into_iter().enumerate() {
                    let page = index + 2;
                    reader.insert_page_for_test(page, image, 1.0, 1024, 100 + page as u64);
                }
            }
        });
        reader.update(&mut *cx, |reader, cx| reader.evict_cache_for_test(cx));
        let cached = reader.read_with(&mut *cx, |reader, _| reader.cached_pages_for_test());
        assert!(
            !cached.contains(&1),
            "the least recently used page must be evicted"
        );
        assert!(cached.contains(&5), "newer pages must stay cached");
        assert!(
            !cx.update(|window, _| window.has_image_atlas_entry(&evicted)),
            "an evicted page must be removed from the sprite atlas"
        );
        assert!(
            cx.update(|window, _| window.has_image_atlas_entry(&kept)),
            "pages that stay cached must keep their atlas entries"
        );
    }

    struct PaintImages(Vec<Arc<gpui::RenderImage>>);

    impl Render for PaintImages {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            v_flex().children(
                self.0
                    .iter()
                    .cloned()
                    .map(|image| gpui::img(image).w(px(40.)).h(px(40.)))
                    .collect::<Vec<_>>(),
            )
        }
    }
}
