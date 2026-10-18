use crate::git_graph::{self, GitGraph};
use crate::git_panel_settings::GitPanelSettings;
use git::repository::LogSource;
use gpui::{
    Action, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Styled, WeakEntity, Window, actions,
};
use project::Project;
use project::git_store::{GitStoreEvent, RepositoryId};
use settings::Settings;
use ui::{Color, IconName, IconSize, Label, LabelSize, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    git_history_panel,
    [
        /// Opens the Git history tool window.
        ToggleFocus,
    ]
);

/// Bottom-docked commit history: branch tree, commit graph and commit details.
pub struct GitHistoryPanel {
    graph: Entity<GitGraph>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
}

impl GitHistoryPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        // Without a repository the graph renders its empty state; the id is
        // refreshed by `sync_active_repository` once one appears.
        let repository_id = project
            .read(cx)
            .active_repository(cx)
            .map(|repository| repository.read(cx).id)
            .unwrap_or(RepositoryId(0));
        let graph = cx.new(|cx| {
            GitGraph::new(
                repository_id,
                git_store.clone(),
                workspace.clone(),
                Some(LogSource::All),
                window,
                cx,
            )
        });
        cx.subscribe_in(
            &git_store,
            window,
            |this, _git_store, event, _window, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_)
                | GitStoreEvent::RepositoryAdded
                | GitStoreEvent::RepositoryRemoved(_) => this.sync_active_repository(cx),
                _ => {}
            },
        )
        .detach();

        Self {
            graph,
            focus_handle: cx.focus_handle(),
            workspace,
        }
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone())?;
        cx.new_window_entity(|window, cx| Self::new(workspace.clone(), &project, window, cx))
    }

    fn sync_active_repository(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            return;
        };
        let repository_id: RepositoryId = repository.read(cx).id;
        self.graph
            .update(cx, |graph, cx| graph.set_repo_id(repository_id, cx));
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();

        h_flex()
            .h_8()
            .flex_none()
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(i18n::t!("0e76960093379060"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("git-history-open-graph", IconName::GitGraph)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(i18n::t!("1df8b019162aaa05")))
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(git_graph::Open.boxed_clone(), cx);
                    })),
            )
            .into_any_element()
    }
}

impl EventEmitter<PanelEvent> for GitHistoryPanel {}

impl Focusable for GitHistoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitHistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_active_repository(cx);

        v_flex()
            .size_full()
            .key_context("GitHistoryPanel")
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.graph.clone()))
    }
}

impl Panel for GitHistoryPanel {
    fn persistent_name() -> &'static str {
        "GitHistoryPanel"
    }

    fn panel_key() -> &'static str {
        "GitHistoryPanel"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.graph.focus_handle(cx)
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        GitPanelSettings::get_global(cx).default_height
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::GitBranch)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Git History")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        2
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<GitHistoryPanel>(window, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    #[gpui::test]
    async fn history_panel_uses_the_git_icon_and_stays_at_the_bottom(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree("/root", json!({ "project": { ".git": {} } }))
            .await;
        let project = Project::test(fs, [std::path::Path::new(path!("/root/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| GitHistoryPanel::new(workspace.weak_handle(), &project, window, cx))
        });

        panel.update_in(cx, |panel, window, cx| {
            assert_eq!(panel.position(window, cx), DockPosition::Bottom);
            assert!(panel.position_is_valid(DockPosition::Bottom));
            assert!(!panel.position_is_valid(DockPosition::Left));
            assert_eq!(panel.icon(window, cx), Some(IconName::GitBranch));
            assert_eq!(
                panel.default_size(window, cx),
                GitPanelSettings::get_global(cx).default_height
            );
        });
    }
}
