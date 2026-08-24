use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[cfg(windows)]
use std::env;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;

use crate::{Result, workspace::AgentId};

pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 40;
pub const MAX_DIMENSION: u16 = 500;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

const READER_CHUNK_BYTES: usize = 4096;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const MAX_HISTORY_EVENTS: usize = 32 * 1024;
const MAX_READ_BYTES: usize = 384 * 1024;
const MAX_READ_EVENTS: usize = 2048;
const SCREEN_SCROLLBACK_ROWS: usize = 10_000;
#[cfg(windows)]
const WINDOWS_INITIAL_CURSOR_RESPONSE: &[u8] = b"\x1b[1;1R";

type SessionTerminalWriter = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

#[derive(Clone, Debug)]
struct ShellSpec {
    program: PathBuf,
    display: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionTerminalEventPayload {
    Output { data: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SessionTerminalRead {
    pub shell: Option<String>,
    pub cwd: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub drained: bool,
    pub reset: bool,
    pub cursor: u64,
    pub tail: u64,
    pub cols: u16,
    pub rows: u16,
    pub events: Vec<SessionTerminalEventPayload>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SessionTerminalOperation {
    pub found: bool,
    pub accepted: bool,
    pub state: String,
    pub error: Option<String>,
}

impl SessionTerminalOperation {
    fn missing() -> Self {
        Self {
            found: false,
            accepted: false,
            state: "missing".into(),
            error: Some("session terminal does not exist".into()),
        }
    }

    fn accepted() -> Self {
        Self {
            found: true,
            accepted: true,
            state: "running".into(),
            error: None,
        }
    }

    fn rejected(state: &SessionTerminalStatus) -> Self {
        Self {
            found: true,
            accepted: false,
            state: state.state.clone(),
            error: state
                .error
                .clone()
                .or_else(|| Some("session terminal is not running".into())),
        }
    }
}

#[derive(Clone, Debug)]
struct SessionTerminalStatus {
    state: String,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl SessionTerminalStatus {
    fn running() -> Self {
        Self {
            state: "running".into(),
            exit_code: None,
            error: None,
        }
    }
}

pub struct SessionTerminalRegistry {
    workspace: PathBuf,
    shell: std::result::Result<ShellSpec, String>,
    terminals: Mutex<BTreeMap<AgentId, SessionTerminalSlot>>,
}

impl SessionTerminalRegistry {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace.as_ref())?;
        if !workspace.is_dir() {
            return Err("SessionTerminal workspace must be a directory".into());
        }
        Ok(Self {
            workspace,
            shell: detect_shell().map_err(|error| error.to_string()),
            terminals: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn reconcile(&self, agent_ids: Vec<AgentId>) -> Result<()> {
        let expected = agent_ids.into_iter().collect::<BTreeSet<_>>();
        let mut terminals = self
            .terminals
            .lock()
            .map_err(|_| "SessionTerminal registry lock is poisoned")?;
        terminals.retain(|agent_id, _| expected.contains(agent_id));
        for terminal in terminals.values_mut() {
            terminal.refresh();
        }
        for agent_id in expected {
            if terminals.contains_key(&agent_id) {
                continue;
            }
            terminals.insert(
                agent_id,
                SessionTerminalSlot::create(&self.workspace, self.shell.as_ref()),
            );
        }
        Ok(())
    }

    pub fn read(
        &self,
        agent_id: &AgentId,
        cursor: Option<u64>,
    ) -> Result<Option<SessionTerminalRead>> {
        let mut terminals = self
            .terminals
            .lock()
            .map_err(|_| "SessionTerminal registry lock is poisoned")?;
        let Some(terminal) = terminals.get_mut(agent_id) else {
            return Ok(None);
        };
        terminal.read(cursor).map(Some)
    }

    pub fn input(&self, agent_id: &AgentId, data: &[u8]) -> Result<SessionTerminalOperation> {
        if data.len() > MAX_INPUT_BYTES {
            return Err("SessionTerminal input is too large".into());
        }
        let mut terminals = self
            .terminals
            .lock()
            .map_err(|_| "SessionTerminal registry lock is poisoned")?;
        let Some(terminal) = terminals.get_mut(agent_id) else {
            return Ok(SessionTerminalOperation::missing());
        };
        terminal.input(data)
    }

    pub fn resize(
        &self,
        agent_id: &AgentId,
        cols: u16,
        rows: u16,
    ) -> Result<SessionTerminalOperation> {
        validate_size(cols, rows)?;
        let mut terminals = self
            .terminals
            .lock()
            .map_err(|_| "SessionTerminal registry lock is poisoned")?;
        let Some(terminal) = terminals.get_mut(agent_id) else {
            return Ok(SessionTerminalOperation::missing());
        };
        terminal.resize(cols, rows)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.terminals.lock().unwrap().len()
    }
}

impl Drop for SessionTerminalRegistry {
    fn drop(&mut self) {
        if let Ok(terminals) = self.terminals.get_mut() {
            terminals.clear();
        }
    }
}

enum SessionTerminalSlot {
    Live(SessionTerminal),
    Unavailable {
        cwd: String,
        shell: Option<String>,
        error: String,
    },
}

impl SessionTerminalSlot {
    fn create(workspace: &Path, shell: std::result::Result<&ShellSpec, &String>) -> Self {
        let cwd = crate::host_path::public_host_path(workspace);
        let shell = match shell {
            Ok(shell) => shell,
            Err(error) => {
                return Self::Unavailable {
                    cwd,
                    shell: None,
                    error: error.clone(),
                };
            }
        };
        match SessionTerminal::spawn(workspace, shell) {
            Ok(terminal) => Self::Live(terminal),
            Err(error) => Self::Unavailable {
                cwd,
                shell: Some(shell.display.clone()),
                error: format!("failed to start session terminal: {error}"),
            },
        }
    }

    fn refresh(&mut self) {
        if let Self::Live(terminal) = self
            && let Err(error) = terminal.refresh_status()
        {
            terminal.lose(format!(
                "failed to refresh session terminal status: {error}"
            ));
        }
    }

    fn read(&mut self, cursor: Option<u64>) -> Result<SessionTerminalRead> {
        match self {
            Self::Live(terminal) => terminal.read(cursor),
            Self::Unavailable { cwd, shell, error } => Ok(SessionTerminalRead {
                shell: shell.clone(),
                cwd: cwd.clone(),
                state: "unavailable".into(),
                exit_code: None,
                error: Some(error.clone()),
                drained: true,
                reset: cursor.is_some_and(|cursor| cursor != 0),
                cursor: 0,
                tail: 0,
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                events: Vec::new(),
            }),
        }
    }

    fn input(&mut self, data: &[u8]) -> Result<SessionTerminalOperation> {
        match self {
            Self::Live(terminal) => terminal.input(data),
            Self::Unavailable { error, .. } => Ok(SessionTerminalOperation {
                found: true,
                accepted: false,
                state: "unavailable".into(),
                error: Some(error.clone()),
            }),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<SessionTerminalOperation> {
        match self {
            Self::Live(terminal) => terminal.resize(cols, rows),
            Self::Unavailable { error, .. } => Ok(SessionTerminalOperation {
                found: true,
                accepted: false,
                state: "unavailable".into(),
                error: Some(error.clone()),
            }),
        }
    }
}

struct SessionTerminal {
    shell: String,
    cwd: String,
    writer: SessionTerminalWriter,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<SessionTerminalOutput>,
    reader_discard: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    status: SessionTerminalStatus,
}

impl SessionTerminal {
    fn spawn(workspace: &Path, shell: &ShellSpec) -> Result<Self> {
        let pair = native_pty_system().openpty(pty_size(DEFAULT_COLS, DEFAULT_ROWS))?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        #[cfg(windows)]
        let writer = {
            let mut writer = writer;
            // ConPTY requests the inherited cursor position before PowerShell
            // finishes initializing; answer it through the PTY input stream.
            writer.write_all(WINDOWS_INITIAL_CURSOR_RESPONSE)?;
            writer.flush()?;
            writer
        };

        let writer = Arc::new(Mutex::new(Some(writer)));

        let mut command = CommandBuilder::new(&shell.program);
        #[cfg(windows)]
        command.arg("-NoLogo");
        command.cwd(workspace);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        #[cfg(unix)]
        command.env("SHELL", &shell.program);
        let mut child = pair.slave.spawn_command(command)?;
        drop(pair.slave);

        let output = Arc::new(SessionTerminalOutput::new(DEFAULT_ROWS, DEFAULT_COLS));
        let reader_discard = match spawn_reader(reader, Arc::clone(&output), Arc::clone(&writer)) {
            Ok(discard) => discard,
            Err(error) => {
                let _ = child.kill();
                return Err(error.into());
            }
        };
        Ok(Self {
            shell: shell.display.clone(),
            cwd: crate::host_path::public_host_path(workspace),
            writer,
            child,
            output,
            reader_discard,
            master: pair.master,
            status: SessionTerminalStatus::running(),
        })
    }

    fn read(&mut self, cursor: Option<u64>) -> Result<SessionTerminalRead> {
        self.refresh_status()?;
        let batch = self.output.read(cursor)?;
        Ok(SessionTerminalRead {
            shell: Some(self.shell.clone()),
            cwd: self.cwd.clone(),
            state: self.status.state.clone(),
            exit_code: self.status.exit_code,
            error: self.status.error.clone().or(batch.reader_error),
            drained: batch.drained,
            reset: batch.reset,
            cursor: batch.cursor,
            tail: batch.tail,
            cols: batch.cols,
            rows: batch.rows,
            events: batch.events,
        })
    }

    fn input(&mut self, data: &[u8]) -> Result<SessionTerminalOperation> {
        self.refresh_status()?;
        if self.status.state != "running" {
            return Ok(SessionTerminalOperation::rejected(&self.status));
        }
        if data.is_empty() {
            return Ok(SessionTerminalOperation::accepted());
        }
        if let Err(error) = write_session_terminal(&self.writer, data) {
            self.lose(format!("PTY write failed: {error}"));
            return Ok(SessionTerminalOperation::rejected(&self.status));
        }
        Ok(SessionTerminalOperation::accepted())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<SessionTerminalOperation> {
        self.refresh_status()?;
        if self.status.state != "running" {
            return Ok(SessionTerminalOperation::rejected(&self.status));
        }
        self.output.resize(self.master.as_ref(), cols, rows)?;
        Ok(SessionTerminalOperation::accepted())
    }

    fn refresh_status(&mut self) -> Result<()> {
        if self.status.state != "running" {
            return Ok(());
        }
        if let Some(status) = self.child.try_wait()? {
            close_session_terminal_writer(&self.writer);
            self.status = SessionTerminalStatus {
                state: "exited".into(),
                exit_code: i32::try_from(status.exit_code()).ok(),
                error: status
                    .signal()
                    .map(|signal| format!("terminal shell exited from signal {signal}")),
            };
            return Ok(());
        }
        let activity = self.output.activity()?;
        if let Some(error) = activity.reader_error {
            self.lose(error);
        }
        Ok(())
    }

    fn lose(&mut self, error: String) {
        let _ = self.child.kill();
        close_session_terminal_writer(&self.writer);
        self.status = SessionTerminalStatus {
            state: "lost".into(),
            exit_code: None,
            error: Some(error),
        };
    }
}

impl Drop for SessionTerminal {
    fn drop(&mut self) {
        close_session_terminal_writer(&self.writer);
        let _ = self.child.kill();
        self.reader_discard.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug)]
enum SessionTerminalEvent {
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

impl SessionTerminalEvent {
    fn weight(&self) -> usize {
        match self {
            Self::Output(data) => data.len(),
            Self::Resize { .. } => 8,
        }
    }

    fn payload(&self) -> SessionTerminalEventPayload {
        match self {
            Self::Output(data) => SessionTerminalEventPayload::Output {
                data: BASE64.encode(data),
            },
            Self::Resize { cols, rows } => SessionTerminalEventPayload::Resize {
                cols: *cols,
                rows: *rows,
            },
        }
    }
}

#[derive(Clone, Debug)]
struct SequencedEvent {
    cursor: u64,
    event: SessionTerminalEvent,
}

struct SessionTerminalOutput {
    state: Mutex<SessionTerminalOutputState>,
}

struct SessionTerminalOutputState {
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    events: VecDeque<SequencedEvent>,
    history_bytes: usize,
    tail: u64,
    saw_eof: bool,
    reader_error: Option<String>,
}

#[derive(Clone, Debug)]
struct SessionTerminalActivity {
    reader_error: Option<String>,
}

struct SessionTerminalBatch {
    reset: bool,
    cursor: u64,
    tail: u64,
    cols: u16,
    rows: u16,
    reader_error: Option<String>,
    drained: bool,
    events: Vec<SessionTerminalEventPayload>,
}

impl SessionTerminalOutput {
    fn new(rows: u16, cols: u16) -> Self {
        let mut state = SessionTerminalOutputState {
            parser: vt100::Parser::new(rows, cols, SCREEN_SCROLLBACK_ROWS),
            cols,
            rows,
            events: VecDeque::new(),
            history_bytes: 0,
            tail: 0,
            saw_eof: false,
            reader_error: None,
        };
        state.append(SessionTerminalEvent::Resize { cols, rows });
        Self {
            state: Mutex::new(state),
        }
    }

    fn process(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.parser.process(data);
            state.append(SessionTerminalEvent::Output(data.to_vec()));
        }
    }

    fn cursor_report(&self) -> Result<(u16, u16)> {
        let state = self
            .state
            .lock()
            .map_err(|_| "SessionTerminal output lock is poisoned")?;
        let (row, raw_col) = state.parser.screen().cursor_position();
        let col = raw_col.min(state.cols.saturating_sub(1));
        Ok((row.saturating_add(1), col.saturating_add(1)))
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.saw_eof = true;
        }
    }

    fn fail(&self, error: String) {
        if let Ok(mut state) = self.state.lock() {
            state.reader_error = Some(error);
        }
    }

    fn activity(&self) -> Result<SessionTerminalActivity> {
        let state = self
            .state
            .lock()
            .map_err(|_| "SessionTerminal output lock is poisoned")?;
        Ok(SessionTerminalActivity {
            reader_error: state.reader_error.clone(),
        })
    }

    fn resize(&self, master: &dyn MasterPty, cols: u16, rows: u16) -> Result<()> {
        validate_size(cols, rows)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "SessionTerminal output lock is poisoned")?;
        if state.cols == cols && state.rows == rows {
            return Ok(());
        }
        master.resize(pty_size(cols, rows))?;
        state.parser.screen_mut().set_size(rows, cols);
        state.cols = cols;
        state.rows = rows;
        state.append(SessionTerminalEvent::Resize { cols, rows });
        Ok(())
    }

    fn read(&self, cursor: Option<u64>) -> Result<SessionTerminalBatch> {
        let state = self
            .state
            .lock()
            .map_err(|_| "SessionTerminal output lock is poisoned")?;
        Ok(state.read(cursor))
    }
}

impl SessionTerminalOutputState {
    fn append(&mut self, event: SessionTerminalEvent) {
        let weight = event.weight();
        self.events.push_back(SequencedEvent {
            cursor: self.tail,
            event,
        });
        self.tail = self.tail.wrapping_add(1);
        self.history_bytes = self.history_bytes.saturating_add(weight);
        while self.events.len() > MAX_HISTORY_EVENTS || self.history_bytes > MAX_HISTORY_BYTES {
            let Some(removed) = self.events.pop_front() else {
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(removed.event.weight());
        }
    }

    fn screen_snapshot(&self) -> SessionTerminalBatch {
        let state = self.parser.screen().state_formatted();
        let mut events = vec![SessionTerminalEventPayload::Resize {
            cols: self.cols,
            rows: self.rows,
        }];
        if !state.is_empty() {
            events.push(SessionTerminalEventPayload::Output {
                data: BASE64.encode(state),
            });
        }
        SessionTerminalBatch {
            reset: true,
            cursor: self.tail,
            tail: self.tail,
            cols: self.cols,
            rows: self.rows,
            reader_error: self.reader_error.clone(),
            drained: self.saw_eof || self.reader_error.is_some(),
            events,
        }
    }

    fn read(&self, cursor: Option<u64>) -> SessionTerminalBatch {
        let Some(requested) = cursor else {
            return self.screen_snapshot();
        };
        let base = self.events.front().map_or(self.tail, |event| event.cursor);
        if requested < base || requested > self.tail {
            return self.screen_snapshot();
        }

        let mut events = Vec::new();
        let mut bytes = 0_usize;
        let mut next = requested;
        for event in self.events.iter().filter(|event| event.cursor >= requested) {
            let weight = event.event.weight();
            if !events.is_empty()
                && (events.len() >= MAX_READ_EVENTS
                    || bytes.saturating_add(weight) > MAX_READ_BYTES)
            {
                break;
            }
            events.push(event.event.payload());
            bytes = bytes.saturating_add(weight);
            next = event.cursor.saturating_add(1);
        }
        SessionTerminalBatch {
            reset: false,
            cursor: next,
            tail: self.tail,
            cols: self.cols,
            rows: self.rows,
            reader_error: self.reader_error.clone(),
            drained: self.saw_eof || self.reader_error.is_some(),
            events,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionTerminalQuery {
    PrimaryDeviceAttributes,
    CursorPosition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionTerminalTransportAction {
    Output(Vec<u8>),
    Query(SessionTerminalQuery),
}

#[derive(Default)]
struct SessionTerminalQueryFilter {
    pending: Vec<u8>,
}

impl SessionTerminalQueryFilter {
    fn push(&mut self, data: &[u8]) -> Vec<SessionTerminalTransportAction> {
        let mut actions = Vec::new();
        let mut output = Vec::new();
        for &byte in data {
            self.pending.push(byte);
            loop {
                if let Some(query) = session_terminal_query(&self.pending) {
                    if !output.is_empty() {
                        actions.push(SessionTerminalTransportAction::Output(std::mem::take(
                            &mut output,
                        )));
                    }
                    self.pending.clear();
                    actions.push(SessionTerminalTransportAction::Query(query));
                    break;
                }
                if session_terminal_query_prefix(&self.pending) {
                    break;
                }
                output.push(self.pending.remove(0));
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        if !output.is_empty() {
            actions.push(SessionTerminalTransportAction::Output(output));
        }
        actions
    }

    fn finish(&mut self) -> Vec<SessionTerminalTransportAction> {
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![SessionTerminalTransportAction::Output(std::mem::take(
                &mut self.pending,
            ))]
        }
    }
}

fn session_terminal_query(data: &[u8]) -> Option<SessionTerminalQuery> {
    match data {
        b"\x1b[0c" | b"\x1b[c" => Some(SessionTerminalQuery::PrimaryDeviceAttributes),
        b"\x1b[6n" => Some(SessionTerminalQuery::CursorPosition),
        _ => None,
    }
}

fn session_terminal_query_prefix(data: &[u8]) -> bool {
    [
        b"\x1b[0c".as_slice(),
        b"\x1b[c".as_slice(),
        b"\x1b[6n".as_slice(),
    ]
    .iter()
    .any(|query| query.starts_with(data))
}

fn write_session_terminal(writer: &SessionTerminalWriter, data: &[u8]) -> Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| "SessionTerminal writer lock is poisoned")?;
    let writer = writer.as_mut().ok_or("PTY input is closed")?;
    writer.write_all(data)?;
    writer.flush()?;
    Ok(())
}

fn close_session_terminal_writer(writer: &SessionTerminalWriter) {
    if let Ok(mut writer) = writer.lock() {
        writer.take();
    }
}

fn apply_session_terminal_transport_actions(
    actions: Vec<SessionTerminalTransportAction>,
    output: &SessionTerminalOutput,
    writer: &SessionTerminalWriter,
) -> Result<()> {
    for action in actions {
        match action {
            SessionTerminalTransportAction::Output(data) => output.process(&data),
            SessionTerminalTransportAction::Query(
                SessionTerminalQuery::PrimaryDeviceAttributes,
            ) => write_session_terminal(writer, b"\x1b[?1;2c")?,
            SessionTerminalTransportAction::Query(SessionTerminalQuery::CursorPosition) => {
                let (row, col) = output.cursor_report()?;
                write_session_terminal(writer, format!("\x1b[{row};{col}R").as_bytes())?;
            }
        }
    }
    Ok(())
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    output: Arc<SessionTerminalOutput>,
    writer: SessionTerminalWriter,
) -> std::io::Result<Arc<AtomicBool>> {
    let discard = Arc::new(AtomicBool::new(false));
    let thread_discard = Arc::clone(&discard);
    thread::Builder::new()
        .name("me-session-terminal-reader".into())
        .spawn(move || {
            let mut buffer = [0_u8; READER_CHUNK_BYTES];
            let mut query_filter = SessionTerminalQueryFilter::default();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        if !thread_discard.load(Ordering::Acquire) {
                            if let Err(error) = apply_session_terminal_transport_actions(
                                query_filter.finish(),
                                output.as_ref(),
                                &writer,
                            ) {
                                output.fail(format!("PTY terminal response failed: {error}"));
                            }
                            output.close();
                        }
                        return;
                    }
                    Ok(length) => {
                        if !thread_discard.load(Ordering::Acquire)
                            && let Err(error) = apply_session_terminal_transport_actions(
                                query_filter.push(&buffer[..length]),
                                output.as_ref(),
                                &writer,
                            )
                        {
                            output.fail(format!("PTY terminal response failed: {error}"));
                            return;
                        }
                    }
                    Err(error) => {
                        if !thread_discard.load(Ordering::Acquire) {
                            output.fail(error.to_string());
                        }
                        return;
                    }
                }
            }
        })?;
    Ok(discard)
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn validate_size(cols: u16, rows: u16) -> Result<()> {
    if !(1..=MAX_DIMENSION).contains(&cols) || !(1..=MAX_DIMENSION).contains(&rows) {
        return Err(
            format!("SessionTerminal dimensions must be between 1 and {MAX_DIMENSION}").into(),
        );
    }
    Ok(())
}

fn detect_shell() -> Result<ShellSpec> {
    #[cfg(unix)]
    {
        unix_default_shell()
    }
    #[cfg(windows)]
    {
        let program = windows_powershell().ok_or("PowerShell was not found")?;
        Ok(ShellSpec {
            display: program.to_string_lossy().into_owned(),
            program,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err("SessionTerminal is unsupported on this operating system".into())
    }
}

#[cfg(unix)]
fn unix_default_shell() -> Result<ShellSpec> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};

    let uid = unsafe { libc::geteuid() };
    let recommended = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut capacity = if recommended > 0 {
        usize::try_from(recommended).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    };
    capacity = capacity.clamp(1024, 1024 * 1024);

    loop {
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; capacity];
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && capacity < 1024 * 1024 {
            capacity = (capacity.saturating_mul(2)).min(1024 * 1024);
            continue;
        }
        if code != 0 {
            return Err(std::io::Error::from_raw_os_error(code).into());
        }
        if result.is_null() {
            return Err(format!("no account record exists for effective user {uid}").into());
        }
        let passwd = unsafe { passwd.assume_init() };
        if passwd.pw_shell.is_null() {
            return Err("the effective user account has no default shell".into());
        }
        let bytes = unsafe { CStr::from_ptr(passwd.pw_shell) }.to_bytes();
        if bytes.is_empty() {
            return Err("the effective user account has an empty default shell".into());
        }
        let program = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        if !program.is_file() {
            return Err(
                format!("account default shell {} does not exist", program.display()).into(),
            );
        }
        return Ok(ShellSpec {
            display: program.to_string_lossy().into_owned(),
            program,
        });
    }
}

#[cfg(windows)]
fn windows_powershell() -> Option<PathBuf> {
    let system_root = env::var_os("SystemRoot").or_else(|| env::var_os("WINDIR"));
    if let Some(system_root) = system_root {
        let powershell = PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if powershell.is_file() {
            return Some(powershell);
        }
    }
    let path = env::var_os("PATH")?;
    windows_powershell_on_path(&path)
}

#[cfg(windows)]
fn windows_powershell_on_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    let directories = env::split_paths(path).collect::<Vec<_>>();
    for executable in ["powershell.exe", "pwsh.exe"] {
        for directory in &directories {
            let candidate = directory.join(executable);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    #[test]
    fn terminal_query_filter_consumes_answered_queries_across_reader_chunks() {
        let mut filter = SessionTerminalQueryFilter::default();
        assert_eq!(
            filter.push(b"before\x1b["),
            vec![SessionTerminalTransportAction::Output(b"before".to_vec())]
        );
        assert_eq!(
            filter.push(b"0cafter\x1b[6"),
            vec![
                SessionTerminalTransportAction::Query(
                    SessionTerminalQuery::PrimaryDeviceAttributes
                ),
                SessionTerminalTransportAction::Output(b"after".to_vec()),
            ]
        );
        assert_eq!(
            filter.push(b"nend"),
            vec![
                SessionTerminalTransportAction::Query(SessionTerminalQuery::CursorPosition),
                SessionTerminalTransportAction::Output(b"end".to_vec()),
            ]
        );
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn fresh_attachment_snapshots_current_screen_and_valid_cursors_remain_incremental() {
        let output = SessionTerminalOutput::new(24, 80);
        output.process(b"old line\r\n");
        output.process(b"\x1b[2J\x1b[Hcurrent");

        let first = output.read(None).unwrap();
        assert!(first.reset);
        assert_eq!(first.cursor, first.tail);
        assert_eq!(first.events.len(), 2);
        assert!(matches!(
            first.events.first(),
            Some(SessionTerminalEventPayload::Resize { cols: 80, rows: 24 })
        ));
        let SessionTerminalEventPayload::Output { data } = &first.events[1] else {
            panic!("fresh attachment must include the current screen");
        };
        let screen = BASE64.decode(data).unwrap();
        let screen = String::from_utf8_lossy(&screen);
        assert!(screen.contains("current"));
        assert!(!screen.contains("old line"));

        output.process(b" world");
        let second = output.read(Some(first.cursor)).unwrap();
        assert!(!second.reset);
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.cursor, second.tail);
        let SessionTerminalEventPayload::Output { data } = &second.events[0] else {
            panic!("valid cursor must receive incremental output");
        };
        assert_eq!(BASE64.decode(data).unwrap(), b" world");

        let reset = output.read(Some(second.tail + 1)).unwrap();
        assert!(reset.reset);
        assert_eq!(reset.cursor, reset.tail);
    }

    #[test]
    fn output_history_is_bounded_and_reconstructs_the_current_screen() {
        let output = SessionTerminalOutput::new(3, 20);
        for _ in 0..(MAX_HISTORY_BYTES / READER_CHUNK_BYTES + 8) {
            output.process(&vec![b'x'; READER_CHUNK_BYTES]);
        }
        let state = output.state.lock().unwrap();
        let base = state.events.front().unwrap().cursor;
        assert!(base > 0);
        assert!(state.history_bytes <= MAX_HISTORY_BYTES);
        drop(state);

        let reset = output.read(Some(0)).unwrap();
        assert!(reset.reset);
        assert_eq!(reset.cursor, reset.tail);
        assert!(reset.events.len() >= 2);
    }

    #[test]
    fn registry_reconcile_keeps_one_slot_per_agent() {
        let directory = std::env::temp_dir().join(format!(
            "me-session-terminal-registry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = SessionTerminalRegistry::new(&directory).unwrap();
        registry
            .reconcile(vec![agent("agent-a"), agent("agent-b")])
            .unwrap();
        assert_eq!(registry.len(), 2);
        registry.reconcile(vec![agent("agent-b")]).unwrap();
        assert_eq!(registry.len(), 1);
        drop(registry);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn live_terminal_accepts_input_resize_and_drains_without_a_browser_reader() {
        let directory = std::env::temp_dir().join(format!(
            "me-session-terminal-live-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = SessionTerminalRegistry {
            workspace: std::fs::canonicalize(&directory).unwrap(),
            shell: Ok(ShellSpec {
                program: PathBuf::from("/bin/sh"),
                display: "/bin/sh".into(),
            }),
            terminals: Mutex::new(BTreeMap::new()),
        };
        let id = agent("live-agent");
        registry.reconcile(vec![id.clone()]).unwrap();
        let initial = registry.read(&id, None).unwrap().unwrap();
        let mut cursor = initial.cursor;
        assert!(registry.resize(&id, 91, 27).unwrap().accepted);
        let command = b"i=0; while [ \"$i\" -lt 5000 ]; do printf 'drain-%04d\\n' \"$i\"; i=$((i+1)); done; printf 'session-terminal-token\\n'; pwd; exit\n";
        assert!(registry.input(&id, command).unwrap().accepted);

        // Deliberately leave the browser-side read API idle while output exceeds
        // a typical PTY kernel buffer. The dedicated reader must keep draining.
        std::thread::sleep(std::time::Duration::from_millis(350));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut output = Vec::new();
        let final_read = loop {
            let read = registry.read(&id, Some(cursor)).unwrap().unwrap();
            for event in &read.events {
                if let SessionTerminalEventPayload::Output { data } = event {
                    output.extend(BASE64.decode(data).unwrap());
                }
            }
            cursor = read.cursor;
            if read.state != "running" && read.drained && read.cursor == read.tail {
                break read;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("drain-4999"));
        assert!(output.contains("session-terminal-token"));
        assert!(output.contains(&directory.to_string_lossy().to_string()));
        assert_eq!(final_read.state, "exited");
        assert_eq!((final_read.cols, final_read.rows), (91, 27));

        drop(registry);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_shell_comes_from_the_effective_user_account() {
        let shell = unix_default_shell().unwrap();
        assert!(shell.program.is_file());
        assert!(!shell.display.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_prefers_powershell_before_pwsh() {
        let directory = std::env::temp_dir().join(format!(
            "me-session-terminal-powershell-{}",
            std::process::id()
        ));
        let early = directory.join("early");
        let late = directory.join("late");
        std::fs::create_dir_all(&early).unwrap();
        std::fs::create_dir_all(&late).unwrap();
        std::fs::write(early.join("pwsh.exe"), []).unwrap();
        std::fs::write(late.join("powershell.exe"), []).unwrap();
        let path = std::env::join_paths([&early, &late]).unwrap();

        assert_eq!(
            windows_powershell_on_path(&path),
            Some(late.join("powershell.exe"))
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dimensions_are_bounded() {
        assert!(validate_size(1, 1).is_ok());
        assert!(validate_size(MAX_DIMENSION, MAX_DIMENSION).is_ok());
        assert!(validate_size(0, 1).is_err());
        assert!(validate_size(1, MAX_DIMENSION + 1).is_err());
    }
}
