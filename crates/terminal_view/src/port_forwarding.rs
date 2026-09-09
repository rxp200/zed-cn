use std::{
    collections::HashSet,
    net::TcpListener,
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use editor::Editor;
use futures::{FutureExt as _, channel::oneshot, io::AsyncReadExt as _, select_biased};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, Task, WeakEntity, Window,
};
use menu::{Cancel, Confirm};
use project::{
    Project,
    trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent},
};
use regex::Regex;
use remote::{RemoteClient, RemoteConnectionOptions};
use ui::{
    Button, ButtonStyle, Color, Headline, HeadlineSize, Icon, IconButton, IconName, IconSize,
    Label, LabelSize, WithScrollbar, prelude::*,
};
use util::{
    ResultExt as _,
    command::{Stdio, new_command},
};
use workspace::{ModalView, Workspace};

static URL_PORT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://)?(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])[:：](\d{1,5})")
        .expect("valid port detection regex")
});
static LISTENING_PORT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:listening|listen|running|started|server|服务)[^\r\n]{0,32}?(?:port|端口)\s*[:：]?\s*(\d{1,5})")
        .expect("valid listening port detection regex")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForwardDirection {
    RemoteToLocal,
    LocalToRemote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardSource {
    Automatic,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardStatus {
    Starting,
    RunningUnconfirmed,
    Failed(String),
}

struct ForwardEntry {
    direction: ForwardDirection,
    remote_port: u16,
    local_port: Option<u16>,
    source: ForwardSource,
    status: ForwardStatus,
    generation: u64,
    cancellation: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct ForwardSnapshot {
    pub direction: ForwardDirection,
    pub remote_port: u16,
    pub local_port: Option<u16>,
    pub source: ForwardSource,
    pub status: ForwardStatus,
}

#[derive(Clone)]
struct AutomaticForwardContext {
    project: Entity<Project>,
    client: Entity<RemoteClient>,
    connection: Arc<dyn remote::RemoteConnection>,
    roots: Vec<(settings::WorktreeId, std::path::PathBuf)>,
}

impl AutomaticForwardContext {
    fn capture(project: Entity<Project>, cx: &mut App) -> Option<Self> {
        let client = ssh_remote_client(&project, cx)?;
        let connection = client.read(cx).connection()?;
        let roots = Self::trusted_roots(&project, cx)?;
        Some(Self {
            project,
            client,
            connection,
            roots,
        })
    }

    fn trusted_roots(
        project: &Entity<Project>,
        cx: &mut App,
    ) -> Option<Vec<(settings::WorktreeId, std::path::PathBuf)>> {
        let trusted = TrustedWorktrees::try_get_global(cx)?;
        let store = project.read(cx).worktree_store();
        let roots = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (worktree.id(), worktree.abs_path().to_path_buf())
            })
            .collect::<Vec<_>>();
        if roots.is_empty()
            || !trusted.update(cx, |trusted, cx| {
                roots
                    .iter()
                    .all(|(id, _)| trusted.can_trust(&store, *id, cx))
            })
        {
            return None;
        }
        Some(roots)
    }

    fn is_current(&self, cx: &mut App) -> bool {
        Self::capture(self.project.clone(), cx).is_some_and(|current| {
            current.project == self.project
                && current.client == self.client
                && same_automatic_scope(
                    &current.connection,
                    &current.roots,
                    &self.connection,
                    &self.roots,
                )
        })
    }
}

fn same_automatic_scope<T: ?Sized, R: PartialEq>(
    current_connection: &Arc<T>,
    current_roots: &R,
    captured_connection: &Arc<T>,
    captured_roots: &R,
) -> bool {
    Arc::ptr_eq(current_connection, captured_connection) && current_roots == captured_roots
}

enum ForwardChild {
    Process(Option<util::command::Child>),
    #[cfg(test)]
    Streaming {
        status: oneshot::Receiver<std::io::Result<std::process::ExitStatus>>,
        stderr: Option<Box<dyn futures::io::AsyncRead + Unpin>>,
        cancelled: std::rc::Rc<std::cell::Cell<bool>>,
    },
    #[cfg(test)]
    Controlled {
        output: oneshot::Receiver<std::process::Output>,
        cancelled: std::rc::Rc<std::cell::Cell<bool>>,
    },
}

impl Drop for ForwardChild {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Self::Controlled { cancelled, .. } | Self::Streaming { cancelled, .. } = self {
            cancelled.set(true);
        }
    }
}

impl ForwardChild {
    fn take_stderr(&mut self) -> Box<dyn futures::io::AsyncRead + Unpin> {
        match self {
            Self::Process(child) => Box::new(
                child
                    .as_mut()
                    .expect("owned child")
                    .stderr
                    .take()
                    .expect("piped stderr"),
            ),
            #[cfg(test)]
            Self::Controlled { .. } => Box::new(futures::io::Cursor::new(Vec::new())),
            #[cfg(test)]
            Self::Streaming { stderr, .. } => stderr.take().expect("controlled stderr"),
        }
    }

    async fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            Self::Process(child) => child.as_mut().expect("owned child").status().await,
            #[cfg(test)]
            Self::Streaming { status, .. } => status.await.map_err(std::io::Error::other)?,
            #[cfg(test)]
            Self::Controlled { output, .. } => output
                .await
                .map(|output| output.status)
                .map_err(std::io::Error::other),
        }
    }

    async fn output(mut self) -> std::io::Result<std::process::Output> {
        match &mut self {
            Self::Process(child) => child.take().expect("owned child").output().await,
            #[cfg(test)]
            Self::Controlled { output, .. } => output.await.map_err(std::io::Error::other),
            #[cfg(test)]
            Self::Streaming { .. } => unreachable!("streaming child is only used for tunnels"),
        }
    }
}

const DIAGNOSTIC_TAIL_LIMIT: usize = 16 * 1024;

#[derive(Default)]
struct DiagnosticTail {
    bytes: std::collections::VecDeque<u8>,
    truncated: bool,
    escape: u8,
}

impl DiagnosticTail {
    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            // Keep escape state across reads, including OSC hyperlinks and unterminated sequences.
            match self.escape {
                1 => {
                    self.escape = match byte {
                        b'[' => 2,
                        b']' => 3,
                        _ => 0,
                    };
                    continue;
                }
                2 => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape = 0;
                    }
                    continue;
                }
                3 => {
                    if byte == 7 {
                        self.escape = 0;
                    } else if byte == 27 {
                        self.escape = 4;
                    }
                    continue;
                }
                4 => {
                    self.escape = if byte == b'\\' { 0 } else { 3 };
                    continue;
                }
                _ => {}
            }
            if byte == 27 {
                self.escape = 1;
                continue;
            }
            if byte < 32 && byte != b'\n' && byte != b'\t' || byte == 127 {
                continue;
            }
            if self.bytes.len() == DIAGNOSTIC_TAIL_LIMIT {
                self.bytes.pop_front();
                self.truncated = true;
            }
            self.bytes.push_back(byte);
        }
    }

    fn display(&self, environment: &HashMap<String, String>) -> String {
        let bytes: Vec<_> = self.bytes.iter().copied().collect();
        let mut text: String = String::from_utf8_lossy(&bytes)
            .chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .filter(|character| !matches!(*character as u32, 0x202a..=0x202e | 0x2066..=0x2069))
            .collect();
        for (key, value) in environment {
            let key = key.to_ascii_lowercase();
            if !value.is_empty()
                && ["password", "token", "secret", "credential"]
                    .iter()
                    .any(|word| key.contains(word))
            {
                text = text.replace(value, "[已隐藏]");
            }
        }
        static SECRETS: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)(password|passwd|token|secret|authorization)([=: ]+)[^\s&]+|[a-z][a-z0-9+.-]*://[^\s/@]+:[^\s/@]+@").expect("diagnostic secret pattern")
        });
        text = SECRETS.replace_all(&text, "[已隐藏]").into_owned();
        format!(
            "{}{}\n诊断可能仍含连接信息。",
            if self.truncated {
                "[诊断已截断，仅保留末尾]\n"
            } else {
                ""
            },
            text.trim()
        )
    }
}

async fn yield_diagnostic_drain() {
    let mut yielded = false;
    futures::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

pub struct PortForwardManager {
    entries: HashMap<(ForwardDirection, u16), ForwardEntry>,
    automatically_seen: HashSet<u16>,
    next_generation: u64,
    _tasks: Vec<Task<()>>,
    detection_task: Option<Task<()>>,
    pending_ports: HashSet<u16>,
    detection_error: Option<String>,
    automatic_context: Option<AutomaticForwardContext>,
    context_generation: u64,
    automatic_subscriptions: Vec<gpui::Subscription>,
    #[cfg(test)]
    probe_launcher: Option<Box<dyn FnMut() -> ForwardChild>>,
    #[cfg(test)]
    forward_launcher: Option<Box<dyn FnMut(ForwardDirection, u16) -> ForwardChild>>,
}

impl PortForwardManager {
    pub fn new() -> Self {
        Self {
            entries: HashMap::default(),
            automatically_seen: HashSet::default(),
            next_generation: 0,
            _tasks: Vec::new(),
            detection_task: None,
            pending_ports: HashSet::new(),
            detection_error: None,
            automatic_context: None,
            context_generation: 0,
            automatic_subscriptions: Vec::new(),
            #[cfg(test)]
            probe_launcher: None,
            #[cfg(test)]
            forward_launcher: None,
        }
    }

    fn invalidate_automatic(&mut self, cx: &mut Context<Self>) {
        if self.automatic_context.is_none()
            && self.detection_task.is_none()
            && self.pending_ports.is_empty()
            && self.automatically_seen.is_empty()
            && self.detection_error.is_none()
        {
            return;
        }
        self.detection_task = None;
        self.pending_ports.clear();
        self.automatically_seen.clear();
        self.automatic_context = None;
        self.detection_error = None;
        let automatic = self
            .entries
            .iter()
            .filter_map(|(key, entry)| (entry.source == ForwardSource::Automatic).then_some(*key))
            .collect::<Vec<_>>();
        for (direction, port) in automatic {
            self.stop(direction, port, cx);
        }
        cx.notify();
    }

    fn check_automatic_context(&mut self, cx: &mut Context<Self>) {
        if self
            .automatic_context
            .as_ref()
            .is_some_and(|context| !context.is_current(cx))
        {
            self.invalidate_automatic(cx);
        }
    }

    pub fn snapshots(&self) -> Vec<ForwardSnapshot> {
        let mut snapshots = self
            .entries
            .values()
            .map(|entry| ForwardSnapshot {
                direction: entry.direction,
                remote_port: entry.remote_port,
                local_port: entry.local_port,
                source: entry.source,
                status: entry.status.clone(),
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|entry| {
            (
                entry.direction != ForwardDirection::RemoteToLocal,
                entry.remote_port,
            )
        });
        snapshots
    }

    pub fn detect_from_terminal_output(
        &mut self,
        output: &str,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        self.check_automatic_context(cx);
        let Some(context) = AutomaticForwardContext::capture(project.clone(), cx) else {
            self.invalidate_automatic(cx);
            return;
        };
        if self
            .automatic_context
            .as_ref()
            .is_some_and(|old| old.project != project)
        {
            self.invalidate_automatic(cx);
        }
        if self.automatic_context.is_none() {
            let store = project.read(cx).worktree_store();
            self.context_generation = self.context_generation.wrapping_add(1);
            let generation = self.context_generation;
            self.automatic_subscriptions = vec![
                cx.observe(&project, move |this, _, cx| {
                    if this.context_generation == generation {
                        this.check_automatic_context(cx);
                    }
                }),
                cx.subscribe(&store, move |this, _, event, cx| {
                    if this.context_generation != generation {
                        return;
                    }
                    use project::worktree_store::WorktreeStoreEvent;
                    if matches!(
                        event,
                        WorktreeStoreEvent::WorktreeAdded(_)
                            | WorktreeStoreEvent::WorktreeRemoved(..)
                            | WorktreeStoreEvent::WorktreeOrderChanged
                    ) {
                        this.invalidate_automatic(cx);
                    } else {
                        this.check_automatic_context(cx);
                    }
                }),
                cx.observe(&context.client, move |this, _, cx| {
                    if this.context_generation == generation {
                        this.invalidate_automatic(cx);
                    }
                }),
            ];
            if let Some(trusted) = TrustedWorktrees::try_get_global(cx) {
                self.automatic_subscriptions
                    .push(cx.subscribe(&trusted, move |this, _, event, cx| {
                        if this.context_generation != generation { return; }
                        if matches!(event, TrustedWorktreesEvent::Restricted(changed, _) if *changed == store.downgrade()) {
                            this.invalidate_automatic(cx);
                        } else {
                            this.check_automatic_context(cx);
                        }
                    }));
            }
            self.automatic_context = Some(context.clone());
        }
        self.pending_ports.extend(
            detected_ports(output)
                .into_iter()
                .filter(|port| !self.automatically_seen.contains(port)),
        );
        if self.detection_task.is_some() {
            return;
        }
        let candidates = self.pending_ports.drain().collect::<Vec<_>>();
        if candidates.is_empty() {
            return;
        }
        let client = context.client.clone();
        let mut args = vec![
            "-c".to_string(),
            include_str!("port_forwarding_probe.py").to_string(),
            candidates
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ];
        args.extend(
            context
                .roots
                .iter()
                .map(|(_, root)| root.to_string_lossy().into_owned()),
        );
        let template = client.read(cx).build_command(
            Some("python3".to_string()),
            &args,
            &HashMap::default(),
            None,
            None,
            remote::Interactive::No,
        );
        let Some(template) = template.log_err() else {
            return;
        };
        self.detection_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            // Spawn on the foreground without an await between authorization and execution.
            let child = this
                .update(cx, |this, cx| {
                    if !context.is_current(cx) {
                        return None;
                    }
                    #[cfg(test)]
                    if let Some(launcher) = this.probe_launcher.as_mut() {
                        return Some(Ok(launcher()));
                    }
                    #[cfg(not(test))]
                    let _ = this;
                    let mut command = new_command(&template.program);
                    command
                        .args(&template.args)
                        .envs(&template.env)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    Some(
                        command
                            .spawn()
                            .map(|child| ForwardChild::Process(Some(child))),
                    )
                })
                .ok()
                .flatten();
            let Some(child) = child else {
                this.update(cx, |this, cx| this.invalidate_automatic(cx))
                    .log_err();
                return;
            };
            let probe = async move {
                match child {
                    Ok(child) => child.output().await,
                    Err(error) => Err(error),
                }
            }
            .boxed_local();
            let timeout = cx.background_executor().timer(Duration::from_secs(10));
            let result = futures::future::select(probe, timeout).await;
            this.update(cx, |this, cx| {
                if !context.is_current(cx) {
                    this.invalidate_automatic(cx);
                    return;
                }
                this.detection_task = None;
                this.detection_error = None;
                for port in &candidates {
                    this.pending_ports.remove(port);
                }
                if let futures::future::Either::Left((result, _)) = result {
                    match result {
                        Ok(output) if output.status.success() => {
                            for port in String::from_utf8_lossy(&output.stdout)
                                .lines()
                                .filter_map(|line| line.parse::<u16>().ok())
                                .filter(|port| candidates.contains(port))
                            {
                                if this.automatically_seen.insert(port)
                                    && !this
                                        .entries
                                        .contains_key(&(ForwardDirection::RemoteToLocal, port))
                                {
                                    this.start(
                                        ForwardDirection::RemoteToLocal,
                                        port,
                                        ForwardSource::Automatic,
                                        project.clone(),
                                        cx,
                                    );
                                }
                            }
                        }
                        Ok(output) => {
                            log::warn!(
                                "端口归属校验失败：{}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                            this.detection_error = Some(
                                "自动检测失败：请确认远端为 Linux 且已安装 python3，或手动添加端口"
                                    .into(),
                            );
                        }
                        Err(error) => {
                            log::warn!("端口归属校验失败：{error}");
                            this.detection_error = Some("自动检测连接失败，请手动添加端口".into());
                        }
                    }
                } else {
                    this.detection_error = Some("自动检测超时，请手动添加端口".into());
                }
                cx.notify();
                if !this.pending_ports.is_empty() {
                    this.detect_from_terminal_output("", project, cx);
                }
            })
            .log_err();
        }));
    }

    pub fn add_manual(
        &mut self,
        direction: ForwardDirection,
        port: u16,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        self.stop(direction, port, cx);
        self.start(direction, port, ForwardSource::Manual, project, cx);
    }

    pub fn stop(&mut self, direction: ForwardDirection, port: u16, cx: &mut Context<Self>) {
        if let Some(mut entry) = self.entries.remove(&(direction, port))
            && let Some(cancellation) = entry.cancellation.take()
        {
            cancellation.send(()).ok();
        }
        cx.notify();
    }

    fn start(
        &mut self,
        direction: ForwardDirection,
        port: u16,
        source: ForwardSource,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        let automatic_context = if source == ForwardSource::Automatic {
            let Some(context) = self
                .automatic_context
                .clone()
                .filter(|context| context.project == project && context.is_current(cx))
            else {
                return;
            };
            Some(context)
        } else {
            None
        };
        self._tasks.retain(|task| !task.is_ready());
        let Some(remote_client) = ssh_remote_client(&project, cx) else {
            return;
        };

        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        let (cancellation, cancellation_receiver) = oneshot::channel();
        self.entries.insert(
            (direction, port),
            ForwardEntry {
                direction,
                remote_port: port,
                local_port: (direction == ForwardDirection::LocalToRemote).then_some(port),
                source,
                status: ForwardStatus::Starting,
                generation,
                cancellation: Some(cancellation),
            },
        );
        cx.notify();

        let task = cx.spawn(async move |manager, cx| {
            let result = run_forward(
                remote_client,
                automatic_context,
                direction,
                port,
                cancellation_receiver,
                manager.clone(),
                generation,
                cx,
            )
            .await;
            if let Err(error) = result {
                manager
                    .update(cx, |manager, cx| {
                        if let Some(entry) = manager.entries.get_mut(&(direction, port))
                            && entry.generation == generation
                        {
                            entry.status = ForwardStatus::Failed(format!("{error:#}"));
                            entry.cancellation = None;
                            cx.notify();
                        }
                    })
                    .log_err();
            }
        });
        self._tasks.push(task);
    }
}

pub fn is_available(project: &Entity<Project>, cx: &App) -> bool {
    ssh_remote_client(project, cx).is_some()
}

fn ssh_remote_client(project: &Entity<Project>, cx: &App) -> Option<Entity<RemoteClient>> {
    let remote_client = project.read(cx).remote_client()?;
    matches!(
        remote_client.read(cx).connection_options(),
        RemoteConnectionOptions::Ssh(_)
    )
    .then_some(remote_client)
}

async fn run_forward(
    remote_client: Entity<RemoteClient>,
    automatic_context: Option<AutomaticForwardContext>,
    direction: ForwardDirection,
    port: u16,
    cancellation: oneshot::Receiver<()>,
    manager: WeakEntity<PortForwardManager>,
    generation: u64,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let mut last_error = None;
    let mut cancellation = cancellation.fuse();
    let mut next_local_port = match direction {
        ForwardDirection::RemoteToLocal => {
            available_local_port(Some(port)).or_else(|_| available_local_port(None))?
        }
        ForwardDirection::LocalToRemote => port,
    };

    for attempt in 0..5 {
        if !cx.update(|cx| {
            automatic_context
                .as_ref()
                .is_none_or(|context| context.is_current(cx))
        }) {
            return Ok(());
        }
        if !manager.read_with(cx, |manager, _| {
            manager
                .entries
                .get(&(direction, port))
                .is_some_and(|entry| entry.generation == generation)
        })? {
            return Ok(());
        }
        let local_port = next_local_port;
        let command_template =
            remote_client.read_with(cx, |client, _| match direction {
                ForwardDirection::RemoteToLocal => client.build_forward_ports_command(vec![(
                    local_port,
                    "localhost".to_string(),
                    port,
                )]),
                ForwardDirection::LocalToRemote => client.build_reverse_forward_ports_command(
                    vec![(port, "127.0.0.1".to_string(), local_port)],
                ),
            })?;

        let mut command = new_command(&command_template.program);
        command
            .args(&command_template.args)
            .envs(&command_template.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(test)]
        let controlled = manager.update(cx, |manager, _| {
            manager
                .forward_launcher
                .as_mut()
                .map(|launch| launch(direction, port))
        })?;
        #[cfg(not(test))]
        let controlled: Option<ForwardChild> = None;
        let mut child = match controlled {
            Some(child) => child,
            None => {
                ForwardChild::Process(Some(command.spawn().context("无法启动 SSH 端口转发进程")?))
            }
        };

        if !cx.update(|cx| {
            automatic_context
                .as_ref()
                .is_none_or(|context| context.is_current(cx))
        }) {
            return Ok(());
        }
        manager.update(cx, |manager, cx| {
            if let Some(entry) = manager.entries.get_mut(&(direction, port))
                && entry.generation == generation
            {
                entry.local_port = Some(local_port);
                entry.status = ForwardStatus::RunningUnconfirmed;
                cx.notify();
            }
        })?;

        let mut tail = DiagnosticTail::default();
        let mut stderr = child.take_stderr();
        let mut early = true;
        let mut eof = false;
        let mut incomplete = false;
        let early_timer = cx
            .background_executor()
            .timer(Duration::from_millis(400))
            .fuse();
        let status = child.status().fuse();
        futures::pin_mut!(early_timer, status);
        let mut chunk = [0; 4096];
        let result = loop {
            let read = async {
                if eof {
                    futures::future::pending().await
                } else {
                    stderr.read(&mut chunk).await
                }
            }
            .fuse();
            futures::pin_mut!(read);
            select_biased! {
                _ = cancellation => return Ok(()),
                _ = early_timer => early = false,
                result = status => break result,
                result = read => match result {
                    Ok(0) => eof = true,
                    Ok(count) => {
                        tail.push(&chunk[..count]);
                        yield_diagnostic_drain().await;
                    }
                    Err(error) => return Err(anyhow!("读取 SSH 诊断失败（{direction:?}，请求端口 {port}，第 {} 次）：{error}\n{}", attempt + 1, tail.display(&command_template.env))),
                },
            }
        };
        let grace = cx
            .background_executor()
            .timer(Duration::from_millis(250))
            .fuse();
        futures::pin_mut!(grace);
        while !eof {
            let read = stderr.read(&mut chunk).fuse();
            futures::pin_mut!(read);
            select_biased! {
                _ = cancellation => return Ok(()),
                _ = grace => { incomplete = true; break; },
                result = read => match result {
                    Ok(0) => eof = true,
                    Ok(count) => { tail.push(&chunk[..count]); yield_diagnostic_drain().await; }
                    Err(error) => return Err(anyhow!("读取 SSH 诊断失败（{direction:?}，请求端口 {port}，第 {} 次）：{error}\n{}", attempt + 1, tail.display(&command_template.env))),
                },
            }
        }
        let exited = result.is_ok();
        let outcome = match result {
            Ok(status) => format!("SSH 端口转发进程已退出：{status}"),
            Err(error) => format!("无法等待 SSH 端口转发进程：{error}"),
        };
        last_error = Some(anyhow!(
            "{outcome}（{direction:?}，请求端口 {port}，本地端口 {local_port}，第 {} 次）{}\n{}",
            attempt + 1,
            if incomplete {
                "；诊断尾部收集超时，可能不完整"
            } else {
                ""
            },
            tail.display(&command_template.env)
        ));
        if early && exited && attempt < 4 && direction == ForwardDirection::RemoteToLocal {
            manager.update(cx, |manager, cx| {
                if let Some(entry) = manager.entries.get_mut(&(direction, port))
                    && entry.generation == generation
                {
                    entry.status = ForwardStatus::Starting;
                    entry.local_port = None;
                    cx.notify();
                }
            })?;
            next_local_port = available_local_port(None)?;
        } else {
            break;
        }
    }

    Err(last_error.unwrap_or_else(|| match direction {
        ForwardDirection::RemoteToLocal => anyhow!("无法分配本地端口"),
        ForwardDirection::LocalToRemote => anyhow!("无法建立反向 SSH 端口转发"),
    }))
}

fn available_local_port(preferred: Option<u16>) -> Result<u16> {
    let listener =
        TcpListener::bind(("127.0.0.1", preferred.unwrap_or(0))).context("无法分配本地端口")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

pub fn detected_ports(output: &str) -> Vec<u16> {
    let mut ports = HashSet::new();
    for pattern in [&*URL_PORT_PATTERN, &*LISTENING_PORT_PATTERN] {
        for captures in pattern.captures_iter(output) {
            if let Some(port) = captures
                .get(1)
                .and_then(|value| value.as_str().parse::<u16>().ok())
                .filter(|port| *port != 0)
            {
                ports.insert(port);
            }
        }
    }
    let mut ports = ports.into_iter().collect::<Vec<_>>();
    ports.sort_unstable();
    ports
}

pub struct PortForwardModal {
    manager: Entity<PortForwardManager>,
    project: Entity<Project>,
    editor: Entity<Editor>,
    direction: ForwardDirection,
    error: Option<String>,
    scroll_handle: gpui::ScrollHandle,
}

impl PortForwardModal {
    pub fn new(
        manager: Entity<PortForwardManager>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("端口，例如 9999", window, cx);
            editor
        });
        cx.observe(&manager, |_, _, cx| cx.notify()).detach();
        Self {
            manager,
            project,
            editor,
            direction: ForwardDirection::RemoteToLocal,
            error: None,
            scroll_handle: gpui::ScrollHandle::new(),
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx).trim().to_string();
        let Ok(port) = text.parse::<u16>() else {
            self.error = Some("请输入 1 到 65535 之间的端口号".to_string());
            cx.notify();
            return;
        };
        if port == 0 {
            self.error = Some("端口号不能为 0".to_string());
            cx.notify();
            return;
        }
        self.manager.update(cx, |manager, cx| {
            manager.add_manual(self.direction, port, self.project.clone(), cx)
        });
        self.editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.error = None;
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for PortForwardModal {}
impl ModalView for PortForwardModal {}
impl Focusable for PortForwardModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for PortForwardModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.manager.read(cx).snapshots();
        v_flex()
            .key_context("PortForwardModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(42.))
            .max_h(window.viewport_size().height * 0.85)
            .overflow_hidden()
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .flex_shrink_0()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Link).size(IconSize::Small))
                            .child(Headline::new("SSH 端口转发").size(HeadlineSize::Small)),
                    )
                    .child(
                        IconButton::new("close-port-forward-modal", IconName::Close)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        Button::new("remote-to-local-direction", "远程 → 本地")
                            .style(if self.direction == ForwardDirection::RemoteToLocal {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.direction = ForwardDirection::RemoteToLocal;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("local-to-remote-direction", "本地 → 远程")
                            .style(if self.direction == ForwardDirection::LocalToRemote {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.direction = ForwardDirection::LocalToRemote;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(div().flex_1().child(self.editor.clone()))
                    .child(
                        Button::new("add-port-forward", "添加")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&Confirm, window, cx)
                            })),
                    ),
            )
            .child(Label::new("自动：仅本项目目录内的监听进程（Linux，需 python3）。无法校验时请手动添加。")
                .size(LabelSize::XSmall).color(Color::Muted))
            .when_some(self.manager.read(cx).detection_error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Warning))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(div().id("port-forward-list").max_h(rems(24.)).min_h_0()
                .overflow_y_scroll().track_scroll(&self.scroll_handle)
                .children(entries.into_iter().map(|entry| {
                let direction = entry.direction;
                let remote_port = entry.remote_port;
                let source = match entry.source {
                    ForwardSource::Automatic => "自动",
                    ForwardSource::Manual => "手动",
                };
                let local_port = entry.local_port.map(|port| port.to_string()).unwrap_or_else(|| "待分配".to_string());
                let (address, status_color) = match &entry.status {
                    ForwardStatus::Starting => ("正在启动…".to_string(), Color::Muted),
                    ForwardStatus::RunningUnconfirmed => (
                        match direction {
                            ForwardDirection::RemoteToLocal => {
                                format!("http://127.0.0.1:{local_port}")
                            }
                            ForwardDirection::LocalToRemote => {
                                format!("远程 localhost:{remote_port} → 本地 127.0.0.1:{local_port}")
                            }
                        },
                        Color::Muted,
                    ),
                    ForwardStatus::Failed(error) => (format!("失败：{error}"), Color::Error),
                };
                h_flex()
                    .px_2()
                    .py_1p5()
                    .gap_3()
                    .justify_between()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                Label::new(match direction {
                                    ForwardDirection::RemoteToLocal => {
                                        format!("远程端口 {remote_port} → 本地")
                                    }
                                    ForwardDirection::LocalToRemote => {
                                        format!("本地端口 {local_port} → 远程")
                                    }
                                })
                                .size(LabelSize::Small),
                            )
                            .when(matches!(entry.status, ForwardStatus::RunningUnconfirmed), |this| this.child(Label::new("SSH 进程运行中，监听未确认").size(LabelSize::Small).color(Color::Muted)))
                            .child(if direction == ForwardDirection::RemoteToLocal
                                && entry.local_port.is_some()
                                && matches!(entry.status, ForwardStatus::RunningUnconfirmed)
                            {
                                let address_for_click = address.clone();
                                div()
                                    .id((
                                        match direction {
                                            ForwardDirection::RemoteToLocal => {
                                                "remote-forward-address"
                                            }
                                            ForwardDirection::LocalToRemote => {
                                                "reverse-forward-address"
                                            }
                                        },
                                        u64::from(remote_port),
                                    ))
                                    .cursor_pointer()
                                    .tooltip(ui::Tooltip::text(
                                        "此地址尚未验证。单击复制；Ctrl+单击尝试在默认浏览器打开",
                                    ))
                                    .child(
                                        Label::new(address)
                                            .size(LabelSize::Small)
                                            .color(status_color),
                                    )
                                    .on_click(move |event, _, cx| {
                                        if event.modifiers().control {
                                            cx.open_url(&address_for_click);
                                        } else {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                address_for_click.clone(),
                                            ));
                                        }
                                    })
                                    .into_any_element()
                            } else {
                                Label::new(address)
                                    .size(LabelSize::Small)
                                    .color(status_color)
                                    .into_any_element()
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(source)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new(
                                    (
                                        match direction {
                                            ForwardDirection::RemoteToLocal => {
                                                "stop-remote-port-forward"
                                            }
                                            ForwardDirection::LocalToRemote => {
                                                "stop-reverse-port-forward"
                                            }
                                        },
                                        u64::from(remote_port),
                                    ),
                                    IconName::Stop,
                                )
                                .tooltip(ui::Tooltip::text("停止转发"))
                                .on_click({
                                    let manager = self.manager.clone();
                                    move |_, _, cx| {
                                        manager.update(cx, |manager, cx| {
                                            manager.stop(direction, remote_port, cx)
                                        })
                                    }
                                }),
                            ),
                    )
            })).vertical_scrollbar_for(&self.scroll_handle, window, cx))
            .when(self.manager.read(cx).snapshots().is_empty(), |this| {
                this.child(
                    Label::new("暂无转发。自动检测仅转发工作目录位于本项目内的 Linux 监听进程（需 python3）；其他端口请手动添加。")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
    }
}

pub fn show_modal(
    workspace: &mut Workspace,
    manager: Entity<PortForwardManager>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    if ssh_remote_client(&project, cx).is_none() {
        workspace.show_error("SSH 端口转发仅适用于 SSH 远程项目", cx);
        return;
    }
    workspace.toggle_modal(window, cx, move |window, cx| {
        PortForwardModal::new(manager, project, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::detected_ports;

    struct DiagnosticReader {
        chunks: futures::channel::mpsc::UnboundedReceiver<std::io::Result<Vec<u8>>>,
        current: std::collections::VecDeque<u8>,
        consumed: std::rc::Rc<std::cell::Cell<usize>>,
        dropped: std::rc::Rc<std::cell::Cell<bool>>,
    }

    impl Drop for DiagnosticReader {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    impl futures::io::AsyncRead for DiagnosticReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buffer: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            use futures::Stream as _;
            if self.current.is_empty() {
                match std::task::ready!(std::pin::Pin::new(&mut self.chunks).poll_next(cx)) {
                    Some(Ok(chunk)) => self.current.extend(chunk),
                    Some(Err(error)) => return std::task::Poll::Ready(Err(error)),
                    None => return std::task::Poll::Ready(Ok(0)),
                }
            }
            let count = buffer.len().min(self.current.len());
            for byte in buffer.iter_mut().take(count) {
                *byte = self.current.pop_front().expect("available byte");
            }
            self.consumed.set(self.consumed.get() + count);
            std::task::Poll::Ready(Ok(count))
        }
    }

    #[test]
    fn diagnostic_tail_bounds_utf8_controls_and_known_secrets() {
        let mut tail = super::DiagnosticTail::default();
        tail.push(&vec![b'x'; 100_000]);
        assert_eq!(tail.bytes.len(), super::DIAGNOSTIC_TAIL_LIMIT);
        tail.push(b"\x1b]8;;hidden");
        tail.push(b"\x1b\\visible\x1b[31m");
        let unicode = "中文".as_bytes();
        tail.push(&unicode[..2]);
        tail.push(&unicode[2..]);
        tail.push(b"\xff password=private https://user:pass@example.test token=abc custom-value");
        let text = tail.display(&collections::HashMap::from_iter([(
            "MY_SECRET".to_string(),
            "custom-value".to_string(),
        )]));
        assert!(text.contains("中文") && text.contains("已截断") && text.contains("visible"));
        for secret in [
            "hidden",
            "private",
            "user:pass",
            "token=abc",
            "custom-value",
            "\x1b",
        ] {
            assert!(!text.contains(secret), "{secret}");
        }
        assert!(tail.bytes.len() <= super::DIAGNOSTIC_TAIL_LIMIT);
    }

    #[gpui::test]
    async fn forwarding_stream_lifecycle(cx: &mut gpui::TestAppContext) {
        use super::{ForwardDirection::*, ForwardSource, ForwardStatus};
        use gpui::AppContext as _;
        use std::{
            cell::{Cell, RefCell},
            rc::Rc,
        };
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let client = remote::RemoteClient::test_connected_ssh(cx).await;
        project.update(cx, |project, cx| {
            project.set_remote_client_for_test(client, cx)
        });
        let manager = cx.new(|_| super::PortForwardManager::new());
        let launches = Rc::new(RefCell::new(Vec::new()));
        manager.update(cx, |manager, _| {
            let launches = launches.clone();
            manager.forward_launcher = Some(Box::new(move |_, _| {
                let (exit, status) = futures::channel::oneshot::channel();
                let (sender, chunks) = futures::channel::mpsc::unbounded();
                let consumed = Rc::new(Cell::new(0));
                let dropped = Rc::new(Cell::new(false));
                let cancelled = Rc::new(Cell::new(false));
                let stderr = Box::new(DiagnosticReader {
                    chunks,
                    current: Default::default(),
                    consumed: consumed.clone(),
                    dropped: dropped.clone(),
                });
                launches.borrow_mut().push((
                    Some(exit),
                    sender,
                    consumed,
                    dropped,
                    cancelled.clone(),
                ));
                super::ForwardChild::Streaming {
                    status,
                    stderr: Some(stderr),
                    cancelled,
                }
            }));
        });
        manager.update(cx, |manager, cx| {
            manager.start(
                RemoteToLocal,
                39001,
                ForwardSource::Manual,
                project.clone(),
                cx,
            )
        });
        cx.run_until_parked();
        let first_port = manager.read_with(cx, |manager, _| {
            let entry = &manager.entries[&(RemoteToLocal, 39001)];
            assert_eq!(entry.status, ForwardStatus::RunningUnconfirmed);
            assert_ne!(entry.local_port, Some(0));
            entry.local_port
        });
        launches.borrow()[0]
            .1
            .unbounded_send(Ok(vec![b'x'; 1_000_000]))
            .unwrap();
        launches.borrow()[0]
            .1
            .unbounded_send(Ok(b"late diagnostic".to_vec()))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(launches.borrow()[0].2.get(), 1_000_015);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(30));
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert_eq!(
                manager.entries[&(RemoteToLocal, 39001)].status,
                ForwardStatus::RunningUnconfirmed
            )
        });
        launches.borrow_mut()[0]
            .0
            .take()
            .unwrap()
            .send(Ok(success_status()))
            .unwrap();
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            let ForwardStatus::Failed(error) = &manager.entries[&(RemoteToLocal, 39001)].status
            else {
                panic!("expected late failure")
            };
            assert!(
                error.contains("late diagnostic")
                    && error.contains("收集超时")
                    && error.contains("已截断")
            );
        });
        assert_eq!(launches.borrow().len(), 1, "late exit must not retry");
        assert!(launches.borrow()[0].3.get() && launches.borrow()[0].4.get());
        manager.update(cx, |manager, cx| {
            manager.add_manual(RemoteToLocal, 39001, project.clone(), cx)
        });
        cx.run_until_parked();
        // Independent EOF and exit: early exit retries even while EOF is delayed.
        launches.borrow_mut()[1]
            .0
            .take()
            .unwrap()
            .send(Ok(success_status()))
            .unwrap();
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 3);
        manager.read_with(cx, |manager, _| {
            let entry = &manager.entries[&(RemoteToLocal, 39001)];
            assert_eq!(entry.status, ForwardStatus::RunningUnconfirmed);
            assert_ne!(entry.local_port, first_port);
        });
        launches.borrow()[2]
            .1
            .unbounded_send(Err(std::io::Error::other("injected read failure")))
            .unwrap();
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            let ForwardStatus::Failed(error) = &manager.entries[&(RemoteToLocal, 39001)].status
            else {
                panic!("expected read failure")
            };
            assert!(error.contains("读取 SSH 诊断失败") && error.contains("injected read failure"));
        });
        assert!(launches.borrow()[2].3.get() && launches.borrow()[2].4.get());
        manager.update(cx, |manager, cx| {
            manager.add_manual(LocalToRemote, 39002, project.clone(), cx)
        });
        cx.run_until_parked();
        launches.borrow_mut()[3]
            .0
            .take()
            .unwrap()
            .send(Ok(success_status()))
            .unwrap();
        cx.run_until_parked();
        manager.update(cx, |manager, cx| manager.stop(LocalToRemote, 39002, cx));
        cx.run_until_parked();
        assert!(launches.borrow()[3].3.get() && launches.borrow()[3].4.get());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert!(!manager.entries.contains_key(&(LocalToRemote, 39002)))
        });
        assert_eq!(launches.borrow().len(), 4, "cancelled grace must not retry");
        manager.update(cx, |manager, cx| {
            manager.add_manual(LocalToRemote, 39002, project.clone(), cx)
        });
        cx.run_until_parked();
        launches.borrow()[4].1.close_channel();
        launches.borrow_mut()[4]
            .0
            .take()
            .unwrap()
            .send(Ok(success_status()))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            launches.borrow().len(),
            5,
            "reverse early exit must not retry"
        );
        manager.read_with(cx, |manager, _| {
            assert!(matches!(
                manager.entries[&(LocalToRemote, 39002)].status,
                ForwardStatus::Failed(_)
            ))
        });
    }

    #[gpui::test]
    async fn manager_observers_cancel_queued_and_inflight_probes(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use project::trusted_worktrees::{self, PathTrust};
        use std::{
            cell::{Cell, RefCell},
            path::Path,
            rc::Rc,
        };
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            trusted_worktrees::init(Default::default(), cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({"file":""}))
            .await;
        let project =
            project::Project::test_with_worktree_trust(fs, [Path::new("/project")], cx).await;
        let client = remote::RemoteClient::test_connected_ssh(cx).await;
        project.update(cx, |project, cx| {
            project.set_remote_client_for_test(client.clone(), cx)
        });
        let store = project.read_with(cx, |project, _| project.worktree_store());
        let id = project.read_with(cx, |project, cx| {
            project.visible_worktrees(cx).next().unwrap().read(cx).id()
        });
        let trusted = cx.update(|cx| super::TrustedWorktrees::try_get_global(cx).unwrap());
        let manager = cx.new(|_| super::PortForwardManager::new());
        let launches = Rc::new(RefCell::new(Vec::new()));
        manager.update(cx, |manager, _| {
            let launches = launches.clone();
            manager.probe_launcher = Some(Box::new(move || {
                let (sender, output) = futures::channel::oneshot::channel();
                let cancelled = Rc::new(Cell::new(false));
                launches.borrow_mut().push((sender, cancelled.clone()));
                super::ForwardChild::Controlled { output, cancelled }
            }));
        });
        let emissions = Rc::new(Cell::new(0));
        let subscription = cx.update(|cx| {
            let emissions = emissions.clone();
            cx.subscribe(&trusted, move |_, event, _| {
                if matches!(event, super::TrustedWorktreesEvent::Restricted(..)) {
                    emissions.set(emissions.get() + 1);
                }
            })
        });
        let detect = |cx: &mut gpui::TestAppContext| {
            manager.update(cx, |manager, cx| {
                manager.detect_from_terminal_output("localhost:3000", project.clone(), cx)
            })
        };
        detect(cx);
        detect(cx);
        cx.run_until_parked();
        assert_eq!(
            emissions.get(),
            0,
            "already restricted roots do not re-emit"
        );
        let trust = |cx: &mut gpui::TestAppContext| {
            trusted.update(cx, |trusted, cx| {
                trusted.trust(&store, [PathTrust::Worktree(id)].into_iter().collect(), cx)
            })
        };
        let revoke = |cx: &mut gpui::TestAppContext| {
            trusted.update(cx, |trusted, cx| {
                trusted.restrict(
                    store.downgrade(),
                    [PathTrust::Worktree(id)].into_iter().collect(),
                    cx,
                )
            })
        };
        trust(cx);
        cx.run_until_parked();
        detect(cx);
        revoke(cx);
        trust(cx);
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(launches.borrow().is_empty());
        detect(cx);
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 1);
        revoke(cx);
        trust(cx);
        cx.run_until_parked();
        let (late, cancelled) = launches.borrow_mut().remove(0);
        assert!(cancelled.get());
        assert!(
            late.send(std::process::Output {
                status: success_status(),
                stdout: b"3000\n".to_vec(),
                stderr: Vec::new()
            })
            .is_err()
        );
        detect(cx);
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert!(manager.detection_task.is_some());
            assert!(manager.entries.is_empty());
            assert!(manager.automatically_seen.is_empty());
        });
        // Restore the identical Arc before notifications are dispatched.

        let connection = client.read_with(cx, |client, _| client.connection().unwrap());
        cx.update(|cx| {
            client.update(cx, |client, cx| client.set_connection_for_test(None, cx));
            client.update(cx, |client, cx| {
                client.set_connection_for_test(Some(connection), cx)
            });
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(launches.borrow().is_empty());
        detect(cx);
        cx.run_until_parked();
        let replacement_client = remote::RemoteClient::test_connected_ssh(cx).await;
        cx.update(|cx| {
            client.update(cx, |_, cx| cx.notify());
            project.update(cx, |project, cx| {
                project.set_remote_client_for_test(replacement_client.clone(), cx)
            });
            manager.update(cx, |manager, cx| {
                manager.detect_from_terminal_output("localhost:3000", project.clone(), cx)
            });
        });
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert!(
                manager.detection_task.is_some(),
                "old client notification must not cancel replacement context"
            )
        });
        manager.update(cx, |_, cx| cx.notify());
        detect(cx);
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert!(
                manager.detection_task.is_some(),
                "manager notifications and stable wakeups do not self-cancel"
            )
        });
        let forwards = Rc::new(RefCell::new(Vec::new()));
        manager.update(cx, |manager, cx| {
            let forwards = forwards.clone();
            manager.forward_launcher = Some(Box::new(move |direction, port| {
                let (sender, output) = futures::channel::oneshot::channel();
                let cancelled = Rc::new(Cell::new(false));
                forwards
                    .borrow_mut()
                    .push((direction, port, sender, cancelled.clone()));
                super::ForwardChild::Controlled { output, cancelled }
            }));
            manager.start(
                super::ForwardDirection::RemoteToLocal,
                3000,
                super::ForwardSource::Automatic,
                project.clone(),
                cx,
            );
            manager.add_manual(
                super::ForwardDirection::RemoteToLocal,
                4000,
                project.clone(),
                cx,
            );
            manager.add_manual(
                super::ForwardDirection::LocalToRemote,
                4001,
                project.clone(),
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(forwards.borrow().len(), 3);
        manager.update(cx, |manager, cx| {
            manager.add_manual(
                super::ForwardDirection::RemoteToLocal,
                3000,
                project.clone(),
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            forwards.borrow()[0].3.get(),
            "old automatic child cancelled on replacement"
        );
        manager.update(cx, |manager, cx| {
            manager.start(
                super::ForwardDirection::RemoteToLocal,
                3002,
                super::ForwardSource::Automatic,
                project.clone(),
                cx,
            )
        });
        cx.run_until_parked();
        revoke(cx);
        trust(cx);
        cx.run_until_parked();
        assert!(
            forwards.borrow().last().unwrap().3.get(),
            "revocation cancels automatic child before Active"
        );
        forwards.borrow_mut().pop();
        manager.read_with(cx, |manager, _| {
            assert_eq!(manager.entries.len(), 3);
            assert!(
                manager
                    .entries
                    .values()
                    .all(|entry| entry.source == super::ForwardSource::Manual
                        && entry.cancellation.is_some())
            );
        });
        assert!(forwards.borrow().iter().skip(1).all(|entry| !entry.3.get()));
        // The old automatic completion cannot mutate the new manual generation.
        let (_, _, late_forward, _) = forwards.borrow_mut().remove(0);
        assert!(
            late_forward
                .send(std::process::Output {
                    status: success_status(),
                    stdout: Vec::new(),
                    stderr: b"old failure".to_vec()
                })
                .is_err()
        );
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert!(
                manager
                    .entries
                    .values()
                    .all(|entry| entry.status == super::ForwardStatus::RunningUnconfirmed)
            )
        });
        detect(cx);
        cx.run_until_parked();
        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(launches.borrow().is_empty());
        drop(subscription);
    }

    #[cfg(unix)]
    fn success_status() -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }
    #[cfg(windows)]
    fn success_status() -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    #[gpui::test]
    async fn automatic_forwarding_requires_positive_root_trust(cx: &mut gpui::TestAppContext) {
        use project::{
            Project,
            trusted_worktrees::{self, PathTrust, TrustedWorktrees},
        };
        use std::path::Path;
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            trusted_worktrees::init(Default::default(), cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({"file": ""}))
            .await;
        let project = Project::test_with_worktree_trust(fs, [Path::new("/project")], cx).await;
        let store = project.read_with(cx, |project, _| project.worktree_store());
        let id = project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("root")
                .read(cx)
                .id()
        });
        let trusted = cx.update(|cx| TrustedWorktrees::try_get_global(cx).expect("trust store"));
        cx.update(|cx| {
            assert!(super::AutomaticForwardContext::trusted_roots(&project, cx).is_none());
            // Repeated wakeups must not emit another restriction for the same root.
            assert!(super::AutomaticForwardContext::trusted_roots(&project, cx).is_none());
        });
        trusted.update(cx, |trusted, cx| {
            trusted.trust(&store, [PathTrust::Worktree(id)].into_iter().collect(), cx)
        });
        let roots = cx.update(|cx| {
            super::AutomaticForwardContext::trusted_roots(&project, cx)
                .expect("trusted allowed path")
        });
        trusted.update(cx, |trusted, cx| {
            trusted.restrict(
                store.downgrade(),
                [PathTrust::Worktree(id)].into_iter().collect(),
                cx,
            )
        });
        cx.update(|cx| {
            assert!(super::AutomaticForwardContext::trusted_roots(&project, cx).is_none())
        });
        trusted.update(cx, |trusted, cx| {
            trusted.trust(&store, [PathTrust::Worktree(id)].into_iter().collect(), cx)
        });
        assert_eq!(
            roots,
            cx.update(
                |cx| super::AutomaticForwardContext::trusted_roots(&project, cx)
                    .expect("trusted again")
            )
        );
        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.update(|cx| {
            assert!(super::AutomaticForwardContext::trusted_roots(&project, cx).is_none())
        });
    }

    #[gpui::test]
    fn invalidating_automatic_forwarding_cancels_wait_and_preserves_manual(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        let manager = cx.new(|_| super::PortForwardManager::new());
        let executed = std::rc::Rc::new(std::cell::Cell::new(false));
        manager.update(cx, |manager, cx| {
            let executed = executed.clone();
            manager.detection_task = Some(cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                executed.set(true);
            }));
            manager.pending_ports.insert(3000);
            manager.automatically_seen.insert(3001);
            for (port, source) in [
                (3000, super::ForwardSource::Automatic),
                (4000, super::ForwardSource::Manual),
            ] {
                manager.entries.insert(
                    (super::ForwardDirection::RemoteToLocal, port),
                    super::ForwardEntry {
                        direction: super::ForwardDirection::RemoteToLocal,
                        remote_port: port,
                        local_port: Some(port),
                        source,
                        status: super::ForwardStatus::Starting,
                        generation: 0,
                        cancellation: None,
                    },
                );
            }
            manager.invalidate_automatic(cx);
            assert!(manager.pending_ports.is_empty());
            assert!(manager.automatically_seen.is_empty());
            assert_eq!(manager.snapshots().len(), 1);
            assert_eq!(manager.snapshots()[0].source, super::ForwardSource::Manual);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        assert!(!executed.get());
    }

    #[gpui::test]
    fn invalidating_automatic_forwarding_drops_in_flight_probe(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        struct Probe(std::rc::Rc<std::cell::Cell<bool>>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let manager = cx.new(|_| super::PortForwardManager::new());
        let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
        let accepted = std::rc::Rc::new(std::cell::Cell::new(false));
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        manager.update(cx, |manager, cx| {
            let probe = Probe(dropped.clone());
            let accepted = accepted.clone();
            manager.detection_task = Some(cx.spawn(async move |_, _| {
                let _probe = probe;
                if receiver.await.is_ok() {
                    accepted.set(true);
                }
            }));
        });
        cx.run_until_parked();
        assert!(!dropped.get());
        manager.update(cx, |manager, cx| manager.invalidate_automatic(cx));
        cx.run_until_parked();
        assert!(dropped.get());
        assert!(sender.send(()).is_err());
        assert!(!accepted.get());
    }

    #[test]
    fn automatic_scope_rejects_reconnected_identity_and_changed_roots() {
        let connection = std::sync::Arc::new("same host");
        let reconnected = std::sync::Arc::new("same host");
        let roots = vec![(1, "/project")];
        assert!(super::same_automatic_scope(
            &connection,
            &roots,
            &connection.clone(),
            &roots
        ));
        assert!(!super::same_automatic_scope(
            &reconnected,
            &roots,
            &connection,
            &roots
        ));
        assert!(!super::same_automatic_scope(
            &connection,
            &vec![(2, "/project")],
            &connection,
            &roots
        ));
        assert!(!super::same_automatic_scope(
            &connection,
            &vec![(1, "/other")],
            &connection,
            &roots
        ));
    }

    #[test]
    fn detects_common_server_addresses() {
        assert_eq!(
            detected_ports("Server running at http://localhost:9999"),
            vec![9999]
        );
        assert_eq!(detected_ports("Listening on port 3000"), vec![3000]);
        assert_eq!(detected_ports("访问 http://0.0.0.0:8080/path"), vec![8080]);
        assert_eq!(detected_ports("https://[::1]:5173"), vec![5173]);
    }

    #[test]
    fn ignores_invalid_ports() {
        assert!(detected_ports("localhost:0 localhost:99999").is_empty());
    }

    #[test]
    fn falls_back_when_preferred_local_port_is_busy() {
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test port");
        let occupied_port = occupied.local_addr().expect("test address").port();
        assert!(super::available_local_port(Some(occupied_port)).is_err());
        assert_ne!(
            super::available_local_port(None).expect("allocate fallback port"),
            occupied_port
        );
    }
}
