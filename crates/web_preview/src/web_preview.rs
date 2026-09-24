use std::{
    io::{ErrorKind, Read as _, Write as _},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use editor::Editor;
use gpui::{App, AppContext as _, Context, Entity, Window};
use percent_encoding::percent_decode_str;
use project::{Project, ProjectItem as _, TaskSourceKind, trusted_worktrees::TrustedWorktrees};
use task::{RevealStrategy, SaveStrategy, Shell, TaskContext, TaskTemplate};
use terminal_view::{
    port_forwarding::{ForwardDirection, ForwardSource, ForwardStatus},
    terminal_panel::TerminalPanel,
};
use util::ResultExt as _;
use workspace::Workspace;
use zed_actions::preview::web::{OpenPreview, StopPreview};

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const RELOAD_PATH: &str = "/.zed-live-preview/reload";
const RELOAD_SCRIPT: &str = r#"<script>(()=>{let v="";const poll=()=>{const q=new URLSearchParams();q.append('p',location.pathname);performance.getEntriesByType('resource').slice(0,63).forEach(e=>{const u=new URL(e.name,location.href);if(u.origin===location.origin)q.append('p',u.pathname)});fetch('/.zed-live-preview/reload?'+q,{cache:'no-store'}).then(r=>r.text()).then(n=>{if(v&&v!==n)location.reload();v=n}).catch(()=>{}).finally(()=>setTimeout(poll,700))};poll()})()</script>"#;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(_window) = window else {
            return;
        };
        let state = cx.new(|_| PreviewState::default());
        workspace.register_action({
            let state = state.clone();
            move |workspace, _: &OpenPreview, window, cx| {
                open_preview(workspace, state.clone(), window, cx)
            }
        });
        workspace.register_action(move |workspace, _: &StopPreview, _, cx| {
            stop_preview(workspace, state.clone(), cx)
        });
    })
    .detach();
}

#[derive(Default)]
struct PreviewState {
    local_server: Option<LocalServer>,
    remote_port: Option<u16>,
    remote_task_id: Option<task::TaskId>,
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
        workspace.show_error("请先打开要预览的 HTML 文件。", cx);
        return;
    };
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
        workspace.show_error("网页预览不支持多文件视图。", cx);
        return;
    };
    let Some(project_path) = buffer.read(cx).project_path(cx) else {
        workspace.show_error("请先保存 HTML 文件，再打开网页预览。", cx);
        return;
    };
    if !is_html_path(project_path.path.as_ref().as_std_path()) {
        workspace.show_error("当前文件不是 HTML 页面。", cx);
        return;
    }
    let project = workspace.project().clone();
    if project.read(cx).is_disconnected(cx) {
        workspace.show_error("远程连接已断开，无法启动网页预览。", cx);
        return;
    }
    if project.read(cx).is_via_collab()
        || TrustedWorktrees::has_restricted_worktrees(&project.read(cx).worktree_store(), cx)
    {
        workspace.show_error("请在受信任的本地或 SSH 项目中启动网页预览。", cx);
        return;
    }
    let Some(worktree) = project
        .read(cx)
        .worktree_for_id(project_path.worktree_id, cx)
    else {
        workspace.show_error("无法找到当前文件所属的项目目录。", cx);
        return;
    };
    let root = worktree.read(cx).abs_path().to_path_buf();
    let relative_path = project_path.path.as_ref().as_std_path().to_path_buf();
    let save = project.update(cx, |project, cx| project.save_buffer(buffer.clone(), cx));
    let remote = project.read(cx).is_via_remote_server();
    let remote_options = project.read(cx).remote_connection_options(cx);

    cx.spawn_in(window, async move |workspace, cx| {
        let result: Result<()> = async {
            save.await.context("预览前保存文件失败")?;
            if buffer.read_with(cx, |buffer, _| buffer.is_dirty()) {
                bail!("文件在保存期间又发生了变化，请重试");
            }
            if remote {
                if !matches!(
                    remote_options,
                    Some(remote::RemoteConnectionOptions::Ssh(_))
                ) {
                    bail!("远程实时预览目前支持 SSH；WSL/Docker 请先通过任务启动服务器");
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
                    workspace.show_error(format!("无法打开网页预览：{error:#}"), cx);
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
                    .context("本地预览服务器未启动")?
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
        .context("终端面板尚未就绪，请稍后重试")?;

    let existing_port = state.read_with(cx, |state, _| state.remote_port);
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
        .context("无法获取远程文件的任务环境")?;
    let resolved = remote_server_task(&root, remote_port, context)?;
    let task_id = resolved.id.clone();

    workspace.update_in(cx, |workspace, window, cx| {
        terminal_panel.update(cx, |panel, cx| {
            panel.port_forward_manager().update(cx, |manager, cx| {
                manager.add_preview(remote_port, project.clone(), cx);
            });
        });
        workspace.schedule_resolved_task(TaskSourceKind::UserInput, resolved, true, window, cx);
    })?;
    state.update(cx, |state, _| {
        state.remote_port = Some(remote_port);
        state.remote_task_id = Some(task_id);
    });

    let local_port = wait_for_forward(&manager, remote_port, cx).await?;
    workspace.update(cx, |_, cx| {
        cx.open_url(&preview_url(local_port, &relative_path));
    })?;
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
                    && entry.remote_port == remote_port
                    && entry.source == ForwardSource::Preview
            })
        });
        if let Some(snapshot) = snapshot {
            match snapshot.status {
                ForwardStatus::RunningUnconfirmed => {
                    return snapshot.local_port.context("SSH 转发没有本地端口");
                }
                ForwardStatus::Failed(error) => bail!("SSH 端口转发失败：{error}"),
                ForwardStatus::Starting => {}
            }
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    bail!("等待 SSH 端口转发超时")
}

fn remote_server_task(root: &Path, port: u16, context: &TaskContext) -> Result<task::ResolvedTask> {
    let command = remote_server_command(root, port)?;
    let template = TaskTemplate {
        label: "网页实时预览".into(),
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
        .context("无法生成远程网页预览任务")
}

fn remote_server_command(root: &Path, port: u16) -> Result<String> {
    let root = root.to_str().context("远程项目路径不是有效的 Unicode")?;
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
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let (remote_port, remote_task_id) = state.update(cx, |state, _| {
        state.local_server = None;
        (state.remote_port.take(), state.remote_task_id.take())
    });
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        if let Some(remote_port) = remote_port {
            panel
                .read(cx)
                .port_forward_manager()
                .update(cx, |manager, cx| manager.stop_preview(remote_port, cx));
        }
        if let Some(remote_task_id) = remote_task_id {
            panel.update(cx, |panel, cx| panel.stop_task(&remote_task_id, cx));
        }
    }
    if project.read(cx).is_via_remote_server() {
        workspace.show_error("已停止远端网页预览和 SSH 端口转发。", cx);
    }
}

impl LocalServer {
    fn start(root: PathBuf, _cx: &mut App) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .context("无法监听本地预览端口")?;
        listener
            .set_nonblocking(true)
            .context("无法配置本地预览端口")?;
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
            .context("无法启动本地预览线程")?;
        Ok(Self { root, port, stop })
    }
}

fn run_server(listener: TcpListener, root: PathBuf, stop: Arc<AtomicBool>) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = serve_connection(stream, &root) {
                    log::debug!("网页预览请求失败：{error:#}");
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

fn serve_connection(mut stream: TcpStream, root: &Path) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = [0_u8; MAX_REQUEST_BYTES];
    let count = stream.read(&mut request)?;
    let request = std::str::from_utf8(&request[..count]).context("请求不是 UTF-8")?;
    let first_line = request.lines().next().context("请求为空")?;
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
    if path == RELOAD_PATH {
        let revision = resource_revision(root, target)?;
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

fn resource_revision(root: &Path, target: &str) -> Result<String> {
    let canonical_root = std::fs::canonicalize(root)?;
    let mut latest = 0_u128;
    if let Some(query) = target.split_once('?').map(|(_, query)| query) {
        for encoded_path in query
            .split('&')
            .filter_map(|part| part.strip_prefix("p="))
            .take(64)
        {
            let encoded_path = encoded_path.replace('+', " ");
            let decoded = percent_decode_str(&encoded_path)
                .decode_utf8()
                .context("资源路径不是 UTF-8")?;
            let relative = safe_relative_path(decoded.as_ref())?;
            let canonical = match std::fs::canonicalize(root.join(relative)) {
                Ok(path) => path,
                Err(_) => continue,
            };
            if !canonical.starts_with(&canonical_root) {
                continue;
            }
            if let Ok(modified) = canonical
                .metadata()
                .and_then(|metadata| metadata.modified())
                && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                latest = latest.max(duration.as_nanos());
            }
        }
    }
    Ok(latest.to_string())
}

fn safe_relative_path(path: &str) -> Result<PathBuf> {
    let decoded = percent_decode_str(path.trim_start_matches('/'))
        .decode_utf8()
        .context("URL 路径不是 UTF-8")?;
    let mut result = PathBuf::new();
    for component in Path::new(decoded.as_ref()).components() {
        match component {
            Component::Normal(value) => result.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("拒绝项目目录外的路径")
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
}
