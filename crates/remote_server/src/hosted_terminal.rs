use anyhow::{Context as _, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, native_pty_system};
use rpc::proto::{self, HostedTerminalFrame, hosted_terminal_frame};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::persistent_terminal::pty_size;

/// Retained replay bytes per hosted session. A late attachment re-emulates this
/// history to rebuild the screen; when a session outgrows the ring the attach
/// reports truncation instead of pretending the screen is complete.
const HOSTED_OUTPUT_CAPACITY_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const INPUT_BACKLOG_CAPACITY_BYTES: usize = MAX_INPUT_BYTES;
const STREAM_CHUNK_BYTES: u64 = 64 * 1024;
const STREAM_WINDOW_BYTES: u64 = 1024 * 1024;
const MAX_SESSIONS: usize = 32;
const MAX_ATTACHMENTS: usize = 16;
const MAX_NAME_BYTES: usize = 256;
const MAX_ROOT_BYTES: usize = 4096;

/// Grace an unhosted session survives without an attachment. Hosted sessions
/// never expire on their own; only termination or process exit ends them.
pub const ORDINARY_SESSION_GRACE: Duration = Duration::from_secs(30 * 60);

/// How often unhosted sessions are checked against their disconnect grace.
const ORDINARY_SESSION_EXPIRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub enum SessionError {
    AttachmentExists,
    AttachmentLimit,
    UnknownAttachment,
    NotController,
    StaleControl,
    InputSequenceGap,
    InputBacklogFull,
    NotHosted,
    AlreadyExited,
    SessionRunning,
    InvalidName,
    InvalidRoot,
    NotFound,
    SessionLimit,
    ClockWentBackwards,
    SpawnFailed,
}

impl SessionError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::AttachmentExists => "hosted terminal attachment already exists",
            Self::AttachmentLimit => "hosted terminal attachment limit reached",
            Self::UnknownAttachment => "hosted terminal attachment not found",
            Self::NotController => "attachment does not control this terminal",
            Self::StaleControl => "terminal input control was taken over",
            Self::InputSequenceGap => "hosted terminal input sequence gap",
            Self::InputBacklogFull => "hosted terminal input backlog is full",
            Self::NotHosted => "terminal is not hosted",
            Self::AlreadyExited => "hosted terminal has exited",
            Self::SessionRunning => "hosted terminal is still running",
            Self::InvalidName => "invalid hosted terminal name",
            Self::InvalidRoot => "invalid hosted terminal root",
            Self::NotFound => "hosted terminal not found",
            Self::SessionLimit => "hosted terminal limit reached",
            Self::ClockWentBackwards => "hosted terminal clock went backwards",
            Self::SpawnFailed => "hosted terminal program could not be started",
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, PartialEq, Eq)]
pub enum InputDecision {
    Accept,
    Duplicate,
}

/// Lifetime and authority of one terminal. Time is supplied by the caller as a
/// monotonic tick, so the state machine is testable without sleeping.
#[derive(Debug)]
pub struct SessionLifecycle {
    name: String,
    hosted: bool,
    exited: bool,
    attachments: HashMap<String, Attachment>,
    controller: Option<String>,
    control_generation: u64,
    disconnected_at: Option<Duration>,
    last_event_at: Duration,
}

#[derive(Debug)]
struct Attachment {
    last_input_sequence: u64,
}

impl SessionLifecycle {
    pub fn new(attachment_id: String, now: Duration) -> Self {
        let mut attachments = HashMap::new();
        attachments.insert(
            attachment_id.clone(),
            Attachment {
                last_input_sequence: 0,
            },
        );
        Self {
            name: String::new(),
            hosted: false,
            exited: false,
            attachments,
            controller: Some(attachment_id),
            control_generation: 1,
            disconnected_at: None,
            last_event_at: now,
        }
    }

    pub fn host(&mut self, attachment_id: &str, name: &str) -> Result<(), SessionError> {
        self.require_attachment(attachment_id)?;
        if self.exited {
            return Err(SessionError::AlreadyExited);
        }
        self.rename(name)?;
        self.hosted = true;
        Ok(())
    }

    pub fn unhost(&mut self, attachment_id: &str) -> Result<(), SessionError> {
        self.take_control(attachment_id)?;
        self.hosted = false;
        self.attachments.retain(|id, _| id == attachment_id);
        Ok(())
    }

    pub fn rename(&mut self, name: &str) -> Result<(), SessionError> {
        let name = name.trim();
        if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
            return Err(SessionError::InvalidName);
        }
        self.name = name.to_owned();
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_hosted(&self) -> bool {
        self.hosted
    }

    pub fn attach(&mut self, attachment_id: String, now: Duration) -> Result<(), SessionError> {
        self.check_clock(now)?;
        if !self.hosted {
            return Err(SessionError::NotHosted);
        }
        if self.attachments.contains_key(&attachment_id) {
            return Err(SessionError::AttachmentExists);
        }
        if self.attachments.len() >= MAX_ATTACHMENTS {
            return Err(SessionError::AttachmentLimit);
        }
        // Nobody holds control once the previous controller detached or its
        // connection was released, so the next attachment takes it instead of
        // staying permanently read-only.
        let takes_control = self.controller.is_none() && !self.exited;
        self.attachments.insert(
            attachment_id.clone(),
            Attachment {
                last_input_sequence: 0,
            },
        );
        if takes_control {
            self.controller = Some(attachment_id);
            self.control_generation += 1;
        }
        self.disconnected_at = None;
        self.last_event_at = now;
        Ok(())
    }

    /// A repeated claim by the current controller is idempotent. Taking over
    /// invalidates the previous controller's queued input and resize requests.
    pub fn take_control(&mut self, attachment_id: &str) -> Result<u64, SessionError> {
        self.require_attachment(attachment_id)?;
        if self.exited {
            return Err(SessionError::AlreadyExited);
        }
        if self.controller.as_deref() != Some(attachment_id) {
            self.control_generation += 1;
            self.controller = Some(attachment_id.to_owned());
        }
        Ok(self.control_generation)
    }

    pub fn check_control(&self, attachment_id: &str, generation: u64) -> Result<(), SessionError> {
        self.require_attachment(attachment_id)?;
        if self.exited {
            return Err(SessionError::AlreadyExited);
        }
        if generation != self.control_generation {
            return Err(SessionError::StaleControl);
        }
        if self.controller.as_deref() != Some(attachment_id) {
            return Err(SessionError::NotController);
        }
        Ok(())
    }

    /// Validate under the same lock as input enqueue. Commit only after enqueue
    /// succeeds, so backpressure never consumes a sequence number.
    pub fn check_input(
        &self,
        attachment_id: &str,
        generation: u64,
        sequence: u64,
    ) -> Result<InputDecision, SessionError> {
        self.check_control(attachment_id, generation)?;
        let attachment = self.require_attachment(attachment_id)?;
        if sequence <= attachment.last_input_sequence {
            return Ok(InputDecision::Duplicate);
        }
        if attachment.last_input_sequence.checked_add(1) != Some(sequence) {
            return Err(SessionError::InputSequenceGap);
        }
        Ok(InputDecision::Accept)
    }

    pub fn commit_input(
        &mut self,
        attachment_id: &str,
        generation: u64,
        sequence: u64,
    ) -> Result<InputDecision, SessionError> {
        let decision = self.check_input(attachment_id, generation, sequence)?;
        if decision == InputDecision::Accept {
            self.attachments
                .get_mut(attachment_id)
                .ok_or(SessionError::UnknownAttachment)?
                .last_input_sequence = sequence;
        }
        Ok(decision)
    }

    pub fn detach(&mut self, attachment_id: &str, now: Duration) -> Result<(), SessionError> {
        self.check_clock(now)?;
        self.attachments
            .remove(attachment_id)
            .ok_or(SessionError::UnknownAttachment)?;
        if self.controller.as_deref() == Some(attachment_id) {
            self.controller = None;
            self.control_generation += 1;
        }
        if self.attachments.is_empty() {
            self.disconnected_at = Some(now);
        }
        self.last_event_at = now;
        Ok(())
    }

    pub fn has_attachment(&self, attachment_id: &str) -> bool {
        self.attachments.contains_key(attachment_id)
    }

    pub fn controller(&self) -> Option<&str> {
        self.controller.as_deref()
    }

    pub fn controls(&self, attachment_id: &str) -> bool {
        self.controller.as_deref() == Some(attachment_id)
    }

    pub fn control_generation(&self) -> u64 {
        self.control_generation
    }

    pub fn viewer_count(&self) -> usize {
        self.attachments.len()
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    pub fn should_expire(&self, now: Duration, grace: Duration) -> bool {
        !self.hosted
            && self
                .disconnected_at
                .and_then(|disconnected| now.checked_sub(disconnected))
                .is_some_and(|elapsed| elapsed >= grace)
    }

    pub fn mark_exited(&mut self) {
        self.exited = true;
        self.controller = None;
        self.control_generation += 1;
    }

    fn require_attachment(&self, id: &str) -> Result<&Attachment, SessionError> {
        self.attachments
            .get(id)
            .ok_or(SessionError::UnknownAttachment)
    }

    fn check_clock(&self, now: Duration) -> Result<(), SessionError> {
        if now < self.last_event_at {
            Err(SessionError::ClockWentBackwards)
        } else {
            Ok(())
        }
    }
}

struct InputQueue {
    state: Mutex<InputQueueState>,
    available: Condvar,
}

#[derive(Default)]
struct InputQueueState {
    items: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    closed: bool,
}

impl InputQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(InputQueueState::default()),
            available: Condvar::new(),
        }
    }

    fn enqueue(&self, data: Vec<u8>) -> Result<(), SessionError> {
        let mut state = self.state.lock().map_err(|_| SessionError::NotFound)?;
        if state.closed {
            return Err(SessionError::AlreadyExited);
        }
        if state.queued_bytes + data.len() > INPUT_BACKLOG_CAPACITY_BYTES {
            return Err(SessionError::InputBacklogFull);
        }
        state.queued_bytes += data.len();
        state.items.push_back(data);
        self.available.notify_one();
        Ok(())
    }

    fn recv(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(data) = state.items.pop_front() {
                state.queued_bytes -= data.len();
                return Some(data);
            }
            if state.closed {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        self.available.notify_all();
    }
}

struct Session {
    id: String,
    canonical_root: String,
    started_at_unix_seconds: u64,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    input: Arc<InputQueue>,
    state: Mutex<SessionState>,
}

struct SessionState {
    lifecycle: SessionLifecycle,
    exited: bool,
    exit_code: Option<i32>,
    output: VecDeque<u8>,
    output_start: u64,
    output_end: u64,
    cursors: HashMap<String, Cursor>,
    kicks: HashMap<String, async_channel::Sender<()>>,
}

struct Cursor {
    next_offset: u64,
    credit: u64,
    history_truncated: bool,
    final_frame_sent: bool,
    /// Output that already existed when this attachment subscribed. Those
    /// bytes rebuild the screen on a late attach, so side-channel escape
    /// sequences in them must not run again.
    replay_end: u64,
}

impl Cursor {
    fn new() -> Self {
        Self {
            next_offset: 0,
            credit: 0,
            history_truncated: false,
            final_frame_sent: false,
            replay_end: 0,
        }
    }
}

pub enum OutputPull {
    Frame(proto::PersistentTerminalOutput),
    WaitForKick,
    Superseded,
}

impl Session {
    fn append_output(&self, bytes: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            log::error!("hosted terminal state poisoned while appending output");
            return;
        };
        state.output.extend(bytes.iter().copied());
        state.output_end = state.output_end.saturating_add(bytes.len() as u64);
        let excess = state
            .output
            .len()
            .saturating_sub(HOSTED_OUTPUT_CAPACITY_BYTES);
        state.output.drain(..excess);
        state.output_start = state.output_start.saturating_add(excess as u64);
        for kick in state.kicks.values() {
            kick.try_send(()).ok();
        }
    }

    fn mark_exited(&self, exit_code: Option<i32>) {
        let Ok(mut state) = self.state.lock() else {
            log::error!("hosted terminal state poisoned while marking exit");
            return;
        };
        state.exited = true;
        state.exit_code = exit_code;
        state.lifecycle.mark_exited();
        for kick in state.kicks.values() {
            kick.try_send(()).ok();
        }
    }

    fn pull_output(&self, attachment_id: &str) -> OutputPull {
        let Ok(mut state) = self.state.lock() else {
            return OutputPull::Superseded;
        };
        let exited = state.exited;
        let exit_code = state.exit_code;
        let output_start = state.output_start;
        let output_end = state.output_end;
        let Some((
            mut next_offset,
            credit,
            mut history_truncated,
            mut final_frame_sent,
            replay_end,
        )) = state.cursors.get(attachment_id).map(|cursor| {
            (
                cursor.next_offset,
                cursor.credit,
                cursor.history_truncated,
                cursor.final_frame_sent,
                cursor.replay_end,
            )
        })
        else {
            return OutputPull::Superseded;
        };
        if next_offset < output_start {
            next_offset = output_start;
            history_truncated = true;
        }
        let window_remaining = (credit + STREAM_WINDOW_BYTES).saturating_sub(next_offset);
        let available = output_end.saturating_sub(next_offset);
        let replay_remaining = if next_offset < replay_end {
            replay_end - next_offset
        } else {
            u64::MAX
        };
        let count = available
            .min(window_remaining)
            .min(STREAM_CHUNK_BYTES)
            .min(replay_remaining) as usize;
        let drained = next_offset + count as u64 == output_end;
        if count > 0 {
            let start = (next_offset - output_start) as usize;
            let data: Vec<u8> = state
                .output
                .iter()
                .skip(start)
                .take(count)
                .copied()
                .collect();
            let frame_exited = exited && drained;
            let frame = proto::PersistentTerminalOutput {
                server_instance_id: attachment_id.to_owned(),
                terminal_id: self.id.clone(),
                offset: next_offset,
                replay: next_offset < replay_end,
                data,
                history_truncated,
                exited: frame_exited,
                exit_code: if frame_exited { exit_code } else { None },
            };
            next_offset += count as u64;
            if frame_exited {
                final_frame_sent = true;
            }
            store_cursor(
                &mut state,
                attachment_id,
                Cursor {
                    next_offset,
                    credit,
                    history_truncated: false,
                    final_frame_sent,
                    replay_end,
                },
            );
            return OutputPull::Frame(frame);
        }
        if exited && drained && !final_frame_sent {
            let frame = proto::PersistentTerminalOutput {
                server_instance_id: attachment_id.to_owned(),
                terminal_id: self.id.clone(),
                offset: next_offset,
                data: Vec::new(),
                replay: false,
                history_truncated,
                exited: true,
                exit_code,
            };
            store_cursor(
                &mut state,
                attachment_id,
                Cursor {
                    next_offset,
                    credit,
                    history_truncated: false,
                    final_frame_sent: true,
                    replay_end,
                },
            );
            return OutputPull::Frame(frame);
        }
        store_cursor(
            &mut state,
            attachment_id,
            Cursor {
                next_offset,
                credit,
                history_truncated,
                final_frame_sent,
                replay_end,
            },
        );
        OutputPull::WaitForKick
    }
}

fn store_cursor(state: &mut SessionState, attachment_id: &str, cursor: Cursor) {
    state.cursors.insert(attachment_id.to_owned(), cursor);
}

impl Drop for Session {
    fn drop(&mut self) {
        self.input.close();
    }
}

pub struct HostedTerminalService {
    state: Mutex<ServiceState>,
    started: Instant,
    ordinary_grace: Duration,
}

struct ServiceState {
    sessions: HashMap<String, Arc<Session>>,
    by_creation_id: HashMap<String, String>,
}

impl Default for HostedTerminalService {
    fn default() -> Self {
        Self::new()
    }
}

impl HostedTerminalService {
    pub fn new() -> Self {
        Self::with_grace(ORDINARY_SESSION_GRACE)
    }

    pub fn with_grace(ordinary_grace: Duration) -> Self {
        Self {
            state: Mutex::new(ServiceState {
                sessions: HashMap::new(),
                by_creation_id: HashMap::new(),
            }),
            started: Instant::now(),
            ordinary_grace,
        }
    }

    pub fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, ServiceState>, SessionError> {
        self.state.lock().map_err(|_| SessionError::NotFound)
    }

    /// Creating is idempotent by `creation_id` so a retried request cannot spawn
    /// a second PTY. The first attachment id comes from the serving connection.
    pub fn create(&self, request: proto::CreateHostedTerminal) -> Result<String, SessionError> {
        if request.program.is_empty() || request.attachment_id.is_empty() {
            return Err(SessionError::SpawnFailed);
        }
        if request.canonical_root.len() > MAX_ROOT_BYTES {
            return Err(SessionError::InvalidRoot);
        }
        {
            let state = self.state()?;
            if let Some(session_id) = state.by_creation_id.get(&request.creation_id) {
                return Ok(session_id.clone());
            }
            if state.sessions.len() >= MAX_SESSIONS {
                return Err(SessionError::SessionLimit);
            }
        }
        let size = pty_size(
            request.rows,
            request.columns,
            request.pixel_width,
            request.pixel_height,
        )
        .map_err(|_| SessionError::InvalidRoot)?;
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|_| SessionError::SpawnFailed)?;
        let mut command = CommandBuilder::new(&request.program);
        command.args(&request.args);
        if let Some(working_directory) = &request.working_directory {
            command.cwd(working_directory);
        }
        for (name, value) in &request.env {
            command.env(name, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|_| SessionError::SpawnFailed)?;
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|_| SessionError::SpawnFailed)?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|_| SessionError::SpawnFailed)?;

        let id = Uuid::new_v4().to_string();
        let now = self.now();
        let session = Arc::new(Session {
            id: id.clone(),
            canonical_root: request.canonical_root.clone(),
            started_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            input: Arc::new(InputQueue::new()),
            state: Mutex::new(SessionState {
                lifecycle: SessionLifecycle::new(request.attachment_id.clone(), now),
                exited: false,
                exit_code: None,
                output: VecDeque::new(),
                output_start: 0,
                output_end: 0,
                cursors: HashMap::from([(request.attachment_id.clone(), Cursor::new())]),
                kicks: HashMap::new(),
            }),
        });
        spawn_io_threads(session.clone(), reader, writer, child)?;
        let mut state = self.state()?;
        state.by_creation_id.insert(request.creation_id, id.clone());
        state.sessions.insert(id.clone(), session);
        Ok(id)
    }

    pub fn host(
        &self,
        session_id: &str,
        attachment_id: &str,
        name: &str,
    ) -> Result<(), SessionError> {
        let session = self.session(session_id)?;
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        state.lifecycle.host(attachment_id, name)
    }

    /// Mutating a session requires being attached to it; the attachment id is
    /// issued by this daemon and echoed by the serving connection.
    pub fn check_attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<(), SessionError> {
        let session = self.session(session_id)?;
        let state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        if state.lifecycle.has_attachment(attachment_id) {
            Ok(())
        } else {
            Err(SessionError::UnknownAttachment)
        }
    }

    pub fn unhost(&self, session_id: &str, attachment_id: &str) -> Result<(), SessionError> {
        let session = self.session(session_id)?;
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        state.lifecycle.unhost(attachment_id)?;
        state.cursors.retain(|id, _| id == attachment_id);
        state.kicks.retain(|id, kick| {
            if id == attachment_id {
                return true;
            }
            kick.close();
            false
        });
        Ok(())
    }

    pub fn rename_for(&self, session_id: &str, attachment_id: &str) -> Result<(), SessionError> {
        self.check_attachment(session_id, attachment_id)
    }

    pub fn rename(&self, session_id: &str, name: &str) -> Result<(), SessionError> {
        let session = self.session(session_id)?;
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        state.lifecycle.rename(name)
    }

    pub fn attach(
        &self,
        session_id: &str,
        attachment_id: &str,
        canonical_root: &str,
    ) -> Result<proto::AttachedHostedTerminal, SessionError> {
        let session = self.session(session_id)?;
        if session.canonical_root != canonical_root {
            return Err(SessionError::InvalidRoot);
        }
        let now = self.now();
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        state.lifecycle.attach(attachment_id.to_owned(), now)?;
        state
            .cursors
            .insert(attachment_id.to_owned(), Cursor::new());
        Ok(proto::AttachedHostedTerminal {
            attachment_id: attachment_id.to_owned(),
            control_generation: state.lifecycle.control_generation(),
            controls_input: state.lifecycle.controls(attachment_id),
        })
    }

    pub fn subscribe(
        &self,
        session_id: &str,
        attachment_id: &str,
        offset: u64,
    ) -> Result<async_channel::Receiver<()>, SessionError> {
        let session = self.session(session_id)?;
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        if !state.lifecycle.has_attachment(attachment_id) {
            return Err(SessionError::UnknownAttachment);
        }
        let (kick, receiver) = async_channel::bounded(1);
        state.kicks.insert(attachment_id.to_owned(), kick);
        let replay_end = state.output_end;
        if let Some(cursor) = state.cursors.get_mut(attachment_id) {
            cursor.replay_end = replay_end;
            cursor.next_offset = offset;
            cursor.credit = offset;
            cursor.final_frame_sent = false;
        }
        Ok(receiver)
    }

    pub fn take_control(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<proto::HostedTerminalControl, SessionError> {
        let session = self.session(session_id)?;
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        let control_generation = state.lifecycle.take_control(attachment_id)?;
        Ok(proto::HostedTerminalControl {
            control_generation,
            controls_input: state.lifecycle.controls(attachment_id),
        })
    }

    pub fn detach(&self, session_id: &str, attachment_id: &str) -> Result<(), SessionError> {
        let session = self.session(session_id)?;
        let now = self.now();
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        state.lifecycle.detach(attachment_id, now)?;
        state.cursors.remove(attachment_id);
        if let Some(kick) = state.kicks.remove(attachment_id) {
            kick.try_send(()).ok();
        }
        Ok(())
    }

    /// Releases every attachment that a closed connection still held. The
    /// session keeps running; control becomes free for the next attachment, so
    /// a reconnecting client is not left as a read-only viewer.
    pub fn detach_owned(&self, attachments: &[(String, String)]) {
        for (session_id, attachment_id) in attachments {
            if let Err(error) = self.detach(session_id, attachment_id) {
                log::debug!(
                    "hosted terminal attachment {attachment_id} was already released: {error}"
                );
            }
        }
    }

    pub fn input(&self, request: proto::HostedTerminalDaemonInput) -> Result<(), SessionError> {
        if request.data.len() > MAX_INPUT_BYTES {
            return Err(SessionError::InputSequenceGap);
        }
        let session = self.session(&request.session_id)?;
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        let decision = state.lifecycle.check_input(
            &request.attachment_id,
            request.control_generation,
            request.sequence,
        )?;
        if decision == InputDecision::Duplicate {
            return Ok(());
        }
        // Enqueue before committing: a full backlog is retryable and must not
        // consume the sequence number.
        session.input.enqueue(request.data)?;
        state.lifecycle.commit_input(
            &request.attachment_id,
            request.control_generation,
            request.sequence,
        )?;
        Ok(())
    }

    pub fn resize(&self, request: proto::HostedTerminalDaemonResize) -> Result<(), SessionError> {
        let session = self.session(&request.session_id)?;
        {
            let state = session.state.lock().map_err(|_| SessionError::NotFound)?;
            state
                .lifecycle
                .check_control(&request.attachment_id, request.control_generation)?;
        }
        let size = pty_size(
            request.rows,
            request.columns,
            request.pixel_width,
            request.pixel_height,
        )
        .map_err(|_| SessionError::InvalidRoot)?;
        session
            .master
            .lock()
            .map_err(|_| SessionError::NotFound)?
            .resize(size)
            .map_err(|_| SessionError::NotFound)
    }

    pub fn credit(&self, request: proto::HostedTerminalDaemonCredit) -> Result<(), SessionError> {
        let session = self.session(&request.session_id)?;
        let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        let Some(cursor) = state.cursors.get_mut(&request.attachment_id) else {
            return Ok(());
        };
        if request.offset > cursor.credit {
            cursor.credit = request.offset;
        }
        if let Some(kick) = state.kicks.get(&request.attachment_id) {
            kick.try_send(()).ok();
        }
        Ok(())
    }

    fn control_generation(&self, session_id: &str) -> Result<u64, SessionError> {
        let session = self.session(session_id)?;
        let state = session.state.lock().map_err(|_| SessionError::NotFound)?;
        Ok(state.lifecycle.control_generation())
    }

    pub fn pull_output(&self, session_id: &str, attachment_id: &str) -> OutputPull {
        match self.session(session_id) {
            Ok(session) => session.pull_output(attachment_id),
            Err(_) => OutputPull::Superseded,
        }
    }

    pub fn terminate(&self, session_id: &str) -> Result<(), SessionError> {
        let session = self.session(session_id)?;
        {
            let mut state = session.state.lock().map_err(|_| SessionError::NotFound)?;
            if state.lifecycle.is_exited() {
                return Ok(());
            }
            state.lifecycle.mark_exited();
        }
        session.input.close();
        session
            .killer
            .lock()
            .map_err(|_| SessionError::NotFound)?
            .kill()
            .map_err(|_| SessionError::NotFound)?;
        Ok(())
    }

    /// Removing an ended record keeps a running PTY from being orphaned: a live
    /// session must be terminated first.
    pub fn delete(&self, session_id: &str) -> Result<(), SessionError> {
        let session = self.session(session_id)?;
        {
            let state = session.state.lock().map_err(|_| SessionError::NotFound)?;
            if !state.lifecycle.is_exited() {
                return Err(SessionError::SessionRunning);
            }
        }
        let mut state = self.state()?;
        state.sessions.remove(session_id);
        state
            .by_creation_id
            .retain(|_, stored| stored != session_id);
        Ok(())
    }

    pub fn list(&self, canonical_root: &str) -> Vec<proto::HostedTerminalSummary> {
        let Ok(state) = self.state() else {
            return Vec::new();
        };
        let mut sessions: Vec<_> = state
            .sessions
            .values()
            .filter(|session| session.canonical_root == canonical_root)
            .filter_map(|session| {
                let state = session.state.lock().ok()?;
                let lifecycle = &state.lifecycle;
                lifecycle.is_hosted().then(|| proto::HostedTerminalSummary {
                    session_id: session.id.clone(),
                    name: lifecycle.name().to_owned(),
                    running: !lifecycle.is_exited(),
                    exit_code: state.exit_code,
                    controller_label: lifecycle.controller().unwrap_or_default().to_owned(),
                    viewer_count: lifecycle.viewer_count() as u32,
                    started_at_unix_seconds: session.started_at_unix_seconds,
                })
            })
            .collect();
        sessions.sort_by_key(|session| session.started_at_unix_seconds);
        sessions
    }

    /// Drops unhosted sessions that outlived the disconnect grace. Hosted
    /// sessions are never removed here, even with zero attachments.
    pub fn expire_ordinary_sessions(&self) -> usize {
        let now = self.now();
        let grace = self.ordinary_grace;
        let Ok(state) = self.state() else {
            return 0;
        };
        let expired: Vec<(String, Arc<Session>)> = state
            .sessions
            .iter()
            .filter(|(_, session)| {
                session
                    .state
                    .lock()
                    .map(|state| state.lifecycle.should_expire(now, grace))
                    .unwrap_or(false)
            })
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect();
        drop(state);
        for (id, session) in &expired {
            session.input.close();
            if let Ok(mut killer) = session.killer.lock()
                && let Err(error) = killer.kill()
            {
                log::warn!("failed to stop expired hosted terminal: {error:#}");
            }
            if let Ok(mut state) = self.state.lock() {
                state.sessions.remove(id);
                state.by_creation_id.retain(|_, stored| stored != id);
            }
        }
        expired.len()
    }

    #[cfg(test)]
    pub fn hosted_session_count(&self) -> usize {
        let Ok(state) = self.state() else {
            return 0;
        };
        state
            .sessions
            .values()
            .filter(|session| {
                session
                    .state
                    .lock()
                    .map(|state| state.lifecycle.is_hosted())
                    .unwrap_or(false)
            })
            .count()
    }

    #[cfg(test)]
    pub fn session_exists(&self, session_id: &str) -> bool {
        self.session(session_id).is_ok()
    }

    fn session(&self, session_id: &str) -> Result<Arc<Session>, SessionError> {
        self.state()?
            .sessions
            .get(session_id)
            .cloned()
            .ok_or(SessionError::NotFound)
    }
}

fn spawn_io_threads(
    session: Arc<Session>,
    mut reader: Box<dyn std::io::Read + Send>,
    writer: Box<dyn std::io::Write + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) -> Result<(), SessionError> {
    {
        let input = session.input.clone();
        std::thread::Builder::new()
            .name("hosted-terminal-input".into())
            .spawn(move || {
                let mut writer = writer;
                while let Some(data) = input.recv() {
                    if let Err(error) = writer.write_all(&data) {
                        log::warn!("hosted terminal input write failed: {error:#}");
                        input.close();
                        break;
                    }
                }
            })
            .map_err(|_| SessionError::SpawnFailed)?;
    }
    {
        let session = session.clone();
        std::thread::Builder::new()
            .name("hosted-terminal-output".into())
            .spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => session.append_output(&buffer[..count]),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            log::warn!("hosted terminal output read failed: {error:#}");
                            break;
                        }
                    }
                }
            })
            .map_err(|_| SessionError::SpawnFailed)?;
    }
    {
        std::thread::Builder::new()
            .name("hosted-terminal-wait".into())
            .spawn(move || {
                let exit_code = child.wait().ok().map(|status| status.exit_code() as i32);
                session.mark_exited(exit_code);
            })
            .map_err(|_| SessionError::SpawnFailed)?;
    }
    Ok(())
}

fn peer_is_current_user(_stream: &net::async_net::UnixStream) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the descriptor is owned by the stream for the duration of the
        // call and `credentials`/`length` describe valid writable memory.
        let result = unsafe {
            libc::getsockopt(
                _stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut credentials as *mut _ as *mut libc::c_void,
                &mut length,
            )
        };
        if result != 0 {
            return false;
        }
        // SAFETY: geteuid has no preconditions.
        credentials.uid == unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        // The named pipe is created under the current account's private state
        // directory; explicit ACL verification is pending Windows testing.
        true
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("creating hosted terminal directory {path:?}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting hosted terminal directory {path:?}"))?;
    }
    Ok(())
}

fn endpoint_path(state_dir: &Path) -> PathBuf {
    state_dir.join("hosted-terminals-v2.sock")
}

fn daemon_pid_path(state_dir: &Path) -> PathBuf {
    state_dir.join("hosted-terminals-v2.pid")
}

/// Serves the terminal service on its account-private local endpoint. Only the
/// current remote OS account can connect.
pub async fn serve(state_dir: PathBuf) -> Result<()> {
    let (_shutdown_tx, shutdown) = async_channel::bounded(1);
    serve_with_shutdown(state_dir, shutdown).await
}

pub async fn serve_with_shutdown(
    state_dir: PathBuf,
    shutdown: async_channel::Receiver<()>,
) -> Result<()> {
    ensure_private_directory(&state_dir)?;
    std::fs::write(daemon_pid_path(&state_dir), std::process::id().to_string())
        .with_context(|| format!("writing hosted terminal pid file in {state_dir:?}"))?;
    let socket = endpoint_path(&state_dir);
    if socket.exists() {
        let _ = smol::fs::remove_file(&socket).await;
    }
    let listener = net::async_net::UnixListener::bind(&socket)
        .with_context(|| format!("binding hosted terminal endpoint {socket:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting hosted terminal endpoint {socket:?}"))?;
    }
    let service = Arc::new(HostedTerminalService::new());
    {
        let service = service.clone();
        #[allow(
            clippy::disallowed_methods,
            reason = "the daemon is a process-lifetime service loop with no GPUI executor"
        )]
        smol::spawn(async move {
            loop {
                smol::Timer::after(ORDINARY_SESSION_EXPIRY_INTERVAL).await;
                let expired = service.expire_ordinary_sessions();
                if expired > 0 {
                    log::info!("expired {expired} unhosted terminal session(s)");
                }
            }
        })
        .detach();
    }
    log::info!("hosted terminal service listening on {socket:?}");
    loop {
        let accept = Box::pin(listener.accept());
        let stop = Box::pin(shutdown.recv());
        match futures::future::select(accept, stop).await {
            futures::future::Either::Left((accepted, _)) => {
                let (stream, _) = accepted.context("accepting terminal client")?;
                if !peer_is_current_user(&stream) {
                    log::warn!("rejected hosted terminal connection from another account");
                    continue;
                }
                let service = service.clone();
                smol::spawn(async move {
                    if let Err(error) = handle_connection(stream, service).await {
                        log::warn!("hosted terminal connection ended: {error:#}");
                    }
                })
                .detach();
            }
            futures::future::Either::Right(_) => break,
        }
    }
    // The endpoint only ever belongs to this process; removing a stale name must
    // never delete a socket another live daemon is serving.
    let _ = smol::fs::remove_file(&socket).await;
    let _ = std::fs::remove_file(daemon_pid_path(&state_dir));
    Ok(())
}

async fn handle_connection(
    stream: net::async_net::UnixStream,
    service: Arc<HostedTerminalService>,
) -> Result<()> {
    use futures::{AsyncReadExt as _, AsyncWriteExt as _};

    let (mut reader, mut writer) = stream.split();
    let (outbound, outbound_rx) = async_channel::bounded::<Vec<u8>>(128);
    let writer_task = smol::spawn(async move {
        while let Ok(bytes) = outbound_rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut owned_attachments: Vec<(String, String)> = Vec::new();
    let result = read_requests(&mut reader, &service, &outbound, &mut owned_attachments).await;
    // Attachments belong to the connection that created them. Releasing them
    // here hands control back instead of leaving a ghost controller and a
    // leaked attachment slot after every reconnect.
    service.detach_owned(&owned_attachments);
    drop(outbound);
    writer_task.await;
    result
}

async fn read_requests(
    reader: &mut futures::io::ReadHalf<net::async_net::UnixStream>,
    service: &Arc<HostedTerminalService>,
    outbound: &async_channel::Sender<Vec<u8>>,
    owned_attachments: &mut Vec<(String, String)>,
) -> Result<()> {
    use futures::AsyncReadExt as _;

    loop {
        let mut length = [0_u8; 4];
        if reader.read_exact(&mut length).await.is_err() {
            break;
        }
        let length = u32::from_le_bytes(length) as usize;
        anyhow::ensure!(
            length <= 16 * 1024 * 1024,
            "hosted terminal frame too large"
        );
        let mut payload = vec![0_u8; length];
        reader.read_exact(&mut payload).await?;
        let frame = decode_frame(&payload)?;
        let Some(hosted_terminal_frame::Kind::Request(request)) = frame.kind else {
            anyhow::bail!("hosted terminal endpoint received a non-request frame");
        };
        let request_id = frame.request_id;
        let outbound = outbound.clone();
        match request.kind {
            Some(proto::hosted_terminal_daemon_request::Kind::Subscribe(subscribe)) => {
                let kick = service.subscribe(
                    &subscribe.session_id,
                    &subscribe.attachment_id,
                    subscribe.offset,
                );
                let service = service.clone();
                let outbound = outbound.clone();
                let attachment_id = subscribe.attachment_id;
                let session_id = subscribe.session_id;
                match kick {
                    Ok(kick) => {
                        send_response(
                            &outbound,
                            request_id,
                            proto::HostedTerminalDaemonResponse {
                                kind: Some(proto::hosted_terminal_daemon_response::Kind::Ack(
                                    proto::HostedTerminalAck {},
                                )),
                            },
                        )
                        .await;
                        smol::spawn(async move {
                            forward_output(
                                service,
                                session_id,
                                attachment_id,
                                request_id,
                                kick,
                                outbound,
                            )
                            .await;
                        })
                        .detach();
                    }
                    Err(error) => send_error(&outbound, request_id, error).await,
                }
            }
            other => {
                let request = other.clone();
                let response = dispatch(service, other);
                match response {
                    Ok(response) => {
                        if let Some(owned) = owned_attachment(&request, &response) {
                            owned_attachments.push(owned);
                        }
                        send_response(&outbound, request_id, response).await
                    }
                    Err(error) => send_error(&outbound, request_id, error).await,
                }
            }
        }
    }
    Ok(())
}

/// Reports the attachment a request added to this connection, so it can be
/// released when the connection ends. Creation is idempotent, so a retried
/// create may record an attachment that already existed.
fn owned_attachment(
    request: &Option<proto::hosted_terminal_daemon_request::Kind>,
    response: &proto::HostedTerminalDaemonResponse,
) -> Option<(String, String)> {
    use proto::hosted_terminal_daemon_request::Kind as RequestKind;
    use proto::hosted_terminal_daemon_response::Kind as ResponseKind;
    match (request.as_ref()?, response.kind.as_ref()?) {
        (RequestKind::Create(create), ResponseKind::Created(created)) => {
            Some((created.session_id.clone(), create.attachment_id.clone()))
        }
        (RequestKind::Attach(attach), ResponseKind::Attached(attached)) => {
            Some((attach.session_id.clone(), attached.attachment_id.clone()))
        }
        _ => None,
    }
}

async fn forward_output(
    service: Arc<HostedTerminalService>,
    session_id: String,
    attachment_id: String,
    request_id: u64,
    kick: async_channel::Receiver<()>,
    outbound: async_channel::Sender<Vec<u8>>,
) {
    loop {
        match service.pull_output(&session_id, &attachment_id) {
            OutputPull::Frame(output) => {
                let frame = HostedTerminalFrame {
                    request_id,
                    kind: Some(hosted_terminal_frame::Kind::Output(output)),
                };
                if outbound.send(encode_frame(&frame)).await.is_err() {
                    break;
                }
            }
            OutputPull::WaitForKick => {
                if kick.recv().await.is_err() {
                    break;
                }
            }
            OutputPull::Superseded => break,
        }
    }
}

fn dispatch(
    service: &HostedTerminalService,
    kind: Option<proto::hosted_terminal_daemon_request::Kind>,
) -> Result<proto::HostedTerminalDaemonResponse, SessionError> {
    use proto::hosted_terminal_daemon_request::Kind;
    use proto::hosted_terminal_daemon_response::Kind as ResponseKind;

    let ack = proto::HostedTerminalAck {};
    let response = match kind {
        Some(Kind::Create(create)) => ResponseKind::Created(proto::CreatedHostedTerminal {
            session_id: service.create(create)?,
        }),
        Some(Kind::Host(host)) => {
            service.host(&host.terminal_id, &host.attachment_id, &host.name)?;
            ResponseKind::Ack(ack)
        }
        Some(Kind::Unhost(unhost)) => {
            service.unhost(&unhost.terminal_id, &unhost.attachment_id)?;
            ResponseKind::Control(proto::HostedTerminalControl {
                control_generation: service.control_generation(&unhost.terminal_id)?,
                controls_input: true,
            })
        }
        Some(Kind::List(list)) => ResponseKind::Sessions(proto::ListHostedTerminalsResponse {
            sessions: service.list(&list.canonical_root),
        }),
        Some(Kind::Attach(attach)) => {
            let attachment_id = Uuid::new_v4().to_string();
            let attached =
                service.attach(&attach.session_id, &attachment_id, &attach.canonical_root)?;
            ResponseKind::Attached(attached)
        }
        Some(Kind::Detach(detach)) => {
            service.detach(&detach.session_id, &detach.attachment_id)?;
            ResponseKind::Ack(ack)
        }
        Some(Kind::TakeControl(take)) => {
            ResponseKind::Control(service.take_control(&take.session_id, &take.attachment_id)?)
        }
        Some(Kind::Rename(rename)) => {
            {
                service.rename_for(&rename.session_id, &rename.attachment_id)?;
                service.rename(&rename.session_id, &rename.name)?;
            }
            ResponseKind::Ack(ack)
        }
        Some(Kind::Terminate(terminate)) => {
            {
                service.check_attachment(&terminate.session_id, &terminate.attachment_id)?;
                service.terminate(&terminate.session_id)?;
            }
            ResponseKind::Ack(ack)
        }
        Some(Kind::Delete(delete)) => {
            {
                service.check_attachment(&delete.session_id, &delete.attachment_id)?;
                service.delete(&delete.session_id)?;
            }
            ResponseKind::Ack(ack)
        }
        Some(Kind::Input(input)) => {
            service.input(input)?;
            ResponseKind::Ack(ack)
        }
        Some(Kind::Resize(resize)) => {
            service.resize(resize)?;
            ResponseKind::Ack(ack)
        }
        Some(Kind::Credit(credit)) => {
            service.credit(credit)?;
            ResponseKind::Ack(ack)
        }
        Some(Kind::Close(close)) => {
            service.check_attachment(&close.session_id, &close.attachment_id)?;
            service.terminate(&close.session_id)?;
            ResponseKind::Ack(ack)
        }
        Some(Kind::Subscribe(_)) => ResponseKind::Ack(ack),
        None => return Err(SessionError::NotFound),
    };
    Ok(proto::HostedTerminalDaemonResponse {
        kind: Some(response),
    })
}

async fn send_response(
    outbound: &async_channel::Sender<Vec<u8>>,
    request_id: u64,
    response: proto::HostedTerminalDaemonResponse,
) {
    let frame = HostedTerminalFrame {
        request_id,
        kind: Some(hosted_terminal_frame::Kind::Response(response)),
    };
    outbound.send(encode_frame(&frame)).await.ok();
}

async fn send_error(
    outbound: &async_channel::Sender<Vec<u8>>,
    request_id: u64,
    error: SessionError,
) {
    let response = proto::HostedTerminalDaemonResponse {
        kind: Some(proto::hosted_terminal_daemon_response::Kind::Error(
            proto::HostedTerminalDaemonError {
                message: error.to_string(),
            },
        )),
    };
    send_response(outbound, request_id, response).await;
}

fn encode_frame(frame: &HostedTerminalFrame) -> Vec<u8> {
    use prost::Message as _;

    let payload = frame.encode_to_vec();
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend(payload);
    bytes
}

fn decode_frame(payload: &[u8]) -> Result<HostedTerminalFrame> {
    use prost::Message as _;

    Ok(HostedTerminalFrame::decode(payload)?)
}

use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

const DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ClientShared {
    pending: Mutex<HashMap<u64, async_channel::Sender<HostedTerminalFrame>>>,
    subscriptions: Mutex<HashMap<u64, async_channel::Sender<proto::PersistentTerminalOutput>>>,
    next_request_id: AtomicU64,
    disconnected: std::sync::atomic::AtomicBool,
}

/// Client side of the daemon endpoint. One instance belongs to a project server
/// connection scope; the daemon never trusts identity supplied by a client.
/// Wired into project requests together with daemon autostart.
pub struct HostedTerminalClient {
    writer: async_lock::Mutex<futures::io::WriteHalf<net::async_net::UnixStream>>,
    shared: Arc<ClientShared>,
    /// Cancelled on drop, which closes the socket so the daemon can release the
    /// attachments this connection held instead of keeping ghost controllers.
    reader_task: Option<smol::Task<()>>,
}

struct RequestRegistration {
    shared: Arc<ClientShared>,
    request_id: u64,
    subscription: bool,
}

impl Drop for RequestRegistration {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.remove(&self.request_id);
        }
        if self.subscription {
            if let Ok(mut subscriptions) = self.shared.subscriptions.lock() {
                subscriptions.remove(&self.request_id);
            }
        }
    }
}

impl Drop for HostedTerminalClient {
    fn drop(&mut self) {
        drop(self.reader_task.take());
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the daemon link runs outside GPUI, so its request deadlines use the process timer"
)]
impl HostedTerminalClient {
    pub fn is_connected(&self) -> bool {
        !self.shared.disconnected.load(SeqCst)
    }

    /// Connects to the account's terminal service, starting it when absent.
    /// Starting is serialized by the daemon's own endpoint bind, so concurrent
    /// callers end up on one service instead of racing to replace each other.
    pub async fn connect_or_spawn(state_dir: &Path) -> Result<Arc<Self>> {
        if let Ok(client) = Self::connect(state_dir).await {
            return Ok(client);
        }
        spawn_daemon(state_dir)?;
        Self::connect(state_dir).await
    }

    pub async fn connect(state_dir: &Path) -> Result<Arc<Self>> {
        use futures::AsyncReadExt as _;

        ensure_private_directory(state_dir)?;
        let socket = endpoint_path(state_dir);
        let deadline = Instant::now() + DAEMON_CONNECT_TIMEOUT;
        let stream = loop {
            match net::async_net::UnixStream::connect(&socket).await {
                Ok(stream) => break stream,
                Err(error) if Instant::now() >= deadline => {
                    return Err(error).context("connecting to hosted terminal service");
                }
                Err(_) => {
                    smol::Timer::after(Duration::from_millis(50)).await;
                }
            }
        };
        let (reader, writer) = stream.split();
        let shared = Arc::new(ClientShared::default());
        let reader_shared = shared.clone();
        let reader_task = smol::spawn(async move {
            read_client_frames(reader, reader_shared).await;
        });
        Ok(Arc::new(Self {
            writer: async_lock::Mutex::new(writer),
            shared,
            reader_task: Some(reader_task),
        }))
    }

    pub async fn request(
        &self,
        kind: proto::hosted_terminal_daemon_request::Kind,
    ) -> Result<proto::HostedTerminalDaemonResponse> {
        let request_id = self.shared.next_request_id.fetch_add(1, SeqCst);
        let (response_tx, response_rx) = async_channel::bounded(1);
        self.shared
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("hosted terminal client is unavailable"))?
            .insert(request_id, response_tx);
        let _registration = RequestRegistration {
            shared: self.shared.clone(),
            request_id,
            subscription: false,
        };
        self.send(request_id, kind).await?;
        let response = smol::future::or(async { response_rx.recv().await.ok() }, async {
            smol::Timer::after(DAEMON_REQUEST_TIMEOUT).await;
            None
        })
        .await
        .context("hosted terminal service did not answer")?;
        let Some(hosted_terminal_frame::Kind::Response(response)) = response.kind else {
            anyhow::bail!("hosted terminal service sent an unexpected frame");
        };
        match response.kind {
            Some(proto::hosted_terminal_daemon_response::Kind::Error(error)) => {
                anyhow::bail!("{}", error.message)
            }
            Some(kind) => Ok(proto::HostedTerminalDaemonResponse { kind: Some(kind) }),
            None => anyhow::bail!("hosted terminal service sent an empty response"),
        }
    }

    /// Subscribes to one attachment's output stream. Frames keep their cursor
    /// offsets so the caller can resume from the last delivered offset.
    pub async fn subscribe(
        &self,
        session_id: &str,
        attachment_id: &str,
        offset: u64,
    ) -> Result<async_channel::Receiver<proto::PersistentTerminalOutput>> {
        let request_id = self.shared.next_request_id.fetch_add(1, SeqCst);
        let (outputs, receiver) = async_channel::bounded(64);
        {
            let mut subscriptions = self
                .shared
                .subscriptions
                .lock()
                .map_err(|_| anyhow::anyhow!("hosted terminal client is unavailable"))?;
            subscriptions.retain(|_, sender| !sender.is_closed());
            anyhow::ensure!(
                subscriptions.len() < 1024,
                "too many hosted terminal subscriptions"
            );
            subscriptions.insert(request_id, outputs);
        }
        let (response_tx, response_rx) = async_channel::bounded(1);
        self.shared
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("hosted terminal client is unavailable"))?
            .insert(request_id, response_tx);
        let mut registration = RequestRegistration {
            shared: self.shared.clone(),
            request_id,
            subscription: true,
        };
        self.send(
            request_id,
            proto::hosted_terminal_daemon_request::Kind::Subscribe(
                proto::SubscribeHostedTerminal {
                    session_id: session_id.to_owned(),
                    attachment_id: attachment_id.to_owned(),
                    offset,
                },
            ),
        )
        .await?;
        let response = smol::future::or(async { response_rx.recv().await.ok() }, async {
            smol::Timer::after(DAEMON_REQUEST_TIMEOUT).await;
            None
        })
        .await
        .context("hosted terminal service did not acknowledge the subscription")?;
        let Some(hosted_terminal_frame::Kind::Response(response)) = response.kind else {
            anyhow::bail!("hosted terminal service sent an unexpected frame");
        };
        if let Some(proto::hosted_terminal_daemon_response::Kind::Error(error)) = response.kind {
            anyhow::bail!("{}", error.message);
        }
        registration.subscription = false;
        Ok(receiver)
    }

    async fn send(
        &self,
        request_id: u64,
        kind: proto::hosted_terminal_daemon_request::Kind,
    ) -> Result<()> {
        use futures::AsyncWriteExt as _;

        let frame = HostedTerminalFrame {
            request_id,
            kind: Some(hosted_terminal_frame::Kind::Request(
                proto::HostedTerminalDaemonRequest { kind: Some(kind) },
            )),
        };
        self.writer
            .lock()
            .await
            .write_all(&encode_frame(&frame))
            .await
            .context("writing to hosted terminal service")
    }
}

async fn read_client_frames(
    mut reader: futures::io::ReadHalf<net::async_net::UnixStream>,
    shared: Arc<ClientShared>,
) {
    use futures::AsyncReadExt as _;

    loop {
        let mut length = [0_u8; 4];
        if reader.read_exact(&mut length).await.is_err() {
            break;
        }
        let length = u32::from_le_bytes(length) as usize;
        if length > 16 * 1024 * 1024 {
            break;
        }
        let mut payload = vec![0_u8; length];
        if reader.read_exact(&mut payload).await.is_err() {
            break;
        }
        let Ok(frame) = decode_frame(&payload) else {
            break;
        };
        match frame.kind {
            Some(hosted_terminal_frame::Kind::Output(output)) => {
                let sender = shared
                    .subscriptions
                    .lock()
                    .ok()
                    .and_then(|subscriptions| subscriptions.get(&frame.request_id).cloned());
                if let Some(sender) = sender {
                    if sender.try_send(output).is_err() {
                        // Isolate a stalled consumer. Its next subscription resumes
                        // from its acknowledged offset instead of blocking every RPC.
                        if let Ok(mut subscriptions) = shared.subscriptions.lock() {
                            subscriptions.remove(&frame.request_id);
                        }
                    }
                }
            }
            Some(hosted_terminal_frame::Kind::Response(_)) => {
                let pending = shared
                    .pending
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.remove(&frame.request_id));
                if let Some(pending) = pending {
                    pending.send(frame).await.ok();
                }
            }
            Some(hosted_terminal_frame::Kind::Request(_)) | None => break,
        }
    }
    shared.disconnected.store(true, SeqCst);
    if let Ok(mut pending) = shared.pending.lock() {
        pending.clear();
    }
    if let Ok(mut subscriptions) = shared.subscriptions.lock() {
        subscriptions.clear();
    }
}

fn daemon_is_running(state_dir: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(daemon_pid_path(state_dir)) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return false;
    };
    process_is_alive(pid)
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes whether the pid exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Windows process probing is verified together with the named-pipe ACL work.
    false
}

/// Starts the colocated terminal service as an independent process so it
/// outlives the project connection that asked for it. The daemon writes its own
/// pid file, so a caller can never fake liveness.
#[allow(
    clippy::disallowed_methods,
    reason = "the daemon must be a detached OS process with its own stdio, not a smol future"
)]
pub fn spawn_daemon(state_dir: &Path) -> Result<()> {
    use std::process::Stdio;

    if daemon_is_running(state_dir) {
        return Ok(());
    }
    ensure_private_directory(state_dir)?;
    let binary = std::env::current_exe().context("locating the remote server executable")?;
    let mut command = std::process::Command::new(binary);
    command
        .arg("hosted-terminal-daemon")
        .arg("--state-dir")
        .arg(state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: setsid is async-signal-safe and runs between fork and exec,
        // where only async-signal-safe calls are permitted.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS (0x8) and CREATE_NO_WINDOW (0x0800_0000) keep the
        // service independent of the SSH session that started it.
        command.creation_flags(0x8 | 0x0800_0000);
    }
    command
        .spawn()
        .with_context(|| format!("starting hosted terminal service for {state_dir:?}"))?;
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "process-level daemon tests wait on real PTY output and socket readiness"
)]
mod tests {
    use super::*;

    fn session() -> SessionLifecycle {
        SessionLifecycle::new("first".into(), Duration::ZERO)
    }

    #[cfg(unix)]
    #[test]
    fn stalled_output_subscription_does_not_block_control_responses() {
        smol::block_on(async {
            use futures::AsyncWriteExt as _;
            let (reader, mut writer) = net::async_net::UnixStream::pair().expect("socket pair");
            let (reader, _) = futures::AsyncReadExt::split(reader);
            let shared = Arc::new(ClientShared::default());
            let (outputs, _slow_consumer) = async_channel::bounded(1);
            shared
                .subscriptions
                .lock()
                .expect("subscriptions")
                .insert(1, outputs);
            let (response, received) = async_channel::bounded(1);
            shared.pending.lock().expect("pending").insert(2, response);
            let reader = smol::spawn(read_client_frames(reader, shared));
            for _ in 0..2 {
                writer
                    .write_all(&encode_frame(&HostedTerminalFrame {
                        request_id: 1,
                        kind: Some(hosted_terminal_frame::Kind::Output(
                            proto::PersistentTerminalOutput::default(),
                        )),
                    }))
                    .await
                    .expect("output");
            }
            writer
                .write_all(&encode_frame(&HostedTerminalFrame {
                    request_id: 2,
                    kind: Some(hosted_terminal_frame::Kind::Response(
                        proto::HostedTerminalDaemonResponse::default(),
                    )),
                }))
                .await
                .expect("response");
            let received = smol::future::or(async { received.recv().await.is_ok() }, async {
                smol::Timer::after(Duration::from_secs(2)).await;
                false
            })
            .await;
            assert!(
                received,
                "slow terminal must not block another terminal's acknowledgements"
            );
            drop(reader);
        });
    }

    #[test]
    fn unhosting_preserves_the_live_attachment_and_revokes_other_viewers() {
        let mut lifecycle = session();
        lifecycle.host("first", "shell").expect("host");
        lifecycle
            .attach("viewer".into(), Duration::ZERO)
            .expect("attach");
        lifecycle.unhost("viewer").expect("unhost");
        assert!(!lifecycle.is_hosted());
        assert!(!lifecycle.is_exited());
        assert!(lifecycle.controls("viewer"));
        assert!(!lifecycle.has_attachment("first"));
        lifecycle
            .check_input("viewer", lifecycle.control_generation(), 1)
            .expect("input still works");
        lifecycle.unhost("viewer").expect("idempotent cancellation");
        assert_eq!(
            lifecycle.attach("new".into(), Duration::ZERO),
            Err(SessionError::NotHosted)
        );
    }

    #[cfg(unix)]
    #[test]
    fn subscription_restart_replays_the_requested_offset_without_changing_bytes() {
        let service = HostedTerminalService::new();
        let id = create_session(&service, "replay", "/bin/sh", &["-c", "sleep 30"], "owner");
        let session = service.session(&id).expect("session");
        let bytes = b"abc\x1b]52;c;aGVsbG8=\x07def";
        session.append_output(bytes);
        let _kick = service.subscribe(&id, "owner", 0).expect("subscribe");
        let OutputPull::Frame(first) = service.pull_output(&id, "owner") else {
            panic!("output");
        };
        assert_eq!(first.data, bytes);
        assert!(first.replay);
        let _kick = service.subscribe(&id, "owner", 3).expect("resume");
        let OutputPull::Frame(replayed) = service.pull_output(&id, "owner") else {
            panic!("replay");
        };
        assert_eq!(replayed.offset, 3);
        assert_eq!(replayed.data, &bytes[3..]);
        service.terminate(&id).expect("cleanup");
    }

    #[test]
    fn only_explicitly_hosted_sessions_survive_the_disconnect_grace() {
        let mut ordinary = session();
        ordinary.detach("first", Duration::ZERO).unwrap();
        let grace = Duration::from_secs(1800);
        assert!(!ordinary.should_expire(Duration::from_secs(1799), grace));
        assert!(ordinary.should_expire(grace, grace));
        let mut hosted = session();
        hosted.host("first", "agent").unwrap();
        hosted.detach("first", Duration::ZERO).unwrap();
        assert!(!hosted.should_expire(Duration::from_secs(86400), grace));
        hosted.attach("second".into(), grace).unwrap();
        assert_eq!(hosted.name(), "agent");
    }

    #[test]
    fn control_moves_to_the_next_attachment_when_the_controller_detaches() {
        let mut state = session();
        state.host("first", "agent").unwrap();
        state.attach("viewer".into(), Duration::ZERO).unwrap();
        assert!(!state.controls("viewer"));
        state.detach("first", Duration::ZERO).unwrap();
        assert_eq!(state.controller(), None);
        state.attach("second".into(), Duration::ZERO).unwrap();
        assert!(state.controls("second"));
        assert_eq!(
            state.check_input("second", state.control_generation(), 1),
            Ok(InputDecision::Accept)
        );
    }

    #[test]
    fn new_viewer_is_read_only_and_takeover_rejects_stale_input_and_resize() {
        let mut state = session();
        state.host("first", "agent").unwrap();
        state.attach("second".into(), Duration::ZERO).unwrap();
        assert_eq!(
            state.check_control("second", 1),
            Err(SessionError::NotController)
        );
        let generation = state.take_control("second").unwrap();
        assert_eq!(state.take_control("second"), Ok(generation));
        assert_eq!(
            state.check_input("first", 1, 1),
            Err(SessionError::StaleControl)
        );
        assert_eq!(
            state.check_control("first", 1),
            Err(SessionError::StaleControl)
        );
        assert_eq!(
            state.commit_input("second", generation, 1),
            Ok(InputDecision::Accept)
        );
        let next_generation = state.take_control("first").unwrap();
        assert_eq!(
            state.check_control("second", generation),
            Err(SessionError::StaleControl)
        );
        assert_eq!(
            state.commit_input("first", next_generation, 1),
            Ok(InputDecision::Accept)
        );
    }

    #[test]
    fn attaching_while_another_attachment_controls_stays_read_only() {
        let mut state = session();
        state.host("first", "agent").unwrap();
        state.attach("viewer".into(), Duration::ZERO).unwrap();
        assert!(state.controls("first"));
        assert!(!state.controls("viewer"));
        assert_eq!(
            state.check_input("viewer", state.control_generation(), 1),
            Err(SessionError::NotController)
        );
    }

    #[test]
    fn backpressure_and_replay_do_not_lose_or_duplicate_input() {
        let mut state = session();
        assert_eq!(state.check_input("first", 1, 1), Ok(InputDecision::Accept));
        assert_eq!(state.check_input("first", 1, 1), Ok(InputDecision::Accept));
        assert_eq!(state.commit_input("first", 1, 1), Ok(InputDecision::Accept));
        assert_eq!(
            state.commit_input("first", 1, 1),
            Ok(InputDecision::Duplicate)
        );
        assert_eq!(
            state.commit_input("first", 1, 3),
            Err(SessionError::InputSequenceGap)
        );
        assert_eq!(state.commit_input("first", 1, 2), Ok(InputDecision::Accept));
    }

    #[test]
    fn ordinary_sessions_cannot_be_attached_by_other_clients() {
        let mut state = session();
        assert_eq!(
            state.attach("second".into(), Duration::ZERO),
            Err(SessionError::NotHosted)
        );
        assert_eq!(
            state.host("unknown", "agent"),
            Err(SessionError::UnknownAttachment)
        );
        assert!(!state.is_hosted());
    }

    #[test]
    fn invalid_names_do_not_change_hosting_state() {
        let mut state = session();
        for name in ["", " \n", "bad\x1b[31m"] {
            assert_eq!(state.host("first", name), Err(SessionError::InvalidName));
            assert!(!state.is_hosted());
        }
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(state.host("first", &long), Err(SessionError::InvalidName));
        state.host("first", "  代码审查  ").unwrap();
        assert_eq!(state.name(), "代码审查");
    }

    #[test]
    fn attachments_are_bounded_and_duplicate_ids_do_not_replace_state() {
        let mut state = session();
        state.host("first", "agent").unwrap();
        assert_eq!(
            state.attach("first".into(), Duration::ZERO),
            Err(SessionError::AttachmentExists)
        );
        for index in 1..MAX_ATTACHMENTS {
            state
                .attach(format!("viewer-{index}"), Duration::ZERO)
                .unwrap();
        }
        assert_eq!(
            state.attach("overflow".into(), Duration::ZERO),
            Err(SessionError::AttachmentLimit)
        );
        state.detach("viewer-1", Duration::ZERO).unwrap();
        state.attach("replacement".into(), Duration::ZERO).unwrap();
    }

    #[test]
    fn ended_sessions_allow_viewing_but_never_input() {
        let mut state = session();
        state.host("first", "agent").unwrap();
        state.mark_exited();
        state.attach("viewer".into(), Duration::ZERO).unwrap();
        assert_eq!(
            state.take_control("viewer"),
            Err(SessionError::AlreadyExited)
        );
        assert_eq!(
            state.check_input("first", 2, 1),
            Err(SessionError::AlreadyExited)
        );
    }

    #[test]
    fn detach_revokes_control_and_clock_errors_leave_attachment_intact() {
        let mut state = session();
        state.host("first", "agent").unwrap();
        state
            .attach("viewer".into(), Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            state.detach("first", Duration::ZERO),
            Err(SessionError::ClockWentBackwards)
        );
        assert!(state.check_control("first", 1).is_ok());
        state.detach("first", Duration::from_secs(5)).unwrap();
        assert_eq!(
            state.check_control("first", 1),
            Err(SessionError::UnknownAttachment)
        );
        assert!(state.take_control("viewer").is_ok());
    }

    #[cfg(unix)]
    fn create_session(
        service: &HostedTerminalService,
        creation_id: &str,
        program: &str,
        args: &[&str],
        attachment_id: &str,
    ) -> String {
        service
            .create(proto::CreateHostedTerminal {
                creation_id: creation_id.into(),
                program: program.into(),
                args: args.iter().map(|arg| arg.to_string()).collect(),
                env: std::collections::HashMap::default(),
                working_directory: None,
                rows: 24,
                columns: 80,
                pixel_width: 0,
                pixel_height: 0,
                canonical_root: "/home/user/project".into(),
                attachment_id: attachment_id.into(),
            })
            .expect("create hosted terminal")
    }

    #[cfg(unix)]
    fn drain(service: &HostedTerminalService, session_id: &str, attachment_id: &str) -> String {
        let mut collected = Vec::new();
        for _ in 0..50 {
            match service.pull_output(session_id, attachment_id) {
                OutputPull::Frame(frame) => {
                    assert_eq!(frame.terminal_id, session_id);
                    assert_eq!(frame.server_instance_id, attachment_id);
                    collected.extend(frame.data);
                }
                OutputPull::WaitForKick => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                OutputPull::Superseded => break,
            }
        }
        String::from_utf8_lossy(&collected).into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn hosted_session_survives_detach_and_a_late_viewer_replays_retained_output() {
        let service = HostedTerminalService::new();
        let session_id = create_session(
            &service,
            "creation",
            "/bin/sh",
            &["-c", "printf ready; sleep 30"],
            "first",
        );
        service.host(&session_id, "first", "agent").unwrap();
        assert!(drain(&service, &session_id, "first").contains("ready"));
        service.detach(&session_id, "first").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(service.session_exists(&session_id));
        let attached = service
            .attach(&session_id, "second", "/home/user/project")
            .unwrap();
        // Control was free after the previous controller detached, so the late
        // attachment takes it instead of staying read-only forever.
        assert!(attached.controls_input);
        let replay = drain(&service, &session_id, "second");
        assert!(replay.contains("ready"));
        assert_eq!(service.list("/home/user/project").len(), 1);
        assert!(service.list("/home/user/other").is_empty());
        service.terminate(&session_id).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_sessions_expire_after_the_disconnect_grace_but_hosted_ones_do_not() {
        let service = HostedTerminalService::with_grace(Duration::from_millis(20));
        let ordinary = create_session(
            &service,
            "ordinary",
            "/bin/sh",
            &["-c", "sleep 30"],
            "first",
        );
        service.detach(&ordinary, "first").unwrap();
        let hosted = create_session(&service, "hosted", "/bin/sh", &["-c", "sleep 30"], "owner");
        service.host(&hosted, "owner", "agent").unwrap();
        service.detach(&hosted, "owner").unwrap();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(service.expire_ordinary_sessions(), 1);
        assert!(!service.session_exists(&ordinary));
        assert!(service.session_exists(&hosted));
        assert_eq!(service.hosted_session_count(), 1);
        service.terminate(&hosted).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn attaching_requires_the_session_root_and_takeover_revokes_the_old_controller() {
        let service = HostedTerminalService::new();
        let session_id = create_session(&service, "creation", "/bin/cat", &[], "first");
        service.host(&session_id, "first", "agent").unwrap();
        assert_eq!(
            service.attach(&session_id, "second", "/home/user/other"),
            Err(SessionError::InvalidRoot)
        );
        service
            .attach(&session_id, "second", "/home/user/project")
            .unwrap();
        let control = service.take_control(&session_id, "second").unwrap();
        assert!(control.controls_input);
        let stale = proto::HostedTerminalDaemonInput {
            session_id: session_id.clone(),
            attachment_id: "first".into(),
            control_generation: 1,
            sequence: 1,
            data: b"x".to_vec(),
        };
        assert_eq!(service.input(stale), Err(SessionError::StaleControl));
        let accepted = proto::HostedTerminalDaemonInput {
            session_id: session_id.clone(),
            attachment_id: "second".into(),
            control_generation: control.control_generation,
            sequence: 1,
            data: b"y".to_vec(),
        };
        assert_eq!(service.input(accepted.clone()), Ok(()));
        assert_eq!(service.input(accepted), Ok(()));
        service.terminate(&session_id).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn terminated_sessions_stay_listed_and_cannot_be_deleted_while_running() {
        let service = HostedTerminalService::new();
        let session_id = create_session(
            &service,
            "creation",
            "/bin/sh",
            &["-c", "sleep 30"],
            "first",
        );
        service.host(&session_id, "first", "agent").unwrap();
        assert_eq!(
            service.delete(&session_id),
            Err(SessionError::SessionRunning)
        );
        service.terminate(&session_id).unwrap();
        let listed = service.list("/home/user/project");
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].running);
        service.delete(&session_id).unwrap();
        assert!(!service.session_exists(&session_id));
        assert!(service.list("/home/user/project").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn creation_is_idempotent_and_bounded() {
        let service = HostedTerminalService::new();
        let first = create_session(&service, "same", "/bin/sh", &["-c", "sleep 30"], "owner");
        let second = create_session(&service, "same", "/bin/sh", &["-c", "sleep 30"], "owner");
        assert_eq!(first, second);
        service.terminate(&first).unwrap();
    }

    #[test]
    fn daemon_frames_round_trip_with_request_identity() {
        let frame = HostedTerminalFrame {
            request_id: 7,
            kind: Some(hosted_terminal_frame::Kind::Request(
                proto::HostedTerminalDaemonRequest {
                    kind: Some(proto::hosted_terminal_daemon_request::Kind::List(
                        proto::ListHostedTerminals {
                            canonical_root: "/home/user/project".into(),
                        },
                    )),
                },
            )),
        };
        let bytes = encode_frame(&frame);
        let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(length, bytes.len() - 4);
        assert_eq!(decode_frame(&bytes[4..]).unwrap(), frame);
    }

    #[test]
    fn client_and_daemon_round_trip_over_the_private_socket() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = directory.path().join("hosted-terminals");
        let serve_dir = state_dir.clone();
        let (shutdown_tx, shutdown) = async_channel::bounded(1);
        std::thread::spawn(move || {
            smol::block_on(serve_with_shutdown(serve_dir, shutdown)).expect("serve daemon");
        });
        let client = smol::block_on(async {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(client) = HostedTerminalClient::connect(&state_dir).await {
                    break client;
                }
                assert!(Instant::now() < deadline, "daemon endpoint did not appear");
                smol::Timer::after(Duration::from_millis(20)).await;
            }
        });
        let session_id = smol::block_on(async {
            let response = client
                .request(proto::hosted_terminal_daemon_request::Kind::Create(
                    proto::CreateHostedTerminal {
                        creation_id: "socket".into(),
                        program: "/bin/sh".into(),
                        args: vec!["-c".into(), "printf socket-ready; sleep 30".into()],
                        env: std::collections::HashMap::default(),
                        working_directory: None,
                        rows: 24,
                        columns: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                        canonical_root: "/home/user/project".into(),
                        attachment_id: "owner".into(),
                    },
                ))
                .await
                .expect("create over socket");
            match response.kind {
                Some(proto::hosted_terminal_daemon_response::Kind::Created(created)) => {
                    created.session_id
                }
                other => panic!("unexpected create response: {other:?}"),
            }
        });
        smol::block_on(async {
            client
                .request(proto::hosted_terminal_daemon_request::Kind::Host(
                    proto::HostTerminal {
                        server_instance_id: String::new(),
                        terminal_id: session_id.clone(),
                        name: "socket agent".into(),
                        attachment_id: "owner".into(),
                    },
                ))
                .await
                .expect("host over socket");
            let listed = client
                .request(proto::hosted_terminal_daemon_request::Kind::List(
                    proto::ListHostedTerminals {
                        canonical_root: "/home/user/project".into(),
                    },
                ))
                .await
                .expect("list over socket");
            match listed.kind {
                Some(proto::hosted_terminal_daemon_response::Kind::Sessions(sessions)) => {
                    assert_eq!(sessions.sessions.len(), 1);
                    assert_eq!(sessions.sessions[0].name, "socket agent");
                }
                other => panic!("unexpected list response: {other:?}"),
            }
            let receiver = client
                .subscribe(&session_id, "owner", 0)
                .await
                .expect("subscribe over socket");
            let frame = smol::future::or(async { receiver.recv().await.ok() }, async {
                smol::Timer::after(Duration::from_secs(5)).await;
                None
            })
            .await
            .expect("output frame");
            assert!(String::from_utf8_lossy(&frame.data).contains("socket-ready"));
        });
        shutdown_tx.close();
        let deadline = Instant::now() + Duration::from_secs(5);
        while endpoint_path(&state_dir).exists() {
            assert!(
                Instant::now() < deadline,
                "daemon did not release its endpoint"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
