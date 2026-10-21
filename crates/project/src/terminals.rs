use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, Task, WeakEntity};

use futures::{FutureExt, future::BoxFuture, future::Shared};
use itertools::Itertools as _;
use language::LanguageName;
use remote::{Interactive, RemoteClient};
use rpc::proto;
use settings::{Settings, SettingsLocation};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use task::{Shell, ShellBuilder, ShellKind, SpawnInTerminal};
use terminal::{
    Terminal, TerminalBuilder, TerminalMode, insert_zed_terminal_env,
    terminal_settings::TerminalSettings,
};
use util::{
    command::new_std_command, get_default_system_shell, get_system_shell, maybe, paths::PathStyle,
    rel_path::RelPath,
};
use uuid::Uuid;

use crate::{Project, ProjectPath};

pub struct Terminals {
    pub(crate) local_handles: Vec<WeakEntity<terminal::Terminal>>,
}

impl Project {
    pub fn active_entry_directory(&self, cx: &App) -> Option<PathBuf> {
        let entry_id = self.active_entry()?;
        let worktree = self.worktree_for_entry(entry_id, cx)?;
        let worktree = worktree.read(cx);
        let entry = worktree.entry_for_id(entry_id)?;

        let absolute_path = worktree.absolutize(entry.path.as_ref());
        if entry.is_dir() {
            Some(absolute_path)
        } else {
            absolute_path.parent().map(|p| p.to_path_buf())
        }
    }

    pub fn active_project_directory(&self, cx: &App) -> Option<Arc<Path>> {
        self.active_entry()
            .and_then(|entry_id| self.worktree_for_entry(entry_id, cx))
            .into_iter()
            .chain(self.worktrees(cx))
            .find_map(|tree| tree.read(cx).root_dir())
    }

    pub fn first_project_directory(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.worktrees(cx).next()?;
        let worktree = worktree.read(cx);
        if worktree.root_entry()?.is_dir() {
            Some(worktree.abs_path().to_path_buf())
        } else {
            None
        }
    }

    pub fn create_terminal_task(
        &mut self,
        spawn_task: SpawnInTerminal,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let is_via_remote = self.remote_client.is_some();

        let path: Option<Arc<Path>> = if let Some(cwd) = &spawn_task.cwd {
            if is_via_remote {
                Some(Arc::from(cwd.as_ref()))
            } else {
                let cwd = cwd.to_string_lossy();
                let tilde_substituted = shellexpand::tilde(&cwd);
                Some(Arc::from(Path::new(tilde_substituted.as_ref())))
            }
        } else {
            self.active_project_directory(cx)
        };

        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let detect_venv = settings.detect_venv.as_option().is_some();

        let terminal_mode = TerminalMode::task(spawn_task.clone());

        let local_path = if is_via_remote { None } else { path.clone() };
        let remote_client = self.remote_client.clone();
        let shell = match &remote_client {
            Some(remote_client) => remote_client
                .read(cx)
                .shell()
                .unwrap_or_else(get_default_system_shell),
            None => get_system_shell(),
        };
        let path_style = self.path_style(cx);
        let shell_kind = ShellKind::new(&shell, path_style.is_windows());

        // Prepare a task for resolving the environment
        let env_task =
            self.resolve_directory_environment(&shell, path.clone(), remote_client.clone(), cx);

        // Scope the toolchain lookup to the worktree the terminal is being
        // spawned in. Previously this iterated the active editor's worktree
        // and then every visible worktree, so a Python toolchain persisted
        // for worktree A would leak into a terminal opened in worktree B and
        // inject (e.g.) `conda activate base` into a shell that has no
        // business with conda.
        let project_path_contexts: Vec<ProjectPath> = path
            .as_ref()
            .and_then(|p| self.find_worktree(p, cx))
            .map(|(worktree, relative_path)| ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: relative_path,
            })
            .into_iter()
            .collect();
        let toolchains = project_path_contexts
            .into_iter()
            .filter(|_| detect_venv)
            .map(|p| self.active_toolchain(p, LanguageName::new_static("Python"), cx))
            .collect::<Vec<_>>();
        let lang_registry = self.languages.clone();
        cx.spawn(async move |project, cx| {
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env.clone());

            let activation_script = maybe!(async {
                for toolchain in toolchains {
                    let Some(toolchain) = toolchain.await else {
                        continue;
                    };
                    let language = lang_registry
                        .language_for_name(&toolchain.language_name.0)
                        .await
                        .ok();
                    let lister = language?.toolchain_lister()?;
                    let future =
                        cx.update(|cx| lister.activation_script(&toolchain, shell_kind, cx));
                    return Some(future.await);
                }
                None
            })
            .await
            .unwrap_or_default();

            let builder = project
                .update(cx, move |_, cx| {
                    let format_to_run = |spawn_task: &SpawnInTerminal| {
                        format_task_for_activation(
                            spawn_task,
                            shell_kind,
                            &shell,
                            path_style.is_windows(),
                        )
                    };

                    let to_run =
                        (!activation_script.is_empty()).then(|| format_to_run(&spawn_task));
                    env.extend(spawn_task.env);
                    // Remote tasks run on the Remote Server like any other remote
                    // terminal; the client no longer spawns `ssh -t`.
                    match remote_client {
                        Some(remote_client) => {
                            // A toolchain activation script runs first and then
                            // execs the shell, so the task command becomes part of
                            // that script instead of the terminal's program.
                            let (program, args, activation) = match &to_run {
                                Some(to_run) if !activation_script.is_empty() => {
                                    (shell.clone(), Vec::new(), vec![to_run.clone()])
                                }
                                _ => match spawn_task.command.as_ref() {
                                    Some(command) => {
                                        (command.clone(), spawn_task.args.clone(), Vec::new())
                                    }
                                    None => (shell.clone(), Vec::new(), Vec::new()),
                                },
                            };
                            create_hosted_terminal(
                                remote_client,
                                program,
                                args,
                                shell_kind,
                                activation,
                                env,
                                path,
                                &settings,
                                terminal_mode,
                                cx.entity_id().as_u64(),
                                path_style,
                                cx,
                            )
                        }
                        None => {
                            let (shell, env) = match activation_script.clone() {
                                activation_script if !activation_script.is_empty() => {
                                    let separator = shell_kind.sequential_commands_separator();
                                    let activation_script =
                                        activation_script.join(&format!("{separator} "));
                                    let to_run = to_run.expect("activation command was formatted");

                                    let arg = format!("{activation_script}{separator} {to_run}");
                                    let args = shell_kind.args_for_shell(true, arg);

                                    (
                                        Shell::WithArguments {
                                            program: shell,
                                            args,
                                            title_override: None,
                                        },
                                        env,
                                    )
                                }
                                _ => (
                                    if let Some(program) = spawn_task.command {
                                        Shell::WithArguments {
                                            program,
                                            args: spawn_task.args,
                                            title_override: None,
                                        }
                                    } else {
                                        Shell::System
                                    },
                                    env,
                                ),
                            };
                            Ok(TerminalBuilder::new(
                                local_path.map(|path| path.to_path_buf()),
                                terminal_mode,
                                shell,
                                env,
                                settings.cursor_shape,
                                settings.alternate_scroll,
                                settings.max_scroll_history_lines,
                                settings.path_hyperlink_regexes,
                                Duration::from_millis(settings.path_hyperlink_timeout_ms),
                                false,
                                cx.entity_id().as_u64(),
                                cx,
                                activation_script,
                                path_style,
                            )
                            .boxed())
                        }
                    }
                })??
                .await?;
            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));

                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }


    /// Attaches an existing hosted session as a new terminal in this project.
    /// The session keeps running on the remote host; the returned terminal is
    /// only this client's view of it.
    pub fn attach_hosted_terminal(
        &mut self,
        canonical_root: String,
        session_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<(Entity<Terminal>, String, bool)>> {
        let Some(remote_client) = self.remote_client.clone() else {
            return Task::ready(Err(anyhow!(i18n::t!("9f6b5ab6c9cd5eb2"))));
        };
        if !remote_client.read(cx).supports_hosted_terminals() {
            return Task::ready(Err(anyhow!(i18n::t!("4f9d307004f2e8d4"))));
        }
        let settings = TerminalSettings::get_global(cx).clone();
        let shell = remote_client
            .read(cx)
            .shell()
            .unwrap_or_else(get_default_system_shell);
        let request = remote_client
            .read(cx)
            .attach_hosted_terminal(canonical_root, session_id);
        let path_style = self.path_style(cx);
        let background_executor = cx.background_executor().clone();
        cx.spawn(async move |project, cx| {
            let response = request.await?;
            let client = project
                .read_with(cx, |project, cx| {
                    project
                        .remote_client
                        .as_ref()
                        .map(|client| client.read(cx).proto_client())
                })?
                .context(i18n::t!("9f6b5ab6c9cd5eb2"))?;
            Ok(cx.update(|cx| {
                let terminal = cx.new(|cx| {
                    TerminalBuilder::new_remote(
                        TerminalMode::interactive(),
                        Shell::Program(shell),
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes.clone(),
                        Duration::from_millis(settings.path_hyperlink_timeout_ms),
                        0,
                        path_style,
                        client,
                        response.server_instance_id,
                        response.terminal_id,
                        &background_executor,
                    )
                    .subscribe(cx)
                });
                (
                    terminal,
                    response.session_id.clone(),
                    response.controls_input,
                )
            }))
        })
    }

    pub fn create_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.create_terminal_shell_internal(cwd, false, cx)
    }

    /// Creates a local terminal even if the project is remote.
    /// In remote projects: opens in Zed's launch directory (bypasses SSH).
    /// In local projects: opens in the project directory (same as regular terminals).
    pub fn create_local_terminal(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let working_directory = if self.remote_client.is_some() {
            // Remote project: don't use remote paths, let shell use Zed's cwd
            None
        } else {
            // Local project: use project directory like normal terminals
            self.active_project_directory(cx).map(|p| p.to_path_buf())
        };
        self.create_terminal_shell_internal(working_directory, true, cx)
    }

    /// Internal method for creating terminal shells.
    /// If force_local is true, creates a local terminal even if the project has a remote client.
    /// This allows "breaking out" to a local shell in remote projects.
    fn create_terminal_shell_internal(
        &mut self,
        cwd: Option<PathBuf>,
        force_local: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let path = cwd.map(|p| Arc::from(&*p));
        let is_via_remote = !force_local && self.remote_client.is_some();

        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let detect_venv = settings.detect_venv.as_option().is_some();
        let local_path = if is_via_remote { None } else { path.clone() };

        // See create_terminal_task: scope the toolchain lookup to the
        // worktree the terminal is opened in, not the active editor's
        // worktree or other visible worktrees.
        let project_path_contexts: Vec<ProjectPath> = path
            .as_ref()
            .and_then(|p| self.find_worktree(p, cx))
            .map(|(worktree, relative_path)| ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: relative_path,
            })
            .into_iter()
            .collect();
        let toolchains = project_path_contexts
            .into_iter()
            .filter(|_| detect_venv)
            .map(|p| self.active_toolchain(p, LanguageName::new_static("Python"), cx))
            .collect::<Vec<_>>();
        let remote_client = if force_local {
            None
        } else {
            self.remote_client.clone()
        };
        let remote_shell_request = remote_client.as_ref().map(|remote_client| {
            remote_client
                .read(cx)
                .proto_client()
                .request(proto::GetTerminalShell {
                    project_id: proto::REMOTE_SERVER_PROJECT_ID,
                    worktree_id: settings_location
                        .map(|settings_location| settings_location.worktree_id.to_proto()),
                })
        });
        let shell = match &remote_client {
            Some(remote_client) => remote_client
                .read(cx)
                .shell()
                .unwrap_or_else(get_default_system_shell),
            None => settings.shell.program(),
        };
        let env_shell = match &remote_client {
            Some(_) => shell.clone(),
            None => get_system_shell(),
        };

        let path_style = self.path_style(cx);

        // Prepare a task for resolving the environment
        let env_task =
            self.resolve_directory_environment(&env_shell, path.clone(), remote_client.clone(), cx);

        let lang_registry = self.languages.clone();
        cx.spawn(async move |project, cx| {
            let remote_shell = if let Some(remote_shell_request) = remote_shell_request {
                match remote_shell_request.await {
                    Ok(response) => {
                        let shell = response
                            .shell
                            .context("remote server returned no terminal shell")?;
                        let shell = task::shell_from_proto(shell)
                            .context("remote server returned an invalid terminal shell")?;
                        log::debug!(
                            "create_terminal_shell_internal: using remote terminal shell setting: {shell:?}"
                        );
                        Some(shell)
                    }
                    Err(error) => {
                        // Zed CN servers older than this message do not recognize it;
                        // keep the connection shell rather than failing the terminal.
                        log::warn!(
                            "remote terminal shell settings unavailable, using connection shell: {error:#}"
                        );
                        None
                    }
                }
            } else {
                None
            };
            let shell_program = resolve_remote_shell_program(remote_shell.as_ref(), shell);
            let shell_kind = ShellKind::new(&shell_program, path_style.is_windows());
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env.clone());

            let activation_script = maybe!(async {
                for toolchain in toolchains {
                    let Some(toolchain) = toolchain.await else {
                        continue;
                    };
                    let language = lang_registry
                        .language_for_name(&toolchain.language_name.0)
                        .await
                        .ok();
                    let lister = language?.toolchain_lister()?;
                    let future =
                        cx.update(|cx| lister.activation_script(&toolchain, shell_kind, cx));
                    return Some(future.await);
                }
                None
            })
            .await
            .unwrap_or_default();

            // Only a remote shell's directory is meaningful for the cwd
            // observer; local terminals resolve their own process cwd.
            let remote_working_directory = if is_via_remote {
                path.as_ref().map(|path| PathBuf::from(path.as_ref()))
            } else {
                None
            };
            let window_id = project.entity_id().as_u64();
            let builder = project
                .update(cx, move |_, cx| {
                    match remote_client {
                        Some(remote_client) => {
                            let (program, args) =
                                hosted_shell_command(remote_shell.as_ref(), &shell_program);
                            create_hosted_terminal(
                                remote_client,
                                program,
                                args,
                                shell_kind,
                                activation_script,
                                env,
                                path,
                                &settings,
                                TerminalMode::interactive(),
                                window_id,
                                path_style,
                                cx,
                            )
                        }
                        None => Ok(TerminalBuilder::new(
                            local_path.map(|path| path.to_path_buf()),
                            TerminalMode::interactive(),
                            settings.shell,
                            env,
                            settings.cursor_shape,
                            settings.alternate_scroll,
                            settings.max_scroll_history_lines,
                            settings.path_hyperlink_regexes,
                            Duration::from_millis(settings.path_hyperlink_timeout_ms),
                            false,
                            window_id,
                            cx,
                            activation_script,
                            path_style,
                        )
                        .boxed()),
                    }
                })??
                .await?
                .remote_working_directory(remote_working_directory);
            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));

                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn clone_terminal(
        &mut self,
        terminal: &Entity<Terminal>,
        cx: &mut Context<'_, Project>,
        cwd: Option<PathBuf>,
    ) -> Task<Result<Entity<Terminal>>> {
        // We cannot clone the task's terminal, as it will effectively re-spawn the task, which might not be desirable.
        // For now, create a new shell instead.
        // A hosted terminal's PTY lives on the server, and its clone template holds
        // the remote shell path rather than a client-side command, so cloning it
        // would run that shell on the client. Request a new hosted terminal instead.
        if terminal.read(cx).task().is_some() || terminal.read(cx).is_server_hosted() {
            return self.create_terminal_shell(cwd, cx);
        }
        let local_path = if self.is_via_remote_server() {
            None
        } else {
            cwd
        };

        let builder = terminal.read(cx).clone_builder(cx, local_path);
        cx.spawn(async |project, cx| {
            let terminal = builder.await?;
            project.update(cx, |project, cx| {
                let terminal_handle = cx.new(|cx| terminal.subscribe(cx));

                project
                    .terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn terminal_settings<'a>(
        &'a self,
        path: &'a Option<PathBuf>,
        cx: &'a App,
    ) -> &'a TerminalSettings {
        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        TerminalSettings::get(settings_location, cx)
    }

    pub fn exec_in_shell(
        &self,
        command: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<smol::process::Command>> {
        let path = self.first_project_directory(cx);
        let remote_client = self.remote_client.clone();
        let settings = self.terminal_settings(&path, cx).clone();
        let shell = remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).shell())
            .map(Shell::Program)
            .unwrap_or(Shell::System);
        let is_windows = self.path_style(cx).is_windows();
        let builder = ShellBuilder::new(&shell, is_windows).non_interactive();
        let (command, args) = builder.build(Some(command), &Vec::new());

        let env_task = self.resolve_directory_environment(
            &shell.program(),
            path.as_ref().map(|p| Arc::from(&**p)),
            remote_client.clone(),
            cx,
        );

        cx.spawn(async move |project, cx| {
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env);

            project.update(cx, move |_, cx| {
                match remote_client {
                    Some(remote_client) => {
                        let command_template = remote_client.read(cx).build_command(
                            Some(command),
                            &args,
                            &env,
                            None,
                            None,
                            Interactive::Yes,
                        )?;
                        let mut command = new_std_command(command_template.program);
                        command.args(command_template.args);
                        command.envs(command_template.env);
                        Ok(command)
                    }
                    None => {
                        let mut command = new_std_command(command);
                        command.args(args);
                        command.envs(env);
                        if let Some(path) = path {
                            command.current_dir(path);
                        }
                        Ok(command)
                    }
                }
                .map(|mut process| {
                    util::set_pre_exec_to_start_new_session(&mut process);
                    smol::process::Command::from(process)
                })
            })?
        })
    }

    pub fn local_terminal_handles(&self) -> &Vec<WeakEntity<terminal::Terminal>> {
        &self.terminals.local_handles
    }

    fn resolve_directory_environment(
        &self,
        shell: &str,
        path: Option<Arc<Path>>,
        remote_client: Option<Entity<RemoteClient>>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(path) = &path {
            let shell = Shell::Program(shell.to_string());
            self.environment
                .update(cx, |project_env, cx| match &remote_client {
                    Some(remote_client) => project_env.remote_directory_environment(
                        &shell,
                        path.clone(),
                        remote_client.clone(),
                        cx,
                    ),
                    None => project_env.local_directory_environment(&shell, path.clone(), cx),
                })
        } else {
            Task::ready(None).shared()
        }
    }
}

/// Fails when the connection cannot host terminals.
///
/// Hosted terminals are the only supported remote terminal, so a connection that
/// cannot provide one must fail instead of silently starting a client-side
/// `ssh -t` shell. The capability advertisement is the discriminator: every Zed
/// CN Remote Server sends both, and a server that does not is a different build.
fn require_hosted_terminal_support(remote_client: &Entity<RemoteClient>, cx: &App) -> Result<()> {
    let supported = remote_client.read_with(cx, |remote_client, _| {
        remote_client.supports_persistent_terminals()
            && remote_client.supports_persistent_terminal_streaming()
    });
    if !supported {
        anyhow::bail!(i18n::t!("f9da540818e754a9"));
    }
    Ok(())
}

/// Requests a PTY from the Remote Server and wraps it in a client-side terminal.
///
/// The PTY, the program and its exit status live on the remote host; the client
/// keeps terminal emulation and rendering. `program` and `args` must already be
/// remote values: callers resolve the remote `Shell::System` through
/// [`resolve_remote_shell_program`], never through `Shell::program()`, which
/// would substitute the client's own shell.
#[allow(clippy::too_many_arguments)]
fn create_hosted_terminal(
    remote_client: Entity<RemoteClient>,
    program: String,
    args: Vec<String>,
    shell_kind: ShellKind,
    activation_script: Vec<String>,
    env: HashMap<String, String>,
    working_directory: Option<Arc<Path>>,
    settings: &TerminalSettings,
    mode: TerminalMode,
    window_id: u64,
    path_style: PathStyle,
    cx: &mut App,
) -> Result<BoxFuture<'static, Result<TerminalBuilder>>> {
    require_hosted_terminal_support(&remote_client, cx)?;

    // The Remote Server is launched over a non-PTY SSH session, so its own
    // environment carries no terminal type; without these variables the hosted
    // shell loses color and terminfo-based full-screen programs.
    let env = persistent_terminal_env(env, &release_channel::AppVersion::global(cx));
    let (program, args) = if activation_script.is_empty() {
        (program, args)
    } else {
        let separator = shell_kind.sequential_commands_separator();
        let command = format!(
            "{}{separator} exec {program} -l",
            activation_script.join(&format!("{separator} "))
        );
        (program, shell_kind.args_for_shell(true, command))
    };
    // The remote shell program is what the terminal emulator reports as its
    // shell; `program` itself is moved into the request below.
    let template_shell = Shell::Program(program.clone());
    let working_directory = working_directory.map(|path| path.display().to_string());
    let (client, hosted) = remote_client.read_with(cx, |remote_client, _| {
        (
            remote_client.proto_client(),
            remote_client.supports_hosted_terminals(),
        )
    });
    let request = client.request(proto::CreatePersistentTerminal {
        creation_id: Uuid::new_v4().to_string(),
        program,
        args,
        env: env.into_iter().collect(),
        working_directory: working_directory.clone(),
        rows: 24,
        columns: 80,
        pixel_width: 0,
        pixel_height: 0,
        hosted,
    });
    let cursor_shape = settings.cursor_shape;
    let alternate_scroll = settings.alternate_scroll;
    let max_scroll_history_lines = settings.max_scroll_history_lines;
    let path_hyperlink_regexes = settings.path_hyperlink_regexes.clone();
    let path_hyperlink_timeout = Duration::from_millis(settings.path_hyperlink_timeout_ms);
    let background_executor = cx.background_executor().clone();
    Ok(async move {
        let response = request.await?;
        Ok(TerminalBuilder::new_remote(
            mode,
            template_shell,
            cursor_shape,
            alternate_scroll,
            max_scroll_history_lines,
            path_hyperlink_regexes,
            path_hyperlink_timeout,
            window_id,
            path_style,
            client,
            response.server_instance_id,
            response.terminal_id,
            &background_executor,
        )
        .remote_working_directory(working_directory.map(PathBuf::from)))
    }
    .boxed())
}

/// Resolves the program and arguments a hosted terminal must start.
///
/// `connection_shell` is the shell the connection probed on the remote host.
/// `Shell::System` and a missing setting must both resolve to that remote shell:
/// `Shell::program()` would substitute the client's own shell, which is how a
/// Windows `powershell.exe` path once reached a Linux host.
fn hosted_shell_command(
    remote_shell: Option<&Shell>,
    connection_shell: &str,
) -> (String, Vec<String>) {
    match remote_shell {
        Some(Shell::WithArguments { program, args, .. }) => (program.clone(), args.clone()),
        Some(Shell::Program(program)) => (program.clone(), Vec::new()),
        Some(Shell::System) | None => (connection_shell.to_string(), Vec::new()),
    }
}

fn resolve_remote_shell_program(remote_shell: Option<&Shell>, connection_shell: String) -> String {
    match remote_shell {
        // System refers to the remote host, not the client running Shell::program().
        Some(Shell::System) | None => connection_shell,
        Some(Shell::Program(program)) | Some(Shell::WithArguments { program, .. }) => {
            program.clone()
        }
    }
}

fn persistent_terminal_env(
    mut env: HashMap<String, String>,
    version: &impl std::fmt::Display,
) -> HashMap<String, String> {
    insert_zed_terminal_env(&mut env, version);
    env
}

fn format_task_for_activation(
    spawn_task: &SpawnInTerminal,
    shell_kind: ShellKind,
    shell: &str,
    is_windows: bool,
) -> String {
    if let Some(command) = &spawn_task.command {
        let command = shell_kind.prepend_command_prefix(command);
        let command = shell_kind.try_quote_prefix_aware(&command);
        let args = spawn_task
            .args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                quote_prepared_task_arg_for_activation(
                    spawn_task, shell_kind, arg, index, is_windows,
                )
            });

        command.into_iter().chain(args).join(" ")
    } else {
        // todo: this breaks for remotes to windows
        format!("exec {shell} -l")
    }
}

fn quote_prepared_task_arg_for_activation<'a>(
    spawn_task: &SpawnInTerminal,
    shell_kind: ShellKind,
    arg: &'a str,
    index: usize,
    is_windows: bool,
) -> Option<Cow<'a, str>> {
    if spawn_task.shell.shell_kind(is_windows) == ShellKind::Cmd
        && index >= 2
        && spawn_task
            .args
            .get(index - 2)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/S"))
        && spawn_task
            .args
            .get(index - 1)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/C"))
    {
        // The /C argument is already a cmd command string from prepare_task_for_spawn.
        // Quoting it again for venv activation makes cmd see the quotes as literals.
        return quote_cmd_command_arg_for_outer_shell(arg, shell_kind).map(Cow::Owned);
    }

    shell_kind.try_quote(arg)
}

fn quote_cmd_command_arg_for_outer_shell(arg: &str, shell_kind: ShellKind) -> Option<String> {
    match shell_kind {
        ShellKind::PowerShell | ShellKind::Pwsh => Some(format!("'{}'", arg.replace('\'', "''"))),
        ShellKind::Cmd => Some(arg.to_string()),
        ShellKind::Posix
        | ShellKind::Csh
        | ShellKind::Tcsh
        | ShellKind::Fish
        | ShellKind::Nushell
        | ShellKind::Rc
        | ShellKind::Xonsh
        | ShellKind::Elvish => shell_kind.try_quote(arg).map(Cow::into_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn prepared_cmd_task(command_arg: &str) -> SpawnInTerminal {
        SpawnInTerminal {
            command: Some("cmd.exe".to_string()),
            args: vec!["/S".to_string(), "/C".to_string(), command_arg.to_string()],
            shell: Shell::Program("cmd.exe".to_string()),
            ..SpawnInTerminal::default()
        }
    }

    #[test]
    fn formats_prepared_cmd_task_for_powershell_activation() {
        let task = prepared_cmd_task("\"echo Hi there\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /S /C '\"echo Hi there\"'"
        );
    }

    #[test]
    fn formats_prepared_cmd_task_for_cmd_activation() {
        let task = prepared_cmd_task("\"echo Hi there\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::Cmd, "cmd.exe", true),
            "cmd.exe /S /C \"echo Hi there\""
        );
    }

    #[test]
    fn formats_prepared_cmd_task_with_shell_args_for_activation() {
        let task = SpawnInTerminal {
            command: Some("cmd.exe".to_string()),
            args: vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                "\"echo Hi there\"".to_string(),
            ],
            shell: Shell::WithArguments {
                program: "cmd.exe".to_string(),
                args: vec!["/D".to_string()],
                title_override: None,
            },
            ..SpawnInTerminal::default()
        };

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /D /S /C '\"echo Hi there\"'"
        );
    }

    #[test]
    fn formats_prepared_cmd_task_with_single_quote_for_powershell_activation() {
        let task = prepared_cmd_task("\"echo It's fine\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /S /C '\"echo It''s fine\"'"
        );
    }

    #[test]
    fn hosted_shell_command_uses_the_remote_connection_shell_for_system() {
        for connection_shell in ["/bin/bash", "cmd.exe"] {
            for remote_shell in [Some(&Shell::System), None] {
                assert_eq!(
                    hosted_shell_command(remote_shell, connection_shell),
                    (connection_shell.to_string(), Vec::new())
                );
            }
        }
    }

    #[test]
    fn hosted_shell_command_preserves_explicit_remote_programs_and_arguments() {
        assert_eq!(
            hosted_shell_command(Some(&Shell::Program("/bin/fish".to_string())), "cmd.exe"),
            ("/bin/fish".to_string(), Vec::new())
        );
        assert_eq!(
            hosted_shell_command(
                Some(&Shell::WithArguments {
                    program: "/bin/zsh".to_string(),
                    args: vec!["-l".to_string(), "-i".to_string()],
                    title_override: None,
                }),
                "cmd.exe"
            ),
            (
                "/bin/zsh".to_string(),
                vec!["-l".to_string(), "-i".to_string()]
            )
        );
    }

    #[test]
    fn remote_shell_program_uses_connection_shell_for_system_and_older_servers() {
        for connection_shell in ["/bin/bash", "cmd.exe"] {
            for remote_shell in [Some(&Shell::System), None] {
                assert_eq!(
                    resolve_remote_shell_program(remote_shell, connection_shell.to_string()),
                    connection_shell
                );
            }
        }
    }

    #[test]
    fn remote_shell_program_preserves_explicit_remote_programs() {
        for remote_shell in [
            Shell::Program("/bin/fish".to_string()),
            Shell::WithArguments {
                program: "/bin/zsh".to_string(),
                args: vec!["-l".to_string()],
                title_override: None,
            },
        ] {
            assert_eq!(
                resolve_remote_shell_program(Some(&remote_shell), "cmd.exe".to_string()),
                remote_shell.program()
            );
        }
    }

    #[test]
    fn remote_shell_program_preserves_local_shell_without_remote_settings() {
        let local_shell = Shell::Program("pwsh.exe".to_string()).program();
        assert_eq!(
            resolve_remote_shell_program(None, local_shell.clone()),
            local_shell
        );
    }

    #[test]
    fn persistent_remote_terminal_env_sets_terminal_type_and_keeps_project_env() {
        let mut env = HashMap::default();
        env.insert("TERM".to_string(), "dumb".to_string());
        env.insert("PATH".to_string(), "/usr/bin".to_string());

        let env = persistent_terminal_env(env, &"1.2.3");

        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("zed"));
        assert_eq!(env.get("ZED_TERM").map(String::as_str), Some("true"));
        assert_eq!(
            env.get("TERM_PROGRAM_VERSION").map(String::as_str),
            Some("1.2.3")
        );
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    }

    #[test]
    fn formats_non_cmd_task_for_activation() {
        let task = SpawnInTerminal {
            command: Some("cargo".to_string()),
            args: vec!["test".to_string(), "some test".to_string()],
            shell: Shell::System,
            ..SpawnInTerminal::default()
        };

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cargo test 'some test'"
        );
    }
}
