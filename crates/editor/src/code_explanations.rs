use crate::code_explanation_units::{
    first_non_whitespace_column, parse_annotations, units_at, whole_file_unit,
};
use crate::{Autoscroll, BlockPlacement, BlockProperties, BlockStyle, Editor};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use futures::{StreamExt as _, stream::FuturesUnordered};
use gpui::{
    Action as _, App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, IntoElement,
    SharedString, Task,
};
use language_model::{
    LanguageModel, LanguageModelProvider, LanguageModelProviderId, LanguageModelRegistry,
    LanguageModelRequest, LanguageModelRequestMessage, MessageContent, Role,
};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownStyle, WrapButtonVisibility,
};
use project::ProjectItem as _;
use settings::{RegisterSetting, Settings, SettingsContent};
use sha2::{Digest as _, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use text::ToPoint as _;
use ui::{Modal, ModalHeader, Section, SpinnerLabel, Tooltip, WithScrollbar, prelude::*};
use util::ResultExt;
use workspace::ModalView;

static SHARED_RESULTS: std::sync::Mutex<std::collections::VecDeque<(String, SharedString)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

fn shared_result(key: &str, value: Option<SharedString>) -> Option<SharedString> {
    let mut results = SHARED_RESULTS.lock().ok()?;
    let existing = results
        .iter()
        .position(|(candidate, _)| candidate == key)
        .and_then(|index| results.remove(index))
        .map(|(_, text)| text);
    let result = value.or(existing);
    if let Some(text) = &result {
        results.push_back((key.to_owned(), text.clone()));
        while results.len() > 64 {
            results.pop_front();
        }
    }
    result
}

static CACHE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static ACTIVE_REQUESTS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<gpui::EntityId, Vec<String>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::default()));
static WAITERS: std::sync::Mutex<Vec<(u64, gpui::EntityId, String, u8, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());
static NEXT_WAITER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct CodeExplanationRequestWaiter(u64);
impl CodeExplanationRequestWaiter {
    pub fn new(scope: gpui::EntityId, key: String, priority: u8) -> Result<Self> {
        let identifier = NEXT_WAITER.fetch_add(1, Ordering::SeqCst);
        WAITERS
            .lock()
            .map_err(|_| anyhow::anyhow!(i18n::t!("288a4fb2d7ca4bd1")))?
            .push((identifier, scope, key, priority, std::time::Instant::now()));
        Ok(Self(identifier))
    }

    pub fn acquire(&self, maximum: usize) -> Option<CodeExplanationRequestPermit> {
        let waiters = WAITERS.lock().ok()?;
        let active_requests = ACTIVE_REQUESTS.lock().ok()?;
        let (_, scope, _, _, _) = waiters
            .iter()
            .find(|(identifier, _, _, _, _)| *identifier == self.0)?;
        let active = active_requests
            .get(scope)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if active.len() >= maximum {
            return None;
        }
        let next = next_waiter(&waiters, *scope, active);
        let (identifier, scope, key, _, _) = next?;
        if *identifier != self.0 {
            return None;
        }
        let scope = *scope;
        let key = key.clone();
        drop(active_requests);
        CodeExplanationRequestPermit::acquire(scope, key, maximum)
    }
}
fn next_waiter<'a>(
    waiters: &'a [(u64, gpui::EntityId, String, u8, std::time::Instant)],
    scope: gpui::EntityId,
    active: &[String],
) -> Option<&'a (u64, gpui::EntityId, String, u8, std::time::Instant)> {
    waiters
        .iter()
        .filter(|(_, waiter_scope, key, _, _)| *waiter_scope == scope && !active.contains(key))
        .min_by_key(|(identifier, _, _, priority, entered)| {
            let effective = if entered.elapsed() >= std::time::Duration::from_secs(5) {
                0
            } else {
                *priority
            };
            (effective, *identifier)
        })
}

impl Drop for CodeExplanationRequestWaiter {
    fn drop(&mut self) {
        if let Ok(mut waiters) = WAITERS.lock() {
            waiters.retain(|(identifier, _, _, _, _)| *identifier != self.0);
        }
    }
}

#[derive(Clone, Debug)]
struct ProjectScanCandidate {
    path: project::ProjectPath,
    display_path: SharedString,
    size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectScanMode {
    Incremental,
    ReplaceExisting,
}

#[derive(Default)]
struct ProjectScanState {
    running: bool,
    project_label: SharedString,
    cancelled: Arc<AtomicBool>,
    total_files: usize,
    completed_files: usize,
    cached_units: usize,
    fully_cached_files: usize,
    requested_units: usize,
    skipped_files: usize,
    failures: usize,
    diagnostics: Vec<String>,
}

#[derive(Default)]
pub struct CodeExplanationFileIndex {
    files: HashSet<(gpui::EntityId, project::ProjectPath)>,
}

struct GlobalCodeExplanationFileIndex(gpui::Entity<CodeExplanationFileIndex>);

impl gpui::Global for GlobalCodeExplanationFileIndex {}

pub fn code_explanation_file_index(cx: &mut App) -> gpui::Entity<CodeExplanationFileIndex> {
    if let Some(index) = cx.try_global::<GlobalCodeExplanationFileIndex>() {
        return index.0.clone();
    }
    let index = cx.new(|_| CodeExplanationFileIndex::default());
    cx.set_global(GlobalCodeExplanationFileIndex(index.clone()));
    index
}

impl CodeExplanationFileIndex {
    pub fn contains(
        &self,
        project: &gpui::Entity<project::Project>,
        path: &project::ProjectPath,
    ) -> bool {
        self.files.contains(&(project.entity_id(), path.clone()))
    }
}

fn mark_explained_file(
    project: &gpui::Entity<project::Project>,
    path: project::ProjectPath,
    cx: &mut App,
) {
    let project_id = project.entity_id();
    code_explanation_file_index(cx).update(cx, |index, cx| {
        if index.files.insert((project_id, path)) {
            cx.notify();
        }
    });
}

fn unmark_explained_file(
    project: &gpui::Entity<project::Project>,
    path: &project::ProjectPath,
    cx: &mut App,
) {
    let project_id = project.entity_id();
    code_explanation_file_index(cx).update(cx, |index, cx| {
        if index.files.remove(&(project_id, path.clone())) {
            cx.notify();
        }
    });
}

pub async fn load_code_explanation_file_index(
    project: gpui::Entity<project::Project>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let worktree_ids = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>()
    });
    for worktree_id in worktree_ids {
        load_code_explanation_file_index_for_worktree(project.clone(), worktree_id, cx).await?;
    }
    Ok(())
}

pub async fn load_code_explanation_file_index_for_worktree(
    project: gpui::Entity<project::Project>,
    worktree_id: project::WorktreeId,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let database = project.read_with(cx, |project, cx| {
        let worktree = project
            .worktree_for_id(worktree_id, cx)
            .context(i18n::t!("20bd7c2e0c2d946d"))?;
        let namespace = format!(
            "{:?}:{:?}",
            worktree.read(cx).abs_path(),
            project.remote_connection_options(cx)
        );
        anyhow::Ok(
            paths::data_dir()
                .join("code-explanations")
                .join(format!("{}.sqlite", content_hash(&namespace))),
        )
    })?;
    let files = cx
        .background_spawn(async move { cached_file_paths(&database) })
        .await?;
    cx.update(|cx| {
        for path in files {
            if let Ok(path) = util::rel_path::RelPath::from_unix_str(&path) {
                mark_explained_file(
                    &project,
                    project::ProjectPath {
                        worktree_id,
                        path: path.into(),
                    },
                    cx,
                );
            }
        }
    });
    Ok(())
}

#[derive(Default)]
struct ProjectScanRegistry {
    scans: HashMap<gpui::EntityId, gpui::Entity<ProjectScanState>>,
}

struct GlobalProjectScans(gpui::Entity<ProjectScanRegistry>);

impl gpui::Global for GlobalProjectScans {}

pub struct CodeExplanationRequestPermit {
    scope: gpui::EntityId,
    key: String,
}
impl CodeExplanationRequestPermit {
    fn acquire(scope: gpui::EntityId, key: String, maximum: usize) -> Option<Self> {
        let mut active_requests = ACTIVE_REQUESTS.lock().ok()?;
        let active = active_requests.entry(scope).or_default();
        if active.len() >= maximum || active.contains(&key) {
            return None;
        }
        active.push(key.clone());
        Some(Self { scope, key })
    }
}
impl Drop for CodeExplanationRequestPermit {
    fn drop(&mut self) {
        if let Ok(mut active_requests) = ACTIVE_REQUESTS.lock()
            && let Some(active) = active_requests.get_mut(&self.scope)
            && let Some(index) = active.iter().position(|key| key == &self.key)
        {
            active.remove(index);
            if active.is_empty() {
                active_requests.remove(&self.scope);
            }
        }
    }
}

#[derive(Clone, Debug, RegisterSetting)]
pub struct CodeExplanationSettings {
    pub enabled: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub target_language: String,
    pub max_function_lines: u64,
    pub max_concurrent_requests: usize,
    pub preload_lines: u64,
    pub detailed: bool,
    pub prefer_existing_comments: bool,
    pub cache_persist: bool,
    pub cache_max_bytes: u64,
}

impl Settings for CodeExplanationSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.code_explanations.clone().unwrap_or_default();
        Self {
            enabled: content.enabled.unwrap_or(false),
            provider: content.provider.map(|value| value.0),
            model: content.model.map(|value| value.0),
            target_language: content
                .target_language
                .unwrap_or_else(|| i18n::t!("72726d8818f69306").into()),
            max_function_lines: content.max_function_lines.unwrap_or(500),
            max_concurrent_requests: code_explanation_concurrency(
                content.max_concurrent_requests.unwrap_or(5),
            ),
            preload_lines: content.preload_lines.unwrap_or(100).min(5000),
            detailed: content.detailed.unwrap_or(false),
            prefer_existing_comments: content.prefer_existing_comments.unwrap_or(true),
            cache_persist: content.cache_persist.unwrap_or(true),
            cache_max_bytes: content.cache_max_bytes.unwrap_or(50 * 1024 * 1024),
        }
    }
}

fn code_explanation_concurrency(value: u64) -> usize {
    usize::try_from(value.max(1)).unwrap_or(usize::MAX)
}

#[derive(Default)]
pub(crate) struct ExplanationState {
    pub task: Option<Task<()>>,
    pub deep_task: Option<Task<Result<()>>>,
    pub deep_cancelled: Arc<AtomicBool>,
    pub failed: HashSet<std::ops::Range<usize>>,
    pub progress: (usize, usize),
    pub last_error: Option<SharedString>,
    pub memory_order: std::collections::VecDeque<String>,
    pub blocks: HashSet<crate::CustomBlockId>,
    pub block_anchors: HashMap<crate::CustomBlockId, (multi_buffer::Anchor, SharedString)>,
    pub suspended_blocks: Vec<(multi_buffer::Anchor, SharedString)>,
    pub pending_blocks: Option<Vec<(multi_buffer::Anchor, SharedString)>>,
    pub pending_version: Option<clock::Global>,
    pub generation: u64,
    pub last_view: Option<(u32, u32, clock::Global)>,
    pub completed: HashSet<std::ops::Range<usize>>,
    pub approved: HashSet<std::ops::Range<usize>>,
    pub configuration: String,
    pub prompted: HashSet<std::ops::Range<usize>>,
    pub version: Option<clock::Global>,
    pub syntax_version: usize,
    pub viewport: Option<(u32, u32)>,
    pub busy: bool,
    pub dirty: bool,
    pub refresh_requested: bool,
    pub write_generation: Arc<std::sync::atomic::AtomicU64>,
    pub bypass_cache: bool,
    pub cache_epoch: u64,
    pub memory: std::collections::HashMap<String, SharedString>,
    pub deep_explanation_creases: Vec<crate::CreaseId>,
}

fn project_scan_registry(cx: &mut App) -> gpui::Entity<ProjectScanRegistry> {
    if let Some(registry) = cx.try_global::<GlobalProjectScans>() {
        return registry.0.clone();
    }
    let registry = cx.new(|_| ProjectScanRegistry::default());
    cx.set_global(GlobalProjectScans(registry.clone()));
    registry
}

fn project_scan_state(
    project: &gpui::Entity<project::Project>,
    cx: &mut App,
) -> gpui::Entity<ProjectScanState> {
    let project_id = project.entity_id();
    let registry = project_scan_registry(cx);
    if let Some(state) = registry.read(cx).scans.get(&project_id) {
        return state.clone();
    }
    let state = cx.new(|_| ProjectScanState::default());
    registry.update(cx, |registry, cx| {
        registry.scans.insert(project_id, state.clone());
        cx.notify();
    });
    state
}

fn active_request_count(scope: gpui::EntityId) -> usize {
    ACTIVE_REQUESTS
        .lock()
        .ok()
        .and_then(|requests| requests.get(&scope).map(Vec::len))
        .unwrap_or_default()
}

/// 按设置解析出的提供商与模型组合。
///
/// 上游在 LanguageModel 纯数据化重构中删除了同名类型；私有功能仍需要这个组合。
#[derive(Clone)]
pub struct ConfiguredModel {
    pub provider: Arc<dyn LanguageModelProvider>,
    pub model: LanguageModel,
}

pub fn resolve_model(settings: &CodeExplanationSettings, cx: &App) -> Result<ConfiguredModel> {
    let provider_id = settings
        .provider
        .as_ref()
        .context(i18n::t!("46881ec910558a70"))?;
    let model_id = settings
        .model
        .as_ref()
        .context(i18n::t!("45d2e867aaefbc9b"))?;
    let provider = LanguageModelRegistry::read_global(cx)
        .provider(&LanguageModelProviderId(provider_id.clone().into()))
        .context(i18n::t!("e1d101dd283ff727"))?;
    let model = provider
        .provided_models(cx)
        .into_iter()
        .find(|model| model.id().0.as_ref() == model_id)
        .context(i18n::t!("2a0e0444f8d92052"))?;
    Ok(ConfiguredModel { provider, model })
}

pub fn selected_provider_configuration(settings: &CodeExplanationSettings, cx: &App) -> String {
    let models = &cx
        .global::<settings::SettingsStore>()
        .merged_settings()
        .language_models;
    let Ok(value) = serde_json::to_value(models) else {
        return content_hash(&format!("{models:?}"));
    };
    let Some(provider) = settings.provider.as_deref() else {
        return content_hash("unconfigured");
    };
    let configuration_key = match provider {
        "openrouter" => "open_router",
        "amazon-bedrock" => "bedrock",
        other => other,
    };
    let selected = value
        .get(configuration_key)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    // Compatible providers use their configured name as the registry ID. Include both namespaces
    // to conservatively invalidate when a name changes provider protocol or conflicts with a builtin.
    let openai = value
        .get("openai_compatible")
        .and_then(|providers| providers.get(provider));
    let anthropic = value
        .get("anthropic_compatible")
        .and_then(|providers| providers.get(provider));
    content_hash(&format!("{provider}:{selected}:{openai:?}:{anthropic:?}"))
}

pub fn content_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn preload_range(
    visible: std::ops::Range<u32>,
    preload_lines: u64,
    max_row: u32,
) -> std::ops::RangeInclusive<u32> {
    let preload_lines = preload_lines.min(u32::MAX as u64) as u32;
    visible.start.saturating_sub(preload_lines)
        ..=visible.end.saturating_add(preload_lines).min(max_row)
}

pub(crate) fn code_edited(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor
        .explanations
        .write_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    editor.explanations.task = None;
    editor
        .explanations
        .deep_cancelled
        .store(true, Ordering::SeqCst);
    editor.explanations.deep_task = None;
    editor.explanations.failed.clear();
    editor.explanations.last_error = None;
    editor.explanations.busy = false;
    editor.explanations.dirty = true;
    editor.explanations.refresh_requested = false;
    editor.explanations.generation = editor.explanations.generation.wrapping_add(1);
    editor.explanations.last_view = None;
    editor.explanations.completed.clear();
    editor.explanations.approved.clear();
    editor.explanations.prompted.clear();
    editor.explanations.pending_blocks = None;
    editor.explanations.pending_version = None;
    let creases = std::mem::take(&mut editor.explanations.deep_explanation_creases);
    if !creases.is_empty() {
        editor.remove_creases(creases, cx);
    }
    sync_block_visibility(editor, cx);
}

/// Hides explanation blocks whose anchored text was deleted and restores the ones whose text
/// became visible again, so deleting a line removes its explanation in the same frame and undoing
/// the deletion brings it back without waiting for a full refresh.
fn sync_block_visibility(editor: &mut Editor, cx: &mut Context<Editor>) {
    if editor.explanations.block_anchors.is_empty()
        && editor.explanations.suspended_blocks.is_empty()
    {
        return;
    }
    let snapshot = editor.buffer.read(cx).snapshot(cx);
    let mut removed = HashSet::default();
    for (id, (anchor, _)) in editor.explanations.block_anchors.iter() {
        if !anchor.is_valid(&snapshot) {
            removed.insert(*id);
        }
    }
    let mut suspended = std::mem::take(&mut editor.explanations.suspended_blocks);
    for id in &removed {
        if let Some(block) = editor.explanations.block_anchors.remove(id) {
            suspended.push(block);
        }
        editor.explanations.blocks.remove(id);
    }
    if !removed.is_empty() {
        editor.remove_blocks(removed, None, cx);
    }
    let mut restored = Vec::new();
    let mut still_suspended = Vec::new();
    for (anchor, text) in suspended {
        if anchor.is_valid(&snapshot) {
            restored.push((anchor, text));
        } else {
            still_suspended.push((anchor, text));
        }
    }
    editor.explanations.suspended_blocks = still_suspended;
    for (anchor, text) in restored {
        show(editor, anchor, text, true, cx);
    }
}

fn remove_all_blocks(editor: &mut Editor, cx: &mut Context<Editor>) {
    let blocks = std::mem::take(&mut editor.explanations.blocks);
    editor.explanations.block_anchors.clear();
    editor.explanations.suspended_blocks.clear();
    if !blocks.is_empty() {
        editor.remove_blocks(blocks, None, cx);
    }
}

pub(crate) fn request_refresh(editor: &mut Editor) {
    if editor.explanations.dirty {
        editor.explanations.refresh_requested = true;
        editor.explanations.last_view = None;
    }
}

pub(crate) fn clear(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor
        .explanations
        .write_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    editor.explanations.task = None;
    editor
        .explanations
        .deep_cancelled
        .store(true, Ordering::SeqCst);
    editor.explanations.deep_task = None;
    editor.explanations.failed.clear();
    editor.explanations.last_error = None;
    editor.explanations.progress = (0, 0);
    editor.explanations.busy = false;
    editor.explanations.dirty = false;
    editor.explanations.refresh_requested = false;
    editor.explanations.version = None;
    editor.explanations.generation = editor.explanations.generation.wrapping_add(1);
    editor.explanations.last_view = None;
    editor.explanations.completed.clear();
    editor.explanations.approved.clear();
    editor.explanations.prompted.clear();
    editor.explanations.pending_blocks = None;
    editor.explanations.pending_version = None;
    remove_all_blocks(editor, cx);
    let creases = std::mem::take(&mut editor.explanations.deep_explanation_creases);
    if !creases.is_empty() {
        editor.remove_creases(creases, cx);
    }
}

fn apply_pending(editor: &mut Editor, cx: &mut Context<Editor>) {
    let pending_is_current = editor
        .buffer
        .read(cx)
        .as_singleton()
        .zip(editor.explanations.pending_version.as_ref())
        .is_some_and(|(buffer, version)| buffer.read(cx).snapshot().version() == version);
    if !pending_is_current {
        editor.explanations.pending_blocks = None;
        editor.explanations.pending_version = None;
        return;
    }
    let Some(pending) = editor.explanations.pending_blocks.take() else {
        return;
    };
    editor.explanations.pending_version = None;
    remove_all_blocks(editor, cx);
    for (anchor, text) in pending {
        show(editor, anchor, text, true, cx);
    }
}

fn show(
    editor: &mut Editor,
    anchor: multi_buffer::Anchor,
    text: SharedString,
    track_anchor: bool,
    cx: &mut Context<Editor>,
) {
    let tracked_text = text.clone();
    let ids = editor.insert_blocks(
        [BlockProperties {
            placement: BlockPlacement::Above(anchor),
            // `height: None` would reserve no rows and paint the text over the code below; a
            // reserved row lets the prepaint grow the block to the measured wrapped height.
            height: Some(1),
            style: BlockStyle::Flex,
            priority: 0,
            render: Arc::new(move |cx| {
                div()
                    .w(cx.max_width)
                    .pl(cx.anchor_x)
                    .text_color(cx.theme().status().success.opacity(0.55))
                    .child(text.clone())
                    .into_any_element()
            }),
        }],
        None,
        cx,
    );
    for id in ids {
        if track_anchor {
            editor
                .explanations
                .block_anchors
                .insert(id, (anchor, tracked_text.clone()));
        }
        editor.explanations.blocks.insert(id);
    }
}

pub(crate) fn schedule(editor: &mut Editor, window: &gpui::Window, cx: &mut Context<Editor>) {
    let settings = CodeExplanationSettings::get_global(cx).clone();
    let epoch = CACHE_EPOCH.load(std::sync::atomic::Ordering::SeqCst);
    if editor.explanations.cache_epoch != epoch {
        editor.explanations.memory.clear();
        clear(editor, cx);
        editor.explanations.cache_epoch = epoch;
    }
    if !settings.enabled || project::DisableAiSettings::get_global(cx).disable_ai {
        if editor.explanations.version.is_some() {
            clear(editor, cx);
        }
        return;
    }
    let focused = editor.is_focused(window);
    if focused && editor.explanations.pending_blocks.is_some() {
        apply_pending(editor, cx);
    }
    if editor.explanations.dirty && !editor.explanations.refresh_requested {
        return;
    }
    if !focused && !editor.explanations.refresh_requested && !editor.explanations.busy {
        return;
    }
    let provider_configuration = selected_provider_configuration(&settings, cx);
    let configuration = format!("{settings:?}:{provider_configuration}");
    if editor.explanations.configuration != configuration {
        clear(editor, cx);
        editor.explanations.configuration = configuration;
    }
    let Some(buffer) = editor.buffer.read(cx).as_singleton() else {
        return;
    };
    let snapshot = buffer.read(cx).snapshot();
    editor.explanations.version = Some(snapshot.version().clone());
    if editor.explanations.syntax_version != snapshot.syntax_update_count() {
        editor.explanations.syntax_version = snapshot.syntax_update_count();
        editor.explanations.last_view = None;
    }
    let Some(file) = snapshot.file() else {
        return;
    };
    let filename = file.file_name(cx).to_ascii_lowercase();
    if file.is_private()
        || filename.starts_with(".env")
        || filename.ends_with(".pem")
        || filename.ends_with(".key")
        || filename.ends_with(".min.js")
        || filename.ends_with(".lock")
    {
        clear(editor, cx);
        return;
    }
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let store = project.read(cx).worktree_store();
    let Some(trust) = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx) else {
        return;
    };
    let worktree_id = file.worktree_id(cx);
    if !trust.update(cx, |trust, cx| trust.can_trust(&store, worktree_id, cx)) {
        clear(editor, cx);
        return;
    }
    let display = editor.snapshot(window, cx);
    let visible = editor.multi_buffer_visible_range(&display.display_snapshot, cx);
    let preload = preload_range(
        visible.start.row..visible.end.row,
        settings.preload_lines,
        snapshot.max_point().row,
    );
    let preload_start = *preload.start();
    let preload_end = *preload.end();
    let viewport = (preload_start, preload_end);
    editor.explanations.viewport = Some(viewport);
    let view = (
        visible.start.row,
        visible.end.row,
        snapshot.version().clone(),
    );
    if editor.explanations.last_view.as_ref() == Some(&view) {
        return;
    }
    if editor
        .explanations
        .last_view
        .as_ref()
        .is_some_and(|old| old.2 != view.2)
    {
        clear(editor, cx);
    }
    if editor.explanations.busy {
        return;
    }
    editor.explanations.last_view = Some(view);
    let model = match resolve_model(&settings, cx) {
        Ok(model) => model,
        Err(error) => {
            if editor.explanations.blocks.is_empty() {
                let anchor = display
                    .buffer_snapshot()
                    .anchor_before(multi_buffer::MultiBufferOffset(0));
                show(editor, anchor, format!("AI · {error}").into(), false, cx);
            }
            return;
        }
    };
    let mut ranges = whole_file_unit(&snapshot, settings.max_function_lines)
        .into_iter()
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        let mut row = preload_start;
        while row <= preload_end {
            let candidates = units_at(&snapshot, row);
            let next_row = candidates
                .iter()
                .map(|unit| unit.last_row.saturating_add(1))
                .max()
                .unwrap_or(row as usize + 1);
            for unit in candidates {
                if unit.range.is_empty()
                    || editor.explanations.completed.contains(&unit.range)
                    || ranges
                        .iter()
                        .any(|existing: &crate::code_explanation_units::Unit| {
                            existing.range == unit.range
                        })
                {
                    continue;
                }
                if unit.first_row > preload_end as usize || unit.last_row < preload_start as usize {
                    continue;
                }
                if editor.explanations.prompted.contains(&unit.owner)
                    && !editor.explanations.approved.contains(&unit.owner)
                {
                    continue;
                }
                ranges.push(unit);
            }
            row = next_row.min(u32::MAX as usize) as u32;
        }
    }
    let mut ranges = crate::code_explanation_units::fit_units_to_budget(
        &snapshot,
        ranges,
        crate::code_explanation_units::request_code_budget(model.model.max_token_count()),
        |text| model.model.estimate_tokens(text),
    );
    retain_pending_units(&mut ranges, &editor.explanations.completed);
    retain_pending_units(&mut ranges, &editor.explanations.failed);
    ranges.sort_by_key(|unit| {
        let start = visible.start.row as usize;
        let end = visible.end.row as usize;
        if unit.last_row < start {
            start - unit.last_row
        } else {
            unit.first_row.saturating_sub(end)
        }
    });
    if ranges.is_empty() {
        if crate::code_explanation_units::request_code_budget(model.model.max_token_count()) == 0 {
            editor.explanations.last_error = Some(i18n::t!("8819f59b3f2bb1fe").into());
        }
        return;
    }
    let Some(worktree) = store.read(cx).worktree_for_id(worktree_id, cx) else {
        return;
    };
    let language = snapshot
        .language()
        .map(|language| language.name().to_string())
        .unwrap_or_default();
    let project_path = buffer.read(cx).project_path(cx);
    let cache_namespace = format!(
        "{:?}:{:?}",
        worktree.read(cx).abs_path(),
        project.read(cx).remote_connection_options(cx)
    );
    let bypass_cache = editor.explanations.bypass_cache;
    let refresh_requested = editor.explanations.refresh_requested;
    let approved = editor.explanations.approved.clone();
    let write_generation = editor.explanations.write_generation.clone();
    let expected_write_generation = write_generation.load(std::sync::atomic::Ordering::SeqCst);
    let generation = editor.explanations.generation;
    editor.explanations.busy = true;
    editor.explanations.progress = (0, ranges.len());
    editor.explanations.refresh_requested = false;
    cx.notify();
    editor.explanations.task = Some(cx.spawn(async move |this, cx| {
        let mut pending_blocks = Vec::new();
        cx.background_executor()
            .timer(std::time::Duration::from_millis(500))
            .await;
        for unit in ranges {
            let range = unit.range.clone();
            let code: String = snapshot.text_for_range(range.clone()).collect();
            let anchor = display
                .buffer_snapshot()
                .anchor_before(multi_buffer::MultiBufferOffset(range.start));
            if unit.owner_lines as u64 > settings.max_function_lines
                && !approved.contains(&unit.owner)
            {
                if this
                    .update(cx, |editor, cx| {
                        if editor.explanations.generation != generation {
                            return;
                        }
                        editor.explanations.completed.insert(range.clone());
                        if !editor.explanations.prompted.insert(unit.owner.clone()) {
                            return;
                        }
                        let weak = cx.weak_entity();
                        let range = unit.owner.clone();
                        let limit = settings.max_function_lines;
                        let ids =
                            editor.insert_blocks(
                                [BlockProperties {
                                    placement: BlockPlacement::Above(
                                        display.buffer_snapshot().anchor_before(
                                            multi_buffer::MultiBufferOffset(unit.owner.start),
                                        ),
                                    ),
                                    height: Some(2),
                                    style: BlockStyle::Flex,
                                    priority: 0,
                                    render: Arc::new(move |cx| {
                                        let weak = weak.clone();
                                        let range = range.clone();
                                        h_flex()
                                            .pl(cx.anchor_x)
                                            .child(Label::new(i18n::t!(
                                                "f3b53661cd53289c",
                                                limit = limit
                                            )))
                                            .child(
                                                Button::new(
                                                    "explain-large-function",
                                                    i18n::t!("084f01f3ce8feeec"),
                                                )
                                                .on_click(move |_, _, cx| {
                                                    use util::ResultExt as _;
                                                    weak.update(cx, |editor, cx| {
                                                        remove_all_blocks(editor, cx);
                                                        editor
                                                            .explanations
                                                            .approved
                                                            .insert(range.clone());
                                                        editor.explanations.completed.clear();
                                                        editor.explanations.prompted.clear();
                                                        editor.explanations.last_view = None;
                                                        cx.notify();
                                                    })
                                                    .log_err();
                                                }),
                                            )
                                            .into_any_element()
                                    }),
                                }],
                                None,
                                cx,
                            );
                        editor.explanations.blocks.extend(ids);
                    })
                    .is_err()
                {
                    break;
                }
                continue;
            }
            let key = format!(
                "v6:{provider_configuration}:{:?}:{:?}:{}:{}:{}",
                settings.provider,
                settings.model,
                settings.target_language,
                settings.detailed,
                content_hash(&format!("{language}\n{}\n{}", unit.context, code))
            );
            let cache_path = paths::data_dir()
                .join("code-explanations")
                .join(format!("{}.sqlite", content_hash(&cache_namespace)));
            let memory = this
                .update(cx, |editor, _| {
                    let value = editor.explanations.memory.get(&key).cloned();
                    if value.is_some() {
                        editor
                            .explanations
                            .memory_order
                            .retain(|existing| existing != &key);
                        editor.explanations.memory_order.push_back(key.clone());
                    }
                    value
                })
                .ok()
                .flatten();
            let cached = if bypass_cache {
                None
            } else if let Some(text) = memory {
                Some(text.to_string())
            } else if settings.cache_persist {
                let path = cache_path.clone();
                let key = key.clone();
                cx.background_spawn(async move { cache_access(&path, &key, None, 0) })
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let request_key = format!("{cache_namespace}:{key}");
            let permit = if cached.is_none() {
                let waiting = match CodeExplanationRequestWaiter::new(
                    project.entity_id(),
                    request_key.clone(),
                    1,
                ) {
                    Ok(waiting) => waiting,
                    Err(error) => {
                        log::error!("{error}");
                        return;
                    }
                };
                loop {
                    if let Some(permit) = waiting.acquire(settings.max_concurrent_requests) {
                        this.update(cx, |_, cx| cx.notify()).ok();
                        break Some(permit);
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(200))
                        .await;
                    if this
                        .read_with(cx, |editor, _| editor.explanations.generation != generation)
                        .unwrap_or(true)
                    {
                        return;
                    }
                }
            } else {
                None
            };
            let cached = if !bypass_cache && cached.is_none() {
                shared_result(&format!("{epoch}:{request_key}"), None).map(|text| text.to_string())
            } else {
                cached
            };
            let cached = if !bypass_cache && cached.is_none() && settings.cache_persist {
                let path = cache_path.clone();
                let key = key.clone();
                cx.background_spawn(async move { cache_access(&path, &key, None, 0) })
                    .await
                    .ok()
                    .flatten()
            } else {
                cached
            };
            // Cache I/O and admission waits can outlive the authorization snapshot.
            let authorized = |cx: &mut App| {
                this.read_with(cx, |editor, cx| {
                    editor.explanations.generation == generation
                        && editor.project().is_some_and(|current| current == &project)
                        && editor.buffer.read(cx).as_singleton().as_ref() == Some(&buffer)
                })
                .unwrap_or(false)
                    && CodeExplanationSettings::get_global(cx).enabled
                    && !project::DisableAiSettings::get_global(cx).disable_ai
                    && format!("{:?}", CodeExplanationSettings::get_global(cx))
                        == format!("{settings:?}")
                    && selected_provider_configuration(&settings, cx) == provider_configuration
                    && buffer.read(cx).snapshot().version() == snapshot.version()
                    && buffer.read(cx).file().zip(snapshot.file()).is_some_and(
                        |(current, original)| {
                            Arc::ptr_eq(current, original) && !current.is_private()
                        },
                    )
                    && trust.update(cx, |trust, cx| trust.can_trust(&store, worktree_id, cx))
            };
            let result = match cached {
                Some(text) => Ok(text.into()),
                None => {
                    request_if_authorized(
                        authorized,
                        model.clone(),
                        settings.clone(),
                        i18n::t_mix!("8e667b4105a98ce1"; unit.context, code.lines()
                                .enumerate()
                                .map(|(index, line)| format!("{}: {line}", index + 1))
                                .collect::<Vec<_>>()
                                .join("\n"); language = language),
                        cx,
                    )
                    .await
                }
            };
            if !cx.update(authorized) {
                break;
            }
            let result = result.and_then(|text| {
                parse_annotations(&text, &code, &[])?;
                shared_result(&format!("{epoch}:{request_key}"), Some(text.clone()));
                Ok(text)
            });
            if settings.cache_persist
                && epoch == CACHE_EPOCH.load(std::sync::atomic::Ordering::SeqCst)
                && let Ok(text) = &result
            {
                let text = text.to_string();
                let key = key.clone();
                let budget = settings.cache_max_bytes;
                let write_generation = write_generation.clone();
                let cache_path_for_write = cache_path.clone();
                if let Err(error) = cx
                    .background_spawn(async move {
                        cache_access_guarded(
                            &cache_path_for_write,
                            &key,
                            Some(&text),
                            budget,
                            || {
                                write_generation.load(std::sync::atomic::Ordering::SeqCst)
                                    == expected_write_generation
                                    && CACHE_EPOCH.load(std::sync::atomic::Ordering::SeqCst)
                                        == epoch
                            },
                        )
                    })
                    .await
                {
                    log::warn!("代码讲解缓存写入失败：{error}");
                }
            }
            if result.is_ok()
                && let Some(project_path) = &project_path
            {
                if settings.cache_persist {
                    let cache_path = cache_path.clone();
                    let file_path = project_path.path.as_unix_str().to_owned();
                    let key = key.clone();
                    if let Err(error) = cx
                        .background_spawn(
                            async move { cache_mark_file(&cache_path, &file_path, &key) },
                        )
                        .await
                    {
                        log::warn!("代码讲解文件标记写入失败：{error}");
                    }
                }
                cx.update(|cx| mark_explained_file(&project, project_path.clone(), cx));
            }
            drop(permit);
            this.update(cx, |_, cx| cx.notify()).ok();
            if !cx.update(authorized) {
                break;
            }
            let text = match result {
                Ok(text) => text,
                Err(error) => {
                    this.update(cx, |editor, cx| {
                        if editor.explanations.generation == generation {
                            editor.explanations.failed.insert(range.clone());
                            editor.explanations.last_error =
                                Some(i18n::t!("581dc8b3a4819331", error = error).into());
                            editor.explanations.progress.0 += 1;
                            cx.notify();
                        }
                    })
                    .log_err();
                    continue;
                }
            };
            let annotations = parse_annotations(
                &text,
                &code,
                if settings.prefer_existing_comments {
                    &unit.commented_rows
                } else {
                    &[]
                },
            );
            if let Ok(annotations) = &annotations {
                for annotation in annotations {
                    let row = unit.first_row + annotation.line - 1;
                    let column = code
                        .lines()
                        .nth(annotation.line - 1)
                        .map(first_non_whitespace_column)
                        .unwrap_or(0);
                    let anchor = display
                        .buffer_snapshot()
                        .anchor_after(language::Point::new(row as u32, column as u32));
                    pending_blocks.push((anchor, annotation.explanation.clone().into()));
                }
            } else if let Err(error) = &annotations {
                pending_blocks.push((anchor, format!("AI · {error}").into()));
            }
            if this
                .update(cx, |editor, cx| {
                    if editor.explanations.generation == generation
                        && CodeExplanationSettings::get_global(cx).enabled
                        && !project::DisableAiSettings::get_global(cx).disable_ai
                        && buffer.read(cx).snapshot().version() == snapshot.version()
                    {
                        editor
                            .explanations
                            .memory_order
                            .retain(|existing| existing != &key);
                        editor.explanations.memory_order.push_back(key.clone());
                        while editor.explanations.memory_order.len() > 128 {
                            if let Some(oldest) = editor.explanations.memory_order.pop_front() {
                                editor.explanations.memory.remove(&oldest);
                            }
                        }
                        editor.explanations.memory.insert(key.clone(), text.clone());
                        editor.explanations.completed.insert(range);
                        editor.explanations.progress.0 += 1;
                        if !refresh_requested {
                            for (anchor, text) in pending_blocks.drain(..) {
                                show(editor, anchor, text, true, cx);
                            }
                        }
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        }
        use util::ResultExt as _;
        this.update(cx, |editor, cx| {
            if editor.explanations.generation == generation {
                editor.explanations.last_view = None;
                editor.explanations.busy = false;
                editor.explanations.bypass_cache = false;
                if refresh_requested && editor.explanations.failed.is_empty() {
                    editor.explanations.dirty = false;
                    editor.explanations.pending_blocks = Some(pending_blocks);
                    editor.explanations.pending_version = Some(snapshot.version().clone());
                } else if refresh_requested {
                    // Keep the old visible batch when any replacement failed; retry must rebuild all staged units.
                    editor.explanations.completed.clear();
                    editor.explanations.dirty = true;
                } else {
                    for (anchor, text) in pending_blocks {
                        show(editor, anchor, text, true, cx);
                    }
                }
                cx.notify();
            }
        })
        .log_err();
    }));
}

fn retain_pending_units(
    units: &mut Vec<crate::code_explanation_units::Unit>,
    completed: &HashSet<std::ops::Range<usize>>,
) {
    units.retain(|unit| !completed.contains(&unit.range));
}

fn cached_file_paths(path: &std::path::Path) -> Result<Vec<String>> {
    use db::sqlez::{connection::Connection, statement::Statement};
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let _guard = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(i18n::t!("cb2247f24b40dc8a")))?;
    let connection = Connection::open_file(path.to_str().context(i18n::t!("234b05fe0301a9a7"))?);
    if !connection.persistent() {
        return Ok(Vec::new());
    }
    let exists = Statement::prepare(
        &connection,
        "SELECT name FROM sqlite_master WHERE type='table' AND name='explained_files'",
    )?
    .rows::<String>()?
    .into_iter()
    .next()
    .is_some();
    if !exists {
        return Ok(Vec::new());
    }
    Statement::prepare(
        &connection,
        "SELECT DISTINCT explained_files.path FROM explained_files INNER JOIN explanations ON explanations.key = explained_files.key",
    )?
    .rows::<String>()
}

fn cache_contains_complete_file(
    path: &std::path::Path,
    file_path: &str,
    keys: &[String],
) -> Result<bool> {
    use db::sqlez::{connection::Connection, statement::Statement};
    if keys.is_empty() || !path.is_file() {
        return Ok(false);
    }
    let _guard = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(i18n::t!("cb2247f24b40dc8a")))?;
    let connection = Connection::open_file(path.to_str().context(i18n::t!("234b05fe0301a9a7"))?);
    if !connection.persistent() {
        return Ok(false);
    }
    let tables = Statement::prepare(
        &connection,
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('explanations', 'explained_files')",
    )?
    .rows::<i64>()?
    .into_iter()
    .next()
    .unwrap_or_default();
    if tables != 2 {
        return Ok(false);
    }
    let mut select = Statement::prepare(
        &connection,
        "SELECT EXISTS(SELECT 1 FROM explanations INNER JOIN explained_files ON explanations.key = explained_files.key WHERE explanations.key = ?1 AND explained_files.path = ?2)",
    )?;
    for key in keys {
        select.bind_text(1, key)?;
        select.bind_text(2, file_path)?;
        if select.rows::<i64>()?.into_iter().next() != Some(1) {
            return Ok(false);
        }
        select.reset();
    }
    Ok(true)
}

fn cache_remove_file(path: &std::path::Path, file_path: &str) -> Result<()> {
    use db::sqlez::{connection::Connection, statement::Statement};
    if !path.is_file() {
        return Ok(());
    }
    let _guard = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(i18n::t!("cb2247f24b40dc8a")))?;
    let connection = Connection::open_file(path.to_str().context(i18n::t!("234b05fe0301a9a7"))?);
    if !connection.persistent() {
        return Ok(());
    }
    let exists = Statement::prepare(
        &connection,
        "SELECT name FROM sqlite_master WHERE type='table' AND name='explained_files'",
    )?
    .rows::<String>()?
    .into_iter()
    .next()
    .is_some();
    if !exists {
        return Ok(());
    }
    let mut delete_markers =
        Statement::prepare(&connection, "DELETE FROM explained_files WHERE path = ?1")?;
    delete_markers.bind_text(1, file_path)?;
    delete_markers.exec()?;
    Statement::prepare(
        &connection,
        "DELETE FROM explanations WHERE key NOT IN (SELECT DISTINCT key FROM explained_files)",
    )?
    .exec()?;
    Ok(())
}

fn cache_mark_file(path: &std::path::Path, file_path: &str, key: &str) -> Result<()> {
    use db::sqlez::{connection::Connection, statement::Statement};
    let _guard = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(i18n::t!("cb2247f24b40dc8a")))?;
    let connection = Connection::open_file(path.to_str().context(i18n::t!("234b05fe0301a9a7"))?);
    anyhow::ensure!(connection.persistent(), i18n::t!("f2008897bda07aa8"));
    Statement::prepare(&connection, "CREATE TABLE IF NOT EXISTS explained_files (path TEXT NOT NULL, key TEXT NOT NULL, PRIMARY KEY(path, key))")?.exec()?;
    let mut insert = Statement::prepare(
        &connection,
        "INSERT OR REPLACE INTO explained_files VALUES (?1, ?2)",
    )?;
    insert.bind_text(1, file_path)?;
    insert.bind_text(2, key)?;
    insert.exec()?;
    Ok(())
}

fn cache_access(
    path: &std::path::Path,
    key: &str,
    value: Option<&str>,
    budget: u64,
) -> Result<Option<String>> {
    cache_access_guarded(path, key, value, budget, || true)
}

fn cache_access_guarded(
    path: &std::path::Path,
    key: &str,
    value: Option<&str>,
    budget: u64,
    authorized: impl FnOnce() -> bool,
) -> Result<Option<String>> {
    use db::sqlez::{connection::Connection, statement::Statement};
    let _guard = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(i18n::t!("cb2247f24b40dc8a")))?;
    if !authorized() {
        return Ok(None);
    }
    std::fs::create_dir_all(path.parent().context("缓存路径无效")?)?;
    let connection = Connection::open_file(path.to_str().context(i18n::t!("234b05fe0301a9a7"))?);
    anyhow::ensure!(connection.persistent(), i18n::t!("f2008897bda07aa8"));
    Statement::prepare(&connection, "PRAGMA busy_timeout=2000")?.exec()?;
    Statement::prepare(&connection, "PRAGMA auto_vacuum=FULL")?.exec()?;
    Statement::prepare(&connection, "CREATE TABLE IF NOT EXISTS explanations (key TEXT PRIMARY KEY, value TEXT NOT NULL, touched INTEGER NOT NULL)")?.exec()?;
    if let Some(value) = value {
        if value.len() as u64 > budget {
            return Ok(None);
        }
        let mut insert = Statement::prepare(
            &connection,
            "INSERT OR REPLACE INTO explanations VALUES (?1, ?2, unixepoch())",
        )?;
        insert.bind_text(1, key)?;
        insert.bind_text(2, value)?;
        insert.exec()?;
        let mut prune = Statement::prepare(
            &connection,
            "DELETE FROM explanations WHERE key IN (SELECT key FROM (SELECT key, SUM(length(CAST(value AS BLOB))) OVER (ORDER BY touched DESC, rowid DESC) AS total FROM explanations) WHERE total > ?1)",
        )?;
        prune.bind_int64(1, budget.min(i64::MAX as u64) as i64)?;
        prune.exec()?;
        drop(prune);
        drop(insert);
        drop(connection);
        trim_global_cache(
            path.parent().context(i18n::t!("02b853403b499769"))?,
            path,
            500 * 1024 * 1024,
        )?;
        return Ok(None);
    }
    let mut select =
        Statement::prepare(&connection, "SELECT value FROM explanations WHERE key = ?1")?;
    select.bind_text(1, key)?;
    let result = select.rows::<String>()?.into_iter().next();
    drop(select);
    if result.is_some() {
        let mut touch = Statement::prepare(
            &connection,
            "UPDATE explanations SET touched=unixepoch() WHERE key=?1",
        )?;
        touch.bind_text(1, key)?;
        touch.exec()?;
    }
    Ok(result)
}

fn trim_global_cache(
    directory: &std::path::Path,
    current: &std::path::Path,
    budget: u64,
) -> Result<()> {
    let mut entries = Vec::new();
    let mut size = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "sqlite")
        {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        size = size.saturating_add(metadata.len());
        entries.push((metadata.modified()?, metadata.len(), path));
    }
    entries.sort_by_key(|entry| entry.0);
    for (_, bytes, path) in entries {
        if size <= budget {
            break;
        }
        if path == current {
            continue;
        }
        std::fs::remove_file(path)?;
        size = size.saturating_sub(bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explanation_concurrency_has_no_configured_maximum() {
        assert_eq!(code_explanation_concurrency(0), 1);
        assert_eq!(code_explanation_concurrency(100), 100);
    }

    #[gpui::test]
    async fn model_request_is_read_only_and_structured(cx: &mut gpui::TestAppContext) {
        use language_model::fake_provider::FakeLanguageModelProvider;
        crate::editor_tests::init_test(cx, |_| {});
        let provider = Arc::new(FakeLanguageModelProvider::new(
            LanguageModelProviderId::from("fake".to_string()),
            language_model::LanguageModelProviderName::from("Fake".to_string()),
        ));
        let model = provider.model("fake");
        let configured = ConfiguredModel {
            provider: provider.clone(),
            model: model.clone(),
        };
        let task = cx.spawn(async move |mut cx| {
            let settings = cx.update(|cx| CodeExplanationSettings::get_global(cx).clone());
            request(configured, settings, "1: let answer = 42;".into(), &mut cx).await
        });
        cx.run_until_parked();
        let requests = provider.pending_completions();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].messages[0]
                .string_contents()
                .contains("只输出JSON数组")
        );
        provider.send_last_text(&model, r#"[{"line":1,"explanation":"保存答案"}]"#);
        provider.end_last(&model);
        let text = task.await.unwrap();
        assert_eq!(
            parse_annotations(&text, "let answer = 42;", &[])
                .unwrap()
                .len(),
            1
        );
    }

    #[gpui::test]
    async fn deep_request_accepts_default_detail_and_streams(cx: &mut gpui::TestAppContext) {
        use language_model::fake_provider::FakeLanguageModelProvider;
        crate::editor_tests::init_test(cx, |_| {});
        let provider = Arc::new(FakeLanguageModelProvider::new(
            LanguageModelProviderId::from("fake".to_string()),
            language_model::LanguageModelProviderName::from("Fake".to_string()),
        ));
        let model = provider.model("fake");
        let configured = ConfiguredModel {
            provider: provider.clone(),
            model: model.clone(),
        };
        let task = cx.spawn(async move |mut cx| {
            let settings = cx.update(|cx| CodeExplanationSettings::get_global(cx).clone());
            assert!(!settings.detailed);
            request_deep(
                configured,
                settings,
                "let value = 1;".into(),
                |_, _| Ok(()),
                &mut cx,
            )
            .await
        });
        cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);
        provider.send_last_text(&model, "## 目的\n保存数值");
        provider.end_last(&model);
        assert_eq!(task.await.unwrap().as_ref(), "## 目的\n保存数值");
    }

    #[gpui::test]
    async fn disabled_ai_rejects_model_request(cx: &mut gpui::TestAppContext) {
        use language_model::fake_provider::FakeLanguageModelProvider;
        crate::editor_tests::init_test(cx, |_| {});
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        r#"{"disable_ai":true,"code_explanations":{"enabled":true}}"#,
                        cx,
                    )
                    .unwrap();
            });
        });
        let provider = Arc::new(FakeLanguageModelProvider::new(
            LanguageModelProviderId::from("fake".to_string()),
            language_model::LanguageModelProviderName::from("Fake".to_string()),
        ));
        let model = provider.model("fake");
        let configured = ConfiguredModel {
            provider: provider.clone(),
            model: model.clone(),
        };
        let task = cx.spawn(async move |mut cx| {
            let settings = cx.update(|cx| CodeExplanationSettings::get_global(cx).clone());
            request(configured, settings, "secret source".into(), &mut cx).await
        });
        assert!(task.await.is_err());
        assert!(provider.pending_completions().is_empty());
    }

    #[gpui::test]
    async fn revoked_authorization_after_cache_wait_never_calls_model(
        cx: &mut gpui::TestAppContext,
    ) {
        use language_model::fake_provider::FakeLanguageModelProvider;
        crate::editor_tests::init_test(cx, |_| {});
        let provider = Arc::new(FakeLanguageModelProvider::new(
            LanguageModelProviderId::from("fake".to_string()),
            language_model::LanguageModelProviderName::from("Fake".to_string()),
        ));
        let model = provider.model("fake");
        let configured = ConfiguredModel {
            provider: provider.clone(),
            model: model.clone(),
        };
        let allowed = std::rc::Rc::new(std::cell::Cell::new(true));
        let (resume, waiting) = futures::channel::oneshot::channel::<()>();
        let task = cx.spawn({
            let allowed = allowed.clone();
            async move |mut cx| {
                let settings = cx.update(|cx| CodeExplanationSettings::get_global(cx).clone());
                assert!(allowed.get());
                waiting.await.unwrap();
                request_if_authorized(
                    |_| allowed.get(),
                    configured,
                    settings,
                    "private code".into(),
                    &mut cx,
                )
                .await
            }
        });
        cx.run_until_parked();
        allowed.set(false);
        resume.send(()).unwrap();
        assert!(task.await.is_err());
        assert!(provider.pending_completions().is_empty());
    }

    #[test]
    fn sqlite_round_trip_and_eviction() {
        let directory = util::test::TempTree::new(serde_json::json!({}));
        let path = directory.path().join("cache.sqlite");
        assert_eq!(cache_access(&path, "missing", None, 0).unwrap(), None);
        cache_access(&path, "a", Some("first"), 100).unwrap();
        cache_mark_file(&path, "src/main.rs", "a").unwrap();
        assert_eq!(
            cache_access(&path, "a", None, 0).unwrap().as_deref(),
            Some("first")
        );
        assert!(cache_contains_complete_file(&path, "src/main.rs", &["a".to_owned()]).unwrap());
        assert!(
            !cache_contains_complete_file(&path, "src/main.rs", &["a".to_owned(), "b".to_owned()])
                .unwrap()
        );
        cache_access(&path, "b", Some("second"), 6).unwrap();
        cache_mark_file(&path, "src/main.rs", "b").unwrap();
        assert_eq!(
            cache_access(&path, "b", None, 0).unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(cache_access(&path, "a", None, 0).unwrap(), None);
        assert!(
            !cache_contains_complete_file(&path, "src/main.rs", &["a".to_owned(), "b".to_owned()])
                .unwrap()
        );
    }

    #[test]
    fn removing_file_cache_preserves_shared_explanations() {
        let directory = util::test::TempTree::new(serde_json::json!({}));
        let path = directory.path().join("cache.sqlite");
        cache_access(
            &path,
            "selected-only",
            Some("old selected explanation"),
            1024,
        )
        .unwrap();
        cache_access(&path, "shared", Some("shared explanation"), 1024).unwrap();
        cache_mark_file(&path, "src/selected.rs", "selected-only").unwrap();
        cache_mark_file(&path, "src/selected.rs", "shared").unwrap();
        cache_mark_file(&path, "src/other.rs", "shared").unwrap();

        cache_remove_file(&path, "src/selected.rs").unwrap();

        assert!(
            cached_file_paths(&path)
                .unwrap()
                .contains(&"src/other.rs".to_owned())
        );
        assert!(
            !cached_file_paths(&path)
                .unwrap()
                .contains(&"src/selected.rs".to_owned())
        );
        assert_eq!(cache_access(&path, "selected-only", None, 0).unwrap(), None);
        assert_eq!(
            cache_access(&path, "shared", None, 0).unwrap().as_deref(),
            Some("shared explanation")
        );
    }

    #[test]
    fn cancelled_cache_write_does_not_create_database() {
        let directory = util::test::TempTree::new(serde_json::json!({}));
        let path = directory.path().join("cancelled.sqlite");
        cache_access_guarded(&path, "key", Some("private explanation"), 100, || false).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn completed_whole_file_units_are_not_scheduled_again() {
        let mut completed = HashSet::default();
        let unit = || crate::code_explanation_units::Unit {
            range: 0..100,
            owner: 0..100,
            owner_lines: 5,
            first_row: 0,
            last_row: 4,
            context: String::new(),
            commented_rows: Vec::new(),
        };
        let mut units = vec![unit()];
        retain_pending_units(&mut units, &completed);
        assert_eq!(units.len(), 1);
        completed.insert(0..100);
        for _ in 0..10 {
            let mut units = vec![unit()];
            retain_pending_units(&mut units, &completed);
            assert!(units.is_empty());
        }
        completed.clear();
        let mut units = vec![unit()];
        retain_pending_units(&mut units, &completed);
        assert_eq!(units.len(), 1);
    }

    #[test]
    fn selected_scan_paths_include_files_and_recursive_directories() {
        let worktree = project::WorktreeId::from_proto(1);
        let other_worktree = project::WorktreeId::from_proto(2);
        let selected = vec![
            project::ProjectPath {
                worktree_id: worktree,
                path: util::rel_path::RelPath::from_unix_str("src")
                    .unwrap()
                    .into(),
            },
            project::ProjectPath {
                worktree_id: worktree,
                path: util::rel_path::RelPath::from_unix_str("exact.rs")
                    .unwrap()
                    .into(),
            },
        ];
        let path = |value| util::rel_path::RelPath::from_unix_str(value).unwrap();
        assert!(path_is_within_scan_selection(
            worktree,
            path("src/deep/module.rs"),
            Some(&selected)
        ));
        assert!(path_is_within_scan_selection(
            worktree,
            path("exact.rs"),
            Some(&selected)
        ));
        assert!(!path_is_within_scan_selection(
            worktree,
            path("exact.rs.bak"),
            Some(&selected)
        ));
        assert!(!path_is_within_scan_selection(
            other_worktree,
            path("src/deep/module.rs"),
            Some(&selected)
        ));
        assert!(path_is_within_scan_selection(
            worktree,
            path("any.rs"),
            None
        ));
    }

    #[test]
    fn project_scan_filters_sensitive_and_non_source_paths() {
        assert!(!scan_git_status_allowed(git::status::FileStatus::Untracked));
        assert!(!scan_git_status_allowed(git::status::FileStatus::Ignored));
        use util::rel_path::rel_path;

        assert!(scan_source_extension(rel_path("src/main.ts")));
        assert!(!scan_source_extension(rel_path("config.json")));
        assert!(scan_path_is_sensitive(rel_path(
            "node_modules/pkg/index.js"
        )));
        assert!(scan_path_is_sensitive(rel_path("src/generated.min.js")));
        assert!(scan_path_is_sensitive(rel_path("secrets.key")));
        assert!(!scan_path_is_sensitive(rel_path("src/main.ts")));
    }

    #[test]
    fn preload_expands_both_sides_and_clamps_to_buffer() {
        assert_eq!(preload_range(120..140, 100, 500), 20..=240);
        assert_eq!(preload_range(20..40, 100, 80), 0..=80);
    }

    #[test]
    fn retry_only_explicit_rejections_with_bounded_delay() {
        use language_model::{LanguageModelCompletionError, LanguageModelProviderName};
        let rejection = |status: u16, delay| {
            LanguageModelCompletionError::from_http_status(
                LanguageModelProviderName("test".into()),
                status.to_string().parse().unwrap(),
                "rejected".into(),
                delay,
            )
        };
        assert!(rejected_request_retry_delay(&rejection(429, None)).is_some());
        assert!(rejected_request_retry_delay(&rejection(503, None)).is_some());
        assert!(rejected_request_retry_delay(&rejection(401, None)).is_none());
        assert!(rejected_request_retry_delay(&rejection(500, None)).is_none());
        assert!(
            rejected_request_retry_delay(&rejection(429, Some(std::time::Duration::from_secs(60))))
                .is_none()
        );
        assert!(
            rejected_request_retry_delay(&LanguageModelCompletionError::Other(anyhow::anyhow!(
                "connection reset"
            )))
            .is_none()
        );
    }

    #[test]
    fn shared_results_are_scoped_and_reused_without_disk() {
        let key = "test-shared-project:model:source";
        assert_eq!(
            shared_result(key, Some("result".into())).as_deref(),
            Some("result")
        );
        assert_eq!(shared_result(key, None).as_deref(), Some("result"));
        assert!(shared_result("other-project:model:source", None).is_none());
        assert!(shared_result("new-epoch:test-shared-project:model:source", None).is_none());
    }

    #[gpui::test]
    fn selected_provider_identity_ignores_unrelated_providers(cx: &mut gpui::TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});
        cx.update(|cx| {
            let set = |value: &str, cx: &mut App| cx.update_global::<settings::SettingsStore, _>(|store, cx| store.set_user_settings(value, cx).unwrap());
            set(r#"{"code_explanations":{"provider":"openai","detailed":true},"language_models":{"openai":{"api_url":"https://one.example/v1"}}}"#, cx);
            let settings = CodeExplanationSettings::get_global(cx).clone();
            assert!(settings.detailed);
            let before = selected_provider_configuration(&settings, cx);
            set(r#"{"code_explanations":{"provider":"openai","detailed":true},"language_models":{"openai":{"api_url":"https://one.example/v1"},"anthropic":{"api_url":"https://other.example"}}}"#, cx);
            assert_eq!(before, selected_provider_configuration(&settings, cx));
            set(r#"{"code_explanations":{"provider":"openai"},"language_models":{"openai":{"api_url":"https://two.example/v1"}}}"#, cx);
            assert_ne!(before, selected_provider_configuration(&settings, cx));
        });
    }

    #[test]
    fn queue_prioritizes_interaction_ages_background_and_skips_occupied_keys() {
        let now = std::time::Instant::now();
        let first_scope = gpui::EntityId::from(1);
        let second_scope = gpui::EntityId::from(2);
        let mut waiters = vec![
            (1, first_scope, "scan".into(), 2, now),
            (2, first_scope, "visible".into(), 1, now),
            (3, first_scope, "deep".into(), 0, now),
            (4, second_scope, "other-project".into(), 0, now),
        ];
        assert_eq!(next_waiter(&waiters, first_scope, &[]).unwrap().0, 3);
        assert_eq!(
            next_waiter(&waiters, first_scope, &["deep".into()])
                .unwrap()
                .0,
            2
        );
        assert_eq!(next_waiter(&waiters, second_scope, &[]).unwrap().0, 4);
        waiters[0].4 = now - std::time::Duration::from_secs(6);
        assert_eq!(next_waiter(&waiters, first_scope, &[]).unwrap().0, 1);
        assert!(next_waiter(&[], first_scope, &[]).is_none());
    }

    #[test]
    fn public_admission_counts_and_limits_requests_per_project() {
        let scope = gpui::EntityId::from(9_001);
        let first = CodeExplanationRequestWaiter::new(scope, "source".into(), 1).unwrap();
        let second = CodeExplanationRequestWaiter::new(scope, "git-diff".into(), 1).unwrap();
        let first_permit = first.acquire(1).expect("first request should be admitted");
        assert_eq!(active_request_count(scope), 1);
        assert!(second.acquire(1).is_none());
        drop(first);
        drop(first_permit);
        let second_permit = second
            .acquire(1)
            .expect("Git Diff should share the released project slot");
        assert_eq!(active_request_count(scope), 1);
        drop(second_permit);
        assert_eq!(active_request_count(scope), 0);
    }

    #[test]
    fn cache_hash_is_stable_and_content_sensitive() {
        assert_eq!(
            content_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    #[gpui::test]
    async fn virtual_annotations_do_not_change_buffer(cx: &mut gpui::TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});
        let mut context = crate::test::editor_test_context::EditorTestContext::new(cx).await;
        context.set_state("fn example() {ˇ}\n");
        context.update_editor(|editor, _, cx| {
            let before = editor.text(cx);
            let buffer = editor.buffer.read(cx).snapshot(cx);
            let anchor = buffer.anchor_before(multi_buffer::MultiBufferOffset(0));
            show(editor, anchor, "AI · 临时讲解".into(), true, cx);
            assert_eq!(editor.text(cx), before);
            assert!(!editor.explanations.blocks.is_empty());
            clear(editor, cx);
            assert!(editor.explanations.blocks.is_empty());
            assert_eq!(editor.text(cx), before);
        });
    }

    #[gpui::test]
    async fn deleting_an_annotated_line_hides_and_undo_restores_its_block(
        cx: &mut gpui::TestAppContext,
    ) {
        crate::editor_tests::init_test(cx, |_| {});
        let mut context = crate::test::editor_test_context::EditorTestContext::new(cx).await;
        context.cx.update(|_, cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(r#"{"code_explanations":{"enabled":true}}"#, cx)
                    .unwrap();
            });
        });
        context.set_state("fn example() {\n    let value = 1;\n}\nˇ");
        context.update_editor(|editor, window, cx| {
            let version = editor
                .buffer
                .read(cx)
                .as_singleton()
                .expect("讲解测试需要单文件编辑器")
                .read(cx)
                .snapshot()
                .version()
                .clone();
            editor.explanations.version = Some(version);
            let display = editor.snapshot(window, cx);
            let anchor = display
                .buffer_snapshot()
                .anchor_after(language::Point::new(1, 4));
            show(editor, anchor, "AI · 保存数值".into(), true, cx);
            assert_eq!(editor.explanations.blocks.len(), 1);
            assert_eq!(editor.explanations.block_anchors.len(), 1);
        });

        let snapshot = context.buffer_snapshot();
        let line_start = snapshot.point_to_offset(language::Point::new(1, 0));
        let line_end = snapshot.point_to_offset(language::Point::new(2, 0));
        context.update_editor(|editor, _, cx| {
            editor.buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    [(
                        multi_buffer::MultiBufferOffset(line_start)
                            ..multi_buffer::MultiBufferOffset(line_end),
                        "",
                    )],
                    None,
                    cx,
                );
            });
            code_edited(editor, cx);
        });
        context.run_until_parked();
        context.editor(|editor, _, _| {
            assert!(editor.explanations.blocks.is_empty());
            assert!(editor.explanations.block_anchors.is_empty());
            assert_eq!(editor.explanations.suspended_blocks.len(), 1);
        });

        context.update_editor(|editor, window, cx| {
            editor.undo(&crate::Undo, window, cx);
            code_edited(editor, cx);
        });
        context.run_until_parked();
        context.editor(|editor, _, _| {
            assert_eq!(editor.explanations.blocks.len(), 1);
            assert_eq!(editor.explanations.block_anchors.len(), 1);
            assert!(editor.explanations.suspended_blocks.is_empty());
        });
    }

    #[gpui::test]
    async fn virtual_annotation_blocks_reserve_rows(cx: &mut gpui::TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});
        let mut context = crate::test::editor_test_context::EditorTestContext::new(cx).await;
        context
            .cx
            .simulate_resize(gpui::size(gpui::px(240.), gpui::px(480.)));
        context.set_state("fn example() {ˇ}\nlet following = 1;\n");
        context.update_editor(|editor, window, cx| {
            let buffer = editor.buffer.read(cx).snapshot(cx);
            let anchor = buffer.anchor_before(multi_buffer::MultiBufferOffset(0));
            show(editor, anchor, "讲解".repeat(800).into(), true, cx);
            let snapshot = editor.snapshot(window, cx);
            for block_id in editor.explanations.blocks.iter().copied() {
                let block = snapshot
                    .block_for_id(crate::display_map::BlockId::Custom(block_id))
                    .expect("讲解块应存在于显示映射中");
                assert!(
                    block.has_height() && block.height() > 0,
                    "讲解块必须预留行，否则会与代码重叠"
                );
            }
        });

        context.cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });

        context.editor(|editor, _, cx| {
            let snapshot = editor.display_snapshot(cx);
            let heights = editor
                .explanations
                .blocks
                .iter()
                .map(|block_id| {
                    snapshot
                        .block_for_id(crate::display_map::BlockId::Custom(*block_id))
                        .expect("讲解块应存在于显示映射中")
                        .height()
                })
                .collect::<Vec<_>>();
            let height = *heights.first().expect("应插入至少一个讲解块");
            assert!(
                height > 1,
                "长讲解必须增长到换行后的实际高度，而不是压在一行上"
            );
            let following_row = snapshot
                .point_to_display_point(language::Point::new(1, 0), text::Bias::Left)
                .row()
                .0;
            assert_eq!(
                following_row as usize,
                height as usize + 1,
                "讲解块必须把后续代码推到自己下方，而不是盖住它"
            );
        });
    }
}

pub fn deep_explain_selection(
    editor: &mut Editor,
    _: &crate::DeepExplainSelection,
    window: &mut gpui::Window,
    cx: &mut Context<Editor>,
) {
    let snapshot = editor.snapshot(window, cx);
    let selection = editor
        .selections
        .newest::<multi_buffer::MultiBufferOffset>(&snapshot);
    if selection.is_empty() {
        return;
    }
    let Some(buffer) = editor.buffer.read(cx).as_singleton() else {
        return;
    };
    let buffer_snapshot = buffer.read(cx).snapshot();
    let Some(file) = buffer_snapshot.file().cloned() else {
        return;
    };
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let store = project.read(cx).worktree_store();
    let Some(trust) = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx) else {
        return;
    };
    let worktree_id = file.worktree_id(cx);
    let filename = file.file_name(cx).to_ascii_lowercase();
    if file.is_private()
        || filename.starts_with(".env")
        || filename.ends_with(".pem")
        || filename.ends_with(".key")
        || filename.ends_with(".min.js")
        || filename.ends_with(".lock")
        || project::DisableAiSettings::get_global(cx).disable_ai
        || !trust.update(cx, |trust, cx| trust.can_trust(&store, worktree_id, cx))
    {
        editor.explanations.last_error = Some(i18n::t!("894f5a34a8b14fe6").into());
        cx.notify();
        return;
    }
    let code = snapshot
        .buffer_snapshot()
        .text_for_range(selection.start..selection.end)
        .collect::<String>();
    if code.trim().is_empty() || code.len() > 64 * 1024 {
        editor.explanations.last_error = Some(i18n::t!("c06f9daef472226b").into());
        cx.notify();
        return;
    }
    let settings = CodeExplanationSettings::get_global(cx).clone();
    let definition_task =
        editor.definition_locations_of_kind(crate::GotoDefinitionKind::Symbol, cx);
    let model = match resolve_model(&settings, cx) {
        Ok(model) => model,
        Err(error) => {
            editor.explanations.last_error = Some(error.to_string().into());
            cx.notify();
            return;
        }
    };
    let first_point = snapshot.buffer_snapshot().offset_to_point(selection.start);
    let row = first_point.row;
    let line_end = language::Point::new(
        row,
        snapshot
            .buffer_snapshot()
            .line_len(multi_buffer::MultiBufferRow(row)),
    );
    let crease_start = snapshot
        .buffer_snapshot()
        .anchor_before(language::Point::new(row, 0));
    let crease_end = snapshot.buffer_snapshot().anchor_after(line_end);
    let workspace = editor.workspace().map(|workspace| workspace.downgrade());
    let languages = project.read(cx).languages().clone();
    let language = buffer_snapshot.language().map(|language| language.name());
    let editor_handle = cx.weak_entity();
    let expected_version = buffer_snapshot.version().clone();
    let expected_settings = format!("{settings:?}");
    let generation = editor.explanations.generation;
    editor
        .explanations
        .deep_cancelled
        .store(true, Ordering::SeqCst);
    let cancelled = Arc::new(AtomicBool::new(false));
    editor.explanations.deep_cancelled = cancelled.clone();
    let provider_configuration = selected_provider_configuration(&settings, cx);
    let markdown = cx.new(|cx| {
        Markdown::new(
            deep_explanation_markdown(&code, i18n::t!("399172ca1f0ca325")),
            Some(languages.clone()),
            language.clone(),
            cx,
        )
    });
    if let Some(workspace) = workspace.as_ref().and_then(|workspace| workspace.upgrade()) {
        workspace.update(cx, |workspace, cx| {
            let markdown = markdown.clone();
            let workspace_handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |_, cx| DeepExplanationModal {
                markdown,
                source_range: Some(crease_start..crease_end),
                focus_handle: cx.focus_handle(),
                scroll_handle: gpui::ScrollHandle::new(),
                cancelled: Some(cancelled.clone()),
                source_editor: Some(editor_handle.clone()),
                workspace: Some(workspace_handle),
            });
        });
    }
    cx.notify();
    editor.explanations.deep_task = Some(cx.spawn_in(window, async move |_, cx| {
        let related_context = if let Some(task) = definition_task {
            let mut items = Vec::new();
            for location in task.await.unwrap_or_default().into_iter().take(5) {
                let Some(path) = location.buffer.read_with(cx, |buffer, cx| {
                    let file = buffer.file()?;
                    (!file.is_private())
                        .then(|| buffer.project_path(cx))
                        .flatten()
                }) else {
                    continue;
                };
                if !trust.update(cx, |trust, cx| {
                    trust.can_trust(&store, path.worktree_id, cx)
                }) {
                    continue;
                };
                let text = location.buffer.read_with(cx, |buffer, _| {
                    let snapshot = buffer.snapshot();
                    let start = location
                        .range
                        .start
                        .to_point(&snapshot)
                        .row
                        .saturating_sub(3);
                    let end = location
                        .range
                        .end
                        .to_point(&snapshot)
                        .row
                        .saturating_add(4)
                        .min(snapshot.max_point().row);
                    snapshot
                        .text_for_range(
                            snapshot.point_to_offset(language::Point::new(start, 0))
                                ..snapshot.point_to_offset(language::Point::new(
                                    end,
                                    snapshot.line_len(end),
                                )),
                        )
                        .collect::<String>()
                });
                if !text.trim().is_empty() {
                    items.push((path, text));
                }
            }
            if items.is_empty() {
                None
            } else {
                let details = items
                    .iter()
                    .map(|(path, text)| {
                        i18n::t_args!(
                            "076b63825c131456",
                            path.path.as_unix_str(),
                            text.len(),
                            text
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let approved = cx
                    .update(|window, cx| {
                        window.prompt(
                            gpui::PromptLevel::Info,
                            i18n::t!("40153a68686cfea0"),
                            Some(&details),
                            &[i18n::t!("107ac302ddadc2a0"), i18n::t!("511be034f94b52e6")],
                            cx,
                        )
                    })?
                    .await
                    .unwrap_or(1)
                    == 0;
                approved.then(|| details)
            }
        } else {
            None
        };
        let request_code = if let Some(context) = related_context {
            i18n::t!("95b6730120dd8c52", code = code, context = context)
        } else {
            code.clone()
        };
        anyhow::ensure!(
            request_code.len() <= 64 * 1024,
            i18n::t!("ee66d16eca137d65")
        );

        let request_key = format!("deep:{}", content_hash(&code));
        let waiting =
            CodeExplanationRequestWaiter::new(project.entity_id(), request_key.clone(), 0)?;
        let permit = loop {
            if cancelled.load(Ordering::SeqCst) {
                markdown.update(cx, |markdown, cx| {
                    markdown.replace(i18n::t!("a095d9d5e35ba72c"), cx)
                });
                return anyhow::Ok(());
            }
            if editor_handle
                .read_with(cx, |editor, _| editor.explanations.generation != generation)
                .unwrap_or(true)
            {
                return anyhow::Ok(());
            }
            if let Some(permit) = waiting.acquire(settings.max_concurrent_requests) {
                editor_handle.update(cx, |_, cx| cx.notify()).ok();
                break permit;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        };
        drop(waiting);
        let authorized = cx.update(|_, cx| {
            selected_provider_configuration(CodeExplanationSettings::get_global(cx), cx)
                == provider_configuration
                && editor_handle
                    .read_with(cx, |editor, cx| {
                        editor.buffer.read(cx).as_singleton().as_ref() == Some(&buffer)
                    })
                    .unwrap_or(false)
                && buffer.read(cx).snapshot().version() == &expected_version
                && !project::DisableAiSettings::get_global(cx).disable_ai
                && format!("{:?}", CodeExplanationSettings::get_global(cx)) == expected_settings
                && buffer
                    .read(cx)
                    .file()
                    .is_some_and(|current| Arc::ptr_eq(current, &file) && !current.is_private())
                && trust.update(cx, |trust, cx| trust.can_trust(&store, worktree_id, cx))
        })?;
        let result = if authorized {
            markdown.update(cx, |markdown, cx| {
                markdown.replace(
                    deep_explanation_markdown(&code, i18n::t!("eb37fc3399caa9b6")),
                    cx,
                )
            });
            let markdown = markdown.clone();
            let selected_code = code.clone();
            let executor = cx.background_executor().clone();
            let request = request_deep(
                model,
                settings,
                request_code,
                |output, cx| {
                    anyhow::ensure!(
                        !cancelled.load(Ordering::SeqCst),
                        i18n::t!("988aca0875c41d8a")
                    );
                    anyhow::ensure!(
                        cx.update(|cx| {
                            buffer.read(cx).snapshot().version() == &expected_version
                                && buffer.read(cx).file().is_some_and(|current| {
                                    Arc::ptr_eq(current, &file) && !current.is_private()
                                })
                                && trust.update(cx, |trust, cx| {
                                    trust.can_trust(&store, worktree_id, cx)
                                })
                        }),
                        i18n::t!("1fa0b889a0a9af69")
                    );
                    markdown.update(cx, |markdown, cx| {
                        markdown.replace(deep_explanation_markdown(&selected_code, output), cx);
                    });
                    Ok(())
                },
                cx,
            );
            let cancellation = async {
                while !cancelled.load(Ordering::SeqCst) {
                    executor.timer(std::time::Duration::from_millis(100)).await;
                }
            };
            match futures::future::select(Box::pin(request), Box::pin(cancellation)).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(((), _)) => {
                    Err(anyhow::anyhow!(i18n::t!("988aca0875c41d8a")))
                }
            }
        } else {
            Err(anyhow::anyhow!(i18n::t!("03d3044b30299a77")))
        };
        drop(permit);
        let still_authorized = cx.update(|_, cx| {
            selected_provider_configuration(CodeExplanationSettings::get_global(cx), cx)
                == provider_configuration
                && editor_handle
                    .read_with(cx, |editor, cx| {
                        editor.buffer.read(cx).as_singleton().as_ref() == Some(&buffer)
                    })
                    .unwrap_or(false)
                && buffer.read(cx).snapshot().version() == &expected_version
                && !project::DisableAiSettings::get_global(cx).disable_ai
                && format!("{:?}", CodeExplanationSettings::get_global(cx)) == expected_settings
                && buffer
                    .read(cx)
                    .file()
                    .is_some_and(|current| Arc::ptr_eq(current, &file) && !current.is_private())
                && trust.update(cx, |trust, cx| trust.can_trust(&store, worktree_id, cx))
        })?;
        editor_handle.update(cx, |editor, cx| {
            if !still_authorized {
                markdown.update(cx, |markdown, cx| {
                    markdown.replace(i18n::t!("a7761fd72eba2f2a"), cx)
                });
                cx.notify();
                return;
            }
            let explanation: SharedString =
                result.unwrap_or_else(|error| i18n::t!("648df606c1f2361f", error = error).into());
            markdown.update(cx, |markdown, cx| {
                markdown.replace(deep_explanation_markdown(&code, &explanation), cx)
            });
            let explanation_for_modal = explanation;
            let code_for_modal = code.clone();
            let workspace = workspace.clone();
            let crease = crate::Crease::Inline {
                range: crease_start..crease_end,
                placeholder: editor.default_fold_placeholder(cx),
                render_toggle: None,
                render_trailer: Some(Arc::new(move |_, _, _, _| {
                    let explanation = explanation_for_modal.clone();
                    let code = code_for_modal.clone();
                    let workspace = workspace.clone();
                    let languages = languages.clone();
                    let language = language.clone();
                    IconButton::new(("deep-code-explanation", row), IconName::WarningCircle)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .alpha(0.45)
                        .tooltip(Tooltip::text(i18n::t!("8463c2a05d5aa14d")))
                        .on_click(move |_, window, cx| {
                            let Some(workspace) =
                                workspace.as_ref().and_then(|workspace| workspace.upgrade())
                            else {
                                return;
                            };
                            let explanation = explanation.clone();
                            let code = code.clone();
                            workspace.update(cx, |workspace, cx| {
                                workspace.toggle_modal(window, cx, |_, cx| {
                                    DeepExplanationModal::new(
                                        code,
                                        explanation,
                                        languages.clone(),
                                        language.clone(),
                                        cx,
                                    )
                                });
                            });
                        })
                        .into_any_element()
                })),
                metadata: None,
            };
            let ids = editor.insert_creases([crease], cx);
            editor.explanations.deep_explanation_creases.extend(ids);
            cx.notify();
        })?;
        anyhow::Ok(())
    }));
}

async fn request_deep(
    model: ConfiguredModel,
    settings: CodeExplanationSettings,
    code: String,
    mut progress: impl FnMut(&str, &mut gpui::AsyncApp) -> Result<()>,
    cx: &mut gpui::AsyncApp,
) -> Result<SharedString> {
    anyhow::ensure!(
        !cx.update(|cx| project::DisableAiSettings::get_global(cx).disable_ai),
        i18n::t!("0f4b4b34fffd444a")
    );
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(i18n::t_args!(
                    "079e385da65510bb",
                    settings.target_language
                ))],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(code)],
                cache: false,
                reasoning_details: None,
            },
        ],
        temperature: Some(0.2),
        thinking_allowed: false,
        ..Default::default()
    };
    use gpui::FutureExt as _;
    let executor = cx.background_executor().clone();
    let mut stream = model
        .provider
        .stream_completion_text(&model.model, request, cx)
        .with_timeout(std::time::Duration::from_secs(60), &executor)
        .await
        .context(i18n::t!("2755022ee4043bea"))?
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut output = String::new();
    let started = std::time::Instant::now();
    let mut last_update = std::time::Instant::now();
    while let Some(chunk) = stream
        .stream
        .next()
        .with_timeout(std::time::Duration::from_secs(30), &executor)
        .await
        .context(i18n::t!("9da4d28c3f09739c"))?
    {
        anyhow::ensure!(
            !cx.update(|cx| project::DisableAiSettings::get_global(cx).disable_ai),
            i18n::t!("dd230cd531e27def")
        );
        output.push_str(&chunk.map_err(|error| anyhow::anyhow!(error.to_string()))?);
        anyhow::ensure!(
            started.elapsed() < std::time::Duration::from_secs(180),
            i18n::t!("aee24bdcfec64d0c")
        );
        anyhow::ensure!(output.len() <= 64 * 1024, i18n::t!("6de20e96b6e06ce3"));
        if last_update.elapsed() >= std::time::Duration::from_millis(100) {
            progress(&output, cx)?;
            last_update = std::time::Instant::now();
        }
    }
    anyhow::ensure!(!output.trim().is_empty(), i18n::t!("20d86f25396ec566"));
    Ok(output.trim().to_string().into())
}

#[test]
fn test_deep_explanation_markdown_preserves_code_fences() {
    let code = "fn example() {\n    let value = \"```rust\";\n}\n";
    let explanation = i18n::t!("83613503c8822ad2");
    assert_eq!(
        deep_explanation_markdown(code, explanation).as_ref(),
        format!("````\n{code}\n````\n\n{explanation}")
    );
    assert_eq!(
        deep_explanation_markdown("plain code", explanation).as_ref(),
        format!("```\nplain code\n```\n\n{explanation}")
    );
}

fn deep_explanation_markdown(code: &str, explanation: &str) -> SharedString {
    let fence_length = code
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(3);
    let fence = "`".repeat(fence_length);
    format!("{fence}\n{code}\n{fence}\n\n{explanation}").into()
}

struct DeepExplanationModal {
    markdown: gpui::Entity<Markdown>,
    source_range: Option<std::ops::Range<multi_buffer::Anchor>>,
    source_editor: Option<gpui::WeakEntity<Editor>>,
    cancelled: Option<Arc<AtomicBool>>,
    workspace: Option<gpui::WeakEntity<workspace::Workspace>>,
    focus_handle: FocusHandle,
    scroll_handle: gpui::ScrollHandle,
}

impl DeepExplanationModal {
    fn new(
        code: String,
        explanation: SharedString,
        languages: Arc<language::LanguageRegistry>,
        language: Option<language::LanguageName>,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = deep_explanation_markdown(&code, &explanation);
        Self {
            markdown: cx.new(|cx| Markdown::new(source, Some(languages), language, cx)),
            source_range: None,
            cancelled: None,
            source_editor: None,
            workspace: None,
            focus_handle: cx.focus_handle(),
            scroll_handle: gpui::ScrollHandle::new(),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut gpui::Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for DeepExplanationModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for DeepExplanationModal {}
impl ModalView for DeepExplanationModal {}

impl workspace::Item for DeepExplanationModal {
    type Event = DismissEvent;
    fn to_item_events(_: &DismissEvent, emit: &mut dyn FnMut(workspace::item::ItemEvent)) {
        emit(workspace::item::ItemEvent::CloseItem);
    }
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        i18n::t!("16ece1acc00be44a").into()
    }
}

impl gpui::Render for DeepExplanationModal {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height =
            (window.viewport_size().height * 0.8).min(rems(42.).to_pixels(window.rem_size()));
        let markdown = MarkdownElement::new(
            self.markdown.clone(),
            MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
        )
        .code_block_renderer(CodeBlockRenderer::Default {
            copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
            wrap_button_visibility: WrapButtonVisibility::VisibleOnHover,
            border: false,
        })
        .scroll_handle(self.scroll_handle.clone());

        v_flex()
            .key_context("DeepExplanationModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .occlude()
            .w(rems(46.))
            .max_w(window.viewport_size().width * 0.9)
            .h(height)
            .overflow_hidden()
            .child(
                ModalHeader::new()
                    .show_dismiss_button(true)
                    .headline(i18n::t!("d18350e9052d513e")),
            )
            .when_some(self.source_editor.clone(), |this, source| {
                let source_range = self.source_range.clone();
                this.child(
                    Button::new("return-to-explained-source", i18n::t!("8cd52c1af8febf66"))
                        .on_click(move |_, window, cx| {
                            if let Some(editor) = source.upgrade() {
                                editor.update(cx, |editor, cx| {
                                    if let Some(range) = source_range.clone() {
                                        editor.change_selections(
                                            Default::default(),
                                            window,
                                            cx,
                                            |selections| selections.select_ranges([range]),
                                        );
                                        editor.request_autoscroll(Autoscroll::fit(), cx);
                                    }
                                    editor.focus_handle(cx).focus(window, cx);
                                });
                            }
                        }),
                )
            })
            .when(self.workspace.is_some(), |this| {
                this.child(
                    Button::new("follow-up-explanation", i18n::t!("b09fe286c17bf635")).on_click(
                        cx.listener(|this, _, window, cx| {
                            if project::DisableAiSettings::get_global(cx).disable_ai {
                                return;
                            }
                            let text =
                                i18n::t_args!("86758a83bafd6e15", this.markdown.read(cx).source());
                            let workspace = this.workspace.clone();
                            cx.emit(DismissEvent);
                            if let Some(workspace) =
                                workspace.and_then(|workspace| workspace.upgrade())
                            {
                                workspace.update(cx, |workspace, cx| {
                                    workspace.focus_handle(cx).dispatch_action(
                                        &zed_actions::assistant::FollowUpCodeExplanation { text },
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }),
                    ),
                )
            })
            .when_some(self.workspace.clone(), |this, workspace| {
                let pin_workspace = workspace.clone();
                this.child(
                    Button::new("pin-explanation", i18n::t!("5323bbacebd32ca3")).on_click(
                        cx.listener(move |this, _, window, cx| {
                            let Some(workspace) = pin_workspace.upgrade() else {
                                return;
                            };
                            let markdown = this.markdown.clone();
                            let cancelled = this.cancelled.clone();
                            let source_editor = this.source_editor.clone();
                            let source_range = this.source_range.clone();
                            workspace.update(cx, |workspace, cx| {
                                let workspace_handle = cx.weak_entity();
                                let view = cx.new(|cx| DeepExplanationModal {
                                    markdown,
                                    source_range,
                                    cancelled,
                                    source_editor,
                                    workspace: Some(workspace_handle),
                                    focus_handle: cx.focus_handle(),
                                    scroll_handle: gpui::ScrollHandle::new(),
                                });
                                let pane = workspace.split_pane(
                                    workspace.active_pane().clone(),
                                    workspace::SplitDirection::Right,
                                    window,
                                    cx,
                                );
                                pane.update(cx, |pane, cx| {
                                    pane.add_item(Box::new(view), true, true, None, window, cx)
                                });
                            });
                            cx.emit(DismissEvent);
                        }),
                    ),
                )
                .child(
                    Button::new("keep-explanation", i18n::t!("e04b69643cbdb136")).on_click(
                        cx.listener(move |this, _, window, cx| {
                            let Some(workspace) = workspace.upgrade() else {
                                return;
                            };
                            let text = this.markdown.read(cx).source().to_string();
                            workspace.update(cx, |workspace, cx| {
                                let buffer = cx.new(|cx| language::Buffer::local(text, cx));
                                let editor = cx.new(|cx| {
                                    let mut editor = Editor::for_buffer(buffer, None, window, cx);
                                    editor.set_read_only(true);
                                    editor
                                });
                                workspace.add_item_to_active_pane(
                                    Box::new(editor),
                                    None,
                                    true,
                                    window,
                                    cx,
                                );
                            });
                            cx.emit(DismissEvent);
                        }),
                    ),
                )
            })
            .when_some(self.cancelled.clone(), |this, cancelled| {
                this.child(
                    Button::new("stop-deep-explanation", i18n::t!("9ad0aac32ea304be")).on_click(
                        move |_, _, _| {
                            cancelled.store(true, Ordering::SeqCst);
                        },
                    ),
                )
            })
            .child(
                Button::new("copy-deep-explanation", i18n::t!("6258a2c6f9c89143")).on_click(
                    cx.listener(|this, _, _, cx| {
                        let text = this.markdown.read(cx).source().to_string();
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    }),
                ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("deep-code-explanation-content")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .p_4()
                            .child(markdown),
                    )
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
    }
}

fn scan_source_extension(path: &util::rel_path::RelPath) -> bool {
    let Some(extension) = path.extension() else {
        return matches!(
            path.file_name(),
            Some("Dockerfile" | "Makefile" | "Rakefile")
        );
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "cs"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "rb"
            | "php"
            | "lua"
            | "sh"
            | "bash"
            | "zsh"
            | "ps1"
            | "dart"
            | "ex"
            | "exs"
            | "ml"
            | "mli"
    )
}

fn scan_path_is_sensitive(path: &util::rel_path::RelPath) -> bool {
    let lower = path.as_unix_str().to_ascii_lowercase();
    let file_name = path.file_name().unwrap_or_default().to_ascii_lowercase();
    let components = lower.split('/').collect::<Vec<_>>();
    components.iter().any(|component| {
        matches!(
            *component,
            ".git"
                | ".svn"
                | ".hg"
                | ".jj"
                | "node_modules"
                | "vendor"
                | "target"
                | "build"
                | "dist"
                | "out"
                | "coverage"
                | ".next"
                | ".nuxt"
                | ".cache"
                | ".gradle"
                | ".idea"
                | ".vscode"
                | "venv"
                | ".venv"
                | "__pycache__"
                | "pods"
                | "deriveddata"
                | "bazel-bin"
                | "bazel-out"
        )
    }) || file_name.starts_with(".env")
        || matches!(
            file_name.as_str(),
            "id_rsa"
                | "id_ed25519"
                | "authorized_keys"
                | "known_hosts"
                | "credentials"
                | "credentials.json"
        )
        || lower.ends_with("/.aws/credentials")
        || lower.ends_with("/.kube/config")
        || [
            ".pem",
            ".key",
            ".p12",
            ".pfx",
            ".jks",
            ".keystore",
            ".crt",
            ".cer",
            ".cert",
            ".tfstate",
            ".tfvars",
            ".sqlite",
            ".sqlite3",
            ".db",
            ".dump",
            ".backup",
            ".log",
            ".trace",
            ".har",
            ".pcap",
            ".dmp",
            ".core",
            ".lock",
            ".min.js",
            ".bundle.js",
            ".map",
        ]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        || lower.contains(".generated.")
        || lower.contains(".gen.")
        || file_name.starts_with("service-account")
        || file_name.starts_with("service_account")
        || file_name.starts_with("secret.")
        || file_name.starts_with("secrets.")
}

fn scan_git_status_allowed(status: git::status::FileStatus) -> bool {
    !matches!(
        status,
        git::status::FileStatus::Untracked | git::status::FileStatus::Ignored
    )
}

fn path_is_within_scan_selection(
    worktree_id: project::WorktreeId,
    path: &util::rel_path::RelPath,
    selected_paths: Option<&[project::ProjectPath]>,
) -> bool {
    selected_paths.is_none_or(|paths| {
        paths.iter().any(|selected| {
            selected.worktree_id == worktree_id
                && (path == selected.path.as_ref() || path.starts_with(&selected.path))
        })
    })
}

fn collect_project_scan_candidates(
    project: &gpui::Entity<project::Project>,
    selected_paths: Option<&[project::ProjectPath]>,
    cx: &mut App,
) -> Vec<ProjectScanCandidate> {
    let git_store = project.read(cx).git_store().clone();
    let worktree_store = project.read(cx).worktree_store();
    let Some(trust) = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    let worktrees = project.read(cx).visible_worktrees(cx).collect::<Vec<_>>();
    for worktree in worktrees {
        let snapshot = worktree.read(cx).snapshot();
        if !trust.update(cx, |trust, cx| {
            trust.can_trust(&worktree_store, snapshot.id(), cx)
        }) {
            continue;
        }
        let worktree_id = snapshot.id();
        let root_name = snapshot.root_name().as_unix_str();
        for entry in snapshot.files(false, 0) {
            if !path_is_within_scan_selection(worktree_id, &entry.path, selected_paths)
                || entry.is_ignored
                || entry.is_private
                || entry.is_external
                || entry.is_fifo
                || entry.size > 512 * 1024
                || !scan_source_extension(&entry.path)
                || scan_path_is_sensitive(&entry.path)
            {
                continue;
            }
            let project_path = project::ProjectPath {
                worktree_id,
                path: entry.path.clone(),
            };
            let Some((repository, repository_path)) = git_store
                .read(cx)
                .repository_and_path_for_project_path(&project_path, cx)
            else {
                continue;
            };
            if repository
                .read(cx)
                .snapshot()
                .status_for_path(&repository_path)
                .is_some_and(|status| !scan_git_status_allowed(status.status))
            {
                continue;
            }
            candidates.push(ProjectScanCandidate {
                display_path: format!("{root_name}/{}", entry.path.as_unix_str()).into(),
                path: project_path,
                size: entry.size,
            });
        }
    }
    candidates.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    candidates
}

struct ProjectScanModal {
    project: gpui::Entity<project::Project>,
    workspace: gpui::WeakEntity<workspace::Workspace>,
    candidates: Vec<ProjectScanCandidate>,
    error: Option<SharedString>,
    model_label: SharedString,
    focus_handle: FocusHandle,
    scroll_handle: gpui::ScrollHandle,
}

impl Focusable for ProjectScanModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ProjectScanModal {}
impl ModalView for ProjectScanModal {}

impl gpui::Render for ProjectScanModal {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bytes = self
            .candidates
            .iter()
            .map(|candidate| candidate.size)
            .sum::<u64>();
        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .elevation_3(cx)
            .occlude()
            .w(rems(56.))
            .max_w(window.viewport_size().width * 0.9)
            .max_h(window.viewport_size().height * 0.85)
            .child(
                Modal::new(
                    "project-code-explanation-scan",
                    Some(self.scroll_handle.clone()),
                )
                .header(
                    ModalHeader::new()
                        .show_dismiss_button(true)
                        .headline(i18n::t!("201f47c673115f2e")),
                )
                .section(
                    Section::new().child(
                        v_flex()
                            .gap_3()
                            .child(Label::new(i18n::t_args!(
                                "fdb4bd7cbdf3e9db",
                                self.candidates.len(),
                                bytes as f64 / (1024. * 1024.)
                            )))
                            .child(Label::new(i18n::t_args!(
                                "0659fa18b816c7da",
                                self.model_label
                            )))
                            .child(Label::new(i18n::t!("7bd95da73b9541f2")).color(Color::Muted))
                            .child(Label::new(i18n::t!("5c21dbb661225e8e")).color(Color::Muted))
                            .child(Label::new(i18n::t!("43f74961c6aa09df")).color(Color::Muted))
                            .child(Label::new(i18n::t!("097e611f0046f3d2")).color(Color::Warning))
                            .when(self.candidates.is_empty() && self.error.is_none(), |this| {
                                this.child(
                                    Label::new(i18n::t!("652ecdcbcee652e4")).color(Color::Warning),
                                )
                            })
                            .when_some(self.error.clone(), |this, error| {
                                this.child(Label::new(error).color(Color::Warning))
                            })
                            .child(Label::new(i18n::t!("4ebc15cb92de36ba")))
                            .child(
                                div()
                                    .id("scan-candidate-files")
                                    .max_h(rems(24.))
                                    .overflow_y_scroll()
                                    .overflow_x_scroll()
                                    .children(self.candidates.iter().enumerate().map(
                                        |(index, candidate)| {
                                            div().whitespace_nowrap().child(Label::new(format!(
                                                "{}.  {}  ·  {:.2} KiB",
                                                index + 1,
                                                candidate.display_path,
                                                candidate.size as f64 / 1024.
                                            )))
                                        },
                                    )),
                            ),
                    ),
                )
                .section(
                    Section::new().child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("copy-scan-list", i18n::t!("5a91e1cf5fdbcc38"))
                                    .disabled(self.candidates.is_empty())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let list = this
                                            .candidates
                                            .iter()
                                            .map(|candidate| {
                                                format!(
                                                    "{}\t{} bytes",
                                                    candidate.display_path, candidate.size
                                                )
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            list,
                                        ));
                                    })),
                            )
                            .child(
                                Button::new("cancel-scan", i18n::t!("2cd0f3be8738a86c"))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("start-scan", i18n::t!("743497b67cd219dd"))
                                    .disabled(self.error.is_some() || self.candidates.is_empty())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.error.is_some() || this.candidates.is_empty() {
                                            return;
                                        }
                                        let project = this.project.clone();
                                        let workspace = this.workspace.clone();
                                        let candidates = std::mem::take(&mut this.candidates);
                                        cx.emit(DismissEvent);
                                        cx.spawn(async move |_, cx| {
                                            start_project_scan(
                                                project,
                                                workspace,
                                                candidates,
                                                ProjectScanMode::Incremental,
                                                cx,
                                            )
                                        })
                                        .detach_and_log_err(cx);
                                    })),
                            ),
                    ),
                ),
            )
    }
}

fn show_project_scan_confirmation(
    project: gpui::Entity<project::Project>,
    workspace: gpui::WeakEntity<workspace::Workspace>,
    selected_paths: Option<Vec<project::ProjectPath>>,
    window: &mut gpui::Window,
    cx: &mut App,
) {
    let settings = CodeExplanationSettings::get_global(cx).clone();
    let model_label = format!(
        "{} / {}",
        settings
            .provider
            .as_deref()
            .unwrap_or(i18n::t!("71da5563d66c1d94")),
        settings
            .model
            .as_deref()
            .unwrap_or(i18n::t!("9bb63745df8f62cb"))
    )
    .into();
    let scan_running = project_scan_state(&project, cx).read(cx).running;
    let error = if scan_running {
        Some(SharedString::from(i18n::t!("d83664afd2308653")))
    } else {
        (resolve_model(&settings, cx).is_err() || !settings.cache_persist)
            .then(|| SharedString::from(i18n::t!("b30065caf5414bf4")))
    };
    let candidates = if error.is_none() {
        collect_project_scan_candidates(&project, selected_paths.as_deref(), cx)
    } else {
        Vec::new()
    };
    let modal_workspace = workspace.clone();
    workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |_, cx| ProjectScanModal {
                project,
                workspace: modal_workspace,
                candidates,
                error,
                model_label,
                focus_handle: cx.focus_handle(),
                scroll_handle: gpui::ScrollHandle::new(),
            });
        })
        .log_err();
}

pub fn start_selected_project_scan(
    project: gpui::Entity<project::Project>,
    workspace: gpui::WeakEntity<workspace::Workspace>,
    selected_paths: Vec<project::ProjectPath>,
    cx: &mut App,
) {
    if selected_paths.is_empty() {
        return;
    }
    let error = if project_scan_state(&project, cx).read(cx).running {
        Some(SharedString::from(i18n::t!("d83664afd2308653")))
    } else {
        let settings = CodeExplanationSettings::get_global(cx);
        (resolve_model(settings, cx).is_err() || !settings.cache_persist)
            .then(|| SharedString::from(i18n::t!("b30065caf5414bf4")))
    };
    let candidates = if error.is_none() {
        collect_project_scan_candidates(&project, Some(&selected_paths), cx)
    } else {
        Vec::new()
    };
    if let Some(message) = error.or_else(|| {
        candidates
            .is_empty()
            .then(|| SharedString::from(i18n::t!("652ecdcbcee652e4")))
    }) {
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<ProjectScanState>(),
                        message.to_string(),
                    ),
                    cx,
                );
            })
            .log_err();
        return;
    }
    cx.spawn(async move |cx| {
        start_project_scan(
            project,
            workspace,
            candidates,
            ProjectScanMode::ReplaceExisting,
            cx,
        )
    })
    .detach_and_log_err(cx);
}

fn start_project_scan(
    project: gpui::Entity<project::Project>,
    workspace: gpui::WeakEntity<workspace::Workspace>,
    candidates: Vec<ProjectScanCandidate>,
    mode: ProjectScanMode,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let state = cx.update(|cx| project_scan_state(&project, cx));
    let cancelled = Arc::new(AtomicBool::new(false));
    let project_label = candidates
        .first()
        .and_then(|candidate| candidate.display_path.split('/').next())
        .filter(|label| !label.is_empty())
        .unwrap_or(i18n::t!("79f326be4409d51f"))
        .to_owned();
    state.update(cx, |state, cx| {
        *state = ProjectScanState {
            running: true,
            project_label: project_label.into(),
            cancelled: cancelled.clone(),
            total_files: candidates.len(),
            ..Default::default()
        };
        cx.notify();
    });
    cx.spawn(async move |cx| {
        let executor = cx.background_executor().clone();
        let scan = run_project_scan(&project, &state, candidates, mode, cancelled.clone(), cx);
        let cancellation = async {
            while !cancelled.load(Ordering::SeqCst) {
                if workspace.upgrade().is_none() {
                    cancelled.store(true, Ordering::SeqCst);
                    break;
                }
                executor.timer(std::time::Duration::from_millis(100)).await;
            }
        };
        // Dropping the scan future cancels file tasks even when a provider stops responding.
        let result = match futures::future::select(Box::pin(scan), Box::pin(cancellation)).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), _)) => Ok(()),
        };
        state.update(cx, |state, cx| {
            state.running = false;
            cx.notify();
        });
        if let Some(workspace) = workspace.upgrade() {
            let message = state.read_with(cx, |state, _| {
                if cancelled.load(Ordering::SeqCst) {
                    i18n::t_args!(
                        "41b920ab385a9d23",
                        state.completed_files,
                        state.total_files,
                        state.requested_units,
                        state.fully_cached_files,
                        state.cached_units,
                        state.failures
                    )
                } else if let Err(error) = &result {
                    i18n::t!("4fe3cd0e3060d215", error = error)
                } else {
                    i18n::t_args!(
                        "904a0ca20bb173ac",
                        state.completed_files,
                        state.requested_units,
                        state.fully_cached_files,
                        state.cached_units,
                        state.skipped_files,
                        state.failures
                    )
                }
            });
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<ProjectScanState>(),
                        message,
                    ),
                    cx,
                );
            });
        }
        result.log_err();
    })
    .detach();
    Ok(())
}

#[derive(Default)]
struct ScannedFileResult {
    cached_units: usize,
    fully_cached: bool,
    requested_units: usize,
    skipped: bool,
    failed: bool,
    error: Option<String>,
}

enum PreparedProjectScan {
    Skipped,
    Units(Vec<(crate::code_explanation_units::Unit, String, String)>),
}

async fn run_project_scan(
    project: &gpui::Entity<project::Project>,
    state: &gpui::Entity<ProjectScanState>,
    candidates: Vec<ProjectScanCandidate>,
    mode: ProjectScanMode,
    cancelled: Arc<AtomicBool>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    if mode == ProjectScanMode::ReplaceExisting {
        for candidate in &candidates {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(());
            }
            let cache_path = project
                .read_with(cx, |project, cx| {
                    let worktree = project
                        .worktree_store()
                        .read(cx)
                        .worktree_for_id(candidate.path.worktree_id, cx)?;
                    let namespace = format!(
                        "{:?}:{:?}",
                        worktree.read(cx).abs_path(),
                        project.remote_connection_options(cx)
                    );
                    Some(
                        paths::data_dir()
                            .join("code-explanations")
                            .join(format!("{}.sqlite", content_hash(&namespace))),
                    )
                })
                .context(i18n::t!("4a5efd4b3e9d11a9"))?;
            let file_path = candidate.path.path.as_unix_str().to_owned();
            cx.background_spawn(async move { cache_remove_file(&cache_path, &file_path) })
                .await?;
            cx.update(|cx| unmark_explained_file(project, &candidate.path, cx));
        }
    }
    let mut pending = FuturesUnordered::new();
    let concurrency =
        cx.update(|cx| CodeExplanationSettings::get_global(cx).max_concurrent_requests);
    for candidate in candidates {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        while pending.len() >= concurrency {
            if let Some(result) = pending.next().await {
                record_scanned_file(state, result, cx)?;
            }
            if cancelled.load(Ordering::SeqCst) {
                break;
            }
        }
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        let project = project.clone();
        let cancelled = cancelled.clone();
        pending.push(cx.spawn(async move |cx| {
            let display_path = candidate.display_path.clone();
            scan_project_file(project, candidate, mode, cancelled, cx)
                .await
                .with_context(|| i18n::t!("0a0c6a6c4e23af20", display_path = display_path))
        }));
    }
    while !cancelled.load(Ordering::SeqCst) {
        let Some(result) = pending.next().await else {
            break;
        };
        record_scanned_file(state, result, cx)?;
    }
    drop(pending);
    Ok(())
}

fn record_scanned_file(
    state: &gpui::Entity<ProjectScanState>,
    result: Result<ScannedFileResult>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    state.update(cx, |state, cx| {
        state.completed_files += 1;
        match result {
            Ok(result) => {
                state.cached_units += result.cached_units;
                state.fully_cached_files += usize::from(result.fully_cached);
                state.requested_units += result.requested_units;
                state.skipped_files += usize::from(result.skipped);
                state.failures += usize::from(result.failed);
                if let Some(error) = result.error
                    && state.diagnostics.len() < 100
                {
                    state.diagnostics.push(error);
                }
            }
            Err(error) => {
                state.failures += 1;
                if state.diagnostics.len() < 100 {
                    state.diagnostics.push(error.to_string());
                }
            }
        }
        cx.notify();
    });
    Ok(())
}

async fn scan_project_file(
    project: gpui::Entity<project::Project>,
    candidate: ProjectScanCandidate,
    mode: ProjectScanMode,
    cancelled: Arc<AtomicBool>,
    cx: &mut gpui::AsyncApp,
) -> Result<ScannedFileResult> {
    if cancelled.load(Ordering::SeqCst) {
        return Ok(ScannedFileResult {
            skipped: true,
            ..Default::default()
        });
    }
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer(candidate.path.clone(), cx)
        })
        .await?;
    buffer
        .read_with(cx, |buffer, _| buffer.parsing_idle())
        .await;
    let (snapshot, file) = buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        (snapshot.clone(), snapshot.file().cloned())
    });
    let Some(file) = file else {
        return Ok(ScannedFileResult {
            skipped: true,
            ..Default::default()
        });
    };
    if file.is_private() || cancelled.load(Ordering::SeqCst) {
        return Ok(ScannedFileResult {
            skipped: true,
            ..Default::default()
        });
    }
    let settings = cx.update(|cx| CodeExplanationSettings::get_global(cx).clone());
    let expected_settings = format!("{settings:?}");
    let expected_version = snapshot.version().clone();
    let store = project.read_with(cx, |project, _| project.worktree_store());
    let trust = cx
        .update(|cx| project::trusted_worktrees::TrustedWorktrees::try_get_global(cx))
        .context(i18n::t!("ad271451d8ddcb2f"))?;
    let model = cx.update(|cx| resolve_model(&settings, cx))?;
    let language = snapshot
        .language()
        .filter(|language| language.grammar().is_some())
        .map(|language| language.name().to_string());
    let Some(language) = language else {
        return Ok(ScannedFileResult {
            skipped: true,
            ..Default::default()
        });
    };
    let cache_namespace = project
        .read_with(cx, |project, cx| {
            let worktree = project
                .worktree_store()
                .read(cx)
                .worktree_for_id(candidate.path.worktree_id, cx)?;
            Some(format!(
                "{:?}:{:?}",
                worktree.read(cx).abs_path(),
                project.remote_connection_options(cx)
            ))
        })
        .context(i18n::t!("4a5efd4b3e9d11a9"))?;
    let cache_path = paths::data_dir()
        .join("code-explanations")
        .join(format!("{}.sqlite", content_hash(&cache_namespace)));
    let provider_configuration = cx.update(|cx| selected_provider_configuration(&settings, cx));
    let prepared_units = {
        let snapshot = snapshot.clone();
        let model = model.model.clone();
        let settings = settings.clone();
        let provider_configuration = provider_configuration.clone();
        let language = language.clone();
        let cancelled = cancelled.clone();
        cx.background_spawn(async move {
            if snapshot.len() > 512 * 1024
                || snapshot.max_point().row >= 20_000
                || cancelled.load(Ordering::SeqCst)
            {
                return PreparedProjectScan::Skipped;
            }
            let leading_end = snapshot
                .as_rope()
                .floor_char_boundary(snapshot.len().min(4096));
            let leading_lower = snapshot
                .text_for_range(0..leading_end)
                .collect::<String>()
                .to_ascii_lowercase();
            if leading_lower.contains("do not edit")
                && (leading_lower.contains("generated") || leading_lower.contains("auto-generated"))
                || snapshot.text().lines().any(|line| line.len() >= 20 * 1024)
                || cancelled.load(Ordering::SeqCst)
            {
                return PreparedProjectScan::Skipped;
            }
            let units = crate::code_explanation_units::fit_units_to_budget(
                &snapshot,
                crate::code_explanation_units::file_units(&snapshot, settings.max_function_lines),
                crate::code_explanation_units::request_code_budget(model.max_token_count()),
                |text| model.estimate_tokens(text),
            );
            PreparedProjectScan::Units(
                units
                    .into_iter()
                    .filter_map(|unit| {
                        if cancelled.load(Ordering::SeqCst) {
                            return None;
                        }
                        let code = snapshot
                            .text_for_range(unit.range.clone())
                            .collect::<String>();
                        if code.is_empty() {
                            return None;
                        }
                        let key = format!(
                            "v6:{provider_configuration}:{:?}:{:?}:{}:{}:{}",
                            settings.provider,
                            settings.model,
                            settings.target_language,
                            settings.detailed,
                            content_hash(&format!("{language}\n{}\n{}", unit.context, code))
                        );
                        Some((unit, code, key))
                    })
                    .collect(),
            )
        })
        .await
    };
    let prepared_units = match prepared_units {
        PreparedProjectScan::Skipped => {
            return Ok(ScannedFileResult {
                skipped: true,
                ..Default::default()
            });
        }
        PreparedProjectScan::Units(units) => units,
    };
    let mut result = ScannedFileResult::default();
    if prepared_units.is_empty() {
        result.skipped = true;
        result.error = Some(i18n::t_args!("89a0d90fe0aa7f61", candidate.display_path));
        return Ok(result);
    }
    if settings.cache_persist && mode == ProjectScanMode::Incremental {
        let path = cache_path.clone();
        let file_path = candidate.path.path.as_unix_str().to_owned();
        let keys = prepared_units
            .iter()
            .map(|(_, _, key)| key.clone())
            .collect::<Vec<_>>();
        if cx
            .background_spawn(async move { cache_contains_complete_file(&path, &file_path, &keys) })
            .await?
        {
            cx.update(|cx| mark_explained_file(&project, candidate.path.clone(), cx));
            result.cached_units = prepared_units.len();
            result.fully_cached = true;
            return Ok(result);
        }
    }
    for (unit, code, key) in prepared_units {
        if cancelled.load(Ordering::SeqCst) {
            result.skipped = true;
            break;
        }
        if settings.cache_persist && mode == ProjectScanMode::Incremental {
            let path = cache_path.clone();
            let key_for_cache = key.clone();
            if cx
                .background_spawn(async move { cache_access(&path, &key_for_cache, None, 0) })
                .await?
                .is_some()
            {
                let path = cache_path.clone();
                let file_path = candidate.path.path.as_unix_str().to_owned();
                let marker_key = key.clone();
                cx.background_spawn(async move { cache_mark_file(&path, &file_path, &marker_key) })
                    .await?;
                cx.update(|cx| mark_explained_file(&project, candidate.path.clone(), cx));
                result.cached_units += 1;
                continue;
            }
        }
        let authorized = |cx: &mut App| {
            selected_provider_configuration(&settings, cx) == provider_configuration
                && !cancelled.load(Ordering::SeqCst)
                && !project::DisableAiSettings::get_global(cx).disable_ai
                && format!("{:?}", CodeExplanationSettings::get_global(cx)) == expected_settings
                && buffer.read(cx).snapshot().version() == &expected_version
                && buffer
                    .read(cx)
                    .file()
                    .is_some_and(|current| Arc::ptr_eq(current, &file) && !current.is_private())
                && store
                    .read(cx)
                    .worktree_for_id(candidate.path.worktree_id, cx)
                    .and_then(|worktree| {
                        worktree
                            .read(cx)
                            .snapshot()
                            .entry_for_path(&candidate.path.path)
                            .cloned()
                    })
                    .is_some_and(|entry| {
                        !entry.is_private && !entry.is_ignored && !entry.is_external
                    })
                && trust.update(cx, |trust, cx| {
                    trust.can_trust(&store, candidate.path.worktree_id, cx)
                })
        };
        if !cx.update(authorized) {
            result.skipped = true;
            break;
        }
        let request_key = format!("{cache_namespace}:{key}");
        let waiting =
            CodeExplanationRequestWaiter::new(project.entity_id(), request_key.clone(), 2)?;
        let permit = loop {
            if cancelled.load(Ordering::SeqCst) {
                result.skipped = true;
                return Ok(result);
            }
            if let Some(permit) = waiting.acquire(settings.max_concurrent_requests) {
                break permit;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        };
        drop(waiting);
        let epoch = CACHE_EPOCH.load(Ordering::SeqCst);
        if settings.cache_persist {
            let path = cache_path.clone();
            let cache_key = key.clone();
            if cx
                .background_spawn(async move { cache_access(&path, &cache_key, None, 0) })
                .await?
                .is_some()
            {
                let path = cache_path.clone();
                let file_path = candidate.path.path.as_unix_str().to_owned();
                let marker_key = key.clone();
                cx.background_spawn(async move { cache_mark_file(&path, &file_path, &marker_key) })
                    .await?;
                cx.update(|cx| mark_explained_file(&project, candidate.path.clone(), cx));
                result.cached_units += 1;
                drop(permit);
                continue;
            }
        }
        let text = request_if_authorized(
            authorized,
            model.clone(),
            settings.clone(),
            i18n::t_mix!("8e667b4105a98ce1"; unit.context, code.lines()
                    .enumerate()
                    .map(|(index, line)| format!("{}: {line}", index + 1))
                    .collect::<Vec<_>>()
                    .join("\n"); language = language),
            cx,
        )
        .await;
        drop(permit);
        if cancelled.load(Ordering::SeqCst) {
            result.skipped = true;
            break;
        }
        match text {
            Ok(text) if parse_annotations(&text, &code, &[]).is_ok() => {
                result.requested_units += 1;
                let may_write = cx.update(authorized);
                if may_write {
                    shared_result(&format!("{epoch}:{request_key}"), Some(text.clone()));
                }
                if settings.cache_persist && may_write {
                    let path = cache_path.clone();
                    let budget = settings.cache_max_bytes;
                    let text = text.to_string();
                    let cancelled = cancelled.clone();
                    let cache_key = key.clone();
                    cx.background_spawn(async move {
                        cache_access_guarded(&path, &cache_key, Some(&text), budget, || {
                            !cancelled.load(Ordering::SeqCst)
                                && CACHE_EPOCH.load(Ordering::SeqCst) == epoch
                        })
                    })
                    .await?;
                    let path = cache_path.clone();
                    let file_path = candidate.path.path.as_unix_str().to_owned();
                    let marker_key = key.clone();
                    cx.background_spawn(
                        async move { cache_mark_file(&path, &file_path, &marker_key) },
                    )
                    .await?;
                    cx.update(|cx| mark_explained_file(&project, candidate.path.clone(), cx));
                }
            }
            Ok(_) => {
                result.failed = true;
                result.error = Some(i18n::t_args!("d67def645df838e0", candidate.display_path));
            }
            Err(error) => {
                result.failed = true;
                result.error = Some(format!("{}：{error}", candidate.display_path));
            }
        }
    }
    Ok(result)
}

#[derive(Default)]
pub struct CodeExplanationIndicator {
    active: Option<gpui::Entity<Editor>>,
    subscription: Option<gpui::Subscription>,
    scan_subscriptions: HashMap<gpui::EntityId, gpui::Subscription>,
}

impl gpui::Render for CodeExplanationIndicator {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = CodeExplanationSettings::get_global(cx).clone();
        let active = self.active.clone();
        let active_project = active
            .as_ref()
            .and_then(|editor| editor.read(cx).project().cloned());
        let active_project_id = active_project.as_ref().map(gpui::Entity::entity_id);
        let registry = project_scan_registry(cx);
        let scan_states = registry.read(cx).scans.clone();
        self.scan_subscriptions
            .retain(|project_id, _| scan_states.contains_key(project_id));
        for (project_id, state) in &scan_states {
            if !self.scan_subscriptions.contains_key(project_id) {
                self.scan_subscriptions
                    .insert(*project_id, cx.observe(state, |_, _, cx| cx.notify()));
            }
        }
        let active_scan_state = active_project
            .as_ref()
            .map(|project| project_scan_state(project, cx));
        let active_scan = active_scan_state.as_ref().map(|state| state.read(cx));
        let scan_cancelled = active_scan.as_ref().map(|scan| scan.cancelled.clone());
        let busy = active
            .as_ref()
            .is_some_and(|editor| editor.read(cx).explanations.busy);
        let active_requests = active_project_id
            .map(active_request_count)
            .unwrap_or_default();
        let scan_running = active_scan.as_ref().is_some_and(|scan| scan.running);
        let scan_progress = active_scan
            .as_ref()
            .map(|scan| (scan.completed_files, scan.total_files))
            .unwrap_or_default();
        let scan_diagnostics = active_scan
            .as_ref()
            .map(|scan| scan.diagnostics.join("\n"))
            .unwrap_or_default();
        let background_scans = scan_states
            .iter()
            .filter(|(project_id, state)| {
                Some(**project_id) != active_project_id && state.read(cx).running
            })
            .map(|(_, state)| {
                let scan = state.read(cx);
                i18n::t_args!(
                    "80499948cf15561e",
                    scan.project_label,
                    scan.completed_files,
                    scan.total_files
                )
            })
            .collect::<Vec<_>>();
        let background_scan_tooltip = (!background_scans.is_empty())
            .then(|| i18n::t_args!("d08738493459bec5", background_scans.join("；")));
        h_flex()
            .gap_1()
            .child(
                ui::PopoverMenu::new("code-explanations-menu")
                    .trigger(
                        IconButton::new("code-explanations", IconName::Book)
                            .icon_color(if busy || active_requests > 0 || scan_running {
                                Color::Accent
                            } else if settings.enabled {
                                Color::Success
                            } else {
                                Color::Muted
                            })
                            .tooltip(ui::Tooltip::text(
                                if let Some(error) = active.as_ref().and_then(|editor| {
                                    editor.read(cx).explanations.last_error.clone()
                                }) {
                                    error.to_string()
                                } else if active
                                    .as_ref()
                                    .is_some_and(|editor| editor.read(cx).explanations.dirty)
                                {
                                    i18n::t!("76e16b507d13a3e5").into()
                                } else if active_requests > 0 {
                                    i18n::t!("73dc10ecf6cf1fae", active_requests = active_requests)
                                } else if scan_running {
                                    i18n::t_args!(
                                        "f2d3978f0badd1ca",
                                        scan_progress.0,
                                        scan_progress.1
                                    )
                                } else if busy {
                                    i18n::t!("fecfc5efb63609b5").into()
                                } else if settings.enabled {
                                    i18n::t!("aeff499b47e3db8c").into()
                                } else {
                                    i18n::t!("e638de313ea1e28b").into()
                                },
                            ))
                            .when(active_requests > 0, |button| {
                                button.indicator(ui::Indicator::custom(
                                    h_flex()
                                        .items_center()
                                        .gap_px()
                                        .child(
                                            SpinnerLabel::dots_variant()
                                                .size(LabelSize::Custom(rems_from_px(8_f32))),
                                        )
                                        .child(
                                            Label::new(active_requests.to_string())
                                                .size(LabelSize::Custom(rems_from_px(9_f32)))
                                                .color(Color::Accent),
                                        ),
                                ))
                            }),
                    )
                    .menu(move |window, cx| {
                        Some(ui::ContextMenu::build(window, cx, |menu, _, cx| {
                            let active_project = active.as_ref().and_then(|editor| {
                                let editor = editor.read(cx);
                                Some((editor.project()?.clone(), editor.workspace()?.downgrade()))
                            });
                            let mut menu = menu
                                .when_some(active.clone(), |menu, editor| {
                                    let state = &editor.read(cx).explanations;
                                    let message = if let Some(error) = &state.last_error {
                                        error.to_string()
                                    } else if state.dirty {
                                        i18n::t!("9e2d4efc08fa134e").into()
                                    } else {
                                        i18n::t_args!(
                                            "74d3b9f86e04008d",
                                            state.progress.0,
                                            state.progress.1
                                        )
                                    };
                                    menu.entry(message, None, |_, _| {})
                                })
                                .entry(
                                    if settings.enabled {
                                        i18n::t!("255c4678a33efac6")
                                    } else {
                                        i18n::t!("215c0038609b62e5")
                                    },
                                    None,
                                    move |_, cx| {
                                        let enabled =
                                            !CodeExplanationSettings::get_global(cx).enabled;
                                        let fs = workspace::AppState::global(cx).fs.clone();
                                        settings::update_settings_file(
                                            fs,
                                            cx,
                                            move |content, _| {
                                                content
                                                    .code_explanations
                                                    .get_or_insert_default()
                                                    .enabled = Some(enabled);
                                            },
                                        );
                                    },
                                )
                                .when_some(
                                    scan_running.then_some(scan_cancelled.clone()).flatten(),
                                    |menu, cancelled| {
                                        menu.entry(
                                            i18n::t!("464c32bac9406908"),
                                            None,
                                            move |_, _| {
                                                cancelled.store(true, Ordering::SeqCst);
                                            },
                                        )
                                    },
                                )
                                .when(!scan_running && active_project.is_some(), |menu| {
                                    let project = active_project.clone();
                                    menu.entry(
                                        i18n::t!("db60a2f8702a6a6f"),
                                        None,
                                        move |window, cx| {
                                            let Some((project, workspace)) = project.clone() else {
                                                return;
                                            };
                                            show_project_scan_confirmation(
                                                project, workspace, None, window, cx,
                                            );
                                        },
                                    )
                                })
                                .separator();
                            menu = menu.when(!scan_diagnostics.is_empty(), |menu| {
                                let diagnostics = scan_diagnostics.clone();
                                menu.entry(i18n::t!("da72a4729161bc38"), None, move |_, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        diagnostics.clone(),
                                    ));
                                })
                            });
                            let failed_editor = active.clone();
                            menu = menu.entry(i18n::t!("b21ed03849e84fbf"), None, move |_, cx| {
                                if let Some(editor) = &failed_editor {
                                    editor.update(cx, |editor, cx| {
                                        if editor.explanations.busy {
                                            return;
                                        }
                                        editor.explanations.failed.clear();
                                        editor.explanations.last_error = None;
                                        request_refresh(editor);
                                        editor.explanations.last_view = None;
                                        cx.notify();
                                    });
                                }
                            });
                            let stop_editor = active.clone();
                            menu = menu.entry(i18n::t!("315c2b678e53c937"), None, move |_, cx| {
                                if let Some(editor) = &stop_editor {
                                    editor.update(cx, |editor, cx| {
                                        editor
                                            .explanations
                                            .write_generation
                                            .fetch_add(1, Ordering::SeqCst);
                                        editor.explanations.generation =
                                            editor.explanations.generation.wrapping_add(1);
                                        editor.explanations.task = None;
                                        editor
                                            .explanations
                                            .deep_cancelled
                                            .store(true, Ordering::SeqCst);
                                        editor.explanations.deep_task = None;
                                        editor.explanations.busy = false;
                                        editor.explanations.dirty = true;
                                        editor.explanations.refresh_requested = false;
                                        cx.notify();
                                    });
                                }
                            });
                            let retry_editor = active.clone();
                            menu = menu.entry(i18n::t!("7555297858bc72fb"), None, move |_, cx| {
                                if let Some(editor) = &retry_editor {
                                    editor.update(cx, |editor, cx| {
                                        editor.explanations.memory.clear();
                                        editor.explanations.bypass_cache = true;
                                        clear(editor, cx);
                                        cx.notify();
                                    });
                                }
                            });
                            for provider in LanguageModelRegistry::read_global(cx)
                                .visible_providers()
                                .into_iter()
                                .filter(|provider| provider.is_authenticated(cx))
                            {
                                let models = provider.provided_models(cx);
                                menu = menu.submenu(
                                    provider.name().0.clone(),
                                    move |mut menu, _, _| {
                                        for model in &models {
                                            let provider_id = provider.id().0.to_string();
                                            let model_id = model.id().0.to_string();
                                            menu = menu.entry(
                                                format!("{} / {}", provider_id, model.name().0),
                                                None,
                                                move |_, cx| {
                                                    let fs =
                                                        workspace::AppState::global(cx).fs.clone();
                                                    let provider_id = provider_id.clone();
                                                    let model_id = model_id.clone();
                                                    settings::update_settings_file(
                                                        fs,
                                                        cx,
                                                        move |content, _| {
                                                            let settings = content
                                                                .code_explanations
                                                                .get_or_insert_default();
                                                            settings.provider =
                                                                Some(provider_id.into());
                                                            settings.model = Some(model_id.into());
                                                        },
                                                    );
                                                },
                                            );
                                        }
                                        menu
                                    },
                                );
                            }
                            menu.separator()
                                .entry(i18n::t!("1f41c25f8a8429d7"), None, |_, cx| {
                                    code_explanation_file_index(cx).update(cx, |index, cx| {
                                        index.files.clear();
                                        cx.notify();
                                    });
                                    cx.background_spawn(async move {
                                        let _guard = CACHE_LOCK.lock().map_err(|_| {
                                            anyhow::anyhow!(i18n::t!("cb2247f24b40dc8a"))
                                        })?;
                                        CACHE_EPOCH
                                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        SHARED_RESULTS
                                            .lock()
                                            .map_err(|_| {
                                                anyhow::anyhow!(i18n::t!("f3ca8599b189cacd"))
                                            })?
                                            .clear();
                                        let directory = paths::data_dir().join("code-explanations");
                                        if directory.exists() {
                                            trim_global_cache(
                                                &directory,
                                                std::path::Path::new(""),
                                                0,
                                            )?;
                                        }
                                        anyhow::Ok(())
                                    })
                                    .detach_and_log_err(cx);
                                })
                                .entry(i18n::t!("2325bc25f7d5722e"), None, |window, cx| {
                                    window.dispatch_action(
                                        zed_actions::OpenSettingsAt {
                                            path: "code_explanations".into(),
                                            target: None,
                                        }
                                        .boxed_clone(),
                                        cx,
                                    );
                                })
                        }))
                    }),
            )
            .when_some(background_scan_tooltip, |this, tooltip| {
                this.child(
                    IconButton::new("background-code-explanation-scans", IconName::HistoryRerun)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(ui::Tooltip::text(tooltip)),
                )
            })
    }
}

impl workspace::StatusItemView for CodeExplanationIndicator {
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn workspace::item::ItemHandle>,
        _: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.active = item.and_then(|item| item.downcast::<Editor>());
        self.subscription = self
            .active
            .as_ref()
            .map(|editor| cx.observe(editor, |_, _, cx| cx.notify()));
        cx.notify();
    }
}

async fn request_if_authorized(
    mut authorized: impl FnMut(&mut App) -> bool,
    model: ConfiguredModel,
    settings: CodeExplanationSettings,
    code: String,
    cx: &mut gpui::AsyncApp,
) -> Result<SharedString> {
    for attempt in 0..3 {
        anyhow::ensure!(cx.update(&mut authorized), i18n::t!("03d3044b30299a77"));
        match request(model.clone(), settings.clone(), code.clone(), cx).await {
            Ok(text) => return Ok(text),
            Err(error) => {
                let delay = error
                    .downcast_ref::<language_model::LanguageModelCompletionError>()
                    .and_then(rejected_request_retry_delay);
                if attempt == 2 || delay.is_none() {
                    return Err(error);
                }
                if let Some(delay) = delay {
                    cx.background_executor().timer(delay).await;
                }
            }
        }
    }
    anyhow::bail!(i18n::t!("4ce059c8184fad7c"))
}

fn rejected_request_retry_delay(
    error: &language_model::LanguageModelCompletionError,
) -> Option<std::time::Duration> {
    use language_model::LanguageModelCompletionError;
    if let LanguageModelCompletionError::ProviderRejection {
        status: Some(status),
        retry_after,
        ..
    } = error
        && matches!(status.as_u16(), 429 | 503)
    {
        let delay = retry_after.unwrap_or(std::time::Duration::from_secs(2));
        // Do not retry earlier than a provider's requested delay or wait indefinitely in the foreground.
        return (delay <= std::time::Duration::from_secs(30))
            .then_some(delay.max(std::time::Duration::from_millis(250)));
    }
    None
}

pub(crate) async fn request(
    model: ConfiguredModel,
    settings: CodeExplanationSettings,
    code: String,
    cx: &mut gpui::AsyncApp,
) -> Result<SharedString> {
    anyhow::ensure!(
        !cx.update(|cx| project::DisableAiSettings::get_global(cx).disable_ai),
        i18n::t!("0f4b4b34fffd444a")
    );
    anyhow::ensure!(code.len() <= 64 * 1024, i18n::t!("e862135be0181f08"));
    let detail = if settings.detailed {
        i18n::t!("aeefb0767465904b")
    } else {
        i18n::t!("1d1c4c26d7b8ade8")
    };
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(i18n::t_args!(
                    "e8ff425d6c7a1b8a",
                    settings.target_language,
                    detail
                ))],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(code)],
                cache: false,
                reasoning_details: None,
            },
        ],
        temperature: Some(0.2),
        thinking_allowed: false,
        ..Default::default()
    };
    use gpui::FutureExt as _;
    let executor = cx.background_executor().clone();
    let mut stream = model
        .provider
        .stream_completion_text(&model.model, request, cx)
        .with_timeout(std::time::Duration::from_secs(60), &executor)
        .await
        .context(i18n::t!("646651d4f81e90f4"))?
        .map_err(anyhow::Error::new)?;
    let mut output = String::new();
    let started = std::time::Instant::now();
    let mut chunks = 0usize;
    while let Some(chunk) = stream
        .stream
        .next()
        .with_timeout(std::time::Duration::from_secs(30), &executor)
        .await
        .context(i18n::t!("ab5c1069e97e9b68"))?
    {
        anyhow::ensure!(
            !cx.update(|cx| project::DisableAiSettings::get_global(cx).disable_ai),
            i18n::t!("dd230cd531e27def")
        );
        anyhow::ensure!(
            started.elapsed() < std::time::Duration::from_secs(120),
            i18n::t!("e0aef3d3a08a35ad")
        );
        chunks += 1;
        anyhow::ensure!(chunks <= 8192, i18n::t!("af73806c05db33d0"));
        output.push_str(&chunk.map_err(|error| anyhow::anyhow!(error.to_string()))?);
        anyhow::ensure!(output.len() <= 32 * 1024, i18n::t!("c7bddbec8bb2317e"));
    }
    anyhow::ensure!(!output.trim().is_empty(), i18n::t!("20d86f25396ec566"));
    Ok(output.trim().to_string().into())
}
