use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rpc::proto;
use std::{
    collections::VecDeque,
    io::{Read as _, Write as _},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

const OUTPUT_CAPACITY_BYTES: usize = 4 * 1024 * 1024;
const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_TERMINALS: usize = 64;

pub struct PersistentTerminalManager {
    instance_id: String,
    state: Mutex<ManagerState>,
}

#[derive(Default)]
struct ManagerState {
    by_creation_id: HashMap<String, String>,
    terminals: HashMap<String, Arc<PersistentTerminal>>,
}

struct PersistentTerminal {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    state: Mutex<TerminalState>,
}

#[derive(Default)]
struct TerminalState {
    output: VecDeque<u8>,
    output_start: u64,
    output_end: u64,
    last_input_sequence: u64,
    exited: bool,
    exit_code: Option<i32>,
}

impl PersistentTerminalManager {
    pub fn new() -> Self {
        Self {
            instance_id: Uuid::new_v4().to_string(),
            state: Mutex::new(ManagerState::default()),
        }
    }

    pub fn create(
        &self,
        request: proto::CreatePersistentTerminal,
    ) -> Result<proto::CreatePersistentTerminalResponse> {
        anyhow::ensure!(
            !request.creation_id.is_empty(),
            "missing terminal creation id"
        );
        anyhow::ensure!(!request.program.is_empty(), "missing terminal program");

        let mut manager = self
            .state
            .lock()
            .map_err(|_| anyhow!("persistent terminal manager is unavailable"))?;
        if let Some(terminal_id) = manager.by_creation_id.get(&request.creation_id) {
            return Ok(proto::CreatePersistentTerminalResponse {
                server_instance_id: self.instance_id.clone(),
                terminal_id: terminal_id.clone(),
            });
        }
        anyhow::ensure!(
            manager.terminals.len() < MAX_TERMINALS,
            "persistent terminal limit reached"
        );

        let size = pty_size(
            request.rows,
            request.columns,
            request.pixel_width,
            request.pixel_height,
        )?;
        let pair = native_pty_system().openpty(size)?;
        let mut command = CommandBuilder::new(&request.program);
        command.args(&request.args);
        if let Some(working_directory) = request.working_directory {
            command.cwd(working_directory);
        }
        for (name, value) in request.env {
            command.env(name, value);
        }

        let mut child = pair.slave.spawn_command(command).with_context(|| {
            format!(
                "failed to spawn persistent terminal program {}",
                request.program
            )
        })?;
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let terminal = Arc::new(PersistentTerminal {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            state: Mutex::new(TerminalState::default()),
        });

        {
            let terminal = terminal.clone();
            std::thread::Builder::new()
                .name("persistent-terminal-output".into())
                .spawn(move || {
                    let mut buffer = [0_u8; 16 * 1024];
                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(count) => terminal.append_output(&buffer[..count]),
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(error) => {
                                log::warn!("persistent terminal output read failed: {error:#}");
                                break;
                            }
                        }
                    }
                })?;
        }
        {
            let terminal = terminal.clone();
            std::thread::Builder::new()
                .name("persistent-terminal-wait".into())
                .spawn(move || {
                    let result = child.wait();
                    match terminal.state.lock() {
                        Ok(mut state) => {
                            state.exited = true;
                            state.exit_code = result.ok().map(|status| status.exit_code() as i32);
                        }
                        Err(error) => log::error!("persistent terminal state poisoned: {error}"),
                    }
                })?;
        }

        let terminal_id = Uuid::new_v4().to_string();
        manager
            .by_creation_id
            .insert(request.creation_id, terminal_id.clone());
        manager.terminals.insert(terminal_id.clone(), terminal);
        Ok(proto::CreatePersistentTerminalResponse {
            server_instance_id: self.instance_id.clone(),
            terminal_id,
        })
    }

    pub fn input(&self, request: proto::PersistentTerminalInput) -> Result<()> {
        anyhow::ensure!(
            request.data.len() <= MAX_INPUT_BYTES,
            "terminal input exceeds 1 MiB"
        );
        let terminal = self.terminal(&request.server_instance_id, &request.terminal_id)?;
        let mut writer = terminal
            .writer
            .lock()
            .map_err(|_| anyhow!("persistent terminal writer is unavailable"))?;
        {
            let state = terminal
                .state
                .lock()
                .map_err(|_| anyhow!("persistent terminal state is unavailable"))?;
            if request.sequence <= state.last_input_sequence {
                return Ok(());
            }
            anyhow::ensure!(
                request.sequence == state.last_input_sequence + 1,
                "terminal input sequence gap"
            );
            anyhow::ensure!(!state.exited, "persistent terminal has exited");
        }
        writer.write_all(&request.data)?;
        terminal
            .state
            .lock()
            .map_err(|_| anyhow!("persistent terminal state is unavailable"))?
            .last_input_sequence = request.sequence;
        Ok(())
    }

    pub fn resize(&self, request: proto::ResizePersistentTerminal) -> Result<()> {
        let terminal = self.terminal(&request.server_instance_id, &request.terminal_id)?;
        let size = pty_size(
            request.rows,
            request.columns,
            request.pixel_width,
            request.pixel_height,
        )?;
        terminal
            .master
            .lock()
            .map_err(|_| anyhow!("persistent terminal PTY is unavailable"))?
            .resize(size)?;
        Ok(())
    }

    pub fn read(
        &self,
        request: proto::ReadPersistentTerminal,
    ) -> Result<proto::ReadPersistentTerminalResponse> {
        let terminal = self.terminal(&request.server_instance_id, &request.terminal_id)?;
        let state = terminal
            .state
            .lock()
            .map_err(|_| anyhow!("persistent terminal state is unavailable"))?;
        let requested_offset = request.offset.max(state.output_start).min(state.output_end);
        let history_truncated = request.offset < state.output_start;
        let available = state.output_end.saturating_sub(requested_offset) as usize;
        let max_bytes = (request.max_bytes as usize).clamp(1, MAX_READ_BYTES);
        let count = available.min(max_bytes);
        let start = requested_offset.saturating_sub(state.output_start) as usize;
        let data = state
            .output
            .iter()
            .skip(start)
            .take(count)
            .copied()
            .collect();
        Ok(proto::ReadPersistentTerminalResponse {
            data,
            next_offset: requested_offset + count as u64,
            history_truncated,
            exited: state.exited,
            exit_code: state.exit_code,
        })
    }

    pub fn close(&self, server_instance_id: &str, terminal_id: &str) -> Result<()> {
        self.ensure_instance(server_instance_id)?;
        let terminal = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("persistent terminal manager is unavailable"))?;
            let terminal = state
                .terminals
                .remove(terminal_id)
                .context("persistent terminal not found")?;
            state
                .by_creation_id
                .retain(|_, stored_terminal_id| stored_terminal_id != terminal_id);
            terminal
        };
        terminal
            .killer
            .lock()
            .map_err(|_| anyhow!("persistent terminal killer is unavailable"))?
            .kill()?;
        Ok(())
    }

    fn terminal(
        &self,
        server_instance_id: &str,
        terminal_id: &str,
    ) -> Result<Arc<PersistentTerminal>> {
        self.ensure_instance(server_instance_id)?;
        self.state
            .lock()
            .map_err(|_| anyhow!("persistent terminal manager is unavailable"))?
            .terminals
            .get(terminal_id)
            .cloned()
            .context("persistent terminal not found")
    }

    fn ensure_instance(&self, server_instance_id: &str) -> Result<()> {
        anyhow::ensure!(
            server_instance_id == self.instance_id,
            "persistent terminal belongs to another server instance"
        );
        Ok(())
    }
}

impl PersistentTerminal {
    fn append_output(&self, bytes: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            log::error!("persistent terminal state poisoned while appending output");
            return;
        };
        state.output.extend(bytes.iter().copied());
        state.output_end = state.output_end.saturating_add(bytes.len() as u64);
        let excess = state.output.len().saturating_sub(OUTPUT_CAPACITY_BYTES);
        state.output.drain(..excess);
        state.output_start = state.output_start.saturating_add(excess as u64);
    }
}

fn pty_size(rows: u32, columns: u32, pixel_width: u32, pixel_height: u32) -> Result<PtySize> {
    let rows = u16::try_from(rows).context("terminal row count is too large")?;
    let cols = u16::try_from(columns).context("terminal column count is too large")?;
    anyhow::ensure!(rows > 0 && cols > 0, "terminal dimensions must be positive");
    Ok(PtySize {
        rows,
        cols,
        pixel_width: u16::try_from(pixel_width).unwrap_or(u16::MAX),
        pixel_height: u16::try_from(pixel_height).unwrap_or(u16::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_history_is_bounded_and_reports_truncation() {
        let terminal = Arc::new(PersistentTerminal {
            master: Mutex::new(
                native_pty_system()
                    .openpty(PtySize::default())
                    .expect("open PTY")
                    .master,
            ),
            writer: Mutex::new(Box::new(std::io::sink())),
            killer: Mutex::new(Box::new(TestKiller)),
            state: Mutex::new(TerminalState::default()),
        });
        terminal.append_output(&vec![b'a'; OUTPUT_CAPACITY_BYTES + 16]);

        let manager = PersistentTerminalManager::new();
        let terminal_id = "terminal".to_string();
        manager
            .state
            .lock()
            .expect("manager state")
            .terminals
            .insert(terminal_id.clone(), terminal);
        let response = manager
            .read(proto::ReadPersistentTerminal {
                server_instance_id: manager.instance_id.clone(),
                terminal_id,
                offset: 0,
                max_bytes: 32,
            })
            .expect("read terminal");

        assert!(response.history_truncated);
        assert_eq!(response.data, vec![b'a'; 32]);
        assert_eq!(response.next_offset, 48);
    }

    #[test]
    fn rejects_invalid_terminal_dimensions() {
        assert!(pty_size(0, 80, 0, 0).is_err());
        assert!(pty_size(24, 0, 0, 0).is_err());
        assert!(pty_size(u16::MAX as u32 + 1, 80, 0, 0).is_err());
    }

    #[derive(Debug)]
    struct TestKiller;

    impl portable_pty::ChildKiller for TestKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(Self)
        }
    }
}
