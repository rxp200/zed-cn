use std::{collections::HashSet, net::TcpListener, process::Stdio, sync::LazyLock, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use editor::Editor;
use futures::{FutureExt as _, channel::oneshot, select};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Task, WeakEntity, Window,
};
use menu::{Cancel, Confirm};
use project::Project;
use regex::Regex;
use remote::{RemoteClient, RemoteConnectionOptions};
use ui::{
    Button, ButtonStyle, Color, Headline, HeadlineSize, Icon, IconButton, IconName, IconSize,
    Label, LabelSize, prelude::*,
};
use util::{ResultExt as _, command::new_command};
use workspace::{ModalView, Workspace};

static URL_PORT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://)?(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])[:：](\d{1,5})")
        .expect("valid port detection regex")
});
static LISTENING_PORT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:listening|listen|running|started|server|服务)[^\r\n]{0,32}?(?:port|端口)\s*[:：]?\s*(\d{1,5})")
        .expect("valid listening port detection regex")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardSource {
    Automatic,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardStatus {
    Starting,
    Active,
    Failed(String),
}

struct ForwardEntry {
    remote_port: u16,
    local_port: Option<u16>,
    source: ForwardSource,
    status: ForwardStatus,
    generation: u64,
    cancellation: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct ForwardSnapshot {
    pub remote_port: u16,
    pub local_port: Option<u16>,
    pub source: ForwardSource,
    pub status: ForwardStatus,
}

pub struct PortForwardManager {
    entries: HashMap<u16, ForwardEntry>,
    automatically_seen: HashSet<u16>,
    next_generation: u64,
    _tasks: Vec<Task<()>>,
}

impl PortForwardManager {
    pub fn new() -> Self {
        Self {
            entries: HashMap::default(),
            automatically_seen: HashSet::default(),
            next_generation: 0,
            _tasks: Vec::new(),
        }
    }

    pub fn snapshots(&self) -> Vec<ForwardSnapshot> {
        let mut snapshots = self
            .entries
            .values()
            .map(|entry| ForwardSnapshot {
                remote_port: entry.remote_port,
                local_port: entry.local_port,
                source: entry.source,
                status: entry.status.clone(),
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|entry| entry.remote_port);
        snapshots
    }

    pub fn detect_from_terminal_output(
        &mut self,
        output: &str,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        for remote_port in detected_ports(output) {
            if self.automatically_seen.insert(remote_port)
                && !self.entries.contains_key(&remote_port)
            {
                self.start(remote_port, ForwardSource::Automatic, project.clone(), cx);
            }
        }
    }

    pub fn add_manual(
        &mut self,
        remote_port: u16,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        self.stop(remote_port, cx);
        self.start(remote_port, ForwardSource::Manual, project, cx);
    }

    pub fn stop(&mut self, remote_port: u16, cx: &mut Context<Self>) {
        if let Some(mut entry) = self.entries.remove(&remote_port)
            && let Some(cancellation) = entry.cancellation.take()
        {
            cancellation.send(()).ok();
        }
        cx.notify();
    }

    fn start(
        &mut self,
        remote_port: u16,
        source: ForwardSource,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        self._tasks.retain(|task| !task.is_ready());
        let Some(remote_client) = ssh_remote_client(&project, cx) else {
            return;
        };

        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        let (cancellation, cancellation_receiver) = oneshot::channel();
        self.entries.insert(
            remote_port,
            ForwardEntry {
                remote_port,
                local_port: None,
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
                remote_port,
                cancellation_receiver,
                manager.clone(),
                generation,
                cx,
            )
            .await;
            if let Err(error) = result {
                manager
                    .update(cx, |manager, cx| {
                        if let Some(entry) = manager.entries.get_mut(&remote_port)
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
    remote_port: u16,
    cancellation: oneshot::Receiver<()>,
    manager: WeakEntity<PortForwardManager>,
    generation: u64,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let mut last_error = None;
    let mut cancellation = cancellation.fuse();
    let mut next_local_port =
        available_local_port(Some(remote_port)).or_else(|_| available_local_port(None))?;

    for attempt in 0..5 {
        let local_port = next_local_port;
        let command_template = remote_client.read_with(cx, |client, _| {
            client.build_forward_ports_command(vec![(
                local_port,
                "localhost".to_string(),
                remote_port,
            )])
        })?;

        let mut command = new_command(&command_template.program);
        command
            .args(&command_template.args)
            .envs(&command_template.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().context("无法启动 SSH 端口转发进程")?;

        let startup_timer = cx
            .background_executor()
            .timer(Duration::from_millis(400))
            .fuse();
        futures::pin_mut!(startup_timer);
        select! {
            _ = cancellation => return Ok(()),
            _ = startup_timer => {}
        }

        if let Some(status) = child.try_status().context("无法检查 SSH 端口转发状态")? {
            let output = child.output().await.context("无法读取 SSH 端口转发错误")?;
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            last_error = Some(if stderr.is_empty() {
                anyhow!("SSH 端口转发进程已退出：{status}")
            } else {
                anyhow!(stderr)
            });
            if attempt < 4 {
                next_local_port = available_local_port(None)?;
            }
            continue;
        }

        manager.update(cx, |manager, cx| {
            if let Some(entry) = manager.entries.get_mut(&remote_port)
                && entry.generation == generation
            {
                entry.local_port = Some(local_port);
                entry.status = ForwardStatus::Active;
                cx.notify();
            }
        })?;

        let status = child.status().fuse();
        futures::pin_mut!(status);
        select! {
            result = status => {
                let status = result.context("无法等待 SSH 端口转发进程")?;
                return Err(anyhow!("SSH 端口转发已停止：{status}"));
            }
            _ = cancellation => return Ok(()),
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("无法分配本地端口")))
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
    error: Option<String>,
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
            editor.set_placeholder_text("远程端口，例如 9999", window, cx);
            editor
        });
        cx.observe(&manager, |_, _, cx| cx.notify()).detach();
        Self {
            manager,
            project,
            editor,
            error: None,
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx).trim().to_string();
        let Ok(port) = text.parse::<u16>() else {
            self.error = Some("请输入 1 到 65535 之间的远程端口号".to_string());
            cx.notify();
            return;
        };
        if port == 0 {
            self.error = Some("端口号不能为 0".to_string());
            cx.notify();
            return;
        }
        self.manager.update(cx, |manager, cx| {
            manager.add_manual(port, self.project.clone(), cx)
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.manager.read(cx).snapshots();
        v_flex()
            .key_context("PortForwardModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(38.))
            .max_h(rems(32.))
            .p_3()
            .gap_3()
            .child(
                h_flex()
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
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(div().gap_1().children(entries.into_iter().map(|entry| {
                let remote_port = entry.remote_port;
                let source = match entry.source {
                    ForwardSource::Automatic => "自动",
                    ForwardSource::Manual => "手动",
                };
                let (address, status_color) = match &entry.status {
                    ForwardStatus::Starting => ("正在启动…".to_string(), Color::Muted),
                    ForwardStatus::Active => (
                        format!("127.0.0.1:{}", entry.local_port.unwrap_or_default()),
                        Color::Success,
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
                                Label::new(format!("远程端口 {remote_port}"))
                                    .size(LabelSize::Small),
                            )
                            .child(
                                Label::new(address)
                                    .size(LabelSize::Small)
                                    .color(status_color),
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
                            .child(
                                IconButton::new(
                                    ("stop-port-forward", u64::from(remote_port)),
                                    IconName::Stop,
                                )
                                .tooltip(ui::Tooltip::text("停止转发"))
                                .on_click({
                                    let manager = self.manager.clone();
                                    move |_, _, cx| {
                                        manager
                                            .update(cx, |manager, cx| manager.stop(remote_port, cx))
                                    }
                                }),
                            ),
                    )
            })))
            .when(self.manager.read(cx).snapshots().is_empty(), |this| {
                this.child(
                    Label::new("暂无转发。远程终端中出现 localhost 端口时会自动添加。")
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
        PortForwardModal::new(manager, project.clone(), window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::detected_ports;

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
