use git::repository::{Branch, CommitSummary, OutgoingCommits, RepoPath, UpstreamTracking};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, SharedString, Styled, Task, WeakEntity, Window, div,
    prelude::FluentBuilder, rems, uniform_list,
};
use project::git_store::{CommitDiff, OutgoingCommitsError, Repository};
use ui::prelude::*;
use ui::{Divider, LabelSize, ListSeparator};
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt};

use crate::{commit_view::CommitView, git_graph::format_timestamp};

/// How many commits the push preview loads at most. Kept small so opening the
/// dialog stays cheap on branches with a long unpublished history.
pub(crate) const MAX_PREVIEW_COMMITS: usize = 200;

/// The commits the next push would send, newest first.
enum CommitsState {
    Loading,
    Ready(OutgoingCommits),
    Failed(SharedString),
}

/// The files changed by the currently selected commit.
enum FilesState {
    Idle,
    Loading {
        commit_sha: SharedString,
    },
    Ready {
        commit_sha: SharedString,
        files: Vec<PreviewFile>,
    },
    Failed {
        commit_sha: SharedString,
        error: SharedString,
    },
}

#[derive(Clone)]
struct PreviewFile {
    path: RepoPath,
    kind: PreviewFileKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreviewFileKind {
    Added,
    Modified,
    Deleted,
    Binary,
}

impl PreviewFileKind {
    fn from_commit_file(file: &project::git_store::CommitFile) -> Self {
        if file.is_binary {
            Self::Binary
        } else if file.old_text.is_none() {
            Self::Added
        } else if file.new_text.is_none() {
            Self::Deleted
        } else {
            Self::Modified
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Binary => "B",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Added => Color::Created,
            Self::Modified => Color::Modified,
            Self::Deleted => Color::Deleted,
            Self::Binary => Color::Muted,
        }
    }
}

/// Where the next push of the current branch would go.
struct PushTarget {
    local_branch: SharedString,
    remote: Option<SharedString>,
    remote_branch: SharedString,
    /// True when the branch has no upstream yet and pushing publishes it.
    is_publish: bool,
    multiple_remotes: bool,
}

impl PushTarget {
    fn resolve(branch: &Branch, remotes: &[git::repository::Remote]) -> Self {
        let local_branch: SharedString = branch.name().to_owned().into();

        if let Some(upstream) = branch
            .upstream
            .as_ref()
            .filter(|upstream| matches!(upstream.tracking, UpstreamTracking::Tracked(_)))
        {
            let remote = upstream.remote_name().map(SharedString::from);
            let remote_branch = upstream
                .branch_name()
                .map(SharedString::from)
                .unwrap_or_else(|| local_branch.clone());
            return Self {
                local_branch,
                remote,
                remote_branch,
                is_publish: false,
                multiple_remotes: false,
            };
        }

        let remote = match remotes {
            [remote] => Some(remote.name.clone()),
            _ => None,
        };
        Self {
            local_branch: local_branch.clone(),
            remote,
            remote_branch: local_branch,
            is_publish: true,
            multiple_remotes: remotes.len() > 1,
        }
    }

    fn upstream_ref(branch: &Branch) -> Option<SharedString> {
        branch
            .upstream
            .as_ref()
            .filter(|upstream| matches!(upstream.tracking, UpstreamTracking::Tracked(_)))
            .map(|upstream| upstream.ref_name.clone())
    }

    fn display_target(&self) -> String {
        match &self.remote {
            Some(remote) => format!("{remote}/{}", self.remote_branch),
            None => self.remote_branch.to_string(),
        }
    }

    fn can_push(&self) -> bool {
        !self.is_publish || self.remote.is_some()
    }
}

pub struct PushPreview {
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    target: Option<PushTarget>,
    behind: u32,
    commits: CommitsState,
    selected_commit: usize,
    files: FilesState,
    pushing: bool,
    focus_handle: FocusHandle,
    _load_task: Task<()>,
    _files_task: Task<()>,
}

impl PushPreview {
    pub(crate) fn deploy(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: anyhow::Result<()> =
                        Err(anyhow::anyhow!(i18n::t!("dcebdaca81515f31")));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };

        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(repository, workspace_handle, window, cx)
        });
    }

    pub(crate) fn new(
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            repository,
            workspace,
            target: None,
            behind: 0,
            commits: CommitsState::Loading,
            selected_commit: 0,
            files: FilesState::Idle,
            pushing: false,
            focus_handle: cx.focus_handle(),
            _load_task: Task::ready(()),
            _files_task: Task::ready(()),
        };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.repository.read(cx);
        let Some(branch) = snapshot.branch.clone() else {
            self.commits = CommitsState::Failed(i18n::t!("e9593dd7e8f8b9b3").into());
            return;
        };
        let behind = branch
            .upstream
            .as_ref()
            .and_then(|upstream| upstream.tracking.status())
            .map_or(0, |status| status.behind);
        let upstream_ref = PushTarget::upstream_ref(&branch);
        let remotes_task = self
            .repository
            .update(cx, |repository, _| repository.get_remotes(None, true));
        let commits_task = self.repository.update(cx, |repository, cx| {
            repository.outgoing_commits(upstream_ref, MAX_PREVIEW_COMMITS, cx)
        });

        self._load_task = cx.spawn(async move |this, cx| {
            let remotes = match remotes_task.await {
                Ok(Ok(remotes)) => remotes,
                Ok(Err(error)) => {
                    log::warn!("failed to list remotes for push preview: {error:#}");
                    Vec::new()
                }
                Err(_) => Vec::new(),
            };
            let commits = match commits_task.await {
                Ok(Ok(outgoing)) => CommitsState::Ready(outgoing),
                Ok(Err(OutgoingCommitsError::UnsupportedRemoteServer)) => {
                    CommitsState::Failed(i18n::t!("65d18559a0d3d38b").into())
                }
                Ok(Err(OutgoingCommitsError::Failed(error))) => CommitsState::Failed(
                    i18n::t!("4a430850d48a2345", error = format!("{error:#}")).into(),
                ),
                Err(_) => CommitsState::Failed(i18n::t!("8e90bdb8eade02e0").into()),
            };
            let target = PushTarget::resolve(&branch, &remotes);

            this.update(cx, |this, cx| {
                this.target = Some(target);
                this.behind = behind;
                this.commits = commits;
                this.select_commit(0, cx);
            })
            .ok();
        });
    }

    fn select_commit(&mut self, index: usize, cx: &mut Context<Self>) {
        let commit = match &self.commits {
            CommitsState::Ready(outgoing) => outgoing.commits.get(index),
            _ => None,
        };
        let Some(commit) = commit else {
            self.selected_commit = 0;
            self.files = FilesState::Idle;
            cx.notify();
            return;
        };

        self.selected_commit = index;
        let commit_sha = commit.sha.clone();
        self.files = FilesState::Loading {
            commit_sha: commit_sha.clone(),
        };

        let repository = self.repository.clone();
        let load = repository.update(cx, |repository, cx| {
            repository.load_commit_diff(commit_sha.to_string(), false, cx)
        });
        self._files_task = cx.spawn(async move |this, cx| {
            let files = match load.await {
                Ok(CommitDiff { files, .. }) => FilesState::Ready {
                    commit_sha: commit_sha.clone(),
                    files: files
                        .iter()
                        .map(|file| PreviewFile {
                            path: file.path.clone(),
                            kind: PreviewFileKind::from_commit_file(file),
                        })
                        .collect(),
                },
                Err(error) => FilesState::Failed {
                    commit_sha: commit_sha.clone(),
                    error: format!("{error:#}").into(),
                },
            };

            this.update(cx, |this, cx| {
                if this.files_commit_sha() != Some(commit_sha.as_ref()) {
                    return;
                }
                this.files = files;
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn files_commit_sha(&self) -> Option<&str> {
        match &self.files {
            FilesState::Idle => None,
            FilesState::Loading { commit_sha }
            | FilesState::Ready { commit_sha, .. }
            | FilesState::Failed { commit_sha, .. } => Some(commit_sha.as_ref()),
        }
    }

    fn can_push(&self) -> bool {
        if self.pushing {
            return false;
        }
        let Some(target) = self.target.as_ref() else {
            return false;
        };
        target.can_push()
            && matches!(&self.commits, CommitsState::Ready(outgoing) if !outgoing.commits.is_empty())
    }

    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_push() {
            return;
        }
        self.pushing = true;
        cx.notify();
        // Dismiss first so the remote picker the push command may open is not
        // immediately replaced by this dialog closing on top of it.
        cx.emit(DismissEvent);
        window.dispatch_action(Box::new(git::Push), cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.push(window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn open_file(&mut self, file: RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let FilesState::Ready { commit_sha, .. } = &self.files else {
            return;
        };
        let commit_sha = commit_sha.clone();
        let repository = self.repository.downgrade();
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        cx.defer_in(window, move |_, window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            CommitView::open(
                commit_sha.to_string(),
                repository,
                workspace.downgrade(),
                None,
                Some(file),
                window,
                cx,
            );
        });
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let target_line = self
            .target
            .as_ref()
            .map(|target| format!("{} → {}", target.local_branch, target.display_target()));

        v_flex()
            .w_full()
            .px(DynamicSpacing::Base12.rems(cx))
            .pt(DynamicSpacing::Base08.rems(cx))
            .pb(DynamicSpacing::Base04.rems(cx))
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::Small))
                    .child(Headline::new(i18n::t!("9be0e515f47c857f")).size(HeadlineSize::XSmall)),
            )
            .when_some(target_line, |this, target_line| {
                this.child(
                    Label::new(target_line)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(self.target.as_ref(), |this, target| {
                this.when(target.is_publish, |this| {
                    this.child(
                        Label::new(i18n::t!("b14c0f8a0829a892"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .when(target.multiple_remotes, |this| {
                    this.child(
                        Label::new(i18n::t!("d8e7d8f8037ca77d"))
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                })
                .when(
                    target.remote.is_none() && !target.multiple_remotes,
                    |this| {
                        this.child(
                            Label::new(i18n::t!("ed4450142e35339e"))
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    },
                )
            })
            .when(self.behind > 0, |this| {
                this.child(
                    Label::new(i18n::t!("bdfd9fa9319280a0"))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
    }

    fn render_commits(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let commit_count = match &self.commits {
            CommitsState::Ready(outgoing) => Some(outgoing.commits.len()),
            _ => None,
        };
        let commits_loading = matches!(&self.commits, CommitsState::Loading);
        let truncated =
            matches!(&self.commits, CommitsState::Ready(outgoing) if outgoing.truncated);

        let header = match commit_count {
            Some(count) => Some(
                Label::new(i18n::t!("9dbd19c4e1a2db2a", count = count))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
            None if commits_loading => Some(
                Label::new(i18n::t!("4927a53bcc886afb"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
            None => None,
        };

        let body = match &self.commits {
            CommitsState::Loading => div()
                .p(DynamicSpacing::Base08.rems(cx))
                .child(
                    Label::new(i18n::t!("db91afbf3290c93b"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            CommitsState::Failed(error) => div()
                .p(DynamicSpacing::Base08.rems(cx))
                .child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .into_any_element(),
            CommitsState::Ready(outgoing) if outgoing.commits.is_empty() => div()
                .p(DynamicSpacing::Base08.rems(cx))
                .child(
                    Label::new(i18n::t!("170d6d5965657301"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            CommitsState::Ready(outgoing) => {
                let commits = outgoing.commits.clone();
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list("push-preview-commits", commits.len(), {
                            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                                let mut rows = Vec::with_capacity(range.len());
                                for index in range {
                                    let Some(commit) = commits.get(index) else {
                                        continue;
                                    };
                                    rows.push(
                                        this.render_commit_row(index, commit, cx)
                                            .into_any_element(),
                                    );
                                }
                                rows
                            })
                        })
                        .flex_1(),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .w(rems(25.))
            .h_full()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                div()
                    .px(DynamicSpacing::Base08.rems(cx))
                    .py_1()
                    .when_some(header, |this, header| this.child(header)),
            )
            .child(Divider::horizontal())
            .child(body)
            .when(truncated, |this| {
                this.child(
                    div().px(DynamicSpacing::Base08.rems(cx)).py_1().child(
                        Label::new(i18n::t!("59855e20905ca25b", count = MAX_PREVIEW_COMMITS))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
    }

    fn render_commit_row(
        &self,
        index: usize,
        commit: &CommitSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected_commit == index;
        let short_sha: SharedString = commit.sha.chars().take(8).collect::<String>().into();
        let time = format_timestamp(commit.commit_timestamp);

        h_flex()
            .id(("push-preview-commit", index))
            .w_full()
            .px(DynamicSpacing::Base08.rems(cx))
            .py_1()
            .gap_2()
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.select_commit(index, cx);
            }))
            .child(
                Icon::new(IconName::GitCommit)
                    .size(IconSize::XSmall)
                    .color(if selected {
                        Color::Accent
                    } else {
                        Color::Muted
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(commit.subject.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(short_sha)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(commit.author_name.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .child(Label::new(time).size(LabelSize::XSmall).color(Color::Muted)),
                    ),
            )
    }

    fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let file_count = match &self.files {
            FilesState::Ready { files, .. } => Some(files.len()),
            _ => None,
        };
        let files_loading = matches!(&self.files, FilesState::Loading { .. });
        let header = match file_count {
            Some(count) => Some(
                Label::new(i18n::t!("28e9cd2c92ccc31e", count = count))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
            None if files_loading => Some(
                Label::new(i18n::t!("4927a53bcc886afb"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
            None => None,
        };

        let body = match &self.files {
            FilesState::Idle | FilesState::Loading { .. } => div()
                .p(DynamicSpacing::Base08.rems(cx))
                .child(
                    Label::new(i18n::t!("1b4428edaa790df6"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            FilesState::Failed { error, .. } => div()
                .p(DynamicSpacing::Base08.rems(cx))
                .child(
                    Label::new(i18n::t!("f92c726b2ca97879", error = error.clone()))
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .into_any_element(),
            FilesState::Ready { files, .. } if files.is_empty() => div()
                .p(DynamicSpacing::Base08.rems(cx))
                .child(
                    Label::new(i18n::t!("ea747d1520c12441"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            FilesState::Ready { files, .. } => {
                let files: Vec<PreviewFile> = files.iter().cloned().collect();
                let file_rows: Vec<AnyElement> = files
                    .into_iter()
                    .enumerate()
                    .map(|(index, file)| self.render_file_row(index, file, cx).into_any_element())
                    .collect();
                div()
                    .id("push-preview-files")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(file_rows)
                    .into_any_element()
            }
        };

        v_flex()
            .flex_1()
            .h_full()
            .min_h_0()
            .child(
                div()
                    .px(DynamicSpacing::Base08.rems(cx))
                    .py_1()
                    .when_some(header, |this, header| this.child(header)),
            )
            .child(Divider::horizontal())
            .child(body)
    }

    fn render_file_row(
        &self,
        index: usize,
        file: PreviewFile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = file.path.clone();
        let full_path: SharedString = path.as_unix_str().to_string().into();
        let file_name: SharedString = full_path
            .rsplit('/')
            .next()
            .unwrap_or(full_path.as_ref())
            .to_string()
            .into();
        let kind = file.kind;

        h_flex()
            .id(("push-preview-file", index))
            .w_full()
            .px(DynamicSpacing::Base08.rems(cx))
            .py_0p5()
            .gap_2()
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_file(path.clone(), window, cx);
            }))
            .child(
                Label::new(kind.label())
                    .size(LabelSize::XSmall)
                    .color(kind.color()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(file_name).size(LabelSize::Small).truncate())
                    .child(
                        Label::new(full_path)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .justify_end()
            .gap_1()
            .p(DynamicSpacing::Base08.rems(cx))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("push-preview-cancel", i18n::t!("2cd0f3be8738a86c"))
                    .size(ButtonSize::Medium)
                    .on_click(cx.listener(|_, _: &ClickEvent, _window, cx| {
                        cx.emit(DismissEvent);
                    })),
            )
            .child(
                Button::new(
                    "push-preview-push",
                    if self.pushing {
                        i18n::t!("54ef55cff2ef9b2d")
                    } else {
                        i18n::t!("c757c9cc2dfaf9d5")
                    },
                )
                .style(ButtonStyle::Filled)
                .size(ButtonSize::Medium)
                .disabled(!self.can_push())
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.push(window, cx);
                })),
            )
    }
}

impl Focusable for PushPreview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for PushPreview {}

impl ModalView for PushPreview {}

impl Render for PushPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(60.))
            .h(rems(36.))
            .elevation_3(cx)
            .key_context("PushPreview")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(div().flex_none().child(self.render_header(cx)))
            .child(ListSeparator)
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_commits(cx))
                    .child(self.render_files(cx)),
            )
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;
    use workspace::MultiWorkspace;

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn commit(sha: &str, subject: &str) -> CommitSummary {
        CommitSummary {
            sha: sha.into(),
            subject: subject.into(),
            commit_timestamp: 1_700_000_000,
            author_name: "Test Author".into(),
            has_parent: true,
        }
    }

    fn branch(name: &str, upstream: Option<git::repository::Upstream>) -> Branch {
        Branch {
            is_head: true,
            ref_name: format!("refs/heads/{name}").into(),
            upstream,
            most_recent_commit: None,
        }
    }

    fn remote(name: &str) -> git::repository::Remote {
        git::repository::Remote { name: name.into() }
    }

    async fn setup_preview(
        cx: &mut TestAppContext,
        commits: Vec<CommitSummary>,
        remote: Option<(&str, &str)>,
    ) -> (Entity<PushPreview>, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({".git": {}, "file.txt": "content\n"}),
        )
        .await;
        fs.set_head_for_repo(
            path!("/project/.git").as_ref(),
            &[("file.txt", "content\n".to_string())],
            "deadbeef",
        );
        fs.set_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("file.txt", "content\n".to_string())],
        );
        fs.set_branch_name(path!("/project/.git").as_ref(), Some("main"));
        if let Some((remote_name, remote_url)) = remote {
            fs.set_remote_for_repo(path!("/project/.git").as_ref(), remote_name, remote_url);
        }
        fs.set_outgoing_commits(path!("/project/.git").as_ref(), commits);

        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        cx.read(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .as_local()
                .expect("local worktree")
                .scan_complete()
        })
        .await;
        cx.executor().run_until_parked();

        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);

        let repository = cx.read(|cx| {
            project
                .read(cx)
                .active_repository(cx)
                .expect("fake repository")
        });
        for _ in 0..50 {
            if cx.read(|cx| repository.read(cx).branch.is_some()) {
                break;
            }
            cx.run_until_parked();
        }
        cx.read(|cx| {
            assert!(
                repository.read(cx).branch.is_some(),
                "repository branch should be resolved before opening the preview"
            )
        });
        let preview = workspace.update_in(&mut cx, |_workspace, window, cx| {
            let workspace = cx.entity().downgrade();
            cx.new(|cx| PushPreview::new(repository, workspace, window, cx))
        });

        await_load(&preview, &mut cx).await;
        (preview, cx)
    }

    async fn await_load(preview: &Entity<PushPreview>, cx: &mut VisualTestContext) {
        let task = preview.update_in(cx, |preview, _window, _cx| {
            std::mem::replace(&mut preview._load_task, Task::ready(()))
        });
        task.await;
        cx.executor().run_until_parked();
        let files_task = preview.update_in(cx, |preview, _window, _cx| {
            std::mem::replace(&mut preview._files_task, Task::ready(()))
        });
        files_task.await;
        cx.executor().run_until_parked();
    }

    #[gpui::test]
    async fn test_push_preview_lists_commits_and_changed_files(cx: &mut TestAppContext) {
        let (preview, mut cx) = setup_preview(
            cx,
            vec![
                commit("2222222222222222222222222222222222222222", "second commit"),
                commit("1111111111111111111111111111111111111111", "first commit"),
            ],
            Some(("origin", "git@example.com:example/repo.git")),
        )
        .await;

        let (commit_count, selected, can_push, is_publish, target, files_state) = cx.read(|cx| {
            let preview = preview.read(cx);
            let (commit_count, selected, is_publish, target) = match &preview.commits {
                CommitsState::Ready(outgoing) => (
                    outgoing.commits.len(),
                    preview.selected_commit,
                    preview.target.as_ref().map(|target| target.is_publish),
                    preview
                        .target
                        .as_ref()
                        .map(|target| target.display_target()),
                ),
                _ => (0, 0, None, None),
            };
            let files_state = matches!(preview.files, FilesState::Ready { .. });
            (
                commit_count,
                selected,
                preview.can_push(),
                is_publish,
                target,
                files_state,
            )
        });

        assert_eq!(commit_count, 2);
        assert_eq!(selected, 0);
        assert!(can_push);
        assert_eq!(is_publish, Some(true));
        assert_eq!(target.as_deref(), Some("origin/main"));
        assert!(
            files_state,
            "the first commit should load its changed files"
        );

        preview.update_in(&mut cx, |preview, _window, cx| {
            preview.select_commit(1, cx);
        });
        let files_task = preview.update_in(&mut cx, |preview, _window, _cx| {
            std::mem::replace(&mut preview._files_task, Task::ready(()))
        });
        files_task.await;
        cx.executor().run_until_parked();
        cx.read(|cx| {
            let preview = preview.read(cx);
            assert_eq!(preview.selected_commit, 1);
            assert!(matches!(preview.files, FilesState::Ready { .. }));
        });
    }

    #[gpui::test]
    async fn test_push_preview_disables_push_without_outgoing_commits(cx: &mut TestAppContext) {
        let (preview, cx) = setup_preview(
            cx,
            Vec::new(),
            Some(("origin", "git@example.com:example/repo.git")),
        )
        .await;

        cx.read(|cx| {
            let preview = preview.read(cx);
            assert!(matches!(
                &preview.commits,
                CommitsState::Ready(outgoing) if outgoing.commits.is_empty()
            ));
            assert!(!preview.can_push());
        });
    }

    #[gpui::test]
    async fn test_push_preview_without_remote_reports_missing_remote(cx: &mut TestAppContext) {
        let (preview, cx) = setup_preview(
            cx,
            vec![commit(
                "1111111111111111111111111111111111111111",
                "first commit",
            )],
            None,
        )
        .await;

        cx.read(|cx| {
            let preview = preview.read(cx);
            let target = preview.target.as_ref().expect("resolved push target");
            assert!(target.is_publish);
            assert!(target.remote.is_none());
            assert!(!target.multiple_remotes);
            assert!(!preview.can_push());
        });
    }

    #[gpui::test]
    async fn test_push_preview_deploy_opens_modal(cx: &mut TestAppContext) {
        let (preview, mut cx) = setup_preview(
            cx,
            vec![commit(
                "1111111111111111111111111111111111111111",
                "first commit",
            )],
            Some(("origin", "git@example.com:example/repo.git")),
        )
        .await;
        let workspace = cx.read(|cx| {
            preview
                .read(cx)
                .workspace
                .upgrade()
                .expect("workspace still alive")
        });

        workspace.update_in(&mut cx, |workspace, window, cx| {
            PushPreview::deploy(workspace, window, cx);
        });
        cx.executor().run_until_parked();

        cx.read(|cx| {
            assert!(
                workspace.read(cx).active_modal::<PushPreview>(cx).is_some(),
                "the push preview action should open the modal"
            );
        });
    }

    #[test]
    fn test_push_target_resolution_prefers_the_upstream_tracking_branch() {
        let branch = branch(
            "feature",
            Some(git::repository::Upstream {
                ref_name: "refs/remotes/origin/main".into(),
                tracking: git::repository::UpstreamTracking::Tracked(
                    git::repository::UpstreamTrackingStatus {
                        ahead: 2,
                        behind: 1,
                    },
                ),
            }),
        );

        let target = PushTarget::resolve(&branch, &[remote("origin")]);
        assert!(!target.is_publish);
        assert!(!target.multiple_remotes);
        assert_eq!(target.local_branch.as_ref(), "feature");
        assert_eq!(target.remote.as_deref(), Some("origin"));
        assert_eq!(target.remote_branch.as_ref(), "main");
        assert_eq!(target.display_target(), "origin/main");
        assert!(target.can_push());
    }

    #[test]
    fn test_push_target_resolution_publishes_a_branch_without_upstream() {
        let branch = branch("feature", None);
        let target = PushTarget::resolve(&branch, &[remote("origin")]);
        assert!(target.is_publish);
        assert!(!target.multiple_remotes);
        assert_eq!(target.remote.as_deref(), Some("origin"));
        assert_eq!(target.display_target(), "origin/feature");
        assert!(target.can_push());

        let multiple = PushTarget::resolve(&branch, &[remote("origin"), remote("upstream")]);
        assert!(multiple.is_publish);
        assert!(multiple.multiple_remotes);
        assert!(multiple.remote.is_none());
        assert!(!multiple.can_push());

        let none = PushTarget::resolve(&branch, &[]);
        assert!(none.is_publish);
        assert!(!none.multiple_remotes);
        assert!(none.remote.is_none());
        assert!(!none.can_push());
    }
}
