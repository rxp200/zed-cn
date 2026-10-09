use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use editor::code_explanations::ConfiguredModel;
use editor::{
    Editor,
    code_explanations::{
        CodeExplanationRequestWaiter, CodeExplanationSettings, content_hash, resolve_model,
        selected_provider_configuration,
    },
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use futures::StreamExt as _;
use gpui::{AnyElement, App, ElementId, Entity, SharedString, Task, WeakEntity};
use language_model::{LanguageModelRequest, LanguageModelRequestMessage, MessageContent, Role};
use project::Project;
use serde::Deserialize;
use settings::Settings as _;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use ui::{CommonAnimationExt as _, Disclosure, Tooltip, prelude::*};
use util::ResultExt as _;

const CONTEXT_LINES: u32 = 80;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const REQUEST_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub(crate) struct DiffHunkInput {
    pub(crate) identifier: usize,
    pub(crate) old_start_line: u32,
    pub(crate) new_start_line: u32,
    pub(crate) old_text: String,
    pub(crate) new_text: String,
    pub(crate) anchor: multi_buffer::Anchor,
}

#[derive(Clone)]
pub(crate) struct DiffFileInput {
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) old_text: String,
    pub(crate) new_text: String,
    pub(crate) hunks: Vec<DiffHunkInput>,
    pub(crate) anchor: multi_buffer::Anchor,
    pub(crate) private: bool,
    pub(crate) worktree_id: project::WorktreeId,
}

impl DiffFileInput {
    /// 稳定标识：同一 diff 视图内，工作区与仓库内路径唯一确定一个文件。
    fn key(&self) -> String {
        format!("{}\0{}", self.worktree_id, self.path)
    }
}

/// 一次讲解刷新的来源。
#[derive(Clone)]
pub(crate) enum DiffExplanationMode {
    /// 视图自身刷新：同步文件集合与身份，只重新生成真正发生变化的文件。
    Sync,
    /// 顶部按钮：重新生成全部符合条件的文件。
    ForceAll,
    /// 单个文件按钮：只重新生成该文件。
    ForceFile(String),
}

pub(crate) type DiffExplanationRefresh = Arc<dyn Fn(DiffExplanationMode, &mut App) + Send + Sync>;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum FilePhase {
    #[default]
    Idle,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct DiffExplanationProgress {
    pub total: usize,
    pub idle: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
}

impl DiffExplanationProgress {
    pub(crate) fn started(self) -> bool {
        self.running > 0 || self.done > 0 || self.failed > 0
    }
}

#[derive(Clone, Deserialize)]
struct FileExplanation {
    summary: String,
    #[serde(default)]
    changes: Vec<String>,
    #[serde(default)]
    effects: Vec<String>,
    #[serde(default)]
    risks: Vec<String>,
    #[serde(default)]
    hunks: Vec<HunkExplanation>,
}

#[derive(Clone, Deserialize)]
struct HunkExplanation {
    id: usize,
    explanation: String,
}

struct FileEntry {
    identity: String,
    path: String,
    phase: FilePhase,
    blocks: Vec<CustomBlockId>,
    task: Option<Task<()>>,
    render: Option<FileRender>,
    error: Option<SharedString>,
    expanded: Arc<AtomicBool>,
}

struct FileRender {
    path: String,
    file_anchor: multi_buffer::Anchor,
    hunks: Vec<(usize, multi_buffer::Anchor)>,
    explanation: FileExplanation,
}

impl FileEntry {
    fn new(identity: String, path: String) -> Self {
        Self {
            identity,
            path,
            phase: FilePhase::Idle,
            blocks: Vec::new(),
            task: None,
            render: None,
            error: None,
            expanded: Arc::new(AtomicBool::new(false)),
        }
    }

    fn reset(&mut self, identity: String, path: String) {
        self.identity = identity;
        self.path = path;
        self.render = None;
        self.error = None;
    }

    fn remove_blocks(&mut self, editor: &Entity<Editor>, cx: &mut App) {
        let blocks = std::mem::take(&mut self.blocks);
        if !blocks.is_empty() {
            editor.update(cx, |editor, cx| {
                editor.remove_blocks(blocks.into_iter().collect::<HashSet<_>>(), None, cx)
            });
        }
    }

    /// 按当前阶段重建该文件的讲解块。`anchor` 用于非完成阶段；完成阶段使用
    /// 结果自带的位置，避免文件内容变化后锚点错位。
    fn reinsert(
        &mut self,
        editor: &Entity<Editor>,
        anchor: multi_buffer::Anchor,
        key: &str,
        refresh: Option<DiffExplanationRefresh>,
        cx: &mut App,
    ) {
        self.remove_blocks(editor, cx);
        match self.phase {
            FilePhase::Done => {
                let Some(render) = self.render.take() else {
                    return;
                };
                let mut text = format!("✦ {} — {}", render.path, render.explanation.summary.trim());
                append_items(
                    &mut text,
                    i18n::t!("881ffae80365c1fb"),
                    &render.explanation.changes,
                );
                append_items(
                    &mut text,
                    i18n::t!("40f6d6bab7e54cc4"),
                    &render.explanation.effects,
                );
                append_items(
                    &mut text,
                    i18n::t!("7eff7c0527931d9a"),
                    &render.explanation.risks,
                );
                let collapsible = !render.explanation.changes.is_empty()
                    || !render.explanation.effects.is_empty()
                    || !render.explanation.risks.is_empty();
                let ids = insert_explanation_block(
                    editor,
                    render.file_anchor,
                    text,
                    BlockVisual::Success,
                    collapsible,
                    Some(self.expanded.clone()),
                    Some(BlockButton::Regenerate),
                    key,
                    refresh.clone(),
                    true,
                    cx,
                );
                self.blocks.extend(ids);
                for (identifier, hunk_anchor) in &render.hunks {
                    let Some(hunk) = render
                        .explanation
                        .hunks
                        .iter()
                        .find(|candidate| candidate.id == *identifier)
                    else {
                        continue;
                    };
                    let text =
                        i18n::t_args!("7a8afcc87bb995ba", identifier, hunk.explanation.trim());
                    let ids = insert_explanation_block(
                        editor,
                        *hunk_anchor,
                        text,
                        BlockVisual::Hunk,
                        false,
                        None,
                        None,
                        key,
                        refresh.clone(),
                        false,
                        cx,
                    );
                    self.blocks.extend(ids);
                }
                self.render = Some(render);
            }
            FilePhase::Idle => {
                let text = format!("✦ {} — {}", self.path, i18n::t!("0e90e1672e726d44"));
                let ids = insert_explanation_block(
                    editor,
                    anchor,
                    text,
                    BlockVisual::Info,
                    false,
                    None,
                    Some(BlockButton::Generate),
                    key,
                    refresh,
                    true,
                    cx,
                );
                self.blocks.extend(ids);
            }
            FilePhase::Running => {
                let text = format!("✦ {} — {}", self.path, i18n::t!("80a177dbe35aa045"));
                let ids = insert_explanation_block(
                    editor,
                    anchor,
                    text,
                    BlockVisual::Running,
                    false,
                    None,
                    Some(BlockButton::Generate),
                    key,
                    refresh,
                    true,
                    cx,
                );
                self.blocks.extend(ids);
            }
            FilePhase::Failed => {
                let failure = i18n::t!(
                    "2b5db0cf8e33d422",
                    error = self.error.clone().unwrap_or_default()
                );
                let text = format!("✦ {} — {}", self.path, failure);
                let ids = insert_explanation_block(
                    editor,
                    anchor,
                    text,
                    BlockVisual::Error,
                    false,
                    None,
                    Some(BlockButton::Retry),
                    key,
                    refresh,
                    true,
                    cx,
                );
                self.blocks.extend(ids);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockVisual {
    Info,
    Running,
    Success,
    Error,
    Hunk,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockButton {
    Generate,
    Regenerate,
    Retry,
}

impl BlockButton {
    fn label(self) -> &'static str {
        match self {
            Self::Generate => i18n::t!("2930e90161e327f9"),
            Self::Regenerate => i18n::t!("1651031bf58d8eea"),
            Self::Retry => i18n::t!("942087cc2d41e013"),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Generate => IconName::Sparkle,
            Self::Regenerate | Self::Retry => IconName::RotateCw,
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Regenerate => i18n::t!("53794caa6ec56797"),
            Self::Generate | Self::Retry => i18n::t!("d6e6d3359665a227"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_explanation_block(
    editor: &Entity<Editor>,
    anchor: multi_buffer::Anchor,
    text: String,
    visual: BlockVisual,
    collapsible: bool,
    expanded: Option<Arc<AtomicBool>>,
    button: Option<BlockButton>,
    file_key: &str,
    refresh: Option<DiffExplanationRefresh>,
    prominent: bool,
    cx: &mut App,
) -> Vec<CustomBlockId> {
    let expanded = expanded.unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
    let expanded_for_click = expanded.clone();
    let file_key = file_key.to_string();
    editor.update(cx, |editor, cx| {
        editor.insert_blocks(
            [BlockProperties {
                placement: BlockPlacement::Above(anchor),
                height: Some(1),
                style: BlockStyle::Flex,
                priority: if prominent { 2 } else { 1 },
                render: Arc::new(move |cx| {
                    let is_expanded = expanded.load(Ordering::SeqCst);
                    let border = match visual {
                        BlockVisual::Running | BlockVisual::Success => {
                            cx.theme().colors().border_focused
                        }
                        BlockVisual::Error => cx.theme().status().error,
                        BlockVisual::Info => cx.theme().colors().border_variant,
                        BlockVisual::Hunk => cx.theme().status().success,
                    };
                    let running = matches!(visual, BlockVisual::Running);
                    let disclosure_id =
                        ElementId::from(format!("diff-explain-disclosure-{:?}", cx.block_id));
                    let button_id =
                        ElementId::from(format!("diff-explain-button-{:?}", cx.block_id));
                    v_flex()
                        .w(cx.max_width)
                        .pl(cx.anchor_x)
                        .pr_3()
                        .py_1()
                        .gap_0p5()
                        .border_l_2()
                        .border_color(border)
                        .bg(cx
                            .theme()
                            .colors()
                            .editor_subheader_background
                            .opacity(0.72))
                        .text_color(cx.theme().colors().text_muted)
                        .child(
                            h_flex()
                                .min_w_0()
                                .items_start()
                                .gap_1()
                                .when(running, |element| {
                                    element.child(
                                        Icon::new(IconName::LoadCircle)
                                            .size(IconSize::Small)
                                            .color(Color::Accent)
                                            .with_rotate_animation(3),
                                    )
                                })
                                .when(collapsible, |element| {
                                    let expanded = expanded_for_click.clone();
                                    element.child(
                                        Disclosure::new(disclosure_id, is_expanded)
                                            .tooltip(Tooltip::text(if is_expanded {
                                                i18n::t!("71f5ca13e4cc93ea")
                                            } else {
                                                i18n::t!("3ee938ff83ff88b4")
                                            }))
                                            .on_click(move |_, window, _| {
                                                expanded.fetch_xor(true, Ordering::SeqCst);
                                                window.refresh();
                                            }),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .when(collapsible && !is_expanded, |element| {
                                            element.h(cx.line_height).overflow_hidden()
                                        })
                                        .child(text.clone()),
                                )
                                .when_some(button, |element, button| {
                                    let refresh = refresh.clone();
                                    let file_key = file_key.clone();
                                    element.child(
                                        Button::new(button_id, button.label())
                                            .start_icon(Icon::new(button.icon()))
                                            .disabled(running || refresh.is_none())
                                            .tooltip(Tooltip::text(button.tooltip()))
                                            .on_click(move |_, _, cx| {
                                                cx.stop_propagation();
                                                if let Some(refresh) = &refresh {
                                                    refresh(
                                                        DiffExplanationMode::ForceFile(
                                                            file_key.clone(),
                                                        ),
                                                        cx,
                                                    );
                                                }
                                            }),
                                    )
                                }),
                        )
                        .into_any_element()
                }),
            }],
            None,
            cx,
        )
    })
}

/// 用视图当前的文件集合对齐控制器状态。
///
/// 返回需要重建的文件（请求或占位）以及应删除的文件。身份未变的文件既不在
/// 返回结果里，也不会被重新请求。
fn reconcile(
    existing: &[(String, String, FilePhase)],
    incoming: &[(String, String)],
    force_file: Option<&str>,
    force_all: bool,
) -> (Vec<(String, ReconcileAction)>, Vec<String>) {
    let incoming_keys = incoming
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<HashSet<_>>();
    let drops = existing
        .iter()
        .filter(|(key, _, _)| !incoming_keys.contains(key.as_str()))
        .map(|(key, _, _)| key.clone())
        .collect::<Vec<_>>();

    let mut actions = Vec::new();
    for (key, identity) in incoming {
        let forced = force_all || force_file == Some(key.as_str());
        match existing
            .iter()
            .find(|(existing_key, _, _)| existing_key == key)
        {
            None => actions.push((
                key.clone(),
                if forced {
                    ReconcileAction::Request
                } else {
                    ReconcileAction::Placeholder
                },
            )),
            Some((_, existing_identity, phase)) => {
                if forced || existing_identity != identity {
                    if forced || *phase != FilePhase::Idle {
                        actions.push((key.clone(), ReconcileAction::Request));
                    } else {
                        actions.push((key.clone(), ReconcileAction::Placeholder));
                    }
                }
            }
        }
    }
    (actions, drops)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReconcileAction {
    Request,
    Placeholder,
}

#[derive(Default)]
pub(crate) struct DiffExplanationController {
    generation: u64,
    files: HashMap<String, FileEntry>,
    refresh: Option<DiffExplanationRefresh>,
    configuration_error: Option<SharedString>,
    total: usize,
}

impl DiffExplanationController {
    pub(crate) fn update_from_view(
        controller: &Entity<Self>,
        editor: Entity<Editor>,
        project: Entity<Project>,
        files: Vec<DiffFileInput>,
        mode: DiffExplanationMode,
        refresh: DiffExplanationRefresh,
        cx: &mut App,
    ) {
        let settings = CodeExplanationSettings::get_global(cx).clone();
        if !settings.enabled || project::DisableAiSettings::get_global(cx).disable_ai {
            controller.update(cx, |this, cx| this.clear_all(&editor, cx));
            return;
        }

        let eligible = files
            .into_iter()
            .filter(|file| {
                !file.private && !is_sensitive_path(&file.path) && !file.hunks.is_empty()
            })
            .filter(|file| worktree_is_trusted(&project, file.worktree_id, cx))
            .collect::<Vec<_>>();

        let provider_configuration = selected_provider_configuration(&settings, cx);
        let model_result = resolve_model(&settings, cx);
        let configuration_error = model_result
            .as_ref()
            .err()
            .map(|error| SharedString::from(error.to_string()));

        let mut inputs = HashMap::<String, (String, DiffFileInput)>::default();
        let mut incoming = Vec::with_capacity(eligible.len());
        for file in eligible {
            let key = file.key();
            let identity = file_identity(&settings, &provider_configuration, &file);
            incoming.push((key.clone(), identity.clone()));
            inputs.insert(key, (identity, file));
        }

        let force_file = match &mode {
            DiffExplanationMode::ForceFile(key) => Some(key.clone()),
            DiffExplanationMode::Sync | DiffExplanationMode::ForceAll => None,
        };
        let force_all = matches!(mode, DiffExplanationMode::ForceAll);

        let existing = controller
            .read(cx)
            .files
            .iter()
            .map(|(key, entry)| (key.clone(), entry.identity.clone(), entry.phase))
            .collect::<Vec<_>>();
        let (actions, drops) = reconcile(&existing, &incoming, force_file.as_deref(), force_all);

        let project = project.downgrade();
        controller.update(cx, move |this, cx| {
            this.refresh = Some(refresh);
            this.total = incoming.len();
            this.configuration_error = configuration_error;

            for key in drops {
                this.drop_entry(&editor, &key, cx);
            }

            let model = model_result.ok();
            for (key, action) in actions {
                let Some((identity, file)) = inputs.remove(&key) else {
                    continue;
                };
                match action {
                    ReconcileAction::Placeholder => {
                        let anchor = file.anchor;
                        let path = file.path.clone();
                        let refresh = this.refresh.clone();
                        let entry = this
                            .files
                            .entry(key.clone())
                            .or_insert_with(|| FileEntry::new(identity.clone(), path.clone()));
                        entry.reset(identity, path);
                        entry.phase = FilePhase::Idle;
                        entry.reinsert(&editor, anchor, &key, refresh, cx);
                    }
                    ReconcileAction::Request => {
                        let Some(model) = model.clone() else {
                            let anchor = file.anchor;
                            let path = file.path.clone();
                            let refresh = this.refresh.clone();
                            let entry = this
                                .files
                                .entry(key.clone())
                                .or_insert_with(|| FileEntry::new(identity.clone(), path.clone()));
                            entry.reset(identity, path);
                            entry.phase = FilePhase::Idle;
                            entry.reinsert(&editor, anchor, &key, refresh, cx);
                            continue;
                        };
                        this.start_request(
                            &editor,
                            key,
                            identity,
                            file,
                            model,
                            settings.clone(),
                            provider_configuration.clone(),
                            project.clone(),
                            cx,
                        );
                    }
                }
            }

            cx.notify();
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn start_request(
        &mut self,
        editor: &Entity<Editor>,
        key: String,
        identity: String,
        file: DiffFileInput,
        model: ConfiguredModel,
        settings: CodeExplanationSettings,
        provider_configuration: String,
        project: WeakEntity<Project>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.generation;
        let refresh = self.refresh.clone();
        let file_anchor = file.anchor;
        let hunk_anchors = file
            .hunks
            .iter()
            .map(|hunk| (hunk.identifier, hunk.anchor))
            .collect::<Vec<_>>();

        let entry = self
            .files
            .entry(key.clone())
            .or_insert_with(|| FileEntry::new(identity.clone(), file.path.clone()));
        entry.reset(identity.clone(), file.path.clone());
        entry.phase = FilePhase::Running;
        entry.reinsert(editor, file_anchor, &key, refresh, cx);

        let editor_for_task = editor.downgrade();
        let task_key = key.clone();
        let task_identity = identity.clone();
        let task = cx.spawn(async move |controller, cx| {
            cx.background_executor().timer(REQUEST_DEBOUNCE).await;
            let result = analyze_file(
                model,
                settings,
                provider_configuration,
                &file,
                project,
                editor_for_task.clone(),
                cx,
            )
            .await;
            controller
                .update(cx, move |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    let refresh = this.refresh.clone();
                    let Some(entry) = this.files.get_mut(&task_key) else {
                        return;
                    };
                    if entry.identity != task_identity {
                        return;
                    }
                    entry.task = None;
                    let path = entry.path.clone();
                    match result {
                        Ok(explanation) => {
                            entry.phase = FilePhase::Done;
                            entry.error = None;
                            entry.render = Some(FileRender {
                                path,
                                file_anchor,
                                hunks: hunk_anchors,
                                explanation,
                            });
                        }
                        Err(error) => {
                            entry.phase = FilePhase::Failed;
                            entry.error = Some(SharedString::from(error.to_string()));
                            entry.render = None;
                        }
                    }
                    if let Some(editor) = editor_for_task.upgrade() {
                        entry.reinsert(&editor, file_anchor, &task_key, refresh, cx);
                    }
                    cx.notify();
                })
                .log_err();
        });
        if let Some(entry) = self.files.get_mut(&key) {
            entry.task = Some(task);
        }
        cx.notify();
    }

    pub(crate) fn progress(&self) -> DiffExplanationProgress {
        let mut progress = DiffExplanationProgress {
            total: self.total,
            ..Default::default()
        };
        for entry in self.files.values() {
            match entry.phase {
                FilePhase::Idle => progress.idle += 1,
                FilePhase::Running => progress.running += 1,
                FilePhase::Done => progress.done += 1,
                FilePhase::Failed => progress.failed += 1,
            }
        }
        progress
    }

    fn clear_all(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.total = 0;
        self.configuration_error = None;
        let keys = self.files.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.drop_entry(editor, &key, cx);
        }
        cx.notify();
    }

    fn drop_entry(&mut self, editor: &Entity<Editor>, key: &str, cx: &mut Context<Self>) {
        if let Some(mut entry) = self.files.remove(key) {
            entry.task = None;
            entry.remove_blocks(editor, cx);
        }
    }
}

/// 顶部工具栏控件：生成全部讲解并显示整体进度。没有可讲解的文件或功能关闭时
/// 返回 `None`，不占用任何界面空间。
pub(crate) fn render_explanation_controls(
    controller: &Entity<DiffExplanationController>,
    cx: &mut App,
) -> Option<AnyElement> {
    if !CodeExplanationSettings::get_global(cx).enabled
        || project::DisableAiSettings::get_global(cx).disable_ai
    {
        return None;
    }

    let (progress, error, refresh) = {
        let controller = controller.read(cx);
        (
            controller.progress(),
            controller.configuration_error.clone(),
            controller.refresh.clone(),
        )
    };
    if progress.total == 0 {
        return None;
    }

    let running = progress.running > 0;
    let started = progress.started();
    let completed = progress.done + progress.failed;
    let status = if let Some(error) = &error {
        Label::new(i18n::t!("b9eaf642ed2cfcc1", error = error.clone()))
            .size(LabelSize::Small)
            .color(Color::Error)
            .into_any_element()
    } else if running {
        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Accent)
                    .with_rotate_animation(3),
            )
            .child(
                Label::new(i18n::t!(
                    "0474bdc4d9b49df0",
                    completed = completed,
                    total = progress.total
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    } else if started {
        let mut text = i18n::t!(
            "b1e9dd8eb64b52b6",
            done = progress.done,
            total = progress.total
        );
        if progress.failed > 0 {
            text.push_str(" · ");
            text.push_str(&i18n::t!("98c15fab07867488", failed = progress.failed));
        }
        Label::new(text)
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element()
    } else {
        Label::new(i18n::t!("792bc3db6170925b"))
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element()
    };

    let button_label = if started {
        i18n::t!("c5793fddb5786521")
    } else {
        i18n::t!("35b28a285178cda4")
    };
    let button_tooltip = if started {
        i18n::t!("948305da3407f7e0")
    } else {
        i18n::t!("352e771623dce976")
    };
    let can_generate = refresh.is_some() && error.is_none();
    let disabled = running || !can_generate;

    Some(
        h_flex()
            .w_full()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().editor_subheader_background)
            .child(
                Icon::new(IconName::Sparkle)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
            .child(
                Label::new(i18n::t!("089c5937ddac41ed"))
                    .size(LabelSize::Small)
                    .color(Color::Default),
            )
            .child(status)
            .child(div().flex_1())
            .child(
                Button::new("diff-explanations-generate-all", button_label)
                    .start_icon(Icon::new(IconName::Sparkle))
                    .disabled(disabled)
                    .tooltip(Tooltip::text(button_tooltip))
                    .on_click(move |_, _, cx| {
                        if let Some(refresh) = &refresh {
                            refresh(DiffExplanationMode::ForceAll, cx);
                        }
                    }),
            )
            .into_any_element(),
    )
}
fn append_items(output: &mut String, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    output.push_str("\n");
    output.push_str(heading);
    output.push('：');
    for item in items {
        output.push_str("\n• ");
        output.push_str(item.trim());
    }
}

fn is_sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name.starts_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name == "credentials"
        || name == "credentials.json"
        || lower.contains("/.ssh/")
}

fn worktree_is_trusted(
    project: &Entity<Project>,
    worktree_id: project::WorktreeId,
    cx: &mut App,
) -> bool {
    let Some(trust) = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx) else {
        return false;
    };
    let store = project.read(cx).worktree_store();
    trust.update(cx, |trust, cx| trust.can_trust(&store, worktree_id, cx))
}
fn file_identity(
    settings: &CodeExplanationSettings,
    provider_configuration: &str,
    file: &DiffFileInput,
) -> String {
    content_hash(&format!(
        "git-diff-v2:{settings:?}:{provider_configuration}:{}\0{}\0{}",
        file.path, file.old_text, file.new_text
    ))
}

async fn analyze_file(
    model: ConfiguredModel,
    settings: CodeExplanationSettings,
    provider_configuration: String,
    file: &DiffFileInput,
    project: WeakEntity<Project>,
    editor: WeakEntity<Editor>,
    cx: &mut gpui::AsyncApp,
) -> Result<FileExplanation> {
    let Some(project_id) = project.upgrade().map(|project| project.entity_id()) else {
        anyhow::bail!(i18n::t!("aec2ea947026035c"));
    };
    ensure_authorized(&settings, &provider_configuration, &project, file, cx)?;
    let prompt = build_file_prompt(
        file,
        model.model.max_token_count().min(usize::MAX as u64) as usize,
        |text| model.model.estimate_tokens(text).min(usize::MAX as u64) as usize,
    )?;
    let request_key = format!(
        "git-diff:{}",
        file_identity(&settings, &provider_configuration, file)
    );
    let waiting = CodeExplanationRequestWaiter::new(project_id, request_key, 1)?;
    let permit = loop {
        if let Some(permit) = waiting.acquire(settings.max_concurrent_requests) {
            editor.update(cx, |_, cx| cx.notify()).ok();
            break permit;
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        ensure_authorized(&settings, &provider_configuration, &project, file, cx)?;
    };
    drop(waiting);
    let request_result =
        request_json(&model, &settings.target_language, file_prompt(), prompt, cx).await;
    drop(permit);
    editor.update(cx, |_, cx| cx.notify()).ok();
    let output = request_result?;
    ensure_authorized(&settings, &provider_configuration, &project, file, cx)?;
    let explanation = parse_json::<FileExplanation>(&output)?;
    validate_hunk_explanations(file, &explanation)?;
    Ok(explanation)
}
fn ensure_authorized(
    settings: &CodeExplanationSettings,
    provider_configuration: &str,
    project: &gpui::WeakEntity<Project>,
    file: &DiffFileInput,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let authorized = cx.update(|cx| {
        if project::DisableAiSettings::get_global(cx).disable_ai
            || format!("{:?}", CodeExplanationSettings::get_global(cx)) != format!("{settings:?}")
            || selected_provider_configuration(CodeExplanationSettings::get_global(cx), cx)
                != provider_configuration
        {
            return false;
        }
        let Some(project) = project.upgrade() else {
            return false;
        };
        worktree_is_trusted(&project, file.worktree_id, cx)
    });
    anyhow::ensure!(authorized, i18n::t!("a9f11be05ca4d94a"));
    Ok(())
}

async fn request_json(
    model: &ConfiguredModel,
    target_language: &str,
    instruction: &str,
    input: String,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(i18n::t!(
                    "cb60ea548f7ab2dd",
                    target_language = target_language,
                    instruction = instruction
                ))],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(input)],
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
        .with_timeout(Duration::from_secs(60), &executor)
        .await
        .context(i18n::t!("541b31e8bb3607cf"))?
        .map_err(anyhow::Error::new)?;
    let mut output = String::new();
    let started = std::time::Instant::now();
    while let Some(chunk) = stream
        .stream
        .next()
        .with_timeout(Duration::from_secs(30), &executor)
        .await
        .context(i18n::t!("bb1402abe32b0a61"))?
    {
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(180),
            i18n::t!("2a73bd3729db09e5")
        );
        output.push_str(&chunk.map_err(|error| anyhow::anyhow!(error.to_string()))?);
        anyhow::ensure!(
            output.len() <= MAX_RESPONSE_BYTES,
            i18n::t!("89d091bcb2a2bbe8")
        );
    }
    anyhow::ensure!(!output.trim().is_empty(), i18n::t!("3bc258504b006526"));
    Ok(output)
}

fn parse_json<T: for<'de> Deserialize<'de>>(output: &str) -> Result<T> {
    let trimmed = output.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .strip_suffix("```")
        .unwrap_or(trimmed)
        .trim();
    serde_json::from_str(trimmed).context("模型返回的修改说明格式无效")
}

fn validate_hunk_explanations(file: &DiffFileInput, explanation: &FileExplanation) -> Result<()> {
    let expected = file
        .hunks
        .iter()
        .map(|hunk| hunk.identifier)
        .collect::<HashSet<_>>();
    let actual = explanation
        .hunks
        .iter()
        .map(|hunk| hunk.id)
        .collect::<HashSet<_>>();
    anyhow::ensure!(
        actual.len() == explanation.hunks.len() && actual == expected,
        i18n::t!("986a3f3f4529516c")
    );
    anyhow::ensure!(
        explanation
            .hunks
            .iter()
            .all(|hunk| !hunk.explanation.trim().is_empty()),
        i18n::t!("df6db9f9d6e57162")
    );
    Ok(())
}

fn file_prompt() -> &'static str {
    i18n::t!("69b5e1e105c11f26")
}

fn build_file_prompt(
    file: &DiffFileInput,
    max_tokens: usize,
    estimate_tokens: impl Fn(&str) -> usize,
) -> Result<String> {
    let full = i18n::t_args!(
        "f9e609d567f11b53",
        file.path,
        file.language,
        file.old_text,
        file.new_text,
        render_hunks(&file.hunks)
    );
    let budget = max_tokens.saturating_sub(4096);
    if estimate_tokens(&full) <= budget {
        return Ok(full);
    }

    let contextual = i18n::t_args!(
        "cfd6994600f24d85",
        file.path,
        file.language,
        CONTEXT_LINES,
        render_contextual_hunks(file)
    );
    if estimate_tokens(&contextual) <= budget {
        return Ok(contextual);
    }

    let exact_hunks = i18n::t_args!(
        "ab4272f8bd84666a",
        file.path,
        file.language,
        render_hunks(&file.hunks)
    );
    anyhow::ensure!(
        budget > 0 && estimate_tokens(&exact_hunks) <= budget,
        i18n::t_args!("a472c1996e2acc03", file.path)
    );
    Ok(exact_hunks)
}

fn render_hunks(hunks: &[DiffHunkInput]) -> String {
    hunks
        .iter()
        .map(|hunk| {
            i18n::t_args!(
                "ca31662fadd98468",
                hunk.identifier,
                hunk.old_start_line + 1,
                hunk.new_start_line + 1,
                if hunk.old_text.is_empty() {
                    i18n::t!("c7bcc6d27f3abaaa")
                } else {
                    &hunk.old_text
                },
                if hunk.new_text.is_empty() {
                    i18n::t!("c7bcc6d27f3abaaa")
                } else {
                    &hunk.new_text
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_contextual_hunks(file: &DiffFileInput) -> String {
    file.hunks
        .iter()
        .map(|hunk| {
            let old = line_window(&file.old_text, hunk.old_start_line, CONTEXT_LINES);
            let new = line_window(&file.new_text, hunk.new_start_line, CONTEXT_LINES);
            i18n::t_args!(
                "2eebfaf93a2b0b7f",
                hunk.identifier,
                old,
                new,
                if hunk.old_text.is_empty() {
                    i18n::t!("c7bcc6d27f3abaaa")
                } else {
                    &hunk.old_text
                },
                if hunk.new_text.is_empty() {
                    i18n::t!("c7bcc6d27f3abaaa")
                } else {
                    &hunk.new_text
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn line_window(text: &str, center: u32, radius: u32) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = center.saturating_sub(radius) as usize;
    let end = (center.saturating_add(radius).saturating_add(1) as usize).min(lines.len());
    lines
        .get(start.min(lines.len())..end)
        .unwrap_or_default()
        .join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_file(path: &str, old_text: &str, new_text: &str) -> DiffFileInput {
        DiffFileInput {
            path: path.into(),
            language: "Rust".into(),
            old_text: old_text.into(),
            new_text: new_text.into(),
            hunks: vec![DiffHunkInput {
                identifier: 1,
                old_start_line: 0,
                new_start_line: 0,
                old_text: old_text.into(),
                new_text: new_text.into(),
                anchor: multi_buffer::Anchor::Min,
            }],
            anchor: multi_buffer::Anchor::Min,
            private: false,
            worktree_id: project::WorktreeId::from_usize(1),
        }
    }

    fn sample_settings() -> CodeExplanationSettings {
        CodeExplanationSettings {
            enabled: true,
            provider: Some("test-provider".into()),
            model: Some("test-model".into()),
            target_language: "中文".into(),
            max_function_lines: 500,
            max_concurrent_requests: 5,
            preload_lines: 100,
            detailed: false,
            prefer_existing_comments: true,
            cache_persist: true,
            cache_max_bytes: 50 * 1024 * 1024,
        }
    }

    #[test]
    fn reconcile_requests_only_the_changed_file() {
        let existing = vec![
            ("a".to_string(), "a1".to_string(), FilePhase::Done),
            ("b".to_string(), "b1".to_string(), FilePhase::Done),
        ];
        let incoming = vec![
            ("a".to_string(), "a2".to_string()),
            ("b".to_string(), "b1".to_string()),
        ];
        let (actions, drops) = reconcile(&existing, &incoming, None, false);
        assert_eq!(actions, vec![("a".to_string(), ReconcileAction::Request)]);
        assert!(drops.is_empty());
    }

    #[test]
    fn reconcile_keeps_unchanged_files_untouched() {
        let existing = vec![("a".to_string(), "a1".to_string(), FilePhase::Done)];
        let incoming = vec![("a".to_string(), "a1".to_string())];
        let (actions, drops) = reconcile(&existing, &incoming, None, false);
        assert!(actions.is_empty());
        assert!(drops.is_empty());
    }

    #[test]
    fn reconcile_force_file_targets_only_that_file() {
        let existing = vec![
            ("a".to_string(), "a1".to_string(), FilePhase::Done),
            ("b".to_string(), "b1".to_string(), FilePhase::Done),
        ];
        let incoming = vec![
            ("a".to_string(), "a1".to_string()),
            ("b".to_string(), "b1".to_string()),
        ];
        let (actions, drops) = reconcile(&existing, &incoming, Some("b"), false);
        assert_eq!(actions, vec![("b".to_string(), ReconcileAction::Request)]);
        assert!(drops.is_empty());
    }

    #[test]
    fn reconcile_force_all_requests_every_file() {
        let existing = vec![("a".to_string(), "a1".to_string(), FilePhase::Done)];
        let incoming = vec![
            ("a".to_string(), "a1".to_string()),
            ("b".to_string(), "b1".to_string()),
        ];
        let (actions, drops) = reconcile(&existing, &incoming, None, true);
        assert_eq!(
            actions,
            vec![
                ("a".to_string(), ReconcileAction::Request),
                ("b".to_string(), ReconcileAction::Request),
            ]
        );
        assert!(drops.is_empty());
    }

    #[test]
    fn reconcile_does_not_auto_start_new_files() {
        let existing = Vec::new();
        let incoming = vec![("a".to_string(), "a1".to_string())];
        let (actions, drops) = reconcile(&existing, &incoming, None, false);
        assert_eq!(
            actions,
            vec![("a".to_string(), ReconcileAction::Placeholder)]
        );
        assert!(drops.is_empty());
    }

    #[test]
    fn reconcile_replaces_placeholder_for_changed_idle_file() {
        let existing = vec![("a".to_string(), "a1".to_string(), FilePhase::Idle)];
        let incoming = vec![("a".to_string(), "a2".to_string())];
        let (actions, drops) = reconcile(&existing, &incoming, None, false);
        assert_eq!(
            actions,
            vec![("a".to_string(), ReconcileAction::Placeholder)]
        );
        assert!(drops.is_empty());
    }

    #[test]
    fn reconcile_regenerates_changed_running_file() {
        let existing = vec![("a".to_string(), "a1".to_string(), FilePhase::Running)];
        let incoming = vec![("a".to_string(), "a2".to_string())];
        let (actions, _) = reconcile(&existing, &incoming, None, false);
        assert_eq!(actions, vec![("a".to_string(), ReconcileAction::Request)]);
    }

    #[test]
    fn reconcile_drops_removed_files() {
        let existing = vec![
            ("a".to_string(), "a1".to_string(), FilePhase::Done),
            ("b".to_string(), "b1".to_string(), FilePhase::Done),
        ];
        let incoming = vec![("a".to_string(), "a1".to_string())];
        let (actions, drops) = reconcile(&existing, &incoming, None, false);
        assert!(actions.is_empty());
        assert_eq!(drops, vec!["b".to_string()]);
    }

    #[test]
    fn file_identity_tracks_its_own_content_only() {
        let settings = sample_settings();
        let provider = "provider";
        let first = sample_file("src/a.rs", "old", "new");
        let changed = sample_file("src/a.rs", "old", "newer");
        let other = sample_file("src/b.rs", "old", "new");
        assert_ne!(
            file_identity(&settings, provider, &first),
            file_identity(&settings, provider, &changed)
        );
        assert_ne!(
            file_identity(&settings, provider, &first),
            file_identity(&settings, provider, &other)
        );
    }

    #[test]
    fn file_key_separates_worktrees() {
        let mut first = sample_file("src/a.rs", "old", "new");
        let mut second = first.clone();
        second.worktree_id = project::WorktreeId::from_usize(2);
        assert_ne!(first.key(), second.key());
        first.worktree_id = project::WorktreeId::from_usize(2);
        assert_eq!(first.key(), second.key());
    }

    #[test]
    fn large_file_prompt_keeps_every_hunk_and_context() {
        let text = (0..500)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let file = DiffFileInput {
            path: "src/example.rs".into(),
            language: "Rust".into(),
            old_text: text.clone(),
            new_text: text,
            hunks: vec![
                DiffHunkInput {
                    identifier: 1,
                    old_start_line: 20,
                    new_start_line: 20,
                    old_text: "old one".into(),
                    new_text: "new one".into(),
                    anchor: multi_buffer::Anchor::Min,
                },
                DiffHunkInput {
                    identifier: 2,
                    old_start_line: 420,
                    new_start_line: 420,
                    old_text: "old two".into(),
                    new_text: "new two".into(),
                    anchor: multi_buffer::Anchor::Max,
                },
            ],
            anchor: multi_buffer::Anchor::Min,
            private: false,
            worktree_id: project::WorktreeId::from_usize(1),
        };
        let prompt = build_file_prompt(&file, 5000, |text| text.len()).unwrap();
        assert!(prompt.contains("修改块 1"));
        assert!(prompt.contains("修改块 2"));
        assert!(prompt.contains("old one"));
        assert!(prompt.contains("new two"));
    }

    #[test]
    fn over_budget_exact_hunks_fail_instead_of_sending_partial_change() {
        let file = DiffFileInput {
            path: "src/example.rs".into(),
            language: "Rust".into(),
            old_text: "old".repeat(100),
            new_text: "new".repeat(100),
            hunks: vec![DiffHunkInput {
                identifier: 1,
                old_start_line: 0,
                new_start_line: 0,
                old_text: "removed".repeat(100),
                new_text: "added".repeat(100),
                anchor: multi_buffer::Anchor::Min,
            }],
            anchor: multi_buffer::Anchor::Min,
            private: false,
            worktree_id: project::WorktreeId::from_usize(1),
        };
        let error = build_file_prompt(&file, 4100, str::len).unwrap_err();
        assert!(error.to_string().contains("超过所选模型上下文预算"));
    }

    #[test]
    fn hunk_response_must_cover_every_hunk_once() {
        let file = DiffFileInput {
            path: "src/example.rs".into(),
            language: "Rust".into(),
            old_text: String::new(),
            new_text: String::new(),
            hunks: vec![
                DiffHunkInput {
                    identifier: 1,
                    old_start_line: 0,
                    new_start_line: 0,
                    old_text: String::new(),
                    new_text: "one".into(),
                    anchor: multi_buffer::Anchor::Min,
                },
                DiffHunkInput {
                    identifier: 2,
                    old_start_line: 2,
                    new_start_line: 2,
                    old_text: String::new(),
                    new_text: "two".into(),
                    anchor: multi_buffer::Anchor::Max,
                },
            ],
            anchor: multi_buffer::Anchor::Min,
            private: false,
            worktree_id: project::WorktreeId::from_usize(1),
        };
        let incomplete = FileExplanation {
            summary: "summary".into(),
            changes: Vec::new(),
            effects: Vec::new(),
            risks: Vec::new(),
            hunks: vec![HunkExplanation {
                id: 1,
                explanation: "one".into(),
            }],
        };
        assert!(validate_hunk_explanations(&file, &incomplete).is_err());
    }

    #[test]
    fn sensitive_paths_are_rejected() {
        assert!(is_sensitive_path("service/.env.production"));
        assert!(is_sensitive_path("keys/client.pem"));
        assert!(!is_sensitive_path("src/order.rs"));
    }
}
