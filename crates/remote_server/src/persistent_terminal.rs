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

/// Maximum bytes pushed per stream frame. Small frames keep interactive echo
/// latency low and bound per-frame copy work under the terminal state lock.
const STREAM_CHUNK_BYTES: u64 = 64 * 1024;
/// Maximum pushed-but-unacknowledged bytes per subscription. The client
/// reports consumption through `PersistentTerminalOutputCredit`; when the
/// window is exhausted the forwarder pauses and the 4 MiB ring sheds old
/// scrollback, so a slow network cannot grow memory without bound.
const STREAM_WINDOW_BYTES: u64 = 1024 * 1024;

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
    subscription: Option<OutputSubscription>,
    next_subscription_generation: u64,
}

struct OutputSubscription {
    generation: u64,
    next_offset: u64,
    credit: u64,
    history_truncated: bool,
    final_frame_sent: bool,
    kick: async_channel::Sender<()>,
}

pub enum OutputPull {
    Frame(proto::PersistentTerminalOutput),
    WaitForKick,
    Superseded,
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
                            if let Some(subscription) = &state.subscription {
                                subscription.kick.try_send(()).ok();
                            }
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
                "terminal input sequence gap: expected {}, received {} (terminal {})",
                state.last_input_sequence + 1,
                request.sequence,
                request.terminal_id
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

    pub fn subscribe(
        &self,
        request: proto::SubscribePersistentTerminal,
    ) -> Result<(u64, async_channel::Receiver<()>)> {
        let terminal = self.terminal(&request.server_instance_id, &request.terminal_id)?;
        let mut state = terminal
            .state
            .lock()
            .map_err(|_| anyhow!("persistent terminal state is unavailable"))?;
        state.next_subscription_generation += 1;
        let generation = state.next_subscription_generation;
        let next_offset = request.offset.clamp(state.output_start, state.output_end);
        let history_truncated = request.offset < state.output_start;
        let (kick_tx, kick_rx) = async_channel::bounded(1);
        // Replacing the subscription drops the previous kick sender, which
        // ends the previous forwarder; the generation check in `pull_output`
        // covers the case where it was not waiting on the channel.
        state.subscription = Some(OutputSubscription {
            generation,
            next_offset,
            credit: next_offset,
            history_truncated,
            final_frame_sent: false,
            kick: kick_tx,
        });
        Ok((generation, kick_rx))
    }

    pub fn set_output_credit(&self, request: proto::PersistentTerminalOutputCredit) -> Result<()> {
        self.ensure_instance(&request.server_instance_id)?;
        let terminal = self
            .state
            .lock()
            .map_err(|_| anyhow!("persistent terminal manager is unavailable"))?
            .terminals
            .get(&request.terminal_id)
            .cloned();
        // The terminal may already be closed; late credits are harmless.
        let Some(terminal) = terminal else {
            return Ok(());
        };
        let mut state = terminal
            .state
            .lock()
            .map_err(|_| anyhow!("persistent terminal state is unavailable"))?;
        if let Some(subscription) = &mut state.subscription {
            if request.offset > subscription.credit {
                subscription.credit = request.offset;
                subscription.kick.try_send(()).ok();
            }
        }
        Ok(())
    }

    pub fn pull_output(
        &self,
        server_instance_id: &str,
        terminal_id: &str,
        generation: u64,
    ) -> Result<OutputPull> {
        let terminal = self.terminal(server_instance_id, terminal_id)?;
        let mut state = terminal
            .state
            .lock()
            .map_err(|_| anyhow!("persistent terminal state is unavailable"))?;
        let TerminalState {
            output,
            output_start,
            output_end,
            exited,
            exit_code,
            subscription,
            ..
        } = &mut *state;
        let Some(subscription) = subscription else {
            return Ok(OutputPull::Superseded);
        };
        if subscription.generation != generation {
            return Ok(OutputPull::Superseded);
        }
        if subscription.next_offset < *output_start {
            subscription.next_offset = *output_start;
            subscription.history_truncated = true;
        }
        let window_remaining =
            (subscription.credit + STREAM_WINDOW_BYTES).saturating_sub(subscription.next_offset);
        let available = (*output_end).saturating_sub(subscription.next_offset);
        let count = available.min(window_remaining).min(STREAM_CHUNK_BYTES) as usize;
        let drained = subscription.next_offset + count as u64 == *output_end;
        if count > 0 {
            let start = (subscription.next_offset - *output_start) as usize;
            let data = output.iter().skip(start).take(count).copied().collect();
            let frame_exited = *exited && drained;
            let frame = proto::PersistentTerminalOutput {
                server_instance_id: server_instance_id.to_string(),
                terminal_id: terminal_id.to_string(),
                offset: subscription.next_offset,
                data,
                history_truncated: std::mem::take(&mut subscription.history_truncated),
                exited: frame_exited,
                exit_code: if frame_exited { *exit_code } else { None },
            };
            subscription.next_offset += count as u64;
            if frame_exited {
                subscription.final_frame_sent = true;
            }
            return Ok(OutputPull::Frame(frame));
        }
        if *exited && drained && !subscription.final_frame_sent {
            subscription.final_frame_sent = true;
            return Ok(OutputPull::Frame(proto::PersistentTerminalOutput {
                server_instance_id: server_instance_id.to_string(),
                terminal_id: terminal_id.to_string(),
                offset: subscription.next_offset,
                data: Vec::new(),
                history_truncated: std::mem::take(&mut subscription.history_truncated),
                exited: true,
                exit_code: *exit_code,
            }));
        }
        Ok(OutputPull::WaitForKick)
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
        if let Ok(mut terminal_state) = terminal.state.lock() {
            if let Some(subscription) = terminal_state.subscription.take() {
                subscription.kick.try_send(()).ok();
            }
        }
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
        if let Some(subscription) = &state.subscription {
            subscription.kick.try_send(()).ok();
        }
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

    fn manager_with_terminal(output: &[u8]) -> (PersistentTerminalManager, String) {
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
        terminal.append_output(output);
        let manager = PersistentTerminalManager::new();
        let terminal_id = "terminal".to_string();
        manager
            .state
            .lock()
            .expect("manager state")
            .terminals
            .insert(terminal_id.clone(), terminal);
        (manager, terminal_id)
    }

    fn subscribe(manager: &PersistentTerminalManager, terminal_id: &str, offset: u64) -> u64 {
        manager
            .subscribe(proto::SubscribePersistentTerminal {
                server_instance_id: manager.instance_id.clone(),
                terminal_id: terminal_id.to_string(),
                offset,
            })
            .expect("subscribe")
            .0
    }

    fn pull(manager: &PersistentTerminalManager, terminal_id: &str, generation: u64) -> OutputPull {
        manager
            .pull_output(&manager.instance_id.clone(), terminal_id, generation)
            .expect("pull output")
    }

    fn credit(manager: &PersistentTerminalManager, terminal_id: &str, offset: u64) {
        manager
            .set_output_credit(proto::PersistentTerminalOutputCredit {
                server_instance_id: manager.instance_id.clone(),
                terminal_id: terminal_id.to_string(),
                offset,
            })
            .expect("set credit");
    }

    #[test]
    fn subscription_streams_output_within_credit_window() {
        let output = vec![b'x'; STREAM_WINDOW_BYTES as usize + 100];
        let (manager, terminal_id) = manager_with_terminal(&output);
        let generation = subscribe(&manager, &terminal_id, 0);

        // The window admits exactly STREAM_WINDOW_BYTES across chunk-sized frames.
        let mut received = Vec::new();
        loop {
            match pull(&manager, &terminal_id, generation) {
                OutputPull::Frame(frame) => {
                    assert!(!frame.history_truncated);
                    assert!(!frame.exited);
                    assert_eq!(frame.offset, received.len() as u64);
                    received.extend_from_slice(&frame.data);
                }
                OutputPull::WaitForKick => break,
                OutputPull::Superseded => panic!("subscription should be current"),
            }
        }
        assert_eq!(received.len(), STREAM_WINDOW_BYTES as usize);

        credit(&manager, &terminal_id, received.len() as u64);
        match pull(&manager, &terminal_id, generation) {
            OutputPull::Frame(frame) => {
                assert_eq!(frame.offset, STREAM_WINDOW_BYTES);
                received.extend_from_slice(&frame.data);
            }
            _ => panic!("credit should admit the remaining output"),
        }
        assert_eq!(received, output);
        assert!(matches!(
            pull(&manager, &terminal_id, generation),
            OutputPull::WaitForKick
        ));
    }

    #[test]
    fn subscription_reports_exit_on_final_frame() {
        let (manager, terminal_id) = manager_with_terminal(b"done");
        let generation = subscribe(&manager, &terminal_id, 0);
        match pull(&manager, &terminal_id, generation) {
            OutputPull::Frame(frame) => {
                assert_eq!(frame.data, b"done");
                assert!(!frame.exited);
            }
            _ => panic!("expected output frame"),
        }

        {
            let terminal = manager
                .state
                .lock()
                .expect("manager state")
                .terminals
                .get(&terminal_id)
                .expect("terminal")
                .clone();
            let mut state = terminal.state.lock().expect("terminal state");
            state.exited = true;
            state.exit_code = Some(7);
        }

        match pull(&manager, &terminal_id, generation) {
            OutputPull::Frame(frame) => {
                assert!(frame.data.is_empty());
                assert!(frame.exited);
                assert_eq!(frame.exit_code, Some(7));
            }
            _ => panic!("expected final exit frame"),
        }
        // The final frame is emitted exactly once.
        assert!(matches!(
            pull(&manager, &terminal_id, generation),
            OutputPull::WaitForKick
        ));
    }

    #[test]
    fn resubscribe_supersedes_previous_generation() {
        let (manager, terminal_id) = manager_with_terminal(b"abc");
        let first = subscribe(&manager, &terminal_id, 0);
        let second = subscribe(&manager, &terminal_id, 1);
        assert!(matches!(
            pull(&manager, &terminal_id, first),
            OutputPull::Superseded
        ));
        match pull(&manager, &terminal_id, second) {
            OutputPull::Frame(frame) => {
                assert_eq!(frame.offset, 1);
                assert_eq!(frame.data, b"bc");
            }
            _ => panic!("expected frame from new subscription"),
        }
    }

    #[test]
    fn subscription_jumps_and_reports_truncation_after_ring_overflow() {
        let (manager, terminal_id) = manager_with_terminal(b"abc");
        let generation = subscribe(&manager, &terminal_id, 0);

        // Overflow the ring while the subscriber has not pulled anything.
        let terminal = manager
            .state
            .lock()
            .expect("manager state")
            .terminals
            .get(&terminal_id)
            .expect("terminal")
            .clone();
        terminal.append_output(&vec![b'y'; OUTPUT_CAPACITY_BYTES + 16]);

        match pull(&manager, &terminal_id, generation) {
            OutputPull::Frame(frame) => {
                assert!(frame.history_truncated);
                assert_eq!(frame.offset, 19);
                assert!(!frame.data.is_empty());
                assert!(frame.data.iter().all(|byte| *byte == b'y'));
            }
            _ => panic!("expected truncated frame after ring overflow"),
        }
    }

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
    fn input_replay_is_deduplicated_and_gaps_do_not_advance_sequence() {
        let (manager, terminal_id) = manager_with_terminal(b"output");
        let input = |sequence| proto::PersistentTerminalInput {
            server_instance_id: manager.instance_id.clone(),
            terminal_id: terminal_id.clone(),
            sequence,
            data: b"input".to_vec(),
        };
        manager.input(input(1)).expect("first input");
        manager.input(input(1)).expect("replayed input");
        let error = manager.input(input(3)).expect_err("reject gap");
        assert!(error.to_string().contains("expected 2, received 3"));
        manager.input(input(2)).expect("missing input");
        manager.input(input(3)).expect("next input");
        let terminal = manager
            .terminal(&manager.instance_id, &terminal_id)
            .expect("terminal");
        assert_eq!(terminal.state.lock().expect("state").last_input_sequence, 3);
        let output = manager
            .read(proto::ReadPersistentTerminal {
                server_instance_id: manager.instance_id.clone(),
                terminal_id,
                offset: 0,
                max_bytes: 100,
            })
            .expect("output unaffected by input gap");
        assert_eq!(output.data, b"output");
    }

    #[test]
    fn close_invalidates_terminal_without_recreating_it() {
        let (manager, terminal_id) = manager_with_terminal(b"output");
        manager
            .close(&manager.instance_id, &terminal_id)
            .expect("close");
        assert!(
            manager
                .read(proto::ReadPersistentTerminal {
                    server_instance_id: manager.instance_id.clone(),
                    terminal_id,
                    offset: 0,
                    max_bytes: 100,
                })
                .expect_err("closed")
                .to_string()
                .contains("persistent terminal not found")
        );
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
