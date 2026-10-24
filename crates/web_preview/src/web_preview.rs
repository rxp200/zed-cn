use std::{
    io::{ErrorKind, Read as _, Write as _},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use editor::Editor;
use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, WeakEntity, Window};
use percent_encoding::percent_decode_str;
use project::{Project, ProjectItem as _, trusted_worktrees::TrustedWorktrees};
use task::{RevealStrategy, SaveStrategy, Shell, TaskContext, TaskTemplate};
use terminal_view::{
    TerminalView,
    port_forwarding::{ForwardDirection, ForwardSource, ForwardStatus},
    terminal_panel::TerminalPanel,
};
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};
use zed_actions::preview::web::{OpenPreview, StopPreview};

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const RELOAD_PATH: &str = "/.zed-live-preview/reload";
const EVENTS_PATH: &str = "/.zed-live-preview/events";
const MAX_CONCURRENT_CONNECTIONS: usize = 64;
const RELOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);
// The same script is embedded in remote_preview_server.py for SSH previews;
// keep both copies byte-identical.
const RELOAD_SCRIPT: &str = r#"<script>(()=>{const base='/.zed-live-preview';const declare=()=>{const q=new URLSearchParams();q.append('p',location.pathname);performance.getEntriesByType('resource').slice(0,63).forEach(e=>{const u=new URL(e.name,location.href);if(u.origin===location.origin)q.append('p',u.pathname)});return q.toString()};const reload=()=>location.reload();if(typeof EventSource!=='function'){let seen='';const poll=()=>{fetch(base+'/reload?'+declare(),{cache:'no-store'}).then(r=>r.text()).then(next=>{if(seen&&seen!==next)reload();seen=next}).catch(()=>{}).finally(()=>setTimeout(poll,1000))};poll();return}let stream=null;let declared='';const connect=()=>{declared=declare();const next=new EventSource(base+'/events?'+declared);next.onmessage=e=>{if(e.data==='reload')reload()};next.onerror=()=>{if(next.readyState===EventSource.CLOSED){next.close();setTimeout(connect,1000)}};if(stream)stream.close();stream=next};const refresh=setInterval(()=>{if(stream&&stream.readyState===EventSource.OPEN&&declare()!==declared)connect()},2000);addEventListener('pagehide',()=>{clearInterval(refresh);if(stream)stream.close()});connect()})()</script>"#;

pub struct GlobalPreviewState(pub Entity<PreviewState>);

impl Global for GlobalPreviewState {}

pub fn preview_state(cx: &App) -> Option<Entity<PreviewState>> {
    cx.try_global::<GlobalPreviewState>()
        .map(|state| state.0.clone())
}

pub fn init(cx: &mut App) {
    let state = cx.new(|_| PreviewState::default());
    cx.observe_new({
        let state = state.clone();
        move |workspace: &mut Workspace, window, _cx| {
            let Some(_window) = window else {
                return;
            };
            let state = state.clone();
            workspace.register_action({
                let state = state.clone();
                move |workspace, _: &OpenPreview, window, cx| {
                    open_preview(workspace, state.clone(), window, cx)
                }
            });
            workspace.register_action(move |workspace, _: &StopPreview, window, cx| {
                stop_preview(workspace, state.clone(), window, cx)
            });
        }
    })
    .detach();
    cx.set_global(GlobalPreviewState(state));
}

#[derive(Default)]
pub struct PreviewState {
    local_server: Option<LocalServer>,
    remote: Option<RemotePreview>,
    // The eye toggle and the preview terminal are one unit: closing either of
    // them must close the other.
    _terminal_release: Option<Subscription>,
}

struct RemotePreview {
    port: u16,
    task_id: task::TaskId,
    terminal_view: Option<WeakEntity<TerminalView>>,
    forward_manager: Entity<terminal_view::port_forwarding::PortForwardManager>,
}

impl PreviewState {
    pub fn is_active(&self) -> bool {
        self.local_server.is_some() || self.remote.is_some()
    }

    fn track_remote(
        &mut self,
        remote: RemotePreview,
        terminal_view: Option<Entity<TerminalView>>,
        cx: &mut Context<Self>,
    ) {
        self._terminal_release = terminal_view.map(|terminal_view| {
            cx.observe_release(&terminal_view, |state, _, cx| state.clear_remote(cx))
        });
        self.remote = Some(remote);
        cx.notify();
    }

    fn clear_remote(&mut self, cx: &mut Context<Self>) {
        let Some(remote) = self.remote.take() else {
            return;
        };
        remote.forward_manager.update(cx, |manager, cx| {
            manager.stop_preview(remote.port, cx);
        });
        cx.notify();
    }
}

struct LocalServer {
    root: PathBuf,
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn open_preview(
    workspace: &mut Workspace,
    state: Entity<PreviewState>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        workspace.show_error(i18n::t!("717b21d6a2533ade"), cx);
        return;
    };
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
        workspace.show_error(i18n::t!("0c3a3b87fe4bf824"), cx);
        return;
    };
    let Some(project_path) = buffer.read(cx).project_path(cx) else {
        workspace.show_error(i18n::t!("6e3ee785154131b6"), cx);
        return;
    };
    if !is_html_path(project_path.path.as_ref().as_std_path()) {
        workspace.show_error(i18n::t!("3200499bdffd1d66"), cx);
        return;
    }
    let project = workspace.project().clone();
    if project.read(cx).is_disconnected(cx) {
        workspace.show_error(i18n::t!("966459d6f15e55df"), cx);
        return;
    }
    if project.read(cx).is_via_collab()
        || TrustedWorktrees::has_restricted_worktrees(&project.read(cx).worktree_store(), cx)
    {
        workspace.show_error(i18n::t!("7719f96750489b48"), cx);
        return;
    }
    let Some(worktree) = project
        .read(cx)
        .worktree_for_id(project_path.worktree_id, cx)
    else {
        workspace.show_error(i18n::t!("a39f02c39db04bad"), cx);
        return;
    };
    let root = worktree.read(cx).abs_path().to_path_buf();
    let relative_path = project_path.path.as_ref().as_std_path().to_path_buf();
    let save = project.update(cx, |project, cx| project.save_buffer(buffer.clone(), cx));
    let remote = project.read(cx).is_via_remote_server();
    let remote_options = project.read(cx).remote_connection_options(cx);

    cx.spawn_in(window, async move |workspace, cx| {
        let result: Result<()> = async {
            save.await.context(i18n::t!("f74090096f5dd5fb"))?;
            if buffer.read_with(cx, |buffer, _| buffer.is_dirty()) {
                bail!(i18n::t!("b8236805d58908a7"));
            }
            if remote {
                if !matches!(
                    remote_options,
                    Some(remote::RemoteConnectionOptions::Ssh(_))
                ) {
                    bail!(i18n::t!("e7e19bfc96c1924e"));
                }
                start_remote_preview(&workspace, state, project, root, relative_path, cx).await
            } else {
                start_local_preview(&workspace, state, root, relative_path, cx).await
            }
        }
        .await;
        if let Err(error) = result {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(i18n::t!("abd879ce7807cac2", error = error), cx);
                })
                .log_err();
        }
    })
    .detach();
}

async fn start_local_preview(
    workspace: &gpui::WeakEntity<Workspace>,
    state: Entity<PreviewState>,
    root: PathBuf,
    relative_path: PathBuf,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let url = state.update(cx, |state, cx| -> Result<String> {
        let server = match state.local_server.as_ref() {
            Some(server) if server.root == root => server,
            _ => {
                state.local_server = Some(LocalServer::start(root.clone(), cx)?);
                state
                    .local_server
                    .as_ref()
                    .context(i18n::t!("6c9125fdf62146de"))?
            }
        };
        Ok(preview_url(server.port, &relative_path))
    })?;
    workspace.update(cx, |_, cx| cx.open_url(&url))?;
    Ok(())
}

async fn start_remote_preview(
    workspace: &gpui::WeakEntity<Workspace>,
    state: Entity<PreviewState>,
    project: Entity<Project>,
    root: PathBuf,
    relative_path: PathBuf,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let (terminal_panel, manager) = workspace
        .read_with(cx, |workspace, cx| {
            let terminal_panel = workspace.panel::<TerminalPanel>(cx)?;
            let manager = terminal_panel.read(cx).port_forward_manager();
            Some((terminal_panel, manager))
        })?
        .context(i18n::t!("0b0bfc39e5218a6c"))?;

    let existing_port = state.read_with(cx, |state, _| {
        state.remote.as_ref().map(|remote| remote.port)
    });
    let remote_port = match existing_port {
        Some(port) => port,
        None => manager.update(cx, |manager, _| manager.available_remote_port())?,
    };

    let contexts = workspace
        .update_in(cx, |workspace, window, cx| {
            tasks_ui::task_contexts(workspace, window, cx)
        })?
        .await;
    let context = contexts
        .active_item_context
        .as_ref()
        .map(|(_, _, context)| context)
        .context(i18n::t!("c93d599fd352711e"))?;
    let resolved = remote_server_task(&root, remote_port, context)?;
    let task_id = resolved.id.clone();

    let terminal = terminal_panel
        .update_in(cx, |panel, window, cx| {
            panel.port_forward_manager().update(cx, |manager, cx| {
                manager.add_preview(remote_port, project.clone(), cx);
            });
            panel.spawn_task(&resolved.resolved, window, cx)
        })?
        .await?;
    let terminal_view =
        terminal_panel.read_with(cx, |panel, cx| panel.terminal_view_for(&terminal, cx));

    state.update(cx, |state, cx| {
        state.track_remote(
            RemotePreview {
                port: remote_port,
                task_id,
                terminal_view: terminal_view.as_ref().map(|view| view.downgrade()),
                forward_manager: manager.clone(),
            },
            terminal_view,
            cx,
        );
    });

    let local_port = wait_for_forward(&manager, remote_port, cx).await;
    if !state.read_with(cx, |state, _| state.remote.is_some()) {
        return Ok(());
    }
    let url = preview_url(local_port?, &relative_path);
    workspace.update(cx, |_, cx| cx.open_url(&url))?;
    Ok(())
}

async fn wait_for_forward(
    manager: &Entity<terminal_view::port_forwarding::PortForwardManager>,
    remote_port: u16,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<u16> {
    for _ in 0..50 {
        let snapshot = manager.read_with(cx, |manager, _| {
            manager.snapshots().into_iter().find(|entry| {
                entry.direction == ForwardDirection::RemoteToLocal
                    && entry.target.port == remote_port
                    && entry.source == ForwardSource::Preview
            })
        });
        if let Some(snapshot) = snapshot {
            match snapshot.status {
                ForwardStatus::RunningUnconfirmed => {
                    return snapshot.local_port.context(i18n::t!("b4315e90e22b46eb"));
                }
                ForwardStatus::Failed(error) => bail!(i18n::t!("45327346a6ea2cd0", error = error)),
                ForwardStatus::Starting | ForwardStatus::Retrying(_) => {}
            }
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    bail!(i18n::t!("8fe57c401bf137e4"))
}

fn remote_server_task(root: &Path, port: u16, context: &TaskContext) -> Result<task::ResolvedTask> {
    let command = remote_server_command(root, port)?;
    let template = TaskTemplate {
        label: i18n::t!("ef161108fa4707e0").into(),
        command,
        args: Vec::new(),
        shell: Shell::Program("sh".into()),
        reveal: RevealStrategy::NoFocus,
        save: SaveStrategy::None,
        allow_concurrent_runs: false,
        use_new_terminal: false,
        show_summary: true,
        show_command: false,
        ..TaskTemplate::default()
    };
    template
        .resolve_task("web-preview", context)
        .context(i18n::t!("d553a8098239a68f"))
}

fn remote_server_command(root: &Path, port: u16) -> Result<String> {
    let root = root.to_str().context(i18n::t!("43711fc3a79e65c2"))?;
    let script_hex = hex_encode(include_str!("remote_preview_server.py").as_bytes());
    let root_hex = hex_encode(root.as_bytes());
    Ok(format!(
        "python3 -u -c 'x=__import__(\"sys\").argv;s=x[1];x[1:3]=[bytes.fromhex(x[2]).decode()];exec(compile(bytes.fromhex(s),\"<zed-web-preview>\",\"exec\"))' {script_hex} {root_hex} {port}"
    ))
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn stop_preview(
    workspace: &mut Workspace,
    state: Entity<PreviewState>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let (stopped_local, remote) = state.update(cx, |state, cx| {
        let stopped_local = state.local_server.take().is_some();
        let remote = state.remote.take();
        if remote.is_some() {
            cx.notify();
        }
        (stopped_local, remote)
    });
    if !stopped_local && remote.is_none() {
        return;
    }
    if let Some(remote) = remote {
        remote.forward_manager.update(cx, |manager, cx| {
            manager.stop_preview(remote.port, cx);
        });
        if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
            panel.update(cx, |panel, cx| panel.stop_task(&remote.task_id, cx));
        }
        if let Some(terminal_view) = remote
            .terminal_view
            .as_ref()
            .and_then(|view| view.upgrade())
            && let Some(pane) = workspace.pane_for(&terminal_view)
        {
            pane.update(cx, |pane, cx| {
                pane.remove_item(terminal_view.entity_id(), true, false, window, cx);
            });
        }
    }
    let message = if project.read(cx).is_via_remote_server() {
        i18n::t!("e7cbc50ff401ac92")
    } else {
        i18n::t!("64abf2619c75f34e")
    };
    workspace.show_toast(
        Toast::new(NotificationId::unique::<PreviewState>(), message),
        cx,
    );
}

impl LocalServer {
    fn start(root: PathBuf, _cx: &mut App) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .context(i18n::t!("a181134cee1db25c"))?;
        listener
            .set_nonblocking(true)
            .context(i18n::t!("35916b6c4facca69"))?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        thread::Builder::new()
            .name("web-preview".into())
            .stack_size(256 * 1024)
            .spawn({
                let stop = stop.clone();
                let root = root.clone();
                move || {
                    if let Err(error) = run_server(listener, root, stop) {
                        log::error!("网页预览服务器退出：{error:#}");
                    }
                }
            })
            .context(i18n::t!("4ae8b331fea53306"))?;
        Ok(Self { root, port, stop })
    }
}

fn run_server(listener: TcpListener, root: PathBuf, stop: Arc<AtomicBool>) -> Result<()> {
    // Each connection gets its own thread: a browser keeps speculative and
    // long-lived event-stream connections open, and serializing them would
    // stall every other request behind an idle socket.
    let live_connections = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if live_connections.load(Ordering::Acquire) >= MAX_CONCURRENT_CONNECTIONS {
                    if let Err(error) = write_response(
                        &mut stream,
                        503,
                        "text/plain; charset=utf-8",
                        b"Too Many Connections",
                        false,
                    ) {
                        log::debug!("网页预览拒绝连接失败：{error:#}");
                    }
                    continue;
                }
                live_connections.fetch_add(1, Ordering::AcqRel);
                let connection_count = live_connections.clone();
                let root = root.clone();
                let stop = stop.clone();
                let spawned = thread::Builder::new()
                    .name("web-preview-conn".into())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        if let Err(error) = serve_connection(stream, &root, &stop) {
                            log::debug!("网页预览请求失败：{error:#}");
                        }
                        connection_count.fetch_sub(1, Ordering::AcqRel);
                    });
                if let Err(error) = spawned {
                    live_connections.fetch_sub(1, Ordering::AcqRel);
                    return Err(error.into());
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn serve_connection(mut stream: TcpStream, root: &Path, stop: &AtomicBool) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = [0_u8; MAX_REQUEST_BYTES];
    let count = stream.read(&mut request)?;
    let request = std::str::from_utf8(&request[..count]).context(i18n::t!("12d8227fc6017981"))?;
    let first_line = request
        .lines()
        .next()
        .context(i18n::t!("7cf3f1b777b030df"))?;
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" && method != "HEAD" {
        return write_response(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            b"Method Not Allowed",
            method == "HEAD",
        );
    }
    let path = target.split('?').next().unwrap_or(target);
    if path == EVENTS_PATH {
        if method == "HEAD" {
            return write_response(&mut stream, 200, "text/event-stream", b"", true);
        }
        return serve_event_stream(&mut stream, root, target, stop);
    }
    if path == RELOAD_PATH {
        let revision = watched_revision(&watched_paths(root, target)?);
        return write_response(
            &mut stream,
            200,
            "text/plain; charset=utf-8",
            revision.as_bytes(),
            method == "HEAD",
        );
    }
    let relative = safe_relative_path(path)?;
    let requested = root.join(relative);
    let canonical = match std::fs::canonicalize(&requested) {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return write_response(
                &mut stream,
                404,
                "text/plain; charset=utf-8",
                b"Not Found",
                method == "HEAD",
            );
        }
        Err(error) => return Err(error.into()),
    };
    let canonical_root = std::fs::canonicalize(root)?;
    if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
        return write_response(
            &mut stream,
            403,
            "text/plain; charset=utf-8",
            b"Forbidden",
            method == "HEAD",
        );
    }
    let mut body = std::fs::read(&canonical)?;
    let content_type = content_type(&canonical);
    if content_type.starts_with("text/html") {
        inject_reload_script(&mut body);
    }
    write_response(&mut stream, 200, content_type, &body, method == "HEAD")
}

// Browsers throttle timers in background tabs, so the page cannot rely on its
// own polling to notice saves; the server watches the declared resources and
// pushes a reload over this stream instead.
fn serve_event_stream(
    stream: &mut TcpStream,
    root: &Path,
    target: &str,
    stop: &AtomicBool,
) -> Result<()> {
    let watched = watched_paths(root, target)?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Accel-Buffering: no\r\n\r\nretry: 1000\n\n"
    )?;
    stream.flush()?;
    let mut revision = watched_revision(&watched);
    while !stop.load(Ordering::Acquire) {
        thread::sleep(RELOAD_POLL_INTERVAL);
        let current = watched_revision(&watched);
        if current != revision {
            revision = current;
            stream.write_all(b"data: reload\n\n")?;
            stream.flush()?;
        }
    }
    Ok(())
}

fn watched_paths(root: &Path, target: &str) -> Result<Vec<PathBuf>> {
    let canonical_root = std::fs::canonicalize(root)?;
    let mut watched = Vec::new();
    let Some(query) = target.split_once('?').map(|(_, query)| query) else {
        return Ok(watched);
    };
    for encoded_path in query
        .split('&')
        .filter_map(|part| part.strip_prefix("p="))
        .take(64)
    {
        let encoded_path = encoded_path.replace('+', " ");
        let decoded = percent_decode_str(&encoded_path)
            .decode_utf8()
            .context(i18n::t!("0c4fd469089800ae"))?;
        let relative = safe_relative_path(decoded.as_ref())?;
        let Ok(canonical) = std::fs::canonicalize(root.join(relative)) else {
            continue;
        };
        if canonical.starts_with(&canonical_root) {
            watched.push(canonical);
        }
    }
    Ok(watched)
}

fn watched_revision(paths: &[PathBuf]) -> String {
    let mut latest = 0_u128;
    for path in paths {
        if let Ok(modified) = path.metadata().and_then(|metadata| metadata.modified())
            && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            latest = latest.max(duration.as_nanos());
        }
    }
    latest.to_string()
}

fn safe_relative_path(path: &str) -> Result<PathBuf> {
    let decoded = percent_decode_str(path.trim_start_matches('/'))
        .decode_utf8()
        .context(i18n::t!("e6aee5e1390880b5"))?;
    let mut result = PathBuf::new();
    for component in Path::new(decoded.as_ref()).components() {
        match component {
            Component::Normal(value) => result.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(i18n::t!("e1f77b5902c23224"))
            }
        }
    }
    if result.as_os_str().is_empty() {
        result.push("index.html");
    }
    Ok(result)
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()?;
    Ok(())
}

fn inject_reload_script(body: &mut Vec<u8>) {
    let lower = String::from_utf8_lossy(body).to_ascii_lowercase();
    let insert = lower.rfind("</body>").unwrap_or(body.len());
    body.splice(insert..insert, RELOAD_SCRIPT.as_bytes().iter().copied());
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn preview_url(port: u16, relative_path: &Path) -> String {
    let path = relative_path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(
                percent_encoding::utf8_percent_encode(
                    &value.to_string_lossy(),
                    percent_encoding::NON_ALPHANUMERIC,
                )
                .to_string(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("http://127.0.0.1:{port}/{path}")
}

pub fn is_html_editor(editor: &Entity<Editor>, cx: &App) -> bool {
    editor
        .read(cx)
        .active_buffer(cx)
        .and_then(|buffer| buffer.read(cx).project_path(cx))
        .is_some_and(|path| is_html_path(path.path.as_ref().as_std_path()))
}

fn is_html_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use task::TaskId;
    use terminal::{
        TerminalBuilder,
        terminal_settings::{AlternateScroll, CursorShape},
    };
    use util::paths::PathStyle;
    use workspace::{AppState, MultiWorkspace, dock::Panel as _};

    async fn init_preview_test(cx: &mut TestAppContext) -> gpui::WindowHandle<MultiWorkspace> {
        let params = cx.update(AppState::test);
        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));
        let project = Project::test(params.fs.clone(), [], cx).await;
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    }

    fn display_only_terminal_view(
        window_handle: gpui::WindowHandle<MultiWorkspace>,
        cx: &mut TestAppContext,
    ) -> Entity<TerminalView> {
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let project = workspace.read(cx).project().clone();
                let terminal = cx.new(|cx| {
                    TerminalBuilder::new_display_only(
                        CursorShape::default(),
                        AlternateScroll::On,
                        None,
                        0,
                        cx.background_executor(),
                        PathStyle::local(),
                    )
                    .subscribe(cx)
                });
                cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                })
            })
            .expect("window with preview terminal view")
    }

    fn remote_preview(view: &Entity<TerminalView>, cx: &mut TestAppContext) -> RemotePreview {
        RemotePreview {
            port: 49152,
            task_id: TaskId("web-preview".into()),
            terminal_view: Some(view.downgrade()),
            forward_manager: cx
                .update(|cx| cx.new(|_| terminal_view::port_forwarding::PortForwardManager::new())),
        }
    }

    #[gpui::test]
    async fn closing_preview_terminal_clears_preview_state(cx: &mut TestAppContext) {
        let window_handle = init_preview_test(cx).await;
        let view = display_only_terminal_view(window_handle, cx);
        let state = cx.update(|cx| cx.new(|_| PreviewState::default()));

        let remote = remote_preview(&view, cx);
        state.update(cx, |state, cx| {
            state.track_remote(remote, Some(view.clone()), cx)
        });
        assert!(state.read_with(cx, |state, _| state.is_active()));

        drop(view);
        // Flush the pending entity release so the release observer runs.
        cx.update(|_| {});

        assert!(
            !state.read_with(cx, |state, _| state.is_active()),
            "closing the preview terminal must turn the eye toggle off"
        );
    }

    #[gpui::test]
    async fn stopping_preview_closes_preview_terminal(cx: &mut TestAppContext) {
        let window_handle = init_preview_test(cx).await;
        let view = display_only_terminal_view(window_handle, cx);
        let (_terminal_panel, pane) = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let terminal_panel = multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                });
                let pane = terminal_panel
                    .read_with(cx, |panel, _| panel.pane())
                    .expect("terminal panel pane");
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(view.clone()), true, false, None, window, cx)
                });
                (terminal_panel, pane)
            })
            .expect("terminal panel with preview terminal");
        // Flush the queued `AddItem` event so the workspace registers the tab's pane.
        cx.update(|_| {});

        let state = cx.update(|cx| cx.new(|_| PreviewState::default()));
        let remote = remote_preview(&view, cx);
        state.update(cx, |state, cx| {
            state.track_remote(remote, Some(view.clone()), cx)
        });

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    stop_preview(workspace, state.clone(), window, cx)
                })
            })
            .expect("workspace");

        assert!(
            !state.read_with(cx, |state, _| state.is_active()),
            "stopping the preview must turn the eye toggle off"
        );
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.items_len()),
            0,
            "stopping the preview must close its terminal tab"
        );
    }

    fn test_project(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("zed-web-preview-{name}-"))
            .tempdir()
            .expect("test project dir")
    }

    fn spawn_test_server(root: &Path) -> (std::net::SocketAddr, Arc<AtomicBool>) {
        let listener =
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).expect("test listener");
        listener.set_nonblocking(true).expect("test listener mode");
        let addr = listener.local_addr().expect("test listener addr");
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let root = root.to_path_buf();
        thread::spawn(move || {
            if let Err(error) = run_server(listener, root, server_stop) {
                panic!("test server failed: {error:#}");
            }
        });
        (addr, stop)
    }

    fn read_until(stream: &mut TcpStream, buffer: &mut Vec<u8>, needle: &[u8]) {
        let mut chunk = [0_u8; 1024];
        while !buffer.windows(needle.len()).any(|window| window == needle) {
            let count = stream.read(&mut chunk).expect("read from stream");
            assert!(count > 0, "stream closed before {needle:?}");
            buffer.extend_from_slice(&chunk[..count]);
        }
    }

    #[test]
    fn rejects_paths_outside_root() {
        assert!(safe_relative_path("/../secret").is_err());
        assert!(safe_relative_path("/%2e%2e/secret").is_err());
    }

    #[test]
    fn remote_server_command_encodes_untrusted_path_and_script() {
        let root = Path::new("/tmp/project'; touch /tmp/injected; echo '");
        let command = remote_server_command(root, 49152).expect("remote command");

        assert!(!command.contains(root.to_string_lossy().as_ref()));
        assert!(!command.contains("touch /tmp/injected"));
        assert!(!command.contains("import http.server"));

        let arguments = command.split_ascii_whitespace().collect::<Vec<_>>();
        assert_eq!(arguments[0..3], ["python3", "-u", "-c"]);
        assert!(arguments[3].starts_with("'x=__import__"));
        assert!(arguments[3].ends_with("))'"));
        assert!(arguments[4].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            String::from_utf8(
                arguments[5]
                    .as_bytes()
                    .chunks_exact(2)
                    .map(|pair| {
                        u8::from_str_radix(std::str::from_utf8(pair).expect("hex pair"), 16)
                            .expect("hex byte")
                    })
                    .collect()
            )
            .expect("UTF-8 path"),
            root.to_string_lossy()
        );
        assert_eq!(arguments[6], "49152");
    }

    #[test]
    fn builds_encoded_preview_url() {
        assert_eq!(
            preview_url(49152, Path::new("页面/index file.html")),
            "http://127.0.0.1:49152/%E9%A1%B5%E9%9D%A2/index%20file%2Ehtml"
        );
    }

    #[test]
    fn injects_reload_before_body_end() {
        let mut body = b"<html><body>Hello</body></html>".to_vec();
        inject_reload_script(&mut body);
        let result = String::from_utf8(body).expect("valid HTML");
        assert!(result.contains(RELOAD_SCRIPT));
        assert!(
            result.find(RELOAD_SCRIPT).expect("reload script")
                < result.find("</body>").expect("body end")
        );
    }

    #[test]
    fn serves_requests_while_another_connection_is_idle() {
        let root = test_project("idle-connection");
        std::fs::write(
            root.path().join("index.html"),
            b"<html><body>hi</body></html>",
        )
        .expect("write index.html");
        let (addr, stop) = spawn_test_server(root.path());

        // Browsers open speculative connections that stay idle until a request
        // is written into them; they must not stall unrelated requests.
        let _idle = TcpStream::connect(addr).expect("idle connection");
        let mut client = TcpStream::connect(addr).expect("client connection");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("client read timeout");
        client
            .write_all(b"GET /index.html HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("write request");
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read response");
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains(RELOAD_SCRIPT), "{response}");

        stop.store(true, Ordering::Release);
    }

    #[test]
    fn pushes_reload_when_watched_file_changes() {
        let root = test_project("event-stream");
        std::fs::write(
            root.path().join("index.html"),
            b"<html><body>one</body></html>",
        )
        .expect("write index.html");
        let (addr, stop) = spawn_test_server(root.path());

        let mut stream = TcpStream::connect(addr).expect("event stream connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("event stream read timeout");
        stream
            .write_all(
                format!("GET {EVENTS_PATH}?p=%2Findex.html HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .expect("write event stream request");
        let mut response = Vec::new();
        read_until(&mut stream, &mut response, b"retry: 1000\n\n");
        assert!(
            String::from_utf8_lossy(&response).contains("text/event-stream"),
            "{response:?}"
        );

        std::fs::write(
            root.path().join("index.html"),
            b"<html><body>two</body></html>",
        )
        .expect("rewrite index.html");
        read_until(&mut stream, &mut response, b"data: reload\n\n");

        stop.store(true, Ordering::Release);
    }
}
