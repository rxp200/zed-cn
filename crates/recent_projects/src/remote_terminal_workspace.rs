use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use askpass::EncryptedPassword;
use futures::{FutureExt as _, channel::oneshot, select};
use gpui::{Action as _, AppContext as _, AsyncApp, PromptLevel, WindowHandle};

use project::trusted_worktrees;
use remote::RemoteConnectionOptions;
use workspace::{AppState, MultiWorkspace, OpenOptions, SerializedWorkspaceLocation, Workspace};

/// Remote terminal mode always opens its terminal in the center pane. The regular
/// `workspace::NewTerminal` action targets the bottom dock unless the active center
/// pane already shows a terminal, so it must not be used here.
fn remote_terminal_mode_terminal_action() -> Box<dyn gpui::Action> {
    workspace::NewCenterTerminal::default().boxed_clone()
}

/// Opens a remote terminal workspace: a workspace whose primary purpose is
/// terminal-centric remote access. The workspace is opened with the remote
/// home directory as its root, and a terminal is automatically created and
/// focused in the center pane.
pub async fn open_remote_terminal_workspace(
    connection_options: RemoteConnectionOptions,
    app_state: Arc<AppState>,
    open_options: OpenOptions,
    cx: &mut AsyncApp,
) -> Result<WindowHandle<MultiWorkspace>> {
    let connection_options = connection_options.clone();

    let (existing, _open_visible) = workspace::find_existing_workspace(
        &[PathBuf::from("/")],
        &open_options,
        &SerializedWorkspaceLocation::Remote(connection_options.clone()),
        cx,
    )
    .await;

    if let Some((existing_window, existing_workspace)) = existing {
        let remote_connection = cx.update(|cx| {
            existing_workspace
                .read(cx)
                .project()
                .read(cx)
                .remote_client()
                .and_then(|client| client.read(cx).remote_connection())
        });

        if remote_connection.is_some() {
            // Reuse the existing window and workspace; just activate it and
            // ensure a terminal is focused.
            existing_window.update(cx, |multi_workspace, window, cx| {
                window.activate_window();
                multi_workspace.activate(existing_workspace.clone(), None, window, cx);
                existing_workspace.update(cx, |workspace, cx| {
                    // If there's already a terminal in the center pane, focus it.
                    // Otherwise, create a new one.
                    let has_terminal = workspace.panes().iter().any(|pane| {
                        pane.read(cx)
                            .items()
                            .any(|item| item.act_as::<terminal_view::TerminalView>(cx).is_some())
                    });
                    if !has_terminal {
                        window.dispatch_action(remote_terminal_mode_terminal_action(), cx);
                    }
                });
            })?;
            return Ok(existing_window);
        }
        log::info!(
            "existing remote terminal workspace found but connection is dead, starting fresh connection"
        );
    }

    let (window, initial_workspace) = if let Some(window) = open_options.requesting_window {
        let workspace = window.update(cx, |multi_workspace, _, _| {
            multi_workspace.workspace().clone()
        })?;
        (window, workspace)
    } else {
        let workspace_position = cx
            .update(|cx| {
                workspace::remote_workspace_position_from_db(
                    connection_options.clone(),
                    &[PathBuf::from("/")],
                    cx,
                )
            })
            .await
            .context("fetching remote workspace position from db")?;

        let mut options =
            cx.update(|cx| (app_state.build_window_options)(workspace_position.display, cx));
        options.window_bounds = workspace_position.window_bounds;

        let window = cx.open_window(options, |window, cx| {
            let project = project::Project::local(
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                app_state.user_store.clone(),
                app_state.languages.clone(),
                app_state.fs.clone(),
                None,
                project::LocalProjectFlags {
                    init_worktree_trust: false,
                    ..Default::default()
                },
                cx,
            );
            let workspace = cx.new(|cx| {
                let mut workspace = Workspace::new(None, project, app_state.clone(), window, cx);
                workspace.centered_layout = workspace_position.centered_layout;
                workspace
            });
            cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
        })?;
        let workspace = window.update(cx, |multi_workspace, _, _cx| {
            multi_workspace.workspace().clone()
        })?;
        (window, workspace)
    };

    loop {
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let delegate = window.update(cx, {
            let connection_options = connection_options.clone();
            let initial_workspace = initial_workspace.clone();
            move |_multi_workspace: &mut MultiWorkspace, window, cx| {
                window.activate_window();
                initial_workspace.update(cx, |workspace, cx| {
                    workspace.hide_modal(window, cx);
                    workspace.toggle_modal(window, cx, |window, cx| {
                        remote_connection::RemoteConnectionModal::new(
                            &connection_options,
                            vec![PathBuf::from("/")],
                            window,
                            cx,
                        )
                    });

                    let ui = workspace
                        .active_modal::<remote_connection::RemoteConnectionModal>(cx)?
                        .read(cx)
                        .prompt
                        .clone();

                    ui.update(cx, |ui, _cx| {
                        ui.set_cancellation_tx(cancel_tx);
                    });

                    Some(Arc::new(remote_connection::RemoteClientDelegate::new(
                        window.window_handle(),
                        ui.downgrade(),
                        if let RemoteConnectionOptions::Ssh(options) = &connection_options {
                            options
                                .password
                                .as_deref()
                                .and_then(|pw| EncryptedPassword::try_from(pw).ok())
                        } else {
                            None
                        },
                    )))
                })
            }
        })?;

        let Some(delegate) = delegate else { break };

        let connection = remote::connect(connection_options.clone(), delegate.clone(), cx);
        let connection = select! {
            _ = cancel_rx => {
                initial_workspace.update(cx, |workspace, cx| {
                    if let Some(ui) = workspace.active_modal::<remote_connection::RemoteConnectionModal>(cx) {
                        ui.update(cx, |modal, cx| modal.finished(cx))
                    }
                });
                break;
            },
            result = connection.fuse() => result,
        };
        let remote_connection = match connection {
            Ok(connection) => connection,
            Err(e) => {
                initial_workspace.update(cx, |workspace, cx| {
                    if let Some(ui) =
                        workspace.active_modal::<remote_connection::RemoteConnectionModal>(cx)
                    {
                        ui.update(cx, |modal, cx| modal.finished(cx))
                    }
                });
                log::error!("Failed to connect over SSH: {e:#}");
                let response = window
                    .update(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Critical,
                            "Failed to connect over SSH",
                            Some(&format!("{e:#}")),
                            &["Retry", "Cancel"],
                            cx,
                        )
                    })?
                    .await;

                if response == Ok(0) {
                    continue;
                }

                if open_options.requesting_window.is_none() {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
                return Ok(window);
            }
        };

        // Get the remote home directory to use as the workspace root.
        let home_dir = remote_connection
            .home_dir()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));

        let opened = cx
            .update(|cx| {
                workspace::open_remote_project_with_new_connection(
                    window,
                    remote_connection,
                    cancel_rx,
                    delegate.clone(),
                    app_state.clone(),
                    vec![home_dir],
                    // Terminal workspaces only need the top level of the remote
                    // home eagerly; deeper directories load on demand.
                    Some(1),
                    cx,
                )
            })
            .await;

        initial_workspace.update(cx, |workspace, cx| {
            if let Some(ui) = workspace.active_modal::<remote_connection::RemoteConnectionModal>(cx)
            {
                ui.update(cx, |modal, cx| modal.finished(cx))
            }
        });

        match opened {
            Err(e) => {
                log::error!("Failed to open remote terminal workspace: {e:#}");
                let response = window
                    .update(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Critical,
                            "Failed to connect over SSH",
                            Some(&format!("{e:#}")),
                            &["Retry", "Cancel"],
                            cx,
                        )
                    })?
                    .await;
                if response == Ok(0) {
                    continue;
                }

                if open_options.requesting_window.is_none() {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
                initial_workspace.update(cx, |workspace, cx| {
                    trusted_worktrees::track_worktree_trust(
                        workspace.project().read(cx).worktree_store(),
                        None,
                        None,
                        None,
                        cx,
                    );
                });
            }

            Ok((Some(workspace), _items)) => {
                // Open a terminal in the center pane and focus it.
                window.update(cx, |multi_workspace, window, cx| {
                    multi_workspace.activate(workspace.clone(), None, window, cx);
                    workspace.update(cx, |_workspace, cx| {
                        window.dispatch_action(remote_terminal_mode_terminal_action(), cx);
                    });
                })?;
            }
            Ok((None, _)) => {
                // Connection was cancelled or failed silently.
            }
        }

        break;
    }

    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension::ExtensionHostProxy;
    use fs::FakeFs;
    use gpui::{AppContext, Entity, TestAppContext};
    use http_client::BlockedHttpClient;
    use node_runtime::NodeRuntime;
    use remote::RemoteClient;
    use remote_server::{HeadlessAppState, HeadlessProject};
    use serde_json::json;
    use util::{path, rel_path::rel_path};

    #[gpui::test]
    async fn test_remote_terminal_workspace_uses_shallow_scan(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let (_window, headless) = open_test_remote_terminal_workspace(cx, server_cx).await;

        // The worktree rooted at the remote home must be scanned shallowly:
        // top-level entries exist, but deeper directories are not loaded.
        headless.read_with(server_cx, |headless, cx| {
            let worktrees: Vec<_> = headless.worktree_store.read(cx).worktrees().collect();
            assert_eq!(worktrees.len(), 1, "expected exactly the home worktree");
            let snapshot = worktrees[0].read(cx).snapshot();
            let code = snapshot
                .entry_for_path(rel_path("code"))
                .expect("top-level entry must exist");
            assert!(
                code.kind.is_unloaded(),
                "top-level directories must stay unloaded with scan depth 1"
            );
            assert!(
                snapshot.entry_for_path(rel_path("code/project")).is_none(),
                "deeper directories must not be scanned eagerly"
            );
            assert!(snapshot.entry_for_path(rel_path("README.md")).is_some());
        });
    }

    // Remote terminal mode must open the terminal in the center pane, while the
    // regular `NewTerminal` action keeps targeting the bottom dock. A full UI
    // assertion is not possible here: a Mock remote connection resolves terminal
    // creation to a client-side `mock` process that cannot actually spawn, so the
    // terminal item never materializes. The action actually dispatched is the
    // observable decision point this regression guards.
    #[gpui::test]
    fn test_remote_terminal_mode_dispatches_center_terminal_action() {
        assert_eq!(
            remote_terminal_mode_terminal_action().name(),
            workspace::NewCenterTerminal::default().name(),
            "remote terminal mode must open the terminal in the center pane"
        );
        assert_ne!(
            remote_terminal_mode_terminal_action().name(),
            workspace::NewTerminal::default().name(),
            "remote terminal mode must not use the bottom dock terminal action"
        );
    }

    async fn open_test_remote_terminal_workspace(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> (WindowHandle<MultiWorkspace>, Entity<HeadlessProject>) {
        let app_state = init_test(cx);
        let executor = cx.executor();

        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);

        let remote_fs = FakeFs::new(server_cx.executor());
        remote_fs
            .insert_tree(
                path!("/"),
                json!({
                    "code": {
                        "project": {
                            "src": {
                                "main.rs": "fn main() {}",
                            },
                        },
                    },
                    "README.md": "# home",
                }),
            )
            .await;

        server_cx.update(HeadlessProject::init);
        let http_client = Arc::new(BlockedHttpClient);
        let node_runtime = NodeRuntime::unavailable();
        let languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
        let proxy = Arc::new(ExtensionHostProxy::new());

        let headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session,
                    fs: remote_fs.clone(),
                    http_client,
                    node_runtime,
                    languages,
                    extension_host_proxy: proxy,
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });

        drop(connect_guard);

        let mut async_cx = cx.to_async();
        let window = open_remote_terminal_workspace(
            opts,
            app_state,
            workspace::OpenOptions::default(),
            &mut async_cx,
        )
        .await
        .expect("open_remote_terminal_workspace should succeed");

        executor.run_until_parked();
        server_cx.executor().run_until_parked();
        executor.run_until_parked();

        assert_eq!(cx.update(|cx| cx.windows().len()), 1);

        (window, headless)
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }
}
