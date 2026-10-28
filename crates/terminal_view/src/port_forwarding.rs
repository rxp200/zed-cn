use std::{collections::HashSet, net::TcpListener, sync::LazyLock, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use editor::Editor;
use futures::{
    FutureExt as _, StreamExt as _, channel::oneshot, io::AsyncReadExt as _, select_biased,
};
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
    Button, ButtonSize, ButtonStyle, Callout, Color, Headline, HeadlineSize, Icon, IconButton,
    IconName, IconSize, Indicator, Label, LabelSize, Severity, TintColor, ToggleButtonGroup,
    ToggleButtonGroupStyle, ToggleButtonWithIcon, WithScrollbar, prelude::*,
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
    Preview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardStatus {
    Starting,
    RunningUnconfirmed,
    /// The tunnel process ended and the manager is waiting to re-establish it.
    /// It keeps retrying until the entry is stopped or the workspace closes.
    Retrying(String),
    Failed(String),
}

/// The endpoint a tunnel connects to.
///
/// For [`ForwardDirection::RemoteToLocal`] it is resolved and reached on the
/// remote host, for [`ForwardDirection::LocalToRemote`] on the local machine.
/// A `None` host means the loopback address of that machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ForwardTarget {
    pub host: Option<String>,
    pub port: u16,
}

impl ForwardTarget {
    fn loopback(port: u16) -> Self {
        Self { host: None, port }
    }

    /// The host argument expected by `ssh -L` / `ssh -R`.
    fn connect_host(&self, fallback: &str) -> String {
        self.host.clone().unwrap_or_else(|| fallback.to_string())
    }

    /// `host:port` with an explicit loopback host, for user-visible labels.
    fn connect_label(&self, fallback: &str) -> String {
        format!(
            "{}:{}",
            bracket_ipv6(&self.connect_host(fallback)),
            self.port
        )
    }
}

impl std::fmt::Display for ForwardTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.host {
            Some(host) => write!(formatter, "{}:{}", bracket_ipv6(host), self.port),
            None => write!(formatter, "{}", self.port),
        }
    }
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]", host)
    } else {
        host.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForwardInputError {
    Empty,
    InvalidHost,
    InvalidPort,
}

fn forward_input_error_message(error: ForwardInputError) -> &'static str {
    match error {
        ForwardInputError::Empty => i18n::t!("151ef2000ff5a2a8"),
        ForwardInputError::InvalidHost => i18n::t!("d7c3cf2d2d8c8134"),
        ForwardInputError::InvalidPort => i18n::t!("2d90356ae0befa45"),
    }
}

/// Parses one line of the manual input field: a bare port, or `host:port`.
///
/// The host is resolved by whichever side owns the target, so it is never
/// resolved here. Loopback and wildcard spellings collapse to the single
/// "that machine's loopback" form, which keeps them identical to entries
/// created by automatic detection and the web preview.
fn parse_forward_input(text: &str) -> Result<ForwardTarget, ForwardInputError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ForwardInputError::Empty);
    }
    let (host, port) = split_host_port(text)?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(ForwardInputError::InvalidPort)?;
    let host = match host {
        Some(host) => normalize_host(host)?,
        None => None,
    };
    Ok(ForwardTarget { host, port })
}

fn split_host_port(text: &str) -> Result<(Option<&str>, &str), ForwardInputError> {
    if let Some(rest) = text.strip_prefix('[') {
        let (host, rest) = rest.split_once(']').ok_or(ForwardInputError::InvalidHost)?;
        let port = rest
            .strip_prefix(':')
            .ok_or(ForwardInputError::InvalidHost)?;
        if host.is_empty() {
            return Err(ForwardInputError::InvalidHost);
        }
        return Ok((Some(host), port));
    }
    match text.rsplit_once(':') {
        Some((host, port)) => {
            // A bare IPv6 address is ambiguous with `host:port`; require brackets.
            if host.is_empty() || host.contains(':') {
                return Err(ForwardInputError::InvalidHost);
            }
            Ok((Some(host), port))
        }
        None => Ok((None, text)),
    }
}

fn normalize_host(host: &str) -> Result<Option<String>, ForwardInputError> {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 255
        || !host.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':' | '%')
        })
    {
        return Err(ForwardInputError::InvalidHost);
    }
    Ok((!matches!(
        host.as_str(),
        "localhost" | "127.0.0.1" | "::1" | "0.0.0.0" | "::"
    ))
    .then_some(host))
}

struct ForwardEntry {
    direction: ForwardDirection,
    target: ForwardTarget,
    local_port: Option<u16>,
    source: ForwardSource,
    status: ForwardStatus,
    generation: u64,
    cancellation: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct ForwardSnapshot {
    pub direction: ForwardDirection,
    pub target: ForwardTarget,
    pub local_port: Option<u16>,
    pub source: ForwardSource,
    pub status: ForwardStatus,
}

#[derive(Clone)]
struct AutomaticForwardContext {
    project: Entity<Project>,
    client: Entity<RemoteClient>,
    roots: Vec<(settings::WorktreeId, std::path::PathBuf)>,
}

impl AutomaticForwardContext {
    /// Captures the scope only while a live transport exists, because the
    /// ownership probe builds its command from the current connection.
    fn capture(project: Entity<Project>, cx: &mut App) -> Option<Self> {
        let client = ssh_remote_client(&project, cx)?;
        client.read(cx).connection()?;
        let roots = Self::trusted_roots(&project, cx)?;
        Some(Self {
            project,
            client,
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

    /// Whether the project trust decision captured here still applies.
    ///
    /// A transport reconnect keeps the same project, client entity and trusted
    /// roots but replaces the live connection, so automatic forwards must
    /// survive it and be re-established by the retry loop instead of being
    /// discarded and never re-detected.
    fn is_current(&self, cx: &mut App) -> bool {
        ssh_remote_client(&self.project, cx).is_some_and(|client| client == self.client)
            && Self::trusted_roots(&self.project, cx).as_ref() == Some(&self.roots)
    }

    fn same_scope(&self, other: &Self) -> bool {
        self.project == other.project && self.client == other.client && self.roots == other.roots
    }
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
                text = text.replace(value, i18n::t!("3c019359a396649a"));
            }
        }
        static SECRETS: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)(password|passwd|token|secret|authorization)([=: ]+)[^\s&]+|[a-z][a-z0-9+.-]*://[^\s/@]+:[^\s/@]+@").expect("diagnostic secret pattern")
        });
        text = SECRETS
            .replace_all(&text, i18n::t!("3c019359a396649a"))
            .into_owned();
        i18n::t_args!(
            "ff876592329dfa4e",
            if self.truncated {
                i18n::t!("0a3af49198bcb1f0")
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
    entries: HashMap<(ForwardDirection, ForwardTarget), ForwardEntry>,
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
    forward_launcher:
        Option<Box<dyn FnMut(ForwardDirection, &ForwardTarget, &[String]) -> ForwardChild>>,
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
            .filter_map(|(key, entry)| {
                (entry.source == ForwardSource::Automatic).then_some(key.clone())
            })
            .collect::<Vec<_>>();
        for (direction, target) in automatic {
            self.stop(direction, &target, cx);
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
                target: entry.target.clone(),
                local_port: entry.local_port,
                source: entry.source,
                status: entry.status.clone(),
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|entry| {
            (
                entry.direction != ForwardDirection::RemoteToLocal,
                entry.target.port,
                entry.target.host.clone(),
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
            // A missing transport is transient and handled by the retry loop. Only
            // a scope that no longer holds drops the automatic forwards.
            if !self
                .automatic_context
                .as_ref()
                .is_some_and(|context| context.is_current(cx))
            {
                self.invalidate_automatic(cx);
            }
            return;
        };
        if !self
            .automatic_context
            .as_ref()
            .is_some_and(|current| current.same_scope(&context))
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
                        this.check_automatic_context(cx);
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
                                    && !this.entries.contains_key(&(
                                        ForwardDirection::RemoteToLocal,
                                        ForwardTarget::loopback(port),
                                    ))
                                {
                                    this.start(
                                        ForwardDirection::RemoteToLocal,
                                        ForwardTarget::loopback(port),
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
                            this.detection_error = Some(i18n::t!("a5284be7c155b1e4").to_string());
                        }
                        Err(error) => {
                            log::warn!("端口归属校验失败：{error}");
                            this.detection_error = Some(i18n::t!("2498c6da445bd724").to_string());
                        }
                    }
                } else {
                    this.detection_error = Some(i18n::t!("641fb6fb58d7249d").to_string());
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
        target: ForwardTarget,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        self.stop(direction, &target, cx);
        self.start(direction, target, ForwardSource::Manual, project, cx);
    }

    pub fn add_preview(&mut self, port: u16, project: Entity<Project>, cx: &mut Context<Self>) {
        let target = ForwardTarget::loopback(port);
        if !self
            .entries
            .contains_key(&(ForwardDirection::RemoteToLocal, target.clone()))
        {
            self.start(
                ForwardDirection::RemoteToLocal,
                target,
                ForwardSource::Preview,
                project,
                cx,
            );
        }
    }

    pub fn stop_preview(&mut self, port: u16, cx: &mut Context<Self>) {
        let target = ForwardTarget::loopback(port);
        if self
            .entries
            .get(&(ForwardDirection::RemoteToLocal, target.clone()))
            .is_some_and(|entry| entry.source == ForwardSource::Preview)
        {
            self.stop(ForwardDirection::RemoteToLocal, &target, cx);
        }
    }

    pub fn available_remote_port(&self) -> Result<u16> {
        for port in 49152..=65535 {
            let taken = self.entries.keys().any(|(direction, target)| {
                *direction == ForwardDirection::RemoteToLocal && target.port == port
            });
            if !taken {
                return Ok(port);
            }
        }
        Err(anyhow!(i18n::t!("c8c1564775a48d41")))
    }

    pub fn stop(
        &mut self,
        direction: ForwardDirection,
        target: &ForwardTarget,
        cx: &mut Context<Self>,
    ) {
        if let Some(mut entry) = self.entries.remove(&(direction, target.clone()))
            && let Some(cancellation) = entry.cancellation.take()
        {
            cancellation.send(()).ok();
        }
        cx.notify();
    }

    fn start(
        &mut self,
        direction: ForwardDirection,
        target: ForwardTarget,
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
            (direction, target.clone()),
            ForwardEntry {
                direction,
                target: target.clone(),
                local_port: (direction == ForwardDirection::LocalToRemote).then_some(target.port),
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
                target.clone(),
                cancellation_receiver,
                manager.clone(),
                generation,
                cx,
            )
            .await;
            if let Err(error) = result {
                manager
                    .update(cx, |manager, cx| {
                        if let Some(entry) = manager.entries.get_mut(&(direction, target))
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

/// A tunnel that stayed up this long counts as a fresh outage instead of a
/// continuation of the previous failure streak, so recovery starts fast again.
const STABLE_TUNNEL_THRESHOLD: Duration = Duration::from_secs(10);
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(1);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

fn retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    RETRY_INITIAL_DELAY
        .saturating_mul(1_u32 << exponent)
        .min(RETRY_MAX_DELAY)
}

/// Keeps one entry's tunnel alive until the entry is stopped.
///
/// Every way a tunnel can end (process exit, start failure, diagnostic read
/// error, remote session reconnect) only schedules another attempt with a
/// capped exponential backoff, because the user expects forwarding to survive
/// network outages without reopening the workspace.
async fn run_forward(
    remote_client: Entity<RemoteClient>,
    automatic_context: Option<AutomaticForwardContext>,
    direction: ForwardDirection,
    target: ForwardTarget,
    cancellation: oneshot::Receiver<()>,
    manager: WeakEntity<PortForwardManager>,
    generation: u64,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let key = (direction, target.clone());
    let mut failures = 0_u32;
    let mut cancellation = cancellation.fuse();
    let mut next_local_port = match direction {
        ForwardDirection::RemoteToLocal => {
            available_local_port(Some(target.port)).or_else(|_| available_local_port(None))?
        }
        ForwardDirection::LocalToRemote => target.port,
    };

    // Replacing the remote session kills the forwarding process because it is a
    // mux client of the old master connection. Retrying as soon as the session
    // is back avoids waiting out the current backoff.
    let (reconnected_tx, mut reconnected_rx) = futures::channel::mpsc::unbounded::<()>();
    let _reconnected_subscription = cx.update(|cx| {
        cx.subscribe(
            &remote_client,
            move |_, event: &remote::RemoteClientEvent, _| {
                if matches!(event, remote::RemoteClientEvent::Reconnected) {
                    // Sending only fails once the forwarding task has ended.
                    reconnected_tx.unbounded_send(()).log_err();
                }
            },
        )
    });

    loop {
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
                .get(&key)
                .is_some_and(|entry| entry.generation == generation)
        })? {
            return Ok(());
        }

        let local_port = next_local_port;
        let attempt = failures.saturating_add(1);
        let mut early_exit = false;
        let mut stable = false;
        let diagnostic: Option<anyhow::Error>;

        'attempt: {
            let command_template = match remote_client.read_with(cx, |client, _| match direction {
                ForwardDirection::RemoteToLocal => client.build_forward_ports_command(vec![(
                    local_port,
                    target.connect_host("localhost"),
                    target.port,
                )]),
                ForwardDirection::LocalToRemote => client.build_reverse_forward_ports_command(
                    vec![(target.port, target.connect_host("127.0.0.1"), local_port)],
                ),
            }) {
                Ok(template) => template,
                Err(error) => {
                    diagnostic = Some(error.context(i18n::t!("d19ff0ea62ad0732")));
                    break 'attempt;
                }
            };

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
                    .map(|launch| launch(direction, &target, &command_template.args))
            })?;
            #[cfg(not(test))]
            let controlled: Option<ForwardChild> = None;
            let mut child = match controlled {
                Some(child) => child,
                None => match command.spawn().context(i18n::t!("8aac5b1a043ae283")) {
                    Ok(child) => ForwardChild::Process(Some(child)),
                    Err(error) => {
                        diagnostic = Some(error);
                        break 'attempt;
                    }
                },
            };

            if !cx.update(|cx| {
                automatic_context
                    .as_ref()
                    .is_none_or(|context| context.is_current(cx))
            }) {
                return Ok(());
            }
            manager.update(cx, |manager, cx| {
                if let Some(entry) = manager.entries.get_mut(&key)
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
            let stable_timer = cx
                .background_executor()
                .timer(STABLE_TUNNEL_THRESHOLD)
                .fuse();
            let status = child.status().fuse();
            futures::pin_mut!(early_timer, stable_timer, status);
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
                    _ = stable_timer => stable = true,
                    result = status => break result,
                    result = read => match result {
                        Ok(0) => eof = true,
                        Ok(count) => {
                            tail.push(&chunk[..count]);
                            yield_diagnostic_drain().await;
                        }
                        Err(error) => {
                            diagnostic = Some(anyhow!(i18n::t_mix!(
                                "3c83d895fc4bafdd";
                                attempt,
                                tail.display(&command_template.env);
                                direction = format!("{direction:?}"),
                                port = target.to_string(),
                                error = error
                            )));
                            break 'attempt;
                        }
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
                        Err(error) => {
                            diagnostic = Some(anyhow!(i18n::t_mix!(
                                "3c83d895fc4bafdd";
                                attempt,
                                tail.display(&command_template.env);
                                direction = format!("{direction:?}"),
                                port = target.to_string(),
                                error = error
                            )));
                            break 'attempt;
                        }
                    },
                }
            }
            let exited = result.is_ok();
            let outcome = match result {
                Ok(status) => i18n::t!("34922cb45f442133", status = status),
                Err(error) => i18n::t!("6872562fccf85da4", error = error),
            };
            early_exit = early && exited;
            diagnostic = Some(anyhow!(i18n::t_mix!(
                "562dde06c4719e16";
                attempt,
                if incomplete {
                    i18n::t!("984b154e11a98a30")
                } else {
                    ""
                },
                tail.display(&command_template.env);
                outcome = outcome,
                direction = format!("{direction:?}"),
                port = target.to_string(),
                local_port = local_port
            )));
        }

        let Some(diagnostic) = diagnostic else {
            return Ok(());
        };
        failures = if stable {
            1
        } else {
            failures.saturating_add(1)
        };
        if early_exit && direction == ForwardDirection::RemoteToLocal {
            next_local_port = available_local_port(None)?;
        }
        let delay = retry_delay(failures);
        let message = format!("{diagnostic:#}");
        manager.update(cx, |manager, cx| {
            if let Some(entry) = manager.entries.get_mut(&key)
                && entry.generation == generation
            {
                entry.local_port = Some(next_local_port);
                entry.status = ForwardStatus::Retrying(message);
                cx.notify();
            }
        })?;
        log::info!(
            "SSH port forward {direction:?} {target} ended; retrying in {delay:?} (attempt {failures})"
        );

        let retry_timer = cx.background_executor().timer(delay).fuse();
        futures::pin_mut!(retry_timer);
        select_biased! {
            _ = cancellation => return Ok(()),
            _ = retry_timer => {}
            message = reconnected_rx.next() => {
                if message.is_none() {
                    return Ok(());
                }
            }
        }
    }
}

fn available_local_port(preferred: Option<u16>) -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", preferred.unwrap_or(0)))
        .context(i18n::t!("16f0a4150c7d5647"))?;
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

/// The control that closes one port forward.
///
/// The port forwarding modal and the system monitor's forwarding card both
/// build their stop button here, so a tunnel is closed through the same
/// affordance from either surface. The card asks for a smaller button because
/// its rows are denser than the modal's list.
pub fn stop_forward_button(
    entry: &ForwardSnapshot,
    manager: &Entity<PortForwardManager>,
    icon_size: IconSize,
    button_size: ButtonSize,
) -> impl IntoElement {
    let direction = entry.direction;
    let target = entry.target.clone();
    let manager = manager.clone();
    IconButton::new(
        format!(
            "{}:{target}",
            match direction {
                ForwardDirection::RemoteToLocal => "stop-remote-port-forward",
                ForwardDirection::LocalToRemote => "stop-reverse-port-forward",
            }
        ),
        IconName::Stop,
    )
    .icon_size(icon_size)
    .size(button_size)
    .tooltip(ui::Tooltip::text(i18n::t!("d961583cc6bfa37f")))
    .on_click(move |_, _, cx| {
        manager.update(cx, |manager, cx| manager.stop(direction, &target, cx))
    })
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
            editor.set_placeholder_text(i18n::t!("7dc7b8c099a1138e"), window, cx);
            editor
        });
        cx.observe(&manager, |_, _, cx| cx.notify()).detach();
        cx.observe(&editor, |this, _, cx| {
            this.error = None;
            cx.notify();
        })
        .detach();
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
        let text = self.editor.read(cx).text(cx);
        let port_input_focus = self.editor.focus_handle(cx);
        let target = match parse_forward_input(&text) {
            Ok(target) => target,
            Err(error) => {
                self.error = Some(forward_input_error_message(error).to_string());
                window.focus(&port_input_focus, cx);
                cx.notify();
                return;
            }
        };
        self.manager.update(cx, |manager, cx| {
            manager.add_manual(self.direction, target, self.project.clone(), cx)
        });
        self.editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.error = None;
        window.focus(&port_input_focus, cx);
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
        let is_empty = entries.is_empty();
        let detection_error = self.manager.read(cx).detection_error.clone();
        let can_add = parse_forward_input(&self.editor.read(cx).text(cx)).is_ok();
        let border_color = cx.theme().colors().border;
        let row_hover_background = cx.theme().colors().element_hover;
        v_flex()
            .key_context("PortForwardModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
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
                            .child(
                                Headline::new(i18n::t!("ff66c760b4eb4d6c"))
                                    .size(HeadlineSize::Small),
                            ),
                    )
                    .child(
                        IconButton::new("close-port-forward-modal", IconName::Close)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        // In an `auto_width` group only the group's own box shrinks to its
                        // content; as a direct child of this column it would still be
                        // stretched across the whole row.
                        h_flex()
                            .self_flex_start()
                            .debug_selector(|| "port-forward-direction-toggle".into())
                            .child(
                                ToggleButtonGroup::single_row(
                                    "port-forward-direction",
                                    [
                                        ToggleButtonWithIcon::new(
                                            i18n::t!("4e156a6242d654b7"),
                                            IconName::ArrowDown,
                                            cx.listener(|this, _, window, cx| {
                                                this.direction = ForwardDirection::RemoteToLocal;
                                                window.focus(&this.editor.focus_handle(cx), cx);
                                                cx.notify();
                                            }),
                                        ),
                                        ToggleButtonWithIcon::new(
                                            i18n::t!("2fe4aee45e505d4a"),
                                            IconName::ArrowUp,
                                            cx.listener(|this, _, window, cx| {
                                                this.direction = ForwardDirection::LocalToRemote;
                                                window.focus(&this.editor.focus_handle(cx), cx);
                                                cx.notify();
                                            }),
                                        ),
                                    ],
                                )
                                .style(ToggleButtonGroupStyle::Outlined)
                                .label_size(LabelSize::Default)
                                .auto_width()
                                .selected_index(
                                    match self.direction {
                                        ForwardDirection::RemoteToLocal => 0,
                                        ForwardDirection::LocalToRemote => 1,
                                    },
                                ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .debug_selector(|| "port-forward-port-input".into())
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .px_2()
                                    .py_1p5()
                                    .border_1()
                                    .border_color(border_color)
                                    .rounded_md()
                                    .overflow_hidden()
                                    .child(self.editor.clone()),
                            )
                            .child(
                                Button::new("add-port-forward", i18n::t!("7a8a11ead50742a2"))
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                                    .disabled(!can_add)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm(&Confirm, window, cx)
                                    })),
                            ),
                    ),
            )
            .when_some(detection_error, |this, error| {
                this.child(
                    Callout::new()
                        .severity(Severity::Warning)
                        .icon(IconName::Warning)
                        .description(error),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    Callout::new()
                        .severity(Severity::Error)
                        .icon(IconName::XCircle)
                        .description(error),
                )
            })
            .child(
                div()
                    .id("port-forward-list")
                    .max_h(rems(24.))
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(entries.into_iter().map(|entry| {
                        let direction = entry.direction;
                        let target = entry.target.clone();
                        let remote_port = target.port;
                        let source = match entry.source {
                            ForwardSource::Automatic => i18n::t!("7eb336e42cb5076b"),
                            ForwardSource::Manual => i18n::t!("962f41ef825b7266"),
                            ForwardSource::Preview => i18n::t!("07a1166b502215b7"),
                        };
                        let local_port = entry
                            .local_port
                            .map(|port| port.to_string())
                            .unwrap_or_else(|| i18n::t!("17bf181625e8da75").to_string());
                        let (address, status_color, status_dot_color) = match &entry.status {
                            ForwardStatus::Starting => (
                                i18n::t!("43f3dc12a6077835").to_string(),
                                Color::Muted,
                                Color::Muted,
                            ),
                            ForwardStatus::RunningUnconfirmed => (
                                match direction {
                                    ForwardDirection::RemoteToLocal => {
                                        format!("http://127.0.0.1:{local_port}")
                                    }
                                    ForwardDirection::LocalToRemote => {
                                        i18n::t!(
                                            "5c4f133bd53fce80",
                                            remote_port = remote_port,
                                            local_target = target.connect_label("127.0.0.1")
                                        )
                                    }
                                },
                                Color::Muted,
                                Color::Success,
                            ),
                            ForwardStatus::Retrying(error) => (
                                i18n::t!("432561af550457ec", error = error),
                                Color::Muted,
                                Color::Modified,
                            ),
                            ForwardStatus::Failed(error) => (
                                i18n::t!("377e359e55ec05ab", error = error),
                                Color::Error,
                                Color::Error,
                            ),
                        };
                        h_flex()
                            .px_2()
                            .py_1p5()
                            .gap_3()
                            .justify_between()
                            .rounded_md()
                            .hover(move |style| style.bg(row_hover_background))
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap_2()
                                    .child(Indicator::dot().color(status_dot_color))
                                    .child(
                                        v_flex()
                                            .min_w_0()
                                            .gap_0p5()
                                            .child(
                                                Label::new(match direction {
                                                    ForwardDirection::RemoteToLocal => {
                                                        i18n::t!(
                                                            "22880116ce7d18fe",
                                                            remote_port = target.to_string()
                                                        )
                                                    }
                                                    ForwardDirection::LocalToRemote => {
                                                        i18n::t!(
                                                            "f2e88cffef1ff16e",
                                                            local_port = target.to_string()
                                                        )
                                                    }
                                                })
                                                .size(LabelSize::Small),
                                            )
                                            .when(
                                                matches!(
                                                    entry.status,
                                                    ForwardStatus::RunningUnconfirmed
                                                ),
                                                |this| {
                                                    this.child(
                                                        Label::new(i18n::t!("c7f7510832ed11ff"))
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                },
                                            )
                                            .child(
                                                if direction == ForwardDirection::RemoteToLocal
                                                    && entry.local_port.is_some()
                                                    && matches!(
                                                        entry.status,
                                                        ForwardStatus::RunningUnconfirmed
                                                    )
                                                {
                                                    let address_for_click = address.clone();
                                                    div()
                                                        .id(format!(
                                                            "{}:{target}",
                                                            match direction {
                                                                ForwardDirection::RemoteToLocal => {
                                                                    "remote-forward-address"
                                                                }
                                                                ForwardDirection::LocalToRemote => {
                                                                    "reverse-forward-address"
                                                                }
                                                            }
                                                        ))
                                                        .cursor_pointer()
                                                        .tooltip(ui::Tooltip::text(i18n::t!(
                                                            "f5b68300857567ee"
                                                        )))
                                                        .child(
                                                            Label::new(address)
                                                                .size(LabelSize::Small)
                                                                .color(status_color),
                                                        )
                                                        .on_click(move |event, _, cx| {
                                                            if event.modifiers().control {
                                                                cx.open_url(&address_for_click);
                                                            } else {
                                                                cx.write_to_clipboard(
                                                                    ClipboardItem::new_string(
                                                                        address_for_click.clone(),
                                                                    ),
                                                                );
                                                            }
                                                        })
                                                        .into_any_element()
                                                } else {
                                                    Label::new(address)
                                                        .size(LabelSize::Small)
                                                        .color(status_color)
                                                        .into_any_element()
                                                },
                                            ),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Label::new(source)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(stop_forward_button(
                                        &entry,
                                        &self.manager,
                                        IconSize::Medium,
                                        ButtonSize::Default,
                                    )),
                            )
                    }))
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
            .when(is_empty, |this| {
                this.child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .py_6()
                        .child(
                            Icon::new(IconName::ArrowRightLeft)
                                .size(IconSize::Medium)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(i18n::t!("c7e2adcd4fbeb8c6"))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .text_center(),
                        ),
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
        workspace.show_error(i18n::t!("a4a2b04c76e4a48d"), cx);
        return;
    }
    workspace.toggle_modal(window, cx, move |window, cx| {
        PortForwardModal::new(manager, project, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::detected_ports;

    fn loopback(port: u16) -> super::ForwardTarget {
        super::ForwardTarget { host: None, port }
    }

    fn host_target(host: &str, port: u16) -> super::ForwardTarget {
        super::ForwardTarget {
            host: Some(host.to_string()),
            port,
        }
    }

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
            manager.forward_launcher = Some(Box::new(move |_, _, _| {
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
                loopback(39001),
                ForwardSource::Manual,
                project.clone(),
                cx,
            )
        });
        cx.run_until_parked();
        let first_port = manager.read_with(cx, |manager, _| {
            let entry = &manager.entries[&(RemoteToLocal, loopback(39001))];
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
                manager.entries[&(RemoteToLocal, loopback(39001))].status,
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
            let ForwardStatus::Retrying(error) =
                &manager.entries[&(RemoteToLocal, loopback(39001))].status
            else {
                panic!("expected a scheduled retry after the late exit")
            };
            assert!(
                error.contains("late diagnostic")
                    && error.contains("收集超时")
                    && error.contains("已截断")
            );
        });
        assert_eq!(launches.borrow().len(), 1, "retry waits for the backoff");
        assert!(launches.borrow()[0].3.get() && launches.borrow()[0].4.get());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 2, "late exit must retry");
        manager.read_with(cx, |manager, _| {
            let entry = &manager.entries[&(RemoteToLocal, loopback(39001))];
            assert_eq!(entry.status, ForwardStatus::RunningUnconfirmed);
            assert_eq!(
                entry.local_port, first_port,
                "a late exit keeps the advertised local port"
            );
        });
        manager.update(cx, |manager, cx| {
            manager.add_manual(RemoteToLocal, loopback(39001), project.clone(), cx)
        });
        cx.run_until_parked();
        // Independent EOF and exit: early exit retries even while EOF is delayed.
        launches.borrow_mut()[2]
            .0
            .take()
            .unwrap()
            .send(Ok(success_status()))
            .unwrap();
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        cx.run_until_parked();
        assert_eq!(
            launches.borrow().len(),
            3,
            "early exit waits for the backoff"
        );
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 4, "early exit must retry");
        manager.read_with(cx, |manager, _| {
            let entry = &manager.entries[&(RemoteToLocal, loopback(39001))];
            assert_eq!(entry.status, ForwardStatus::RunningUnconfirmed);
            assert_ne!(entry.local_port, first_port);
        });
        launches.borrow()[3]
            .1
            .unbounded_send(Err(std::io::Error::other("injected read failure")))
            .unwrap();
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            let ForwardStatus::Retrying(error) =
                &manager.entries[&(RemoteToLocal, loopback(39001))].status
            else {
                panic!("expected a scheduled retry after the read failure")
            };
            assert!(error.contains("读取 SSH 诊断失败") && error.contains("injected read failure"));
        });
        assert!(launches.borrow()[3].3.get() && launches.borrow()[3].4.get());
        manager.update(cx, |manager, cx| {
            manager.stop(RemoteToLocal, &loopback(39001), cx)
        });
        cx.run_until_parked();
        manager.update(cx, |manager, cx| {
            manager.add_manual(LocalToRemote, loopback(39002), project.clone(), cx)
        });
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 5);
        launches.borrow_mut()[4]
            .0
            .take()
            .unwrap()
            .send(Ok(success_status()))
            .unwrap();
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        cx.run_until_parked();
        manager.update(cx, |manager, cx| {
            manager.stop(LocalToRemote, &loopback(39002), cx)
        });
        cx.run_until_parked();
        assert!(launches.borrow()[4].3.get() && launches.borrow()[4].4.get());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert!(
                !manager
                    .entries
                    .contains_key(&(LocalToRemote, loopback(39002)))
            )
        });
        assert_eq!(
            launches.borrow().len(),
            5,
            "a stopped retry must not launch"
        );
        manager.update(cx, |manager, cx| {
            manager.add_manual(LocalToRemote, loopback(39002), project.clone(), cx)
        });
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 6);
        launches.borrow()[5].1.close_channel();
        launches.borrow_mut()[5]
            .0
            .take()
            .unwrap()
            .send(Ok(success_status()))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            launches.borrow().len(),
            6,
            "reverse retry waits for the backoff"
        );
        manager.read_with(cx, |manager, _| {
            assert!(matches!(
                manager.entries[&(LocalToRemote, loopback(39002))].status,
                ForwardStatus::Retrying(_)
            ))
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 7, "reverse early exit must retry");
        manager.update(cx, |manager, cx| {
            manager.stop(LocalToRemote, &loopback(39002), cx)
        });
        cx.run_until_parked();
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
        // Replacing only the live transport of the same client is a reconnect:
        // the captured automatic scope and its pending probe must survive it.
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
        assert_eq!(
            launches.borrow().len(),
            1,
            "a transport replacement keeps the pending automatic probe"
        );
        let (probe, cancelled) = launches.borrow_mut().remove(0);
        assert!(!cancelled.get());
        probe
            .send(std::process::Output {
                status: success_status(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
            .unwrap();
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert!(manager.entries.is_empty());
            assert!(manager.detection_task.is_none());
            assert!(
                manager.automatic_context.is_some(),
                "a transport reconnect keeps the automatic scope"
            );
        });
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
            manager.forward_launcher = Some(Box::new(move |direction, target, _args| {
                let (sender, output) = futures::channel::oneshot::channel();
                let cancelled = Rc::new(Cell::new(false));
                forwards
                    .borrow_mut()
                    .push((direction, target.clone(), sender, cancelled.clone()));
                super::ForwardChild::Controlled { output, cancelled }
            }));
            manager.start(
                super::ForwardDirection::RemoteToLocal,
                loopback(3000),
                super::ForwardSource::Automatic,
                project.clone(),
                cx,
            );
            manager.add_manual(
                super::ForwardDirection::RemoteToLocal,
                loopback(4000),
                project.clone(),
                cx,
            );
            manager.add_manual(
                super::ForwardDirection::LocalToRemote,
                loopback(4001),
                project.clone(),
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(forwards.borrow().len(), 3);
        manager.update(cx, |manager, cx| {
            manager.add_manual(
                super::ForwardDirection::RemoteToLocal,
                loopback(3000),
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
                loopback(3002),
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
                    (
                        super::ForwardDirection::RemoteToLocal,
                        super::ForwardTarget::loopback(port),
                    ),
                    super::ForwardEntry {
                        direction: super::ForwardDirection::RemoteToLocal,
                        target: super::ForwardTarget::loopback(port),
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
    fn retry_backoff_is_bounded_and_capped() {
        use std::time::Duration;
        assert_eq!(super::retry_delay(1), Duration::from_secs(1));
        assert_eq!(super::retry_delay(2), Duration::from_secs(2));
        assert_eq!(super::retry_delay(3), Duration::from_secs(4));
        assert_eq!(super::retry_delay(4), Duration::from_secs(8));
        assert_eq!(super::retry_delay(5), Duration::from_secs(16));
        assert_eq!(super::retry_delay(6), super::RETRY_MAX_DELAY);
        assert_eq!(super::retry_delay(1_000), super::RETRY_MAX_DELAY);
    }

    #[gpui::test]
    async fn automatic_scope_survives_transport_reconnect_but_not_replacement(
        cx: &mut gpui::TestAppContext,
    ) {
        use project::trusted_worktrees::{self, PathTrust};
        use std::path::Path;
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            trusted_worktrees::init(Default::default(), cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({"file": ""}))
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
        cx.update(|cx| {
            assert!(
                super::AutomaticForwardContext::capture(project.clone(), cx).is_none(),
                "untrusted roots never capture a scope"
            )
        });
        trusted.update(cx, |trusted, cx| {
            trusted.trust(&store, [PathTrust::Worktree(id)].into_iter().collect(), cx)
        });
        let context = cx.update(|cx| {
            super::AutomaticForwardContext::capture(project.clone(), cx).expect("trusted scope")
        });

        let other_client = remote::RemoteClient::test_connected_ssh(cx).await;
        let reconnected = other_client
            .read_with(cx, |client, _| client.connection())
            .expect("replacement transport");
        let previous = client
            .read_with(cx, |client, _| client.connection())
            .expect("transport");
        assert!(!std::sync::Arc::ptr_eq(&previous, &reconnected));
        client.update(cx, |client, cx| {
            client.set_connection_for_test(Some(reconnected), cx)
        });
        cx.update(|cx| {
            assert!(
                context.is_current(cx),
                "a transport reconnect keeps the automatic scope"
            )
        });

        let replacement_client = remote::RemoteClient::test_connected_ssh(cx).await;
        project.update(cx, |project, cx| {
            project.set_remote_client_for_test(replacement_client, cx)
        });
        cx.update(|cx| {
            assert!(
                !context.is_current(cx),
                "a replaced remote client ends the captured scope"
            )
        });
    }

    #[gpui::test]
    async fn forward_waits_for_the_session_and_retries_after_reconnect(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::{ForwardDirection::*, ForwardStatus};
        use gpui::AppContext as _;
        use std::{cell::RefCell, rc::Rc};
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let client = remote::RemoteClient::test_connected_ssh(cx).await;
        project.update(cx, |project, cx| {
            project.set_remote_client_for_test(client.clone(), cx)
        });
        let connection = client
            .read_with(cx, |client, _| client.connection())
            .expect("transport");
        let manager = cx.new(|_| super::PortForwardManager::new());
        let launches = Rc::new(RefCell::new(Vec::new()));
        manager.update(cx, |manager, _| {
            let launches = launches.clone();
            manager.forward_launcher = Some(Box::new(move |_, _, _| {
                let cancelled = Rc::new(std::cell::Cell::new(false));
                let (output, receiver) = futures::channel::oneshot::channel();
                launches.borrow_mut().push((output, cancelled.clone()));
                super::ForwardChild::Controlled {
                    output: receiver,
                    cancelled,
                }
            }));
        });

        client.update(cx, |client, cx| client.set_connection_for_test(None, cx));
        manager.update(cx, |manager, cx| {
            manager.add_manual(RemoteToLocal, loopback(39101), project.clone(), cx)
        });
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            let ForwardStatus::Retrying(error) =
                &manager.entries[&(RemoteToLocal, loopback(39101))].status
            else {
                panic!("expected a retry while the session is unavailable")
            };
            assert!(error.contains("无法创建 SSH 端口转发命令"), "{error}");
        });
        assert!(launches.borrow().is_empty());
        manager.update(cx, |manager, cx| {
            manager.add_manual(RemoteToLocal, loopback(39102), project.clone(), cx)
        });
        cx.run_until_parked();
        assert!(launches.borrow().is_empty());

        // A successful session reconnect wakes every pending backoff at once, so
        // the tunnels come back without waiting out the retry delay.
        client.update(cx, |client, cx| {
            client.set_connection_for_test(Some(connection), cx)
        });
        client.update(cx, |_, cx| cx.emit(remote::RemoteClientEvent::Reconnected));
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 2, "the tunnels recover");
        manager.read_with(cx, |manager, _| {
            for port in [39101, 39102] {
                let entry = &manager.entries[&(RemoteToLocal, loopback(port))];
                assert_eq!(entry.status, ForwardStatus::RunningUnconfirmed);
                assert!(entry.local_port.is_some());
            }
        });
        manager.update(cx, |manager, cx| {
            manager.stop(RemoteToLocal, &loopback(39101), cx);
            manager.stop(RemoteToLocal, &loopback(39102), cx);
        });
        cx.run_until_parked();
        assert!(
            launches
                .borrow()
                .iter()
                .all(|(_, cancelled)| cancelled.get()),
            "stopping releases both tunnels"
        );
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
    fn forward_input_accepts_ports_and_hosts() {
        use super::ForwardInputError;
        for text in [
            "7990",
            " 8080 ",
            "65535",
            "192.168.1.3:7990",
            "HOST.example:7990",
            "[fe80::1]:7990",
            "[::1]:7990",
        ] {
            assert!(super::parse_forward_input(text).is_ok(), "{text:?}");
        }
        for (text, expected) in [
            (
                "7990",
                super::ForwardTarget {
                    host: None,
                    port: 7990,
                },
            ),
            (
                "192.168.1.3:7990",
                super::ForwardTarget {
                    host: Some("192.168.1.3".to_string()),
                    port: 7990,
                },
            ),
            (
                "HOST.example:7990",
                super::ForwardTarget {
                    host: Some("host.example".to_string()),
                    port: 7990,
                },
            ),
            (
                "[fe80::1]:7990",
                super::ForwardTarget {
                    host: Some("fe80::1".to_string()),
                    port: 7990,
                },
            ),
            ("localhost:3000", loopback(3000)),
            ("127.0.0.1:3000", loopback(3000)),
            ("0.0.0.0:3000", loopback(3000)),
            ("[::]:3000", loopback(3000)),
        ] {
            assert_eq!(super::parse_forward_input(text), Ok(expected), "{text:?}");
        }
        for (text, expected) in [
            ("", ForwardInputError::Empty),
            ("   ", ForwardInputError::Empty),
            ("0", ForwardInputError::InvalidPort),
            ("65536", ForwardInputError::InvalidPort),
            ("-1", ForwardInputError::InvalidPort),
            ("abc", ForwardInputError::InvalidPort),
            ("3000abc", ForwardInputError::InvalidPort),
            ("192.168.1.3:", ForwardInputError::InvalidPort),
            ("192.168.1.3", ForwardInputError::InvalidPort),
            (":7990", ForwardInputError::InvalidHost),
            ("::1:7990", ForwardInputError::InvalidHost),
            ("[::1]", ForwardInputError::InvalidHost),
            ("[]:7990", ForwardInputError::InvalidHost),
            ("a b:7990", ForwardInputError::InvalidHost),
            ("host/path:7990", ForwardInputError::InvalidHost),
            ("http://host:7990", ForwardInputError::InvalidHost),
        ] {
            assert_eq!(super::parse_forward_input(text), Err(expected), "{text:?}");
        }
    }

    #[test]
    fn target_labels_bracket_ipv6_and_hide_loopback() {
        assert_eq!(loopback(7990).to_string(), "7990");
        assert_eq!(
            host_target("192.168.1.3", 7990).to_string(),
            "192.168.1.3:7990"
        );
        assert_eq!(host_target("fe80::1", 7990).to_string(), "[fe80::1]:7990");
        assert_eq!(loopback(7990).connect_label("localhost"), "localhost:7990");
        assert_eq!(
            host_target("192.168.1.3", 7990).connect_label("localhost"),
            "192.168.1.3:7990"
        );
        assert_eq!(
            host_target("fe80::1", 7990).connect_label("127.0.0.1"),
            "[fe80::1]:7990"
        );
    }

    #[gpui::test]
    async fn host_targets_are_keyed_independently(cx: &mut gpui::TestAppContext) {
        use super::{ForwardDirection, ForwardDirection::*, ForwardSource, ForwardStatus};
        use gpui::AppContext as _;
        use std::{cell::RefCell, rc::Rc};
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
        let launches: Rc<
            RefCell<
                Vec<(
                    ForwardDirection,
                    super::ForwardTarget,
                    Vec<String>,
                    futures::channel::oneshot::Sender<std::process::Output>,
                )>,
            >,
        > = Rc::new(RefCell::new(Vec::new()));
        manager.update(cx, |manager, _| {
            let launches = launches.clone();
            manager.forward_launcher = Some(Box::new(move |direction, target, args| {
                let (sender, output) = futures::channel::oneshot::channel();
                launches
                    .borrow_mut()
                    .push((direction, target.clone(), args.to_vec(), sender));
                super::ForwardChild::Controlled {
                    output,
                    cancelled: Rc::new(std::cell::Cell::new(false)),
                }
            }));
        });
        manager.update(cx, |manager, cx| {
            manager.add_manual(
                RemoteToLocal,
                host_target("192.168.1.3", 7990),
                project.clone(),
                cx,
            );
            manager.add_manual(
                RemoteToLocal,
                host_target("10.0.0.5", 7990),
                project.clone(),
                cx,
            );
            manager.add_manual(
                LocalToRemote,
                host_target("192.168.1.3", 7990),
                project.clone(),
                cx,
            );
        });
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert_eq!(manager.entries.len(), 3);
            assert!(
                manager
                    .entries
                    .contains_key(&(RemoteToLocal, host_target("192.168.1.3", 7990)))
            );
            assert!(
                manager
                    .entries
                    .contains_key(&(RemoteToLocal, host_target("10.0.0.5", 7990)))
            );
            assert!(
                manager
                    .entries
                    .contains_key(&(LocalToRemote, host_target("192.168.1.3", 7990)))
            );
            assert!(
                manager
                    .snapshots()
                    .iter()
                    .all(|entry| entry.status == ForwardStatus::RunningUnconfirmed)
            );
            assert!(
                manager
                    .snapshots()
                    .iter()
                    .all(|entry| entry.source == ForwardSource::Manual)
            );
        });
        let args_for = |direction: ForwardDirection, host: &str| {
            launches
                .borrow()
                .iter()
                .find(|(launch_direction, target, _, _)| {
                    *launch_direction == direction
                        && target.host.as_deref() == Some(host)
                        && target.port == 7990
                })
                .map(|(_, _, args, _)| args.clone())
                .expect("forward was launched")
        };
        // The mock transport renders `<local>:<host>:<remote>` for `-L` and
        // `-R<remote>:<host>:<local>` for `-R`.
        assert!(
            args_for(RemoteToLocal, "192.168.1.3")
                .iter()
                .any(|arg| arg.contains("192.168.1.3:7990"))
        );
        assert!(
            args_for(RemoteToLocal, "10.0.0.5")
                .iter()
                .any(|arg| arg.contains("10.0.0.5:7990"))
        );
        assert!(
            args_for(LocalToRemote, "192.168.1.3")
                .iter()
                .any(|arg| arg.contains("192.168.1.3:7990"))
        );
        // Re-adding the same host and port replaces the entry instead of forking it.
        manager.update(cx, |manager, cx| {
            manager.add_manual(
                RemoteToLocal,
                host_target("192.168.1.3", 7990),
                project.clone(),
                cx,
            )
        });
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| assert_eq!(manager.entries.len(), 3));
        assert_eq!(launches.borrow().len(), 4);
    }

    #[gpui::test]
    async fn direction_toggle_stays_content_sized(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, TextRun, VisualTestContext, font, px, size};
        use project::Project;
        use std::path::Path;
        use workspace::AppState;

        let fs = cx.update(AppState::test).fs.as_fake().clone();
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        fs.insert_tree("/project", serde_json::json!({"file": ""}))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let manager = cx.new(|_| super::PortForwardManager::new());
        let window = cx.open_window(size(px(900.), px(600.)), move |window, cx| {
            super::PortForwardModal::new(manager, project, window, cx)
        });
        cx.run_until_parked();

        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let toggle = visual
            .debug_bounds("port-forward-direction-toggle")
            .expect("direction toggle is rendered");
        let input_row = visual
            .debug_bounds("port-forward-port-input")
            .expect("port input row is rendered");
        assert!(
            toggle.size.width < input_row.size.width / 2.,
            "the direction toggle stretched across the modal: {toggle:?} vs {input_row:?}"
        );

        let (to_remote, remote_to_local) = visual.update(|window, _| {
            let text_system = window.text_system();
            let label_width = |label: &str| {
                let run = TextRun {
                    len: label.len(),
                    font: font("Helvetica"),
                    ..Default::default()
                };
                text_system.layout_line(label, px(14.), &[run], None).width
            };
            (
                label_width(i18n::t!("2fe4aee45e505d4a")),
                label_width(i18n::t!("4e156a6242d654b7")),
            )
        });
        assert_eq!(
            to_remote, remote_to_local,
            "the two direction labels must render at the same width, otherwise the segments become asymmetric"
        );
    }

    #[gpui::test]
    async fn shared_stop_button_closes_the_forward(cx: &mut gpui::TestAppContext) {
        use super::{
            ForwardDirection::*, ForwardEntry, ForwardSnapshot, ForwardSource, ForwardStatus,
            PortForwardManager, stop_forward_button,
        };
        use gpui::{
            AppContext as _, Context, Entity, IntoElement, Modifiers, Render, VisualTestContext,
            Window, px, size,
        };
        use settings::SettingsStore;

        struct StopButtonHarness {
            manager: Entity<PortForwardManager>,
            entry: ForwardSnapshot,
        }

        impl Render for StopButtonHarness {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                stop_forward_button(
                    &self.entry,
                    &self.manager,
                    super::IconSize::Medium,
                    super::ButtonSize::Default,
                )
            }
        }

        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let manager = cx.new(|_| PortForwardManager::new());
        let target = host_target("192.168.1.3", 7990);
        manager.update(cx, |manager, _| {
            manager.entries.insert(
                (RemoteToLocal, target.clone()),
                ForwardEntry {
                    direction: RemoteToLocal,
                    target,
                    local_port: Some(7990),
                    source: ForwardSource::Manual,
                    status: ForwardStatus::RunningUnconfirmed,
                    generation: 1,
                    cancellation: None,
                },
            );
        });
        let entry = manager.read_with(cx, |manager, _| {
            manager
                .snapshots()
                .into_iter()
                .next()
                .expect("the seeded forward is listed")
        });
        let window_manager = manager.clone();
        let window = cx.open_window(size(px(320.), px(120.)), move |_, _| StopButtonHarness {
            manager: window_manager,
            entry,
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let stop = visual
            .debug_bounds("ICON-Stop")
            .expect("the shared stop button is rendered");
        visual.simulate_click(stop.center(), Modifiers::default());
        visual.run_until_parked();
        assert!(
            visual.update(|_, cx| manager.read(cx).snapshots().is_empty()),
            "clicking the stop button must remove the forward"
        );
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
