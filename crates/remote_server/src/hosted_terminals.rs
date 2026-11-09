use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use rpc::proto;
use std::{path::PathBuf, sync::Mutex};
use uuid::Uuid;

use crate::hosted_terminal::HostedTerminalClient;

/// Identity the client sees for a daemon-backed terminal. The daemon session id
/// never leaves the server, so a client cannot address another client's session.
pub const HOSTED_INSTANCE_ID: &str = "hosted-terminal";

struct TerminalLink {
    session_id: String,
    attachment_id: String,
    control_generation: u64,
    hosted: bool,
    controls_input: bool,
}

#[derive(Default)]
struct LinkState {
    terminals: HashMap<String, TerminalLink>,
}

/// Translates the existing persistent-terminal RPCs into daemon operations and
/// serves the hosting management RPCs. One instance serves one project server,
/// i.e. one client connection scope.
pub struct HostedTerminals {
    service_dir: PathBuf,
    client: async_lock::Mutex<Option<std::sync::Arc<HostedTerminalClient>>>,
    links: Mutex<LinkState>,
}

impl HostedTerminals {
    pub fn new(service_dir: PathBuf) -> Self {
        Self {
            service_dir,
            client: async_lock::Mutex::new(None),
            links: Mutex::new(LinkState::default()),
        }
    }

    async fn client(&self) -> Result<std::sync::Arc<HostedTerminalClient>> {
        let mut client = self.client.lock().await;
        if let Some(connected) = client.as_ref() {
            anyhow::ensure!(
                connected.is_connected(),
                "hosted terminal service connection was lost; reopen the remote project"
            );
            return Ok(connected.clone());
        }
        let connected = HostedTerminalClient::connect_or_spawn(&self.service_dir).await?;
        *client = Some(connected.clone());
        Ok(connected)
    }

    fn links(&self) -> Result<std::sync::MutexGuard<'_, LinkState>> {
        self.links
            .lock()
            .map_err(|_| anyhow!("hosted terminal links are unavailable"))
    }

    fn attachment_for_session(&self, session_id: &str) -> Result<(String, u64)> {
        self.links()?
            .terminals
            .values()
            .find(|link| link.session_id == session_id)
            .map(|link| (link.attachment_id.clone(), link.control_generation))
            .context("this connection is not attached to the hosted terminal")
    }

    fn link(&self, terminal_id: &str) -> Result<(String, String, u64)> {
        self.links()?
            .terminals
            .get(terminal_id)
            .map(|link| {
                (
                    link.session_id.clone(),
                    link.attachment_id.clone(),
                    link.control_generation,
                )
            })
            .context("hosted terminal is no longer attached")
    }

    pub async fn canonical_root(&self, path: Option<&str>) -> Result<String> {
        let Some(path) = path.filter(|path| !path.is_empty()) else {
            return Ok(String::new());
        };
        let path = path.to_owned();
        let canonical = smol::unblock(move || std::fs::canonicalize(path)).await?;
        Ok(canonical.display().to_string())
    }

    /// The root that scopes a new session. The project root wins so a shell
    /// opened below it stays visible to the project that owns it; a project
    /// without a worktree, or a root that no longer resolves, falls back to the
    /// terminal's working directory.
    async fn session_root(
        &self,
        project_root: Option<&str>,
        working_directory: Option<&str>,
    ) -> String {
        if let Some(project_root) = project_root.filter(|root| !root.is_empty())
            && let Ok(root) = self.canonical_root(Some(project_root)).await
        {
            return root;
        }
        self.canonical_root(working_directory)
            .await
            .unwrap_or_default()
    }

    /// Creates a terminal inside the independent service. `hosted` records that
    /// this client understands promotion; it does not host the session yet.
    ///
    /// `project_root` is the first worktree of the project that asked for the
    /// terminal, and it scopes the session: the client lists and re-attaches by
    /// that same root, so a shell opened in a subdirectory (or by a task) would
    /// otherwise be invisible to its own project. It comes from the server's own
    /// worktree store, not from the client.
    pub async fn create(
        &self,
        request: proto::CreatePersistentTerminal,
        project_root: Option<String>,
    ) -> Result<proto::CreatePersistentTerminalResponse> {
        let canonical_root = self
            .session_root(
                project_root.as_deref(),
                request.working_directory.as_deref(),
            )
            .await;
        let attachment_id = Uuid::new_v4().to_string();
        let client = self.client().await?;
        let response = client
            .request(proto::hosted_terminal_daemon_request::Kind::Create(
                proto::CreateHostedTerminal {
                    creation_id: request.creation_id,
                    program: request.program,
                    args: request.args,
                    env: request.env.into_iter().collect(),
                    working_directory: request.working_directory,
                    rows: request.rows,
                    columns: request.columns,
                    pixel_width: request.pixel_width,
                    pixel_height: request.pixel_height,
                    canonical_root,
                    attachment_id: attachment_id.clone(),
                },
            ))
            .await?;
        let Some(proto::hosted_terminal_daemon_response::Kind::Created(created)) = response.kind
        else {
            return Err(anyhow!("hosted terminal service did not create a session"));
        };
        let terminal_id = Uuid::new_v4().to_string();
        self.links()?.terminals.insert(
            terminal_id.clone(),
            TerminalLink {
                session_id: created.session_id,
                attachment_id,
                control_generation: 1,
                hosted: false,
                controls_input: true,
            },
        );
        Ok(proto::CreatePersistentTerminalResponse {
            server_instance_id: HOSTED_INSTANCE_ID.to_owned(),
            terminal_id,
        })
    }

    pub async fn input(&self, request: proto::PersistentTerminalInput) -> Result<()> {
        self.require_instance(&request.server_instance_id)?;
        let (session_id, attachment_id, control_generation) = self.link(&request.terminal_id)?;
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Input(
                proto::HostedTerminalDaemonInput {
                    session_id,
                    attachment_id,
                    control_generation,
                    sequence: request.sequence,
                    data: request.data,
                },
            ))
            .await
            .map(|_| ())
    }

    pub async fn resize(&self, request: proto::ResizePersistentTerminal) -> Result<()> {
        self.require_instance(&request.server_instance_id)?;
        let (session_id, attachment_id, control_generation) = self.link(&request.terminal_id)?;
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Resize(
                proto::HostedTerminalDaemonResize {
                    session_id,
                    attachment_id,
                    control_generation,
                    rows: request.rows,
                    columns: request.columns,
                    pixel_width: request.pixel_width,
                    pixel_height: request.pixel_height,
                },
            ))
            .await
            .map(|_| ())
    }

    pub async fn credit(&self, request: proto::PersistentTerminalOutputCredit) -> Result<()> {
        self.require_instance(&request.server_instance_id)?;
        let Ok((session_id, attachment_id, _)) = self.link(&request.terminal_id) else {
            // A credit for an already detached terminal is harmless.
            return Ok(());
        };
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Credit(
                proto::HostedTerminalDaemonCredit {
                    session_id,
                    attachment_id,
                    offset: request.offset,
                },
            ))
            .await
            .map(|_| ())
    }

    pub async fn subscribe(
        &self,
        request: proto::SubscribePersistentTerminal,
    ) -> Result<async_channel::Receiver<proto::PersistentTerminalOutput>> {
        self.require_instance(&request.server_instance_id)?;
        let (session_id, attachment_id, _) = self.link(&request.terminal_id)?;
        let client = self.client().await?;
        client
            .subscribe(&session_id, &attachment_id, request.offset)
            .await
    }

    /// Hosted terminals outlive the view, so closing only detaches them. An
    /// unhosted terminal keeps the old behavior and is terminated.
    pub async fn close(&self, request: proto::ClosePersistentTerminal) -> Result<()> {
        self.require_instance(&request.server_instance_id)?;
        let Some(link) = self.links()?.terminals.remove(&request.terminal_id) else {
            return Ok(());
        };
        let client = self.client().await?;
        let kind = if link.hosted {
            proto::hosted_terminal_daemon_request::Kind::Detach(proto::DetachHostedTerminal {
                session_id: link.session_id,
                attachment_id: link.attachment_id,
            })
        } else {
            proto::hosted_terminal_daemon_request::Kind::Terminate(proto::TerminateHostedTerminal {
                session_id: link.session_id,
                attachment_id: link.attachment_id,
            })
        };
        match client.request(kind).await {
            Ok(_) => Ok(()),
            // The daemon releases an attachment when its connection ends, so a
            // close that arrives afterwards is already satisfied.
            Err(error)
                if error.to_string()
                    == crate::hosted_terminal::SessionError::UnknownAttachment.message() =>
            {
                log::debug!("hosted terminal was already detached: {error:#}");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn host(&self, request: proto::HostTerminal) -> Result<proto::HostTerminalResponse> {
        self.require_instance(&request.server_instance_id)?;
        let (session_id, attachment_id, _) = self.link(&request.terminal_id)?;
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Host(
                proto::HostTerminal {
                    server_instance_id: String::new(),
                    terminal_id: session_id,
                    name: request.name.clone(),
                    attachment_id,
                },
            ))
            .await?;
        let session_id = self.link(&request.terminal_id)?.0;
        if let Ok(mut links) = self.links()
            && let Some(link) = links.terminals.get_mut(&request.terminal_id)
        {
            link.hosted = true;
        }
        Ok(proto::HostTerminalResponse {
            session_id,
            name: request.name,
        })
    }

    pub async fn unhost(&self, request: proto::UnhostTerminal) -> Result<()> {
        let (session_id, attachment_id, _) = self.link(&request.terminal_id)?;
        let response = self
            .client()
            .await?
            .request(proto::hosted_terminal_daemon_request::Kind::Unhost(
                proto::UnhostTerminal {
                    terminal_id: session_id.clone(),
                    attachment_id: attachment_id.clone(),
                },
            ))
            .await?;
        let Some(proto::hosted_terminal_daemon_response::Kind::Control(control)) = response.kind
        else {
            anyhow::bail!("terminal service did not confirm cancellation");
        };
        let mut links = self.links()?;
        for link in links
            .terminals
            .values_mut()
            .filter(|link| link.session_id == session_id)
        {
            if link.attachment_id == attachment_id {
                link.hosted = false;
                link.controls_input = true;
                link.control_generation = control.control_generation;
            }
        }
        Ok(())
    }

    pub async fn list(
        &self,
        request: proto::ListHostedTerminals,
    ) -> Result<proto::ListHostedTerminalsResponse> {
        let canonical_root = self
            .canonical_root(Some(&request.canonical_root))
            .await
            .unwrap_or_else(|_| request.canonical_root.clone());
        let client = self.client().await?;
        let response = client
            .request(proto::hosted_terminal_daemon_request::Kind::List(
                proto::ListHostedTerminals { canonical_root },
            ))
            .await?;
        match response.kind {
            Some(proto::hosted_terminal_daemon_response::Kind::Sessions(sessions)) => Ok(sessions),
            _ => Err(anyhow!("hosted terminal service returned no session list")),
        }
    }

    /// Attaches the current connection to a hosted session and registers a new
    /// pseudo terminal id for it.
    pub async fn attach(
        &self,
        request: proto::AttachHostedTerminal,
    ) -> Result<proto::AttachHostedTerminalResponse> {
        let canonical_root = self
            .canonical_root(Some(&request.canonical_root))
            .await
            .unwrap_or_else(|_| request.canonical_root.clone());
        let client = self.client().await?;
        let response = client
            .request(proto::hosted_terminal_daemon_request::Kind::Attach(
                proto::AttachHostedTerminal {
                    canonical_root,
                    session_id: request.session_id.clone(),
                },
            ))
            .await?;
        let Some(proto::hosted_terminal_daemon_response::Kind::Attached(attached)) = response.kind
        else {
            return Err(anyhow!(
                "hosted terminal service did not attach the session"
            ));
        };
        let terminal_id = Uuid::new_v4().to_string();
        self.links()?.terminals.insert(
            terminal_id.clone(),
            TerminalLink {
                session_id: request.session_id.clone(),
                attachment_id: attached.attachment_id,
                control_generation: attached.control_generation,
                hosted: true,
                controls_input: attached.controls_input,
            },
        );
        Ok(proto::AttachHostedTerminalResponse {
            server_instance_id: HOSTED_INSTANCE_ID.to_owned(),
            terminal_id,
            controls_input: attached.controls_input,
            running: true,
            exit_code: None,
            session_id: request.session_id,
        })
    }

    pub async fn detach(&self, request: proto::DetachHostedTerminal) -> Result<()> {
        let (attachment_id, _) = self.attachment_for_session(&request.session_id)?;
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Detach(
                proto::DetachHostedTerminal {
                    session_id: request.session_id.clone(),
                    attachment_id,
                },
            ))
            .await?;
        // The pseudo terminal id only ever named this attachment, so it must not
        // stay addressable: a later close or input would hit an attachment the
        // daemon already released.
        self.links()?
            .terminals
            .retain(|_, link| link.session_id != request.session_id);
        Ok(())
    }

    pub async fn take_control(
        &self,
        request: proto::TakeHostedTerminalControl,
    ) -> Result<proto::TakeHostedTerminalControlResponse> {
        let (session_id, attachment_id, _) = self.link(&request.terminal_id)?;
        anyhow::ensure!(
            request.session_id.is_empty() || session_id == request.session_id,
            "terminal session identity mismatch"
        );
        let client = self.client().await?;
        let response = client
            .request(proto::hosted_terminal_daemon_request::Kind::TakeControl(
                proto::TakeHostedTerminalControl {
                    session_id,
                    attachment_id,
                    terminal_id: String::new(),
                },
            ))
            .await?;
        let Some(proto::hosted_terminal_daemon_response::Kind::Control(control)) = response.kind
        else {
            return Err(anyhow!("hosted terminal service did not grant control"));
        };
        if let Ok(mut links) = self.links()
            && let Some(link) = links.terminals.get_mut(&request.terminal_id)
        {
            link.control_generation = control.control_generation;
            link.controls_input = control.controls_input;
        }
        Ok(proto::TakeHostedTerminalControlResponse {
            controls_input: control.controls_input,
        })
    }

    pub async fn rename(&self, request: proto::RenameHostedTerminal) -> Result<()> {
        let (attachment_id, _) = self.attachment_for_session(&request.session_id)?;
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Rename(
                proto::RenameHostedTerminal {
                    session_id: request.session_id,
                    name: request.name,
                    attachment_id,
                },
            ))
            .await
            .map(|_| ())
    }

    pub async fn terminate(&self, request: proto::TerminateHostedTerminal) -> Result<()> {
        let (attachment_id, _) = self.attachment_for_session(&request.session_id)?;
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Terminate(
                proto::TerminateHostedTerminal {
                    session_id: request.session_id,
                    attachment_id,
                },
            ))
            .await
            .map(|_| ())
    }

    pub async fn delete(&self, request: proto::DeleteHostedTerminal) -> Result<()> {
        // Ending records are inert, so a client that is only browsing the list
        // can delete them without attaching first.
        let attachment_id = self
            .attachment_for_session(&request.session_id)
            .map(|(attachment_id, _)| attachment_id)
            .unwrap_or_default();
        let client = self.client().await?;
        client
            .request(proto::hosted_terminal_daemon_request::Kind::Delete(
                proto::DeleteHostedTerminal {
                    session_id: request.session_id,
                    attachment_id,
                },
            ))
            .await
            .map(|_| ())
    }

    fn require_instance(&self, server_instance_id: &str) -> Result<()> {
        anyhow::ensure!(
            server_instance_id == HOSTED_INSTANCE_ID,
            "terminal does not belong to the hosted terminal service"
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "process-level daemon tests wait on real PTY output and socket readiness"
)]
mod tests {
    use super::*;
    use crate::hosted_terminal::serve_with_shutdown;
    use std::path::Path;
    use std::time::Duration;

    struct TestDaemon {
        #[allow(dead_code, reason = "keeps the temporary state directory alive")]
        directory: tempfile::TempDir,
        shutdown: Option<async_channel::Sender<()>>,
    }

    impl Drop for TestDaemon {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                shutdown.close();
            }
        }
    }

    fn daemon() -> (TestDaemon, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().expect("temp dir");
        let service_dir = directory.path().join("hosted-terminals");
        let serve_dir = service_dir.clone();
        let (shutdown, stop) = async_channel::bounded(1);
        std::thread::spawn(move || {
            smol::block_on(serve_with_shutdown(serve_dir, stop)).expect("serve");
        });
        let root = directory.path().join("project");
        std::fs::create_dir_all(&root).expect("project root");
        (
            TestDaemon {
                directory,
                shutdown: Some(shutdown),
            },
            service_dir,
            root,
        )
    }

    fn create_request(root: &Path, creation_id: &str) -> proto::CreatePersistentTerminal {
        proto::CreatePersistentTerminal {
            creation_id: creation_id.into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "printf hosted-ready; sleep 30".into()],
            env: Default::default(),
            working_directory: Some(root.display().to_string()),
            rows: 24,
            columns: 80,
            pixel_width: 0,
            pixel_height: 0,
            hosted: true,
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_hosting_keeps_the_same_shell_alive_and_input_usable() {
        let (_daemon, service_dir, root) = daemon();
        let hosted = HostedTerminals::new(service_dir);
        smol::block_on(async {
            let mut request = create_request(&root, "unhost-live-shell");
            request.args = vec![
                "-c".into(),
                "printf 'before:%s\\n' $$; read value; printf 'after:%s:%s\\n' $$ \"$value\""
                    .into(),
            ];
            let created = hosted.create(request, None).await.expect("create");
            let promoted = hosted
                .host(proto::HostTerminal {
                    server_instance_id: created.server_instance_id.clone(),
                    terminal_id: created.terminal_id.clone(),
                    name: "live shell".into(),
                    attachment_id: String::new(),
                })
                .await
                .expect("host");
            let output = hosted
                .subscribe(proto::SubscribePersistentTerminal {
                    server_instance_id: created.server_instance_id.clone(),
                    terminal_id: created.terminal_id.clone(),
                    offset: 0,
                })
                .await
                .expect("subscribe");
            let before = output_text(&drain(&output).await);
            let pid = before
                .lines()
                .find_map(|line| line.strip_prefix("before:"))
                .expect("shell pid")
                .trim();
            hosted
                .unhost(proto::UnhostTerminal {
                    terminal_id: created.terminal_id.clone(),
                    attachment_id: String::new(),
                })
                .await
                .expect("cancel hosting");
            let root = hosted
                .canonical_root(Some(&root.display().to_string()))
                .await
                .expect("root");
            assert!(
                hosted
                    .list(proto::ListHostedTerminals {
                        canonical_root: root
                    })
                    .await
                    .expect("list")
                    .sessions
                    .iter()
                    .all(|session| session.session_id != promoted.session_id)
            );
            hosted
                .input(proto::PersistentTerminalInput {
                    server_instance_id: created.server_instance_id.clone(),
                    terminal_id: created.terminal_id.clone(),
                    sequence: 1,
                    data: b"still-alive\n".to_vec(),
                })
                .await
                .expect("input after cancellation");
            let after = output_text(&drain(&output).await);
            assert!(
                after.contains(&format!("after:{pid}:still-alive")),
                "{before:?} {after:?}"
            );
            hosted
                .close(proto::ClosePersistentTerminal {
                    server_instance_id: created.server_instance_id,
                    terminal_id: created.terminal_id,
                })
                .await
                .expect("close");
        });
    }

    fn output_text(frames: &[proto::PersistentTerminalOutput]) -> String {
        let mut bytes = Vec::new();
        for frame in frames {
            bytes.extend(frame.data.iter().copied());
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn drain(
        receiver: &async_channel::Receiver<proto::PersistentTerminalOutput>,
    ) -> Vec<proto::PersistentTerminalOutput> {
        let mut frames = Vec::new();
        loop {
            let frame = smol::future::or(async { receiver.recv().await.map_err(|_| ()) }, async {
                smol::Timer::after(Duration::from_millis(250)).await;
                Err(())
            })
            .await;
            match frame {
                Ok(frame) => frames.push(frame),
                Err(()) => break,
            }
        }
        frames
    }

    #[test]
    fn created_terminal_runs_in_the_service_and_can_be_hosted_and_attached_elsewhere() {
        let (_daemon, service_dir, root) = daemon();
        let first = HostedTerminals::new(service_dir.clone());
        let created =
            smol::block_on(first.create(create_request(&root, "one"), None)).expect("create");
        assert_eq!(created.server_instance_id, HOSTED_INSTANCE_ID);
        let subscribed = smol::block_on(first.subscribe(proto::SubscribePersistentTerminal {
            server_instance_id: created.server_instance_id.clone(),
            terminal_id: created.terminal_id.clone(),
            offset: 0,
        }))
        .expect("subscribe");
        let frames = smol::block_on(drain(&subscribed));
        assert!(output_text(&frames).contains("hosted-ready"));
        let hosted = smol::block_on(first.host(proto::HostTerminal {
            server_instance_id: created.server_instance_id.clone(),
            terminal_id: created.terminal_id.clone(),
            name: "agent".into(),
            attachment_id: String::new(),
        }))
        .expect("host");
        assert_ne!(hosted.session_id, created.terminal_id);
        let canonical_root = root.display().to_string();
        let listed = smol::block_on(first.list(proto::ListHostedTerminals {
            canonical_root: canonical_root.clone(),
        }))
        .expect("list");
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].name, "agent");

        // A second project connection attaches the same running session.
        let second = HostedTerminals::new(service_dir);
        let attached = smol::block_on(second.attach(proto::AttachHostedTerminal {
            canonical_root: canonical_root.clone(),
            session_id: listed.sessions[0].session_id.clone(),
        }))
        .expect("attach");
        assert!(!attached.controls_input);
        let replay = smol::block_on(second.subscribe(proto::SubscribePersistentTerminal {
            server_instance_id: attached.server_instance_id.clone(),
            terminal_id: attached.terminal_id.clone(),
            offset: 0,
        }))
        .expect("subscribe");
        assert!(output_text(&smol::block_on(drain(&replay))).contains("hosted-ready"));

        // Closing the attached viewer only detaches it; the session survives.
        smol::block_on(second.close(proto::ClosePersistentTerminal {
            server_instance_id: attached.server_instance_id.clone(),
            terminal_id: attached.terminal_id,
        }))
        .expect("detach");
        let listed = smol::block_on(first.list(proto::ListHostedTerminals {
            canonical_root: canonical_root.clone(),
        }))
        .expect("list");
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].viewer_count, 1);

        // The original client still controls input; takeover moves it.
        let control = smol::block_on(second.attach(proto::AttachHostedTerminal {
            canonical_root,
            session_id: listed.sessions[0].session_id.clone(),
        }))
        .expect("attach again");
        assert_eq!(control.session_id, listed.sessions[0].session_id);
        assert!(!control.controls_input);
        let taken = smol::block_on(second.take_control(proto::TakeHostedTerminalControl {
            terminal_id: control.terminal_id,
            session_id: listed.sessions[0].session_id.clone(),
            attachment_id: String::new(),
        }))
        .expect("take control");
        assert!(taken.controls_input);
        // The previous controller's queued input is rejected after takeover.
        let stale = smol::block_on(first.input(proto::PersistentTerminalInput {
            server_instance_id: HOSTED_INSTANCE_ID.into(),
            terminal_id: created.terminal_id,
            sequence: 9,
            data: b"stale".to_vec(),
        }))
        .expect_err("stale controller input must be rejected");
        assert!(stale.to_string().contains("control"), "{stale:#}");
        smol::block_on(first.terminate(proto::TerminateHostedTerminal {
            session_id: listed.sessions[0].session_id.clone(),
            attachment_id: String::new(),
        }))
        .expect("terminate");
        smol::block_on(second.delete(proto::DeleteHostedTerminal {
            session_id: listed.sessions[0].session_id.clone(),
            attachment_id: String::new(),
        }))
        .expect("delete");
    }

    #[test]
    fn a_closed_connection_releases_its_attachment_and_input_control() {
        let (_daemon, service_dir, root) = daemon();
        let canonical_root = root.display().to_string();
        let first = HostedTerminals::new(service_dir.clone());
        let created =
            smol::block_on(first.create(create_request(&root, "reconnect"), None)).expect("create");
        let hosted = smol::block_on(first.host(proto::HostTerminal {
            server_instance_id: created.server_instance_id,
            terminal_id: created.terminal_id,
            name: "agent".into(),
            attachment_id: String::new(),
        }))
        .expect("host");

        // Reconnecting creates a new project connection while the previous one
        // is gone. Closing its socket must release the old attachment, otherwise
        // the new one can never type.
        drop(first);
        let second = HostedTerminals::new(service_dir);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let listed = smol::block_on(second.list(proto::ListHostedTerminals {
                canonical_root: canonical_root.clone(),
            }))
            .expect("list");
            if listed
                .sessions
                .first()
                .is_some_and(|session| session.viewer_count == 0)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the closed connection kept its hosted terminal attachment"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let attached = smol::block_on(second.attach(proto::AttachHostedTerminal {
            canonical_root,
            session_id: hosted.session_id.clone(),
        }))
        .expect("attach");
        assert!(attached.controls_input);
        smol::block_on(second.input(proto::PersistentTerminalInput {
            server_instance_id: attached.server_instance_id,
            terminal_id: attached.terminal_id,
            sequence: 1,
            data: b"echo reattached\n".to_vec(),
        }))
        .expect("input after reattach");
        smol::block_on(second.terminate(proto::TerminateHostedTerminal {
            session_id: hosted.session_id,
            attachment_id: String::new(),
        }))
        .expect("terminate");
    }

    #[test]
    fn detaching_a_session_makes_its_pseudo_terminal_unaddressable() {
        let (_daemon, service_dir, root) = daemon();
        let terminals = HostedTerminals::new(service_dir.clone());
        let created = smol::block_on(terminals.create(create_request(&root, "detach"), None))
            .expect("create");
        let hosted = smol::block_on(terminals.host(proto::HostTerminal {
            server_instance_id: created.server_instance_id,
            terminal_id: created.terminal_id.clone(),
            name: "agent".into(),
            attachment_id: String::new(),
        }))
        .expect("host");
        smol::block_on(terminals.detach(proto::DetachHostedTerminal {
            session_id: hosted.session_id.clone(),
            attachment_id: String::new(),
        }))
        .expect("detach");
        // The old pseudo terminal must not survive the detach, or a later close
        // and any keystroke would address a released attachment.
        let error = smol::block_on(terminals.input(proto::PersistentTerminalInput {
            server_instance_id: HOSTED_INSTANCE_ID.into(),
            terminal_id: created.terminal_id.clone(),
            sequence: 1,
            data: b"x".to_vec(),
        }))
        .expect_err("a detached pseudo terminal must reject input");
        assert!(
            error.to_string().contains("no longer attached"),
            "{error:#}"
        );
        smol::block_on(terminals.close(proto::ClosePersistentTerminal {
            server_instance_id: HOSTED_INSTANCE_ID.into(),
            terminal_id: created.terminal_id,
        }))
        .expect("closing a detached terminal is a no-op");
        // A different connection can attach the surviving session and clean up.
        let second = HostedTerminals::new(service_dir);
        let attached = smol::block_on(second.attach(proto::AttachHostedTerminal {
            canonical_root: root.display().to_string(),
            session_id: hosted.session_id.clone(),
        }))
        .expect("attach");
        assert!(attached.controls_input);
        smol::block_on(second.terminate(proto::TerminateHostedTerminal {
            session_id: hosted.session_id,
            attachment_id: String::new(),
        }))
        .expect("terminate");
    }

    #[test]
    fn unhosted_terminals_are_terminated_when_the_client_closes_them() {
        let (_daemon, service_dir, root) = daemon();
        let terminals = HostedTerminals::new(service_dir);
        let mut request = create_request(&root, "closing");
        request.hosted = false;
        let created = smol::block_on(terminals.create(request, None)).expect("create");
        smol::block_on(terminals.close(proto::ClosePersistentTerminal {
            server_instance_id: created.server_instance_id,
            terminal_id: created.terminal_id,
        }))
        .expect("close");
        assert!(
            smol::block_on(terminals.list(proto::ListHostedTerminals {
                canonical_root: root.display().to_string(),
            }))
            .expect("list")
            .sessions
            .is_empty()
        );
    }

    /// A hosted terminal belongs to the project that created it, not to the
    /// directory its shell started in. The client lists and re-attaches a
    /// session by its project's first worktree, so a shell opened in a
    /// subdirectory has to stay in that scope, and another project must still not
    /// see or attach it.
    #[test]
    fn a_terminal_opened_below_the_project_root_stays_in_the_project_scope() {
        let (_daemon, service_dir, root) = daemon();
        let project_root = root.display().to_string();
        let subdirectory = root.join("crates").join("agent");
        std::fs::create_dir_all(&subdirectory).expect("subdirectory");
        let other_project = root.with_file_name("another-project");
        std::fs::create_dir_all(&other_project).expect("other project");

        let terminals = HostedTerminals::new(service_dir);
        let created = smol::block_on(terminals.create(
            create_request(&subdirectory, "nested"),
            Some(project_root.clone()),
        ))
        .expect("create in a subdirectory");
        let hosted = smol::block_on(terminals.host(proto::HostTerminal {
            server_instance_id: created.server_instance_id,
            terminal_id: created.terminal_id,
            name: "agent".into(),
            attachment_id: String::new(),
        }))
        .expect("host");

        // The project's own root lists the session and can re-attach it.
        let listed = smol::block_on(terminals.list(proto::ListHostedTerminals {
            canonical_root: project_root.clone(),
        }))
        .expect("list");
        assert_eq!(listed.sessions.len(), 1, "{listed:?}");
        assert_eq!(listed.sessions[0].session_id, hosted.session_id);
        smol::block_on(terminals.attach(proto::AttachHostedTerminal {
            canonical_root: project_root.clone(),
            session_id: hosted.session_id.clone(),
        }))
        .expect("attach from the project root");

        // Another project sees neither the list entry nor a way to attach it.
        let elsewhere = other_project.display().to_string();
        assert!(
            smol::block_on(terminals.list(proto::ListHostedTerminals {
                canonical_root: elsewhere.clone(),
            }))
            .expect("list")
            .sessions
            .is_empty()
        );
        let error = smol::block_on(terminals.attach(proto::AttachHostedTerminal {
            canonical_root: elsewhere,
            session_id: hosted.session_id,
        }))
        .expect_err("another project must not attach the session");
        assert!(error.to_string().contains("invalid"), "{error:#}");

        // Without a project root the session keeps the working directory as its
        // scope, which is the behaviour for projects without a folder.
        let unscoped =
            smol::block_on(terminals.create(create_request(&subdirectory, "unscoped"), None))
                .expect("create without a project root");
        smol::block_on(terminals.host(proto::HostTerminal {
            server_instance_id: unscoped.server_instance_id,
            terminal_id: unscoped.terminal_id,
            name: "unscoped".into(),
            attachment_id: String::new(),
        }))
        .expect("host");
        let listed = smol::block_on(terminals.list(proto::ListHostedTerminals {
            canonical_root: project_root,
        }))
        .expect("list");
        assert_eq!(listed.sessions.len(), 1, "{listed:?}");
        let listed = smol::block_on(terminals.list(proto::ListHostedTerminals {
            canonical_root: subdirectory.display().to_string(),
        }))
        .expect("list");
        assert_eq!(listed.sessions.len(), 1, "{listed:?}");
        assert_eq!(listed.sessions[0].name, "unscoped");
    }

    #[test]
    fn foreign_instance_ids_cannot_drive_a_hosted_terminal() {
        let (_daemon, service_dir, root) = daemon();
        let terminals = HostedTerminals::new(service_dir);
        let created = smol::block_on(terminals.create(create_request(&root, "foreign"), None))
            .expect("create");
        let error = smol::block_on(terminals.input(proto::PersistentTerminalInput {
            server_instance_id: "not-hosted".into(),
            terminal_id: created.terminal_id,
            sequence: 1,
            data: b"x".to_vec(),
        }))
        .expect_err("foreign instance id must be rejected");
        assert!(error.to_string().contains("hosted terminal service"));
    }
}
