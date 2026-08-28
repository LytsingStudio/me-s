use std::{
    collections::HashMap,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::{
    Result,
    event::{
        Event, EventBase, EventId, SystemStaticPromptMode, validate_system_static_prompt_change,
    },
    host_files::{DownloadStream, HostFileConflictPolicy, HostFileJobKind, HostFileManager},
    managed_protocol::{
        MANAGED_AUTH_HEADER, MANAGED_BIND_ADDRESS, MANAGED_PROTOCOL_VERSION, MANAGED_READY_PATH,
        MANAGED_SHUTDOWN_PATH, ManagedReadyResponse, bearer_token_matches,
    },
    remote_control::{
        MAX_REMOTE_CONTROL_BODY_BYTES, REMOTE_CONTROL_PATH_PREFIX, RemoteControlRuntime,
        RemoteInputEvent,
    },
    session_terminal::{MAX_INPUT_BYTES, SessionTerminalOperation, SessionTerminalRegistry},
    turn_history,
    ui_backend::{
        CHAT_ACTIVITY_TOOL_NAMES, CHAT_HIDDEN_TOOL_NAMES, CHAT_HIDDEN_TOOL_PREFIXES, UiBackend,
        UiCommand, UiCommandGateway, UiCommandReceipt, UiModelOption, UiSnapshot,
    },
    web_auth::WebSessionAuth,
    workspace::AgentId,
};

pub const DEFAULT_PORT: u16 = 38199;
pub const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0";

#[derive(Clone)]
pub struct ManagedWebAccess {
    pub token: String,
    pub instance_nonce: String,
    pub workspace_path: String,
    pub terminate: Arc<AtomicBool>,
}
const INDEX_HTML: &str = include_str!("webui/index.html");
const APP_JS: &str = include_str!("webui/app.js");
const THEME_JS: &str = include_str!("webui/theme.js");
const THEME_CSS: &str = include_str!("webui/theme.css");
const TRANSCRIPT_JS: &str = include_str!("webui/transcript.js");
const TOOL_PRESENTERS_JS: &str = include_str!("webui/tool-presenters.js");
const EDB_CACHE_JS: &str = include_str!("webui/edb-cache.js");
const MARKDOWN_JS: &str = include_str!("webui/markdown.js");
const MARKDOWN_IT_JS: &str = include_str!("webui/vendor/markdown-it.min.js");
const KATEX_JS: &str = include_str!("webui/vendor/katex.min.js");
const KATEX_CSS: &str = include_str!("webui/vendor/katex.min.css");
const STYLE_CSS: &str = include_str!("webui/style.css");
const SESSION_TERMINAL_JS: &str = include_str!("webui/session-terminal.js");
const REMOTE_CONTROL_JS: &str = include_str!("webui/remote-control.js");
const FILE_MANAGER_JS: &str = include_str!("webui/file-manager.js");
const XTERM_JS: &str = include_str!("webui/vendor/xterm.js");
const XTERM_ADDON_FIT_JS: &str = include_str!("webui/vendor/xterm-addon-fit.js");
const XTERM_ADDON_UNICODE11_JS: &str = include_str!("webui/vendor/xterm-addon-unicode11.js");
const XTERM_CSS: &str = include_str!("webui/vendor/xterm.css");
const KATEX_FONTS: &[(&str, &[u8])] = &[
    (
        "/fonts/KaTeX_AMS-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_AMS-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Caligraphic-Bold.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Caligraphic-Bold.woff2"),
    ),
    (
        "/fonts/KaTeX_Caligraphic-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Caligraphic-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Fraktur-Bold.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Fraktur-Bold.woff2"),
    ),
    (
        "/fonts/KaTeX_Fraktur-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Fraktur-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Main-Bold.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Main-Bold.woff2"),
    ),
    (
        "/fonts/KaTeX_Main-BoldItalic.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Main-BoldItalic.woff2"),
    ),
    (
        "/fonts/KaTeX_Main-Italic.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Main-Italic.woff2"),
    ),
    (
        "/fonts/KaTeX_Main-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Main-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Math-BoldItalic.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Math-BoldItalic.woff2"),
    ),
    (
        "/fonts/KaTeX_Math-Italic.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Math-Italic.woff2"),
    ),
    (
        "/fonts/KaTeX_SansSerif-Bold.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_SansSerif-Bold.woff2"),
    ),
    (
        "/fonts/KaTeX_SansSerif-Italic.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_SansSerif-Italic.woff2"),
    ),
    (
        "/fonts/KaTeX_SansSerif-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_SansSerif-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Script-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Script-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Size1-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Size1-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Size2-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Size2-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Size3-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Size3-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Size4-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Size4-Regular.woff2"),
    ),
    (
        "/fonts/KaTeX_Typewriter-Regular.woff2",
        include_bytes!("webui/vendor/katex-fonts/KaTeX_Typewriter-Regular.woff2"),
    ),
];
pub(crate) fn shared_katex_font(path: &str) -> Option<&'static [u8]> {
    KATEX_FONTS
        .iter()
        .find_map(|(candidate, content)| (*candidate == path).then_some(*content))
}

pub(crate) fn shared_webui_component_asset(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/session-terminal.js" => Some(("text/javascript; charset=utf-8", SESSION_TERMINAL_JS)),
        "/remote-control.js" => Some(("text/javascript; charset=utf-8", REMOTE_CONTROL_JS)),
        "/xterm.js" => Some(("text/javascript; charset=utf-8", XTERM_JS)),
        "/xterm-addon-fit.js" => Some(("text/javascript; charset=utf-8", XTERM_ADDON_FIT_JS)),
        "/xterm-addon-unicode11.js" => {
            Some(("text/javascript; charset=utf-8", XTERM_ADDON_UNICODE11_JS))
        }
        "/xterm.css" => Some(("text/css; charset=utf-8", XTERM_CSS)),
        _ => None,
    }
}
const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BATCH_BYTES: usize = 1024 * 1024;
const MAX_LOGIN_BYTES: usize = 4096;
const MAX_SESSION_TERMINAL_BODY_BYTES: usize = 128 * 1024;
const MAX_HOST_FILE_BODY_BYTES: usize = 1024 * 1024;
const SESSION_COOKIE_PREFIX: &str = "me_webui_session";

pub struct WebUiServer {
    address: String,
    port: u16,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _remote_control: Arc<RemoteControlRuntime>,
    _session_terminals: Arc<SessionTerminalRegistry>,
    _host_files: Arc<HostFileManager>,
}

impl WebUiServer {
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for WebUiServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self._remote_control.shutdown();
        self._host_files.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start(
    backend: impl UiBackend + 'static,
    commands: impl UiCommandGateway + 'static,
    passkey: Option<&str>,
) -> Result<WebUiServer> {
    start_from(backend, commands, DEFAULT_PORT, passkey)
}

fn start_from(
    backend: impl UiBackend + 'static,
    commands: impl UiCommandGateway + 'static,
    first_port: u16,
    passkey: Option<&str>,
) -> Result<WebUiServer> {
    let (server, port) = bind_first_available(first_port)?;
    start_with_server(backend, commands, server, port, passkey, None)
}

pub fn start_managed(
    backend: impl UiBackend + 'static,
    commands: impl UiCommandGateway + 'static,
    port: u16,
    access: ManagedWebAccess,
) -> Result<WebUiServer> {
    let server = Server::http((MANAGED_BIND_ADDRESS, port))
        .map_err(|error| format!("failed to bind managed WebUI: {error}"))?;
    start_with_server(backend, commands, server, port, None, Some(access))
}

fn bind_first_available(first_port: u16) -> Result<(Server, u16)> {
    for port in first_port..=u16::MAX {
        match Server::http((DEFAULT_BIND_ADDRESS, port)) {
            Ok(server) => return Ok((server, port)),
            Err(error) if bind_error_kind(error.as_ref()) == Some(io::ErrorKind::AddrInUse) => {}
            Err(error) => {
                return Err(format!("failed to bind WebUI port {port}: {error}").into());
            }
        }
    }
    Err(format!("no available WebUI port at or above {first_port}").into())
}

fn bind_error_kind(error: &(dyn std::error::Error + 'static)) -> Option<io::ErrorKind> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<io::Error>() {
            return Some(error.kind());
        }
        current = error.source();
    }
    None
}

fn start_with_server(
    backend: impl UiBackend + 'static,
    commands: impl UiCommandGateway + 'static,
    server: Server,
    requested_port: u16,
    passkey: Option<&str>,
    managed: Option<ManagedWebAccess>,
) -> Result<WebUiServer> {
    let port = server
        .server_addr()
        .to_ip()
        .map(|address| address.port())
        .unwrap_or(requested_port);
    let address = match server.server_addr().to_ip() {
        Some(address) => format!("http://{address}"),
        None => format!("http://{DEFAULT_BIND_ADDRESS}:{port}"),
    };
    let backend: Arc<dyn UiBackend> = Arc::new(backend);
    let initial = backend.snapshot()?;
    let host_files = Arc::new(HostFileManager::new(&initial.environment.workspace)?);
    let remote_control = Arc::new(RemoteControlRuntime::new()?);
    let session_terminals = Arc::new(SessionTerminalRegistry::new(
        &initial.environment.workspace,
    )?);
    session_terminals.reconcile(
        initial
            .agents
            .iter()
            .filter(|agent| agent.orchestrator_name != "chatbot")
            .map(|agent| agent.id.clone())
            .collect(),
    )?;
    let commands: Arc<dyn UiCommandGateway> = Arc::new(commands);
    let auth = Arc::new(WebSessionAuth::new(SESSION_COOKIE_PREFIX, port, passkey)?);
    let managed = Arc::new(managed);
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_remote_control = Arc::clone(&remote_control);
    let worker_session_terminals = Arc::clone(&session_terminals);
    let worker_host_files = Arc::clone(&host_files);
    let worker = thread::Builder::new()
        .name("me-webui".into())
        .spawn(move || {
            let mut last_reconcile_error = None;
            while !worker_shutdown.load(Ordering::Acquire) {
                worker_remote_control.expire_if_stale();
                let reconcile = backend
                    .session_terminal_agent_ids()
                    .and_then(|agent_ids| worker_session_terminals.reconcile(agent_ids));
                match reconcile {
                    Ok(()) => last_reconcile_error = None,
                    Err(error) => {
                        let error = error.to_string();
                        if last_reconcile_error.as_deref() != Some(error.as_str()) {
                            eprintln!("warning: SessionTerminal reconcile failed: {error}");
                            last_reconcile_error = Some(error);
                        }
                    }
                }
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(request)) => {
                        let backend = Arc::clone(&backend);
                        let commands = Arc::clone(&commands);
                        let auth = Arc::clone(&auth);
                        let managed = Arc::clone(&managed);
                        let remote_control = Arc::clone(&worker_remote_control);
                        let session_terminals = Arc::clone(&worker_session_terminals);
                        let host_files = Arc::clone(&worker_host_files);
                        let _ = thread::Builder::new()
                            .name("me-webui-request".into())
                            .spawn(move || {
                                serve(
                                    request,
                                    backend.as_ref(),
                                    commands.as_ref(),
                                    auth.as_ref(),
                                    managed.as_ref().as_ref(),
                                    remote_control.as_ref(),
                                    session_terminals.as_ref(),
                                    host_files.as_ref(),
                                );
                            });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("warning: WebUI request listener stopped: {error}");
                        break;
                    }
                }
            }
        })?;
    Ok(WebUiServer {
        address,
        port,
        shutdown,
        worker: Some(worker),
        _remote_control: remote_control,
        _session_terminals: session_terminals,
        _host_files: host_files,
    })
}

#[derive(Deserialize)]
struct SyncRequest {
    snapshot_revision: Option<u64>,
    #[serde(default)]
    agents: Vec<SyncAgentCursor>,
    selected_agent: Option<String>,
    terminal_session: Option<String>,
    terminal_revision: Option<u64>,
    #[serde(default)]
    cache_metadata_only: bool,
}

#[derive(Deserialize)]
struct SyncAgentCursor {
    id: String,
    event_count: usize,
    mutation_revision: u64,
    #[serde(default)]
    cursor_event_hash: Option<String>,
}

fn cursor_event_hash_matches(cursor: &SyncAgentCursor, events: &[Event]) -> bool {
    let Some(expected) = cursor.cursor_event_hash.as_deref() else {
        return true;
    };
    cursor
        .event_count
        .checked_sub(1)
        .and_then(|index| events.get(index))
        .is_some_and(|event| event.getHash() == expected)
}

fn sync_state_payload(
    backend: &dyn UiBackend,
    snapshot_revision: Option<u64>,
    cursors: Vec<SyncAgentCursor>,
    selected_agent: Option<String>,
    terminal_session: Option<String>,
    terminal_revision: Option<u64>,
    cache_metadata_only: bool,
) -> Result<serde_json::Value> {
    let snapshot = backend.snapshot()?;
    let snapshot_changed = snapshot_revision != Some(snapshot.revision);
    let selected_agent = selected_agent
        .map(AgentId::new)
        .transpose()?
        .filter(|agent| snapshot.contains(agent));
    let cursors = cursors
        .into_iter()
        .map(|cursor| (cursor.id.clone(), cursor))
        .collect::<HashMap<_, _>>();
    let mut event_updates = Vec::new();
    let mut remaining_event_bytes = MAX_EVENT_BATCH_BYTES;
    let mut more_events = false;
    if !cache_metadata_only {
        let mut agent_indexes = (0..snapshot.agents.len()).collect::<Vec<_>>();
        agent_indexes.sort_by_key(|index| {
            usize::from(selected_agent.as_ref() != Some(&snapshot.agents[*index].id))
        });
        for index in agent_indexes {
            let agent = &snapshot.agents[index];
            let cursor = cursors.get(agent.id.as_str());
            // New WebUIs send a cursor hash only while validating an initial or
            // reconnecting raw EDB cache. Even a complete cache still needs the
            // non-persisted turn history regenerated from the authoritative EDB.
            let restore_turn_history = cursor.is_some_and(|cursor| {
                cursor.cursor_event_hash.is_some() && agent.orchestrator_name != "worker-agent"
            });
            if !snapshot_changed
                && cursor.is_some_and(|cursor| {
                    cursor.event_count == agent.events.len()
                        && cursor.mutation_revision == agent.mutation_revision
                        && cursor_event_hash_matches(cursor, &agent.events)
                })
                && !restore_turn_history
            {
                continue;
            }
            let reset = cursor.is_none_or(|cursor| {
                cursor.mutation_revision != agent.mutation_revision
                    || cursor.event_count > agent.events.len()
                    || !cursor_event_hash_matches(cursor, &agent.events)
            });
            let start = if reset {
                0
            } else {
                cursor.map_or(0, |cursor| cursor.event_count)
            };
            if !reset && start == agent.events.len() && !restore_turn_history {
                continue;
            }
            let available = &agent.events[start..];
            if available.is_empty() {
                if reset {
                    event_updates.push(json!({
                        "agent_id": agent.id.to_string(),
                        "reset": true,
                        "event_count": agent.events.len(),
                        "mutation_revision": agent.mutation_revision,
                        "cursor_event_hash": serde_json::Value::Null,
                        "turn_history_updated": true,
                        "turn_history": serde_json::Value::Null,
                        "events": [],
                    }));
                } else if restore_turn_history {
                    event_updates.push(json!({
                        "agent_id": agent.id.to_string(),
                        "reset": false,
                        "event_count": agent.events.len(),
                        "mutation_revision": agent.mutation_revision,
                        "cursor_event_hash": cursor
                            .and_then(|cursor| cursor.cursor_event_hash.clone()),
                        "turn_history_updated": true,
                        "turn_history": turn_history::latest_snapshot(&agent.events)?,
                        "events": [],
                    }));
                }
                continue;
            }
            if remaining_event_bytes == 0 {
                more_events = true;
                continue;
            }
            let (event_count, encoded_bytes) =
                event_prefix_within_budget(available, remaining_event_bytes)?;
            let events = &available[..event_count];
            remaining_event_bytes = remaining_event_bytes.saturating_sub(encoded_bytes);
            more_events |= event_count < available.len();
            let client_event_count = start + event_count;
            let cursor_event_hash = agent
                .events
                .get(client_event_count - 1)
                .map(EventBase::getHash);
            let turn_history_updated =
                restore_turn_history || turn_history_needs_refresh(reset, start, events);
            let turn_history = if turn_history_updated && agent.orchestrator_name != "worker-agent"
            {
                turn_history::latest_snapshot(&agent.events)?
            } else {
                None
            };
            event_updates.push(json!({
                "agent_id": agent.id.to_string(),
                "reset": reset,
                "event_count": agent.events.len(),
                "mutation_revision": agent.mutation_revision,
                "cursor_event_hash": cursor_event_hash,
                "turn_history_updated": turn_history_updated,
                "turn_history": turn_history,
                "events": events,
            }));
        }
    }

    let api_activity = selected_agent
        .as_ref()
        .map(|agent| backend.api_activity(agent))
        .transpose()?
        .unwrap_or_default();
    let terminals = selected_agent
        .as_ref()
        .map(|agent| backend.terminal_sessions(agent))
        .transpose()?
        .unwrap_or_default();
    let (terminal_frame_updated, terminal_frame) =
        match (&selected_agent, terminal_session.as_deref()) {
            (Some(agent), Some(session)) => {
                let frame = backend.terminal_frame(agent, session)?;
                let updated = frame
                    .as_ref()
                    .is_none_or(|frame| Some(frame.revision) != terminal_revision);
                (updated, updated.then_some(frame).flatten())
            }
            _ => (false, None),
        };
    let snapshot_payload =
        snapshot_changed.then(|| snapshot_metadata(snapshot, cache_metadata_only));
    let selected_agent_id = selected_agent.as_ref().map(ToString::to_string);
    Ok(json!({
        "ok": true,
        "type": "state",
        "snapshot": snapshot_payload,
        "event_updates": event_updates,
        "cache_metadata_only": cache_metadata_only,
        "more_events": more_events,
        "selected_agent": selected_agent_id,
        "api_activity": {
            "active": api_activity.active,
            "received_sse_events": api_activity.received_sse_events,
        },
        "terminals": terminals,
        "terminal_session": terminal_session,
        "terminal_frame_updated": terminal_frame_updated,
        "terminal_frame": terminal_frame,
    }))
}

fn event_prefix_within_budget(events: &[Event], budget: usize) -> Result<(usize, usize)> {
    if events.is_empty() || budget == 0 {
        return Ok((0, 0));
    }
    let mut count = 0;
    let mut encoded_bytes = 0_usize;
    for event in events {
        let event_bytes = serde_json::to_vec(event)?.len() + usize::from(count > 0);
        if count > 0 && encoded_bytes.saturating_add(event_bytes) > budget {
            break;
        }
        count += 1;
        encoded_bytes = encoded_bytes.saturating_add(event_bytes);
        if encoded_bytes >= budget {
            break;
        }
    }
    Ok((count, encoded_bytes))
}

fn serve(
    mut request: Request,
    backend: &dyn UiBackend,
    commands: &dyn UiCommandGateway,
    auth: &WebSessionAuth,
    managed: Option<&ManagedWebAccess>,
    remote_control: &RemoteControlRuntime,
    session_terminals: &SessionTerminalRegistry,
    host_files: &HostFileManager,
) {
    let result = match managed {
        Some(managed) => route_managed(
            &mut request,
            backend,
            commands,
            managed,
            remote_control,
            session_terminals,
            host_files,
        ),
        None => route(
            &mut request,
            backend,
            commands,
            auth,
            remote_control,
            session_terminals,
            host_files,
        ),
    };
    let response = match result {
        Ok(response) => response,
        Err(error) => json_response(
            StatusCode(500),
            &json!({"ok": false, "error": error.to_string()}),
        ),
    };
    let _ = request.respond(response);
}

type HttpResponse = Response<Box<dyn Read + Send>>;

fn route_managed(
    request: &mut Request,
    backend: &dyn UiBackend,
    commands: &dyn UiCommandGateway,
    managed: &ManagedWebAccess,
    remote_control: &RemoteControlRuntime,
    session_terminals: &SessionTerminalRegistry,
    host_files: &HostFileManager,
) -> Result<HttpResponse> {
    let authorization = request
        .headers()
        .iter()
        .find(|header| header.field.equiv(MANAGED_AUTH_HEADER))
        .map(|header| header.value.as_str());
    if !bearer_token_matches(authorization, &managed.token) {
        return Ok(json_response(
            StatusCode(401),
            &json!({"ok": false, "error": "managed authentication required"}),
        ));
    }
    let url = request.url().to_owned();
    let (path, query) = split_url(&url);
    match (request.method(), path) {
        (&Method::Get, MANAGED_READY_PATH) => Ok(json_response(
            StatusCode(200),
            &ManagedReadyResponse {
                ok: true,
                ready: true,
                protocol_version: MANAGED_PROTOCOL_VERSION,
                product_version: env!("CARGO_PKG_VERSION").to_owned(),
                workspace_path: managed.workspace_path.clone(),
                instance_nonce: managed.instance_nonce.clone(),
            },
        )),
        (&Method::Post, MANAGED_SHUTDOWN_PATH) => {
            managed.terminate.store(true, Ordering::Release);
            Ok(json_response(
                StatusCode(200),
                &json!({"ok": true, "stopping": true}),
            ))
        }
        (&Method::Post, "/api/sync")
        | (&Method::Get, "/api/snapshot")
        | (&Method::Post, "/api/command") => operational_route(
            request,
            backend,
            commands,
            remote_control,
            session_terminals,
            host_files,
            path,
            query,
        ),
        (&Method::Get, path) if path.starts_with("/api/deletion-blocker/") => operational_route(
            request,
            backend,
            commands,
            remote_control,
            session_terminals,
            host_files,
            path,
            query,
        ),
        (&Method::Post, path) if query.is_none() && path.starts_with("/api/session-terminal/") => {
            operational_route(
                request,
                backend,
                commands,
                remote_control,
                session_terminals,
                host_files,
                path,
                query,
            )
        }
        (method, path)
            if query.is_none()
                && ((*method == Method::Post && is_host_file_post_path(path))
                    || (*method == Method::Get && parse_download_content_path(path).is_some())) =>
        {
            operational_route(
                request,
                backend,
                commands,
                remote_control,
                session_terminals,
                host_files,
                path,
                query,
            )
        }
        (&Method::Post, path)
            if query.is_none() && path.starts_with(REMOTE_CONTROL_PATH_PREFIX) =>
        {
            operational_route(
                request,
                backend,
                commands,
                remote_control,
                session_terminals,
                host_files,
                path,
                query,
            )
        }
        _ => Ok(json_response(
            StatusCode(404),
            &json!({"ok": false, "error": "not found"}),
        )),
    }
}

fn route(
    request: &mut Request,
    backend: &dyn UiBackend,
    commands: &dyn UiCommandGateway,
    auth: &WebSessionAuth,
    remote_control: &RemoteControlRuntime,
    session_terminals: &SessionTerminalRegistry,
    host_files: &HostFileManager,
) -> Result<HttpResponse> {
    let url = request.url().to_owned();
    let (path, query) = split_url(&url);
    if request.method() == &Method::Get
        && let Some((_, font)) = KATEX_FONTS.iter().find(|(font_path, _)| *font_path == path)
    {
        return Ok(bytes_response("font/woff2", font));
    }
    if request.method() == &Method::Get
        && let Some((content_type, content)) = shared_webui_component_asset(path)
    {
        return Ok(text_response(content_type, content));
    }
    match (request.method(), path) {
        (&Method::Get, "/") => {
            return Ok(text_response("text/html; charset=utf-8", INDEX_HTML));
        }
        (&Method::Get, "/theme.js") => {
            return Ok(text_response("text/javascript; charset=utf-8", THEME_JS));
        }
        (&Method::Get, "/app.js") => {
            return Ok(text_response("text/javascript; charset=utf-8", APP_JS));
        }
        (&Method::Get, "/file-manager.js") => {
            return Ok(text_response(
                "text/javascript; charset=utf-8",
                FILE_MANAGER_JS,
            ));
        }
        (&Method::Get, "/transcript.js") => {
            return Ok(text_response(
                "text/javascript; charset=utf-8",
                TRANSCRIPT_JS,
            ));
        }
        (&Method::Get, "/tool-presenters.js") => {
            return Ok(text_response(
                "text/javascript; charset=utf-8",
                TOOL_PRESENTERS_JS,
            ));
        }
        (&Method::Get, "/edb-cache.js") => {
            return Ok(text_response(
                "text/javascript; charset=utf-8",
                EDB_CACHE_JS,
            ));
        }
        (&Method::Get, "/markdown.js") => {
            return Ok(text_response("text/javascript; charset=utf-8", MARKDOWN_JS));
        }
        (&Method::Get, "/markdown-it.js") => {
            return Ok(text_response(
                "text/javascript; charset=utf-8",
                MARKDOWN_IT_JS,
            ));
        }
        (&Method::Get, "/katex.js") => {
            return Ok(text_response("text/javascript; charset=utf-8", KATEX_JS));
        }
        (&Method::Get, "/katex.css") => {
            return Ok(text_response("text/css; charset=utf-8", KATEX_CSS));
        }
        (&Method::Get, "/style.css") => {
            return Ok(text_response("text/css; charset=utf-8", STYLE_CSS));
        }
        (&Method::Get, "/theme.css") => {
            return Ok(text_response("text/css; charset=utf-8", THEME_CSS));
        }
        (&Method::Get, "/api/auth/status") => return auth_status_response(request, auth),
        (&Method::Post, "/api/auth/login") => return login_response(request, auth),
        _ => {}
    }
    if !auth.authorized_any(request_session_tokens(request, auth.cookie_prefix())) {
        return Ok(unauthorized_response());
    }
    operational_route(
        request,
        backend,
        commands,
        remote_control,
        session_terminals,
        host_files,
        path,
        query,
    )
}

fn operational_route(
    request: &mut Request,
    backend: &dyn UiBackend,
    commands: &dyn UiCommandGateway,
    remote_control: &RemoteControlRuntime,
    session_terminals: &SessionTerminalRegistry,
    host_files: &HostFileManager,
    path: &str,
    query: Option<&str>,
) -> Result<HttpResponse> {
    match (request.method(), path) {
        (&Method::Get, "/api/health") => Ok(json_response(
            StatusCode(200),
            &json!({"ok": true, "service": "me-webui"}),
        )),
        (&Method::Post, "/api/sync") => sync_response(request, backend),
        (&Method::Get, "/api/snapshot") => snapshot_response(backend),
        (&Method::Get, path) if path.starts_with("/api/api-activity/") => {
            let id = parse_agent_path(path, "/api/api-activity/")?;
            let activity = backend.api_activity(&id)?;
            Ok(json_response(
                StatusCode(200),
                &json!({
                    "ok": true,
                    "active": activity.active,
                    "received_sse_events": activity.received_sse_events,
                }),
            ))
        }
        (&Method::Get, path) if path.starts_with("/api/events/") => {
            let id = parse_agent_path(path, "/api/events/")?;
            events_response(backend, &id, query)
        }
        (&Method::Get, path) if path.starts_with("/api/terminals/") => {
            let id = parse_agent_path(path, "/api/terminals/")?;
            Ok(json_response(
                StatusCode(200),
                &json!({"ok": true, "sessions": backend.terminal_sessions(&id)?}),
            ))
        }
        (&Method::Get, path) if path.starts_with("/api/terminal-backend/") => {
            let id = parse_agent_path(path, "/api/terminal-backend/")?;
            Ok(json_response(
                StatusCode(200),
                &json!({"ok": true, "backend": backend.terminal_backend(&id)?}),
            ))
        }
        (&Method::Get, path) if path.starts_with("/api/deletion-blocker/") => {
            let id = parse_agent_path(path, "/api/deletion-blocker/")?;
            Ok(json_response(
                StatusCode(200),
                &json!({"ok": true, "blocker": backend.deletion_blocker(&id)?}),
            ))
        }
        (&Method::Get, path) if path.starts_with("/api/terminal/") => {
            terminal_frame_response(backend, path)
        }
        (&Method::Post, path) if query.is_none() && path.starts_with("/api/session-terminal/") => {
            session_terminal_response(request, session_terminals, path)
        }
        (method, path)
            if query.is_none()
                && ((*method == Method::Post && is_host_file_post_path(path))
                    || (*method == Method::Get && parse_download_content_path(path).is_some())) =>
        {
            host_file_response(request, host_files, path)
        }
        (&Method::Post, path)
            if query.is_none() && path.starts_with(REMOTE_CONTROL_PATH_PREFIX) =>
        {
            remote_control_response(request, remote_control, path)
        }
        (&Method::Post, "/api/command") => command_response(request, commands),
        _ => Ok(json_response(
            StatusCode(404),
            &json!({"ok": false, "error": "not found"}),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileListRequest {
    path: Option<String>,
    #[serde(default)]
    roots: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileMkdirRequest {
    parent: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileRenameRequest {
    path: String,
    new_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileJobPrepareRequest {
    kind: HostFileJobKind,
    sources: Vec<String>,
    destination: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileJobConfirmRequest {
    operation_id: String,
    conflict_policy: HostFileConflictPolicy,
    #[serde(default)]
    replace_directories: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileOperationRequest {
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileJobStatusRequest {
    operation_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileUploadCreateRequest {
    destination: String,
    name: String,
    size_bytes: u64,
    conflict_policy: Option<HostFileConflictPolicy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileUploadChunkRequest {
    upload_id: String,
    offset: u64,
    data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileUploadRequest {
    upload_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileDownloadCreateRequest {
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFileDownloadRequest {
    download_id: String,
}

fn is_host_file_post_path(path: &str) -> bool {
    matches!(
        path,
        "/api/files/list"
            | "/api/files/mkdir"
            | "/api/files/rename"
            | "/api/files/jobs/prepare"
            | "/api/files/jobs/confirm"
            | "/api/files/jobs/status"
            | "/api/files/jobs/cancel"
            | "/api/files/uploads/create"
            | "/api/files/uploads/chunk"
            | "/api/files/uploads/finish"
            | "/api/files/uploads/cancel"
            | "/api/files/downloads/create"
            | "/api/files/downloads/status"
            | "/api/files/downloads/cancel"
    )
}

fn parse_download_content_path(path: &str) -> Option<&str> {
    let download_id = path
        .strip_prefix("/api/files/downloads/")?
        .strip_suffix("/content")?;
    (!download_id.is_empty()
        && !download_id.contains('/')
        && download_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
    .then_some(download_id)
}

fn host_file_response(
    request: &mut Request,
    host_files: &HostFileManager,
    path: &str,
) -> Result<HttpResponse> {
    Ok(match host_file_response_inner(request, host_files, path) {
        Ok(response) => response,
        Err(error) => json_response(
            StatusCode(400),
            &json!({"ok": false, "error": error.to_string()}),
        ),
    })
}

fn read_json<T: serde::de::DeserializeOwned>(request: &mut Request, limit: usize) -> Result<T> {
    let length = request.body_length().unwrap_or(0);
    if length > limit {
        return Err("Request body is too large".into());
    }
    let mut body = Vec::with_capacity(length.min(limit));
    request
        .as_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > limit {
        return Err("Request body is too large".into());
    }
    Ok(serde_json::from_slice(&body)?)
}

fn host_file_response_inner(
    request: &mut Request,
    host_files: &HostFileManager,
    path: &str,
) -> Result<HttpResponse> {
    let response = match (request.method(), path) {
        (&Method::Post, "/api/files/list") => {
            let value: HostFileListRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.list(value.path.as_deref(), value.roots)?,
            )
        }
        (&Method::Post, "/api/files/mkdir") => {
            let value: HostFileMkdirRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &json!({"ok": true, "entry": host_files.mkdir(&value.parent, &value.name)?}),
            )
        }
        (&Method::Post, "/api/files/rename") => {
            let value: HostFileRenameRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &json!({"ok": true, "entry": host_files.rename(&value.path, &value.new_name)?}),
            )
        }
        (&Method::Post, "/api/files/jobs/prepare") => {
            let value: HostFileJobPrepareRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.prepare_job(value.kind, value.sources, value.destination)?,
            )
        }
        (&Method::Post, "/api/files/jobs/confirm") => {
            let value: HostFileJobConfirmRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.confirm_job(
                    &value.operation_id,
                    value.conflict_policy,
                    value.replace_directories,
                )?,
            )
        }
        (&Method::Post, "/api/files/jobs/status") => {
            let value: HostFileJobStatusRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &json!({
                    "ok": true,
                    "jobs": host_files.job_status(value.operation_id.as_deref())?,
                }),
            )
        }
        (&Method::Post, "/api/files/jobs/cancel") => {
            let value: HostFileOperationRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.cancel_job(&value.operation_id)?,
            )
        }
        (&Method::Post, "/api/files/uploads/create") => {
            let value: HostFileUploadCreateRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.create_upload(
                    &value.destination,
                    &value.name,
                    value.size_bytes,
                    value.conflict_policy,
                )?,
            )
        }
        (&Method::Post, "/api/files/uploads/chunk") => {
            let value: HostFileUploadChunkRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.upload_chunk(&value.upload_id, value.offset, &value.data)?,
            )
        }
        (&Method::Post, "/api/files/uploads/finish") => {
            let value: HostFileUploadRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.finish_upload(&value.upload_id)?,
            )
        }
        (&Method::Post, "/api/files/uploads/cancel") => {
            let value: HostFileUploadRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.cancel_upload(&value.upload_id)?,
            )
        }
        (&Method::Post, "/api/files/downloads/create") => {
            let value: HostFileDownloadCreateRequest =
                read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(StatusCode(200), &host_files.create_download(value.sources)?)
        }
        (&Method::Post, "/api/files/downloads/status") => {
            let value: HostFileDownloadRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.download_status(&value.download_id)?,
            )
        }
        (&Method::Post, "/api/files/downloads/cancel") => {
            let value: HostFileDownloadRequest = read_json(request, MAX_HOST_FILE_BODY_BYTES)?;
            json_response(
                StatusCode(200),
                &host_files.cancel_download(&value.download_id)?,
            )
        }
        (&Method::Get, path) if parse_download_content_path(path).is_some() => {
            let download_id = parse_download_content_path(path).expect("validated download path");
            let range = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Range"))
                .map(|header| header.value.as_str());
            download_response(host_files.open_download(download_id, range)?)
        }
        _ => json_response(StatusCode(404), &json!({"ok": false, "error": "not found"})),
    };
    Ok(response)
}

fn download_response(stream: DownloadStream) -> HttpResponse {
    let mut headers = vec![
        Header::from_bytes("Content-Type", stream.content_type)
            .expect("download Content-Type is valid"),
        Header::from_bytes("Accept-Ranges", "bytes").expect("Accept-Ranges is valid"),
        Header::from_bytes(
            "Content-Disposition",
            format!(
                "attachment; filename=\"download\"; filename*=UTF-8''{}",
                percent_encode_header(&stream.filename)
            ),
        )
        .expect("download Content-Disposition is valid"),
        no_store(),
    ];
    if let Some(content_range) = stream.content_range {
        headers.push(
            Header::from_bytes("Content-Range", content_range).expect("Content-Range is valid"),
        );
    }
    Response::new(
        StatusCode(stream.status),
        headers,
        stream.reader,
        Some(stream.content_length),
        None,
    )
}

fn percent_encode_header(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionTerminalReadRequest {
    cursor: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionTerminalInputRequest {
    data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionTerminalResizeRequest {
    cols: u16,
    rows: u16,
}

fn session_terminal_response(
    request: &mut Request,
    session_terminals: &SessionTerminalRegistry,
    path: &str,
) -> Result<HttpResponse> {
    let Some(rest) = path.strip_prefix("/api/session-terminal/") else {
        return Ok(json_response(
            StatusCode(404),
            &json!({"ok": false, "error": "not found"}),
        ));
    };
    let Some((agent_id, action)) = rest.split_once('/') else {
        return Ok(json_response(
            StatusCode(404),
            &json!({"ok": false, "error": "not found"}),
        ));
    };
    if agent_id.is_empty()
        || action.is_empty()
        || action.contains('/')
        || !matches!(action, "read" | "input" | "resize")
    {
        return Ok(json_response(
            StatusCode(404),
            &json!({"ok": false, "error": "not found"}),
        ));
    }
    let agent_id = match AgentId::new(agent_id) {
        Ok(agent_id) => agent_id,
        Err(_) => {
            return Ok(json_response(
                StatusCode(404),
                &json!({"ok": false, "error": "not found"}),
            ));
        }
    };
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str());
    if content_type.is_none_or(|value| !value.starts_with("application/json")) {
        return Ok(json_response(
            StatusCode(415),
            &json!({"ok": false, "error": "Content-Type must be application/json"}),
        ));
    }
    let length = request.body_length().unwrap_or(0);
    if length > MAX_SESSION_TERMINAL_BODY_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "SessionTerminal request body is too large"}),
        ));
    }
    let mut body = Vec::with_capacity(length.min(MAX_SESSION_TERMINAL_BODY_BYTES));
    request
        .as_reader()
        .take((MAX_SESSION_TERMINAL_BODY_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_SESSION_TERMINAL_BODY_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "SessionTerminal request body is too large"}),
        ));
    }

    match action {
        "read" => {
            let input: SessionTerminalReadRequest = match serde_json::from_slice(&body) {
                Ok(input) => input,
                Err(error) => {
                    return Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": format!("invalid SessionTerminal read request: {error}")}),
                    ));
                }
            };
            let Some(read) = session_terminals.read(&agent_id, input.cursor)? else {
                return Ok(json_response(
                    StatusCode(404),
                    &json!({"ok": false, "error": "session terminal not found"}),
                ));
            };
            let mut payload = serde_json::to_value(read)?;
            payload
                .as_object_mut()
                .expect("SessionTerminal read serializes as an object")
                .insert("ok".into(), serde_json::Value::Bool(true));
            Ok(json_response(StatusCode(200), &payload))
        }
        "input" => {
            let input: SessionTerminalInputRequest = match serde_json::from_slice(&body) {
                Ok(input) => input,
                Err(error) => {
                    return Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": format!("invalid SessionTerminal input request: {error}")}),
                    ));
                }
            };
            let data = match BASE64.decode(input.data.as_bytes()) {
                Ok(data) if data.len() <= MAX_INPUT_BYTES => data,
                Ok(_) => {
                    return Ok(json_response(
                        StatusCode(413),
                        &json!({"ok": false, "error": "SessionTerminal input is too large"}),
                    ));
                }
                Err(error) => {
                    return Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": format!("invalid SessionTerminal input: {error}")}),
                    ));
                }
            };
            Ok(session_terminal_operation_response(
                session_terminals.input(&agent_id, &data)?,
            ))
        }
        "resize" => {
            let input: SessionTerminalResizeRequest = match serde_json::from_slice(&body) {
                Ok(input) => input,
                Err(error) => {
                    return Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": format!("invalid SessionTerminal resize request: {error}")}),
                    ));
                }
            };
            Ok(session_terminal_operation_response(
                session_terminals.resize(&agent_id, input.cols, input.rows)?,
            ))
        }
        _ => unreachable!("SessionTerminal action was validated"),
    }
}

fn session_terminal_operation_response(operation: SessionTerminalOperation) -> HttpResponse {
    let status = if !operation.found {
        StatusCode(404)
    } else if !operation.accepted {
        StatusCode(409)
    } else {
        StatusCode(200)
    };
    json_response(
        status,
        &json!({
            "ok": operation.accepted,
            "state": operation.state,
            "error": operation.error,
        }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteStatusRequest {
    controller_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteStartRequest {
    fps: u8,
    scale: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteTokenRequest {
    controller_token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSettingsRequest {
    controller_token: String,
    fps: u8,
    scale: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteFrameRequest {
    controller_token: String,
    after_sequence: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteScreenshotRequest {
    scale: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteInputRequest {
    controller_token: String,
    events: Vec<RemoteInputEvent>,
}

fn remote_control_response(
    request: &mut Request,
    remote_control: &RemoteControlRuntime,
    path: &str,
) -> Result<HttpResponse> {
    let action = path
        .strip_prefix(REMOTE_CONTROL_PATH_PREFIX)
        .unwrap_or_default();
    let result = match action {
        "status" => {
            let input: RemoteStatusRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .status(input.controller_token.as_deref())
                .map(|status| json_response(StatusCode(200), &status))
        }
        "start" => {
            let input: RemoteStartRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .start(input.fps, input.scale)
                .map(|started| json_response(StatusCode(200), &started))
        }
        "stop" => {
            let input: RemoteTokenRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .stop(&input.controller_token)
                .map(|operation| json_response(StatusCode(200), &operation))
        }
        "keepalive" => {
            let input: RemoteTokenRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .keepalive(&input.controller_token)
                .map(|operation| json_response(StatusCode(200), &operation))
        }
        "settings" => {
            let input: RemoteSettingsRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .settings(&input.controller_token, input.fps, input.scale)
                .map(|operation| json_response(StatusCode(200), &operation))
        }
        "frame" => {
            let input: RemoteFrameRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .frame(&input.controller_token, input.after_sequence)
                .map(|frame| match frame {
                    Some(frame) => remote_frame_response(frame),
                    None => data_response(StatusCode(204), Vec::new(), vec![no_store()]),
                })
        }
        "screenshot" => {
            let input: RemoteScreenshotRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .screenshot(input.scale)
                .map(|frame| match frame {
                    Some(frame) => remote_frame_response(frame),
                    None => data_response(StatusCode(204), Vec::new(), vec![no_store()]),
                })
        }
        "input" => {
            let input: RemoteInputRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .input(&input.controller_token, &input.events)
                .map(|operation| json_response(StatusCode(200), &operation))
        }
        "release" => {
            let input: RemoteTokenRequest = read_json(request, MAX_REMOTE_CONTROL_BODY_BYTES)?;
            remote_control
                .release_inputs(&input.controller_token)
                .map(|operation| json_response(StatusCode(200), &operation))
        }
        _ => {
            return Ok(json_response(
                StatusCode(404),
                &json!({"ok": false, "error": "not found"}),
            ));
        }
    };
    Ok(result.unwrap_or_else(remote_control_error_response))
}

fn remote_control_error_response(error: crate::remote_control::RemoteControlError) -> HttpResponse {
    json_response(
        StatusCode(error.status()),
        &json!({"ok": false, "code": error.code(), "error": error.message()}),
    )
}

fn remote_frame_response(frame: crate::remote_control::RemoteFrame) -> HttpResponse {
    let headers = [
        ("X-Me-Remote-Sequence", frame.sequence.to_string()),
        ("X-Me-Screen-Width", frame.screen_width.to_string()),
        ("X-Me-Screen-Height", frame.screen_height.to_string()),
        ("X-Me-Frame-Width", frame.frame_width.to_string()),
        ("X-Me-Frame-Height", frame.frame_height.to_string()),
    ]
    .into_iter()
    .filter_map(|(name, value)| Header::from_bytes(name, value).ok())
    .chain([content_type("image/jpeg"), no_store()])
    .collect::<Vec<_>>();
    data_response(StatusCode(200), frame.jpeg.as_ref().clone(), headers)
}

fn sync_response(request: &mut Request, backend: &dyn UiBackend) -> Result<HttpResponse> {
    let accepts_gzip = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("Accept-Encoding"))
        .map(|header| header.value.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let accepts_gzip = accept_encoding_allows_gzip(&accepts_gzip);
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str());
    if content_type.is_none_or(|value| !value.starts_with("application/json")) {
        return Ok(json_response(
            StatusCode(415),
            &json!({"ok": false, "error": "Content-Type must be application/json"}),
        ));
    }
    let length = request.body_length().unwrap_or(0);
    if length > MAX_COMMAND_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "sync body is too large"}),
        ));
    }
    let mut body = Vec::with_capacity(length.min(MAX_COMMAND_BYTES));
    request
        .as_reader()
        .take((MAX_COMMAND_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_COMMAND_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "sync body is too large"}),
        ));
    }
    let sync: SyncRequest = match serde_json::from_slice(&body) {
        Ok(sync) => sync,
        Err(error) => {
            return Ok(json_response(
                StatusCode(400),
                &json!({"ok": false, "error": format!("invalid sync request: {error}")}),
            ));
        }
    };
    let payload = sync_state_payload(
        backend,
        sync.snapshot_revision,
        sync.agents,
        sync.selected_agent,
        sync.terminal_session,
        sync.terminal_revision,
        sync.cache_metadata_only,
    )?;
    Ok(sync_json_response(StatusCode(200), &payload, accepts_gzip))
}

#[derive(Serialize)]
struct SnapshotResponse {
    ok: bool,
    revision: u64,
    environment: EnvironmentMetadata,
    agents: Vec<AgentMetadata>,
    models: Vec<ModelMetadata>,
    orchestrators: Vec<String>,
    default_orchestrator: String,
    chatbot_default_static_prompt: &'static str,
    tool_visibility: ToolVisibilityMetadata,
}

#[derive(Serialize)]
struct ToolVisibilityMetadata {
    hidden_names: &'static [&'static str],
    hidden_prefixes: &'static [&'static str],
    activity_names: &'static [&'static str],
}

#[derive(Serialize)]
struct EnvironmentMetadata {
    workspace: String,
    system: String,
}

#[derive(Serialize)]
struct AgentMetadata {
    id: String,
    title: Option<String>,
    kind: String,
    parent_agent_id: Option<String>,
    orchestrator: String,
    edb_path: String,
    edb_size_bytes: u64,
    event_count: usize,
    last_event_id: Option<EventId>,
    last_event_hash: Option<String>,
    mutation_revision: u64,
    prompt_submission_revision: u64,
    input_draft: String,
    input_draft_revision: u64,
}

#[derive(Serialize)]
struct ModelMetadata {
    name: String,
    context_window: u64,
    reasoning_efforts: Vec<String>,
    output_token_reservations: std::collections::BTreeMap<String, u64>,
}

fn snapshot_response(backend: &dyn UiBackend) -> Result<HttpResponse> {
    Ok(json_response(
        StatusCode(200),
        &snapshot_metadata(backend.snapshot()?, false),
    ))
}

fn snapshot_metadata(snapshot: UiSnapshot, include_event_hashes: bool) -> SnapshotResponse {
    let UiSnapshot {
        revision,
        environment,
        agents,
        models,
        orchestrators,
        default_orchestrator,
    } = snapshot;
    let agents = agents
        .into_iter()
        .map(|agent| AgentMetadata {
            id: agent.id.to_string(),
            title: agent.title,
            kind: agent.kind.to_string(),
            parent_agent_id: agent.parent_agent_id.map(|id| id.to_string()),
            orchestrator: agent.orchestrator_name,
            edb_path: crate::host_path::public_host_path(&agent.edb_path),
            edb_size_bytes: agent.edb_size_bytes,
            event_count: agent.events.len(),
            last_event_id: agent.events.last().map(Event::id),
            last_event_hash: include_event_hashes
                .then(|| agent.events.last().map(EventBase::getHash))
                .flatten(),
            mutation_revision: agent.mutation_revision,
            prompt_submission_revision: agent.prompt_submission_revision,
            input_draft: agent.input_draft,
            input_draft_revision: agent.input_draft_revision,
        })
        .collect();
    let models = models.iter().map(model_metadata).collect::<Vec<_>>();
    SnapshotResponse {
        ok: true,
        revision,
        environment: EnvironmentMetadata {
            workspace: crate::host_path::public_host_path(&environment.workspace),
            system: format!("{}/{}", environment.os, environment.arch),
        },
        agents,
        models,
        orchestrators: orchestrators.to_vec(),
        default_orchestrator,
        chatbot_default_static_prompt: crate::orchestrator::CHATBOT_DEFAULT_STATIC_PROMPT,
        tool_visibility: ToolVisibilityMetadata {
            hidden_names: CHAT_HIDDEN_TOOL_NAMES,
            hidden_prefixes: CHAT_HIDDEN_TOOL_PREFIXES,
            activity_names: CHAT_ACTIVITY_TOOL_NAMES,
        },
    }
}

fn model_metadata(model: &UiModelOption) -> ModelMetadata {
    ModelMetadata {
        name: model.name.clone(),
        context_window: model.context_window,
        reasoning_efforts: model.reasoning_efforts.clone(),
        output_token_reservations: model.output_token_reservations.clone(),
    }
}

#[derive(Serialize)]
struct EventsResponse<'a> {
    ok: bool,
    reset: bool,
    event_count: usize,
    mutation_revision: u64,
    turn_history_updated: bool,
    turn_history: Option<String>,
    events: &'a [Event],
}

fn events_response(
    backend: &dyn UiBackend,
    id: &AgentId,
    query: Option<&str>,
) -> Result<HttpResponse> {
    let snapshot = backend.snapshot()?;
    let agent = snapshot
        .agent(id)
        .ok_or_else(|| format!("Agent {id} does not exist"))?;
    let after = query_value(query, "after")
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| "invalid after EventOrder")?
        .unwrap_or(0);
    let known_mutation = query_value(query, "mutation")
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| "invalid mutation revision")?;
    let reset = known_mutation != Some(agent.mutation_revision) || after > agent.events.len();
    let start = if reset { 0 } else { after };
    let events = &agent.events[start..];
    let turn_history_updated = turn_history_needs_refresh(reset, start, events);
    let turn_history = if turn_history_updated && agent.orchestrator_name != "worker-agent" {
        turn_history::latest_snapshot(&agent.events)?
    } else {
        None
    };
    Ok(json_response(
        StatusCode(200),
        &EventsResponse {
            ok: true,
            reset,
            event_count: agent.events.len(),
            mutation_revision: agent.mutation_revision,
            turn_history_updated,
            turn_history,
            events,
        },
    ))
}

fn turn_history_needs_refresh(reset: bool, start: usize, events: &[Event]) -> bool {
    reset
        || start == 0
        || events.iter().any(|event| {
            matches!(event, Event::ContextCleared(_))
                || matches!(event, Event::CompactStateUpdate(update) if update.state == crate::event::CompactState::Completed)
        })
}

fn terminal_frame_response(backend: &dyn UiBackend, path: &str) -> Result<HttpResponse> {
    let suffix = path
        .strip_prefix("/api/terminal/")
        .ok_or("invalid terminal frame path")?;
    let (agent, session) = suffix
        .split_once('/')
        .ok_or("terminal frame path requires Agent and session IDs")?;
    if agent.is_empty() || session.is_empty() || session.contains('/') {
        return Err("invalid terminal frame path".into());
    }
    let agent = AgentId::new(agent)?;
    Ok(json_response(
        StatusCode(200),
        &json!({"ok": true, "frame": backend.terminal_frame(&agent, session)?}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "command")]
enum WebCommand {
    UpdateInputDraft {
        agent_id: String,
        expected_revision: u64,
        content: String,
    },
    SubmitUserPrompt {
        agent_id: String,
        content: String,
    },
    ChangeEffort {
        agent_id: String,
        effort: String,
    },
    ChangeModel {
        agent_id: String,
        model: String,
    },
    ChangeSystemStaticPrompt {
        agent_id: String,
        mode: SystemStaticPromptMode,
        content: Option<String>,
    },
    ClearContext {
        agent_id: String,
    },
    RewindContext {
        agent_id: String,
        event_id: EventId,
    },
    CloneAgent {
        agent_id: String,
        final_answer_event_id: EventId,
    },
    DeleteTurn {
        agent_id: String,
        prompt_id: EventId,
    },
    Regenerate {
        agent_id: String,
        final_answer_event_id: EventId,
    },
    AbortTurn {
        agent_id: String,
    },
    AddAgent {
        orchestrator: String,
    },
    DeleteAgent {
        agent_id: String,
    },
}

fn command_response(
    request: &mut Request,
    commands: &dyn UiCommandGateway,
) -> Result<HttpResponse> {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str());
    if content_type.is_none_or(|value| !value.starts_with("application/json")) {
        return Ok(json_response(
            StatusCode(415),
            &json!({"ok": false, "error": "Content-Type must be application/json"}),
        ));
    }
    let length = request.body_length().unwrap_or(0);
    if length > MAX_COMMAND_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "command body is too large"}),
        ));
    }
    let mut body = Vec::with_capacity(length.min(MAX_COMMAND_BYTES));
    request
        .as_reader()
        .take((MAX_COMMAND_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_COMMAND_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "command body is too large"}),
        ));
    }
    let command: WebCommand = match serde_json::from_slice(&body) {
        Ok(command) => command,
        Err(error) => {
            return Ok(json_response(
                StatusCode(400),
                &json!({"ok": false, "error": format!("invalid command: {error}")}),
            ));
        }
    };
    let command = into_ui_command(command)?;
    let receipt = receipt_json(commands.submit(command)?);
    Ok(json_response(
        StatusCode(200),
        &json!({"ok": true, "receipt": receipt}),
    ))
}

fn receipt_json(receipt: UiCommandReceipt) -> serde_json::Value {
    match receipt {
        UiCommandReceipt::Accepted => json!({"kind": "accepted"}),
        UiCommandReceipt::InputDraftUpdated { accepted, revision } => json!({
            "kind": "input_draft_updated",
            "accepted": accepted,
            "input_draft_revision": revision,
        }),
        UiCommandReceipt::UserPromptSubmitted {
            prompt_revision,
            input_draft_revision,
        } => json!({
            "kind": "user_prompt_submitted",
            "prompt_submission_revision": prompt_revision,
            "input_draft_revision": input_draft_revision,
        }),
        UiCommandReceipt::AbortRequested(requested) => {
            json!({"kind": "abort_requested", "requested": requested})
        }
        UiCommandReceipt::AgentCreated(draft) => json!({
            "kind": "agent_created",
            "agent_id": draft.id.to_string(),
            "edb_path": crate::host_path::public_host_path(&draft.edb_path),
        }),
    }
}

fn into_ui_command(command: WebCommand) -> Result<UiCommand> {
    Ok(match command {
        WebCommand::UpdateInputDraft {
            agent_id,
            expected_revision,
            content,
        } => UiCommand::UpdateInputDraft {
            agent_id: AgentId::new(agent_id)?,
            expected_revision,
            content,
        },
        WebCommand::SubmitUserPrompt { agent_id, content } => UiCommand::SubmitUserPrompt {
            agent_id: AgentId::new(agent_id)?,
            content,
        },
        WebCommand::ChangeEffort { agent_id, effort } => UiCommand::ChangeEffort {
            agent_id: AgentId::new(agent_id)?,
            effort,
        },
        WebCommand::ChangeModel { agent_id, model } => UiCommand::ChangeModel {
            agent_id: AgentId::new(agent_id)?,
            model,
        },
        WebCommand::ChangeSystemStaticPrompt {
            agent_id,
            mode,
            content,
        } => {
            validate_system_static_prompt_change(mode, content.as_deref())?;
            UiCommand::ChangeSystemStaticPrompt {
                agent_id: AgentId::new(agent_id)?,
                mode,
                content,
            }
        }
        WebCommand::ClearContext { agent_id } => UiCommand::ClearContext {
            agent_id: AgentId::new(agent_id)?,
        },
        WebCommand::RewindContext { agent_id, event_id } => UiCommand::RewindContext {
            agent_id: AgentId::new(agent_id)?,
            event_id,
        },
        WebCommand::CloneAgent {
            agent_id,
            final_answer_event_id,
        } => UiCommand::CloneAgent {
            agent_id: AgentId::new(agent_id)?,
            final_answer_event_id,
        },
        WebCommand::DeleteTurn {
            agent_id,
            prompt_id,
        } => UiCommand::DeleteTurn {
            agent_id: AgentId::new(agent_id)?,
            prompt_id,
        },
        WebCommand::Regenerate {
            agent_id,
            final_answer_event_id,
        } => UiCommand::Regenerate {
            agent_id: AgentId::new(agent_id)?,
            final_answer_event_id,
        },
        WebCommand::AbortTurn { agent_id } => UiCommand::AbortTurn {
            agent_id: AgentId::new(agent_id)?,
        },
        WebCommand::AddAgent { orchestrator } => UiCommand::AddAgent { orchestrator },
        WebCommand::DeleteAgent { agent_id } => UiCommand::DeleteAgent {
            agent_id: AgentId::new(agent_id)?,
        },
    })
}

fn parse_agent_path(path: &str, prefix: &str) -> Result<AgentId> {
    let value = path
        .strip_prefix(prefix)
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .ok_or("invalid Agent path")?;
    AgentId::new(value)
}

fn split_url(url: &str) -> (&str, Option<&str>) {
    url.split_once('?')
        .map_or((url, None), |(path, query)| (path, Some(query)))
}

fn query_value<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (candidate, value) = pair.split_once('=')?;
        (candidate == key).then_some(value)
    })
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
    #[serde(default)]
    browser_port: Option<u16>,
}

fn auth_status_response(request: &Request, auth: &WebSessionAuth) -> Result<HttpResponse> {
    Ok(json_response(
        StatusCode(200),
        &json!({
            "ok": true,
            "required": auth.required(),
            "authenticated": auth.authorized_any(request_session_tokens(
                request,
                auth.cookie_prefix(),
            )),
        }),
    ))
}

fn login_response(request: &mut Request, auth: &WebSessionAuth) -> Result<HttpResponse> {
    if !auth.required() {
        return Ok(json_response(
            StatusCode(200),
            &json!({"ok": true, "authenticated": true}),
        ));
    }
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str());
    if content_type.is_none_or(|value| !value.starts_with("application/json")) {
        return Ok(json_response(
            StatusCode(415),
            &json!({"ok": false, "error": "Content-Type must be application/json"}),
        ));
    }
    let length = request.body_length().unwrap_or(0);
    if length > MAX_LOGIN_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "login body is too large"}),
        ));
    }
    let mut body = Vec::with_capacity(length.min(MAX_LOGIN_BYTES));
    request
        .as_reader()
        .take((MAX_LOGIN_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_LOGIN_BYTES {
        return Ok(json_response(
            StatusCode(413),
            &json!({"ok": false, "error": "login body is too large"}),
        ));
    }
    let login: LoginRequest = match serde_json::from_slice(&body) {
        Ok(login) => login,
        Err(error) => {
            return Ok(json_response(
                StatusCode(400),
                &json!({"ok": false, "error": format!("invalid login request: {error}")}),
            ));
        }
    };
    let cookie_name = match login.browser_port {
        Some(0) => {
            return Ok(json_response(
                StatusCode(400),
                &json!({"ok": false, "error": "browser_port must be nonzero"}),
            ));
        }
        Some(port) => auth.cookie_name_for_port(port),
        None => auth.cookie_name().to_owned(),
    };
    let Some(token) = auth.login(&login.password)? else {
        return Ok(json_response(
            StatusCode(401),
            &json!({"ok": false, "error": "密码错误"}),
        ));
    };
    Ok(
        json_response(StatusCode(200), &json!({"ok": true, "authenticated": true}))
            .with_header(session_cookie(&cookie_name, &token)),
    )
}

fn request_session_tokens<'a>(
    request: &'a Request,
    prefix: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("Cookie"))
        .flat_map(|header| header.value.as_str().split(';'))
        .filter_map(move |cookie| {
            let (candidate, value) = cookie.trim().split_once('=')?;
            let port = candidate.strip_prefix(prefix)?.strip_prefix("_p")?;
            port.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .map(|_| value)
        })
}

fn session_cookie(name: &str, token: &str) -> Header {
    Header::from_bytes(
        "Set-Cookie",
        format!("{name}={token}; Path=/; HttpOnly; SameSite=Strict"),
    )
    .expect("session cookie is generated from ASCII bytes")
}

fn unauthorized_response() -> HttpResponse {
    json_response(
        StatusCode(401),
        &json!({"ok": false, "error": "WebUI authentication required"}),
    )
}

fn accept_encoding_allows_gzip(value: &str) -> bool {
    let mut gzip_quality: Option<f32> = None;
    let mut wildcard_quality: Option<f32> = None;
    for coding in value.split(',') {
        let mut parts = coding.split(';');
        let name = parts.next().unwrap_or_default().trim();
        let mut quality = 1.0_f32;
        for parameter in parts {
            let Some((key, value)) = parameter.split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("q") {
                quality = value
                    .trim()
                    .parse::<f32>()
                    .ok()
                    .filter(|quality| (0.0..=1.0).contains(quality))
                    .unwrap_or(0.0);
            }
        }
        let slot = if name.eq_ignore_ascii_case("gzip") {
            Some(&mut gzip_quality)
        } else if name == "*" {
            Some(&mut wildcard_quality)
        } else {
            None
        };
        if let Some(slot) = slot {
            *slot = Some(slot.map_or(quality, |current| current.max(quality)));
        }
    }
    gzip_quality
        .or(wildcard_quality)
        .is_some_and(|quality| quality > 0.0)
}

fn sync_json_response(
    status: StatusCode,
    value: &impl Serialize,
    accepts_gzip: bool,
) -> HttpResponse {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|error| format!(r#"{{"ok":false,"error":"{error}"}}"#).into_bytes());
    let (body, compressed) = if accepts_gzip {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        match encoder.write_all(&body).and_then(|()| encoder.finish()) {
            Ok(compressed) => (compressed, true),
            Err(_) => (body, false),
        }
    } else {
        (body, false)
    };
    let mut response = data_response(
        status,
        body,
        vec![
            content_type("application/json; charset=utf-8"),
            Header::from_bytes("Vary", "Accept-Encoding").expect("static Vary header is valid"),
            no_store(),
        ],
    );
    if compressed {
        response = response.with_header(
            Header::from_bytes("Content-Encoding", "gzip")
                .expect("static Content-Encoding header is valid"),
        );
    }
    response
}

fn json_response(status: StatusCode, value: &impl Serialize) -> HttpResponse {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|error| format!(r#"{{"ok":false,"error":"{error}"}}"#).into_bytes());
    data_response(
        status,
        body,
        vec![content_type("application/json; charset=utf-8"), no_store()],
    )
}

fn text_response(content_type_value: &'static str, content: &'static str) -> HttpResponse {
    data_response(
        StatusCode(200),
        content.as_bytes().to_vec(),
        vec![content_type(content_type_value), no_store()],
    )
}

fn bytes_response(content_type_value: &'static str, content: &'static [u8]) -> HttpResponse {
    data_response(
        StatusCode(200),
        content.to_vec(),
        vec![content_type(content_type_value), no_store()],
    )
}

fn data_response(status: StatusCode, body: Vec<u8>, headers: Vec<Header>) -> HttpResponse {
    let length = body.len();
    Response::new(
        status,
        headers,
        Box::new(std::io::Cursor::new(body)),
        Some(length),
        None,
    )
}

fn content_type(value: &'static str) -> Header {
    Header::from_bytes("Content-Type", value).expect("static Content-Type header is valid")
}

fn no_store() -> Header {
    Header::from_bytes("Cache-Control", "no-store").expect("static Cache-Control header is valid")
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Read as _, net::TcpListener, path::PathBuf, thread};

    use super::*;
    use crate::{
        config::{ModelCapabilities, ModelConfig, ProviderType, WorkspaceConfig},
        event::UserPromptEvent,
        ui_backend::{UiAgentSnapshot, UiApiActivity, UiEnvironment, workspace_ui_ports},
        workspace::Workspace,
    };

    #[derive(Clone)]
    struct SnapshotBackend(UiSnapshot);

    impl UiBackend for SnapshotBackend {
        fn snapshot(&self) -> Result<UiSnapshot> {
            Ok(self.0.clone())
        }

        fn agent_ids(&self) -> Result<Vec<AgentId>> {
            Ok(self.0.agent_ids())
        }

        fn api_activity(&self, _agent_id: &AgentId) -> Result<UiApiActivity> {
            Ok(UiApiActivity::default())
        }

        fn terminal_sessions(
            &self,
            _agent_id: &AgentId,
        ) -> Result<Vec<crate::terminal::TerminalSessionPreview>> {
            Ok(Vec::new())
        }

        fn terminal_frame(
            &self,
            _agent_id: &AgentId,
            _session_id: &str,
        ) -> Result<Option<crate::terminal::TerminalFrame>> {
            Ok(None)
        }

        fn terminal_backend(&self, _agent_id: &AgentId) -> Result<Option<String>> {
            Ok(None)
        }

        fn deletion_blocker(&self, _agent_id: &AgentId) -> Result<Option<String>> {
            Ok(None)
        }
    }

    fn model() -> ModelConfig {
        ModelConfig {
            name: "test".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "http://127.0.0.1:1/v1".into(),
            endpoint: "chat/completions".into(),
            api_key: Some("must-not-reach-webui".into()),
            api_key_env: None,
            credential_file: None,
            model: "test".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: ModelCapabilities {
                context_window: 4096,
                reasoning_efforts: vec!["unset".into(), "high".into()],
                ..Default::default()
            },
            parameters: toml::from_str("max_tokens = 512").unwrap(),
            effort_parameters: Default::default(),
        }
    }

    fn workspace() -> std::path::PathBuf {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-webui-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        directory
    }

    fn sync_test_events(count: u64) -> Vec<Event> {
        (1..=count)
            .map(|id| {
                Event::UserPrompt(UserPromptEvent {
                    id,
                    timestamp_ms: id,
                    content: format!("event-{id}"),
                })
            })
            .collect()
    }

    fn sync_test_backend(events: Vec<Event>, mutation_revision: u64) -> SnapshotBackend {
        sync_test_backend_for_orchestrator(
            events,
            mutation_revision,
            crate::event::AgentKind::SubAgent,
            "worker-agent",
        )
    }

    fn sync_test_backend_for_orchestrator(
        events: Vec<Event>,
        mutation_revision: u64,
        kind: crate::event::AgentKind,
        orchestrator_name: &str,
    ) -> SnapshotBackend {
        SnapshotBackend(UiSnapshot {
            revision: 7,
            environment: Arc::new(UiEnvironment {
                workspace: PathBuf::from("/cache/workspace"),
                os: "test".into(),
                arch: "test".into(),
            }),
            agents: vec![UiAgentSnapshot {
                id: AgentId::new("main").unwrap(),
                title: Some("Cached session".into()),
                kind,
                parent_agent_id: None,
                orchestrator_name: orchestrator_name.into(),
                edb_path: PathBuf::from("main.edb"),
                edb_size_bytes: 0,
                mutation_revision,
                last_mutation: None,
                prompt_submission_revision: 0,
                input_draft: String::new(),
                input_draft_revision: 0,
                events: events.into(),
            }],
            models: Arc::from([]),
            orchestrators: Arc::from([]),
            default_orchestrator: "main-agent".into(),
        })
    }

    #[test]
    fn url_helpers_reject_ambiguous_agent_paths() {
        assert_eq!(
            split_url("/api/events/main?after=3&mutation=2"),
            ("/api/events/main", Some("after=3&mutation=2"))
        );
        assert_eq!(
            query_value(Some("after=3&mutation=2"), "mutation"),
            Some("2")
        );
        assert_eq!(
            parse_agent_path("/api/events/agent-a", "/api/events/")
                .unwrap()
                .as_str(),
            "agent-a"
        );
        assert!(parse_agent_path("/api/events/a/b", "/api/events/").is_err());
    }

    #[test]
    fn command_json_maps_to_the_isolated_gateway_protocol() {
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "rewind_context",
            "agent_id": "main",
            "event_id": 42
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::RewindContext {
                agent_id: AgentId::new("main").unwrap(),
                event_id: 42,
            }
        );
        assert!(
            serde_json::from_value::<WebCommand>(json!({
                "command": "update_input_draft",
                "agent_id": "main",
                "content": "stale"
            }))
            .is_err()
        );
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "update_input_draft",
            "agent_id": "main",
            "expected_revision": 7,
            "content": "line one\nline two"
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::UpdateInputDraft {
                agent_id: AgentId::new("main").unwrap(),
                expected_revision: 7,
                content: "line one\nline two".into(),
            }
        );
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "add_agent",
            "orchestrator": "chatbot"
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::AddAgent {
                orchestrator: "chatbot".into()
            }
        );
        assert!(serde_json::from_value::<WebCommand>(json!({"command": "add_agent"})).is_err());
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "clone_agent",
            "agent_id": "main",
            "final_answer_event_id": 51
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::CloneAgent {
                agent_id: AgentId::new("main").unwrap(),
                final_answer_event_id: 51,
            }
        );
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "delete_turn",
            "agent_id": "main",
            "prompt_id": 17
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::DeleteTurn {
                agent_id: AgentId::new("main").unwrap(),
                prompt_id: 17,
            }
        );
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "regenerate",
            "agent_id": "main",
            "final_answer_event_id": 51
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::Regenerate {
                agent_id: AgentId::new("main").unwrap(),
                final_answer_event_id: 51,
            }
        );
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "change_system_static_prompt",
            "agent_id": "chat",
            "mode": "Custom",
            "content": "# Custom\n\n完整内容"
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::ChangeSystemStaticPrompt {
                agent_id: AgentId::new("chat").unwrap(),
                mode: SystemStaticPromptMode::Custom,
                content: Some("# Custom\n\n完整内容".into()),
            }
        );
        let parsed: WebCommand = serde_json::from_value(json!({
            "command": "change_system_static_prompt",
            "agent_id": "chat",
            "mode": "Default"
        }))
        .unwrap();
        assert_eq!(
            into_ui_command(parsed).unwrap(),
            UiCommand::ChangeSystemStaticPrompt {
                agent_id: AgentId::new("chat").unwrap(),
                mode: SystemStaticPromptMode::Default,
                content: None,
            }
        );
        assert!(
            serde_json::from_value::<WebCommand>(json!({
                "command": "change_system_static_prompt",
                "agent_id": "chat",
                "mode": "Unknown",
                "content": "invalid"
            }))
            .is_err()
        );
        let invalid_custom: WebCommand = serde_json::from_value(json!({
            "command": "change_system_static_prompt",
            "agent_id": "chat",
            "mode": "Custom",
            "content": "   "
        }))
        .unwrap();
        assert!(into_ui_command(invalid_custom).is_err());
        let invalid_default: WebCommand = serde_json::from_value(json!({
            "command": "change_system_static_prompt",
            "agent_id": "chat",
            "mode": "Default",
            "content": "not allowed"
        }))
        .unwrap();
        assert!(into_ui_command(invalid_default).is_err());

        assert_eq!(
            receipt_json(UiCommandReceipt::UserPromptSubmitted {
                prompt_revision: 7,
                input_draft_revision: 11,
            }),
            json!({
                "kind": "user_prompt_submitted",
                "prompt_submission_revision": 7,
                "input_draft_revision": 11,
            })
        );
        assert_eq!(
            receipt_json(UiCommandReceipt::InputDraftUpdated {
                accepted: false,
                revision: 9,
            }),
            json!({
                "kind": "input_draft_updated",
                "accepted": false,
                "input_draft_revision": 9,
            })
        );
    }

    #[test]
    fn http_sync_protocol_requires_agent_revisions() {
        let parsed: SyncRequest = serde_json::from_value(json!({
            "snapshot_revision": 4,
            "agents": [{"id": "main", "event_count": 9, "mutation_revision": 2}],
            "selected_agent": "main",
            "terminal_session": null,
            "terminal_revision": null,
        }))
        .unwrap();
        assert_eq!(parsed.snapshot_revision, Some(4));
        assert!(!parsed.cache_metadata_only);
        assert_eq!(parsed.agents[0].cursor_event_hash, None);
        assert!(
            serde_json::from_value::<SyncRequest>(json!({
                "snapshot_revision": null,
                "agents": [{"id": "main", "event_count": 9}],
                "selected_agent": null,
                "terminal_session": null,
                "terminal_revision": null,
            }))
            .is_err()
        );
    }

    #[test]
    fn http_sync_metadata_only_returns_authoritative_cache_metadata_without_events() {
        let events = sync_test_events(3);
        let last_hash = events.last().unwrap().getHash();
        let backend = sync_test_backend(events, 4);
        let payload = sync_state_payload(
            &backend,
            None,
            Vec::new(),
            Some("main".into()),
            None,
            None,
            true,
        )
        .unwrap();

        assert_eq!(payload["cache_metadata_only"], true);
        assert_eq!(payload["event_updates"], json!([]));
        assert_eq!(payload["more_events"], false);
        assert_eq!(
            payload["snapshot"]["environment"]["workspace"],
            "/cache/workspace"
        );
        assert_eq!(
            payload["snapshot"]["agents"][0]["last_event_hash"],
            last_hash
        );
        let ordinary =
            serde_json::to_value(snapshot_metadata(backend.snapshot().unwrap(), false)).unwrap();
        assert_eq!(
            ordinary["agents"][0]["last_event_hash"],
            serde_json::Value::Null
        );
        assert_eq!(
            payload["snapshot"]["chatbot_default_static_prompt"],
            crate::orchestrator::CHATBOT_DEFAULT_STATIC_PROMPT
        );
        assert_eq!(
            ordinary["chatbot_default_static_prompt"],
            crate::orchestrator::CHATBOT_DEFAULT_STATIC_PROMPT
        );
    }

    #[test]
    fn http_sync_validates_cached_prefix_hash_and_keeps_old_clients_compatible() {
        let events = sync_test_events(3);
        let first_hash = events[0].getHash();
        let final_hash = events[2].getHash();
        let backend = sync_test_backend(events, 4);
        let cursor = |cursor_event_hash| SyncAgentCursor {
            id: "main".into(),
            event_count: 1,
            mutation_revision: 4,
            cursor_event_hash,
        };

        let valid = sync_state_payload(
            &backend,
            Some(7),
            vec![cursor(Some(first_hash))],
            Some("main".into()),
            None,
            None,
            false,
        )
        .unwrap();
        let update = &valid["event_updates"][0];
        assert_eq!(update["reset"], false);
        assert_eq!(update["events"].as_array().unwrap().len(), 2);
        assert_eq!(update["cursor_event_hash"], final_hash);

        let legacy = sync_state_payload(
            &backend,
            Some(7),
            vec![cursor(None)],
            Some("main".into()),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(legacy["event_updates"][0]["reset"], false);

        let invalid = sync_state_payload(
            &backend,
            Some(7),
            vec![cursor(Some("wrong-prefix".into()))],
            Some("main".into()),
            None,
            None,
            false,
        )
        .unwrap();
        let reset = &invalid["event_updates"][0];
        assert_eq!(reset["reset"], true);
        assert_eq!(reset["events"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn http_sync_full_cached_prefix_restores_turn_history_without_replaying_events() {
        let mut edb = crate::event::EventDataBase::new();
        edb.append_agent_kind_def(
            crate::event::AgentKind::Interactive,
            "main-agent",
            None,
            None,
        )
        .unwrap();
        let prompt = edb.append_user_prompt("remember cached turn").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let call = edb
            .append_tool_call(api, prompt, "compact", crate::compact::TOOL_NAME, "{}")
            .unwrap();
        edb.append_tool_result(call, crate::event::ToolResultState::Succeeded, None, "ok")
            .unwrap();
        let compact = edb
            .append_compact_started(call, prompt, crate::event::CompactKind::MainAgentMultiTurn)
            .unwrap();
        let compact_sections = [
            "analysis",
            "1. Primary Request and Intent\nintent",
            "2. Key Technical Context and Decisions\ndecisions",
            "3. Files, Code, and Artifacts\nfiles",
            "4. Problems, Investigations, and Resolutions\nproblems",
            "5. Current State and Continuation Plan\nnext",
        ];
        for (stage, content) in crate::event::CompactStage::MULTI_TURN
            .into_iter()
            .zip(compact_sections)
        {
            edb.append_compact_stage(compact, stage, content).unwrap();
        }
        let summary =
            crate::compact::merge_multi_turn_summary(compact_sections.into_iter().skip(1));
        edb.append_compact_terminal(compact, crate::event::CompactState::Completed, summary, "")
            .unwrap();
        let events = edb.events().to_vec();
        let event_count = events.len();
        let final_hash = events.last().unwrap().getHash();
        let backend = sync_test_backend_for_orchestrator(
            events,
            4,
            crate::event::AgentKind::Interactive,
            "main-agent",
        );

        let restored = sync_state_payload(
            &backend,
            Some(7),
            vec![SyncAgentCursor {
                id: "main".into(),
                event_count,
                mutation_revision: 4,
                cursor_event_hash: Some(final_hash.clone()),
            }],
            Some("main".into()),
            None,
            None,
            false,
        )
        .unwrap();
        let update = &restored["event_updates"][0];
        assert_eq!(update["reset"], false);
        assert_eq!(update["events"], json!([]));
        assert_eq!(update["cursor_event_hash"], final_hash);
        assert_eq!(update["turn_history_updated"], true);
        assert!(
            update["turn_history"]
                .as_str()
                .is_some_and(|history| history.contains("remember cached turn"))
        );

        let ordinary_poll = sync_state_payload(
            &backend,
            Some(7),
            vec![SyncAgentCursor {
                id: "main".into(),
                event_count,
                mutation_revision: 4,
                cursor_event_hash: None,
            }],
            Some("main".into()),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(ordinary_poll["event_updates"], json!([]));
    }

    #[test]
    fn http_sync_resets_invalid_length_mutation_and_authoritative_empty_edb() {
        let backend = sync_test_backend(sync_test_events(2), 4);
        for cursor in [
            SyncAgentCursor {
                id: "main".into(),
                event_count: 3,
                mutation_revision: 4,
                cursor_event_hash: None,
            },
            SyncAgentCursor {
                id: "main".into(),
                event_count: 1,
                mutation_revision: 5,
                cursor_event_hash: None,
            },
        ] {
            let payload = sync_state_payload(
                &backend,
                Some(7),
                vec![cursor],
                Some("main".into()),
                None,
                None,
                false,
            )
            .unwrap();
            assert_eq!(payload["event_updates"][0]["reset"], true);
        }

        let empty = sync_test_backend(Vec::new(), 9);
        let payload = sync_state_payload(
            &empty,
            Some(7),
            vec![SyncAgentCursor {
                id: "main".into(),
                event_count: 1,
                mutation_revision: 9,
                cursor_event_hash: Some("stale".into()),
            }],
            Some("main".into()),
            None,
            None,
            false,
        )
        .unwrap();
        let reset = &payload["event_updates"][0];
        assert_eq!(reset["reset"], true);
        assert_eq!(reset["event_count"], 0);
        assert_eq!(reset["cursor_event_hash"], serde_json::Value::Null);
        assert_eq!(reset["events"], json!([]));
    }

    #[test]
    fn embedded_webui_loads_the_raw_edb_cache_before_the_application() {
        let cache_script = INDEX_HTML.find("/edb-cache.js").unwrap();
        let app_script = INDEX_HTML.find("/app.js").unwrap();
        assert!(cache_script < app_script);
        assert!(INDEX_HTML.contains("id=\"open-settings\""));
        assert!(EDB_CACHE_JS.contains("const DB_NAME = \"me-edb-cache\""));
        assert!(EDB_CACHE_JS.contains("keyPath: [\"sessionKey\", \"order\"]"));
        assert!(APP_JS.contains("cache_metadata_only: !state.edbCacheInitialized"));
        assert!(APP_JS.contains("if (payload.reset || payload.events.length > 0) {"));
        assert!(APP_JS.contains("persistAgentEdb(meta, store, Boolean(payload.reset))"));
    }

    #[test]
    fn http_sync_gzip_negotiation_respects_quality_and_wildcards() {
        assert!(accept_encoding_allows_gzip("gzip"));
        assert!(accept_encoding_allows_gzip("br, gzip; q=0.7"));
        assert!(accept_encoding_allows_gzip("br, *;q=0.5"));
        assert!(!accept_encoding_allows_gzip(""));
        assert!(!accept_encoding_allows_gzip("br"));
        assert!(!accept_encoding_allows_gzip("gzip;q=0"));
        assert!(!accept_encoding_allows_gzip("gzip;q=0, *;q=1"));
        assert!(!accept_encoding_allows_gzip("gzip;q=invalid"));
    }

    #[test]
    fn http_sync_event_batches_are_bounded_without_splitting_events() {
        assert_eq!(MAX_EVENT_BATCH_BYTES, 1024 * 1024);
        let events = (1..=4)
            .map(|id| {
                Event::UserPrompt(UserPromptEvent {
                    id,
                    timestamp_ms: id,
                    content: "x".repeat(200),
                })
            })
            .collect::<Vec<_>>();
        let first_size = serde_json::to_vec(&events[0]).unwrap().len();
        let (count, bytes) = event_prefix_within_budget(&events, first_size + 1).unwrap();
        assert_eq!(count, 1);
        assert_eq!(bytes, first_size);

        let (count, bytes) = event_prefix_within_budget(&events, 1).unwrap();
        assert_eq!(count, 1, "one atomic event must always make progress");
        assert_eq!(bytes, first_size);
        assert_eq!(event_prefix_within_budget(&events, 0).unwrap(), (0, 0));

        let oversized = vec![Event::UserPrompt(UserPromptEvent {
            id: 9,
            timestamp_ms: 9,
            content: "x".repeat(MAX_EVENT_BATCH_BYTES + 1024),
        })];
        let (count, bytes) = event_prefix_within_budget(&oversized, MAX_EVENT_BATCH_BYTES).unwrap();
        assert_eq!(count, 1, "an oversized atomic event must still be sent");
        assert!(bytes > MAX_EVENT_BATCH_BYTES);
    }

    #[test]
    fn http_sync_initial_replay_prioritizes_the_selected_agent_in_small_batches() {
        fn agent(id: &str, first_event_id: u64) -> UiAgentSnapshot {
            let events = (0..4_000)
                .map(|offset| {
                    Event::UserPrompt(UserPromptEvent {
                        id: first_event_id + offset,
                        timestamp_ms: first_event_id + offset,
                        content: "large public WebUI history ".repeat(24),
                    })
                })
                .collect::<Vec<_>>();
            UiAgentSnapshot {
                id: AgentId::new(id).unwrap(),
                title: None,
                kind: crate::event::AgentKind::SubAgent,
                parent_agent_id: None,
                orchestrator_name: "worker-agent".into(),
                edb_path: PathBuf::from(format!("{id}.edb")),
                edb_size_bytes: 0,
                mutation_revision: 0,
                last_mutation: None,
                prompt_submission_revision: 0,
                input_draft: String::new(),
                input_draft_revision: 0,
                events: events.into(),
            }
        }

        let backend = SnapshotBackend(UiSnapshot {
            revision: 1,
            environment: Arc::new(UiEnvironment {
                workspace: PathBuf::from("."),
                os: "test".into(),
                arch: "test".into(),
            }),
            agents: vec![agent("first", 1), agent("selected", 10_000)],
            models: Arc::from([]),
            orchestrators: Arc::from([]),
            default_orchestrator: "main-agent".into(),
        });
        let payload = sync_state_payload(
            &backend,
            None,
            Vec::new(),
            Some("selected".into()),
            None,
            None,
            false,
        )
        .unwrap();
        let updates = payload["event_updates"].as_array().unwrap();
        assert_eq!(updates[0]["agent_id"], "selected");
        assert!(payload["more_events"].as_bool().unwrap());
        assert!(updates[0]["events"].as_array().unwrap().len() < 4_000);
        assert!(serde_json::to_vec(&payload).unwrap().len() < MAX_EVENT_BATCH_BYTES + 32 * 1024);
    }

    #[test]
    fn embedded_webui_switches_to_the_mobile_layout_in_portrait() {
        assert!(INDEX_HTML.contains("interactive-widget=resizes-content"));
        assert!(INDEX_HTML.contains("viewport-fit=cover"));
        assert!(INDEX_HTML.contains("mobile-sidebar-toggle"));
        assert!(INDEX_HTML.contains("mobile-sidebar-backdrop"));
        assert!(STYLE_CSS.contains("@media (orientation: portrait)"));
        assert!(STYLE_CSS.contains("grid-template-rows: auto minmax(0, 1fr)"));
        assert!(STYLE_CSS.contains("transform: translateX(-105%)"));
        assert!(STYLE_CSS.contains("body.mobile-sidebar-open .sidebar"));
        assert!(STYLE_CSS.contains("env(safe-area-inset-bottom)"));
        assert!(STYLE_CSS.contains("overflow: hidden; overscroll-behavior: none;"));
        assert!(STYLE_CSS.contains("body { position: fixed; inset: 0; }"));
        assert!(STYLE_CSS.contains("height: 100%; height: 100dvh; min-height: 0;"));
        assert!(
            STYLE_CSS.contains(
                ".login-screen { display: grid; width: 100%; height: 100%; min-height: 0;"
            )
        );
        assert!(STYLE_CSS.contains("overflow: auto; overscroll-behavior: contain; padding: 24px;"));
        assert!(STYLE_CSS.contains(
            ".transcript { contain: layout paint style; flex: 1; min-height: 0; overflow: auto;"
        ));
        assert!(APP_JS.contains("const PORTRAIT_LAYOUT = matchMedia(\"(orientation: portrait)\")"));
        assert!(APP_JS.contains("agent.title || agent.id"));
        assert!(APP_JS.contains("function toolIsChatVisible(name)"));
    }

    #[test]
    fn embedded_webui_offers_a_cookie_backed_send_shortcut_preference() {
        assert!(INDEX_HTML.contains("Enter 换行 · Shift/Alt+Enter 发送"));
        assert!(
            APP_JS.contains(
                "const SEND_SHORTCUT_COOKIE = portScopedCookieName(\"me_send_shortcut\")"
            )
        );
        assert!(APP_JS.contains("protocol === \"https:\" ? \"443\" : \"80\""));
        assert!(APP_JS.contains("Max-Age=31536000; Path=/; SameSite=Lax"));
        assert!(APP_JS.contains("openChoiceDrawer(\"发送设置\""));
        assert!(
            APP_JS.contains("elements.send.addEventListener(\"click\", submitOrOpenSendSettings)")
        );
        assert!(APP_JS.contains("if (enterSubmitsPrompt(event))"));
        assert!(!APP_JS.contains("visible && enterSubmitsPrompt(event)"));
        assert!(!APP_JS.contains("openSlashCommand"));
        assert!(APP_JS.contains("sendShortcutPressed(event, state.sendShortcut)"));
        assert!(APP_JS.contains("state.composing || event.isComposing || event.keyCode === 229"));
        assert!(!APP_JS.contains("enterSubmitsInCurrentLayout"));
    }

    #[test]
    fn embedded_webui_keeps_legacy_webkit_startup_compatible() {
        assert!(!APP_JS.contains(".at(-1)"));
        assert!(!APP_JS.contains(".replaceAll("));
        assert!(!APP_JS.contains(".replaceChildren("));
        assert!(!MARKDOWN_JS.contains(".at(-1)"));
        assert!(!MARKDOWN_JS.contains(".replaceAll("));
        assert!(APP_JS.contains("typeof PORTRAIT_LAYOUT.addEventListener === \"function\""));
        assert!(APP_JS.contains("typeof PORTRAIT_LAYOUT.addListener === \"function\""));
        assert!(TRANSCRIPT_JS.contains("if (typeof ResizeObserver === \"function\")"));
        assert!(TRANSCRIPT_JS.contains("return { observe() {}, disconnect() {} };"));
    }

    #[test]
    fn embedded_webui_uses_the_shared_chat_tool_visibility_policy() {
        assert_eq!(CHAT_HIDDEN_TOOL_NAMES, &[crate::agent_title::TOOL_NAME]);
        assert_eq!(CHAT_HIDDEN_TOOL_PREFIXES, &["WorkMap.", "Worker."]);
        assert_eq!(
            CHAT_ACTIVITY_TOOL_NAMES,
            &[crate::agent_toolbox::WORKER_WAIT]
        );
        assert!(APP_JS.contains("state.snapshot.tool_visibility"));
        assert!(APP_JS.contains("_hiddenTools: new Set()"));
        assert!(APP_JS.contains("!toolIsChatVisible(value.name)"));
        assert!(APP_JS.contains("function renderWorkerActivity(wait)"));
        assert!(APP_JS.contains("function workerWaitIsVisible(wait)"));
        assert!(APP_JS.contains("function workerActivityIndex(worker)"));
        assert!(APP_JS.contains("function updateWorkerActivityNode(node, wait)"));
        assert!(APP_JS.contains("node.className = `worker-activity ${view.status}`"));
        assert!(APP_JS.contains("data-worker-tool="));
        assert!(!APP_JS.contains("workerActivityCache.clear()"));
        assert!(APP_JS.contains("kind === \"ManagerPrompt\""));
        assert!(APP_JS.contains("index.byPromptId.get(targetTurnId)"));
        assert!(APP_JS.contains("? \"已完成\""));
        assert!(APP_JS.contains("? \"未完成\" : \"正在执行\""));
        assert!(!APP_JS.contains("Worker 正在执行"));
        assert!(!APP_JS.contains("Worker 已完成"));
        assert!(!APP_JS.contains("Worker 已中断"));
        assert!(!APP_JS.contains("Worker 未完成"));
        assert!(APP_JS.contains("brief: toolBrief(tool),"));
        assert!(
            APP_JS
                .contains("return MeToolPresenters.summarize(tool.name, tool.args || {}).summary;")
        );
        assert!(APP_JS.contains("<span class=\"worker-tool-marker\">●</span>"));
        assert!(!APP_JS.contains("Worker 执行完成"));
        assert!(!APP_JS.contains("Worker 执行失败"));
        assert!(APP_JS.contains("agent.parent_agent_id === state.selectedAgent"));
        assert!(STYLE_CSS.contains(".worker-activity-tools"));
        assert!(STYLE_CSS.contains(".worker-tool-name { color: var(--text); font-weight: 400; }"));
        assert!(STYLE_CSS.contains("grid-template-columns: 15px max-content minmax(0, 1fr)"));
        assert!(STYLE_CSS.contains(".worker-activity-tool.running .worker-tool-marker"));
        assert!(STYLE_CSS.contains(".worker-activity-tool.succeeded .worker-tool-marker"));
        assert!(STYLE_CSS.contains(".worker-activity-tool.failed .worker-tool-marker"));
        assert!(!APP_JS.contains("hiddenTitleTools"));
    }

    #[test]
    fn ordinary_tool_cards_use_shared_structured_presenters() {
        assert!(INDEX_HTML.contains("/tool-presenters.js"));
        assert!(TOOL_PRESENTERS_JS.contains("const KNOWN_TOOLS = ["));
        assert!(TOOL_PRESENTERS_JS.contains("define(\"File.Read\""));
        assert!(TOOL_PRESENTERS_JS.contains("define(\"WebBrowser.Snapshot\""));
        assert!(TOOL_PRESENTERS_JS.contains("define(\"Worker.Wait\""));
        assert!(TOOL_PRESENTERS_JS.contains("missing tool presenters"));
        assert!(APP_JS.contains("function toolCardView(tool)"));
        assert!(APP_JS.contains("MeToolPresenters.summarize(tool.name, tool.args || {})"));
        assert!(APP_JS.contains(
            "MeToolPresenters.describe(tool.name, tool.args || {}, toolPresentationOutput(tool))"
        ));
        assert!(APP_JS.contains("MeToolPresenters.renderDetails(view.details)"));
        assert!(APP_JS.contains("function updateToolCardNode(node, tool, followsTool ="));
        assert!(APP_JS.contains("tool.updates.push(value.content)"));
        assert!(STYLE_CSS.contains(
            ".tool-header { display: grid; grid-template-columns: 15px max-content minmax(0, 1fr) auto"
        ));
        assert!(STYLE_CSS.contains(".tool-brief { min-width: 0; overflow: hidden;"));
        assert!(TOOL_PRESENTERS_JS.contains("tool-output-section"));
        assert!(STYLE_CSS.contains(".tool-raw > summary"));
        assert!(STYLE_CSS.contains(".tool-card.follows-tool { margin-top: -20px; }"));
    }

    #[test]
    fn landscape_chat_surfaces_share_the_wider_content_boundary() {
        assert_eq!(STYLE_CSS.matches("calc((100% - 1200px) / 2)").count(), 3);
        assert!(!STYLE_CSS.contains("calc((100% - 900px) / 2)"));
        assert!(!INDEX_HTML.contains("class=\"topbar\""));
        assert!(!INDEX_HTML.contains("id=\"page-title\""));
        assert!(!INDEX_HTML.contains("id=\"page-subtitle\""));
        assert!(
            STYLE_CSS.contains(
                ".workspace { display: grid; grid-template-rows: auto minmax(0, 1fr) auto;"
            )
        );
    }

    #[test]
    fn embedded_webui_uses_packaged_commonmark_and_latex_renderers() {
        let engine = INDEX_HTML.find("/markdown-it.js").unwrap();
        let latex = INDEX_HTML.find("/katex.js").unwrap();
        let adapter = INDEX_HTML.find("/markdown.js").unwrap();
        let application = INDEX_HTML.find("/app.js").unwrap();
        let transcript = INDEX_HTML.find("/transcript.js").unwrap();
        let latex_style = INDEX_HTML.find("/katex.css").unwrap();
        let application_style = INDEX_HTML.find("/style.css").unwrap();
        assert!(
            engine < latex && latex < adapter && adapter < transcript && transcript < application
        );
        assert!(latex_style < application_style);
        assert!(MARKDOWN_IT_JS.contains("markdownit=t()"));
        assert!(KATEX_JS.contains("KaTeX"));
        assert!(KATEX_CSS.contains("KaTeX_Main-Regular"));
        assert_eq!(KATEX_FONTS.len(), 20);
        assert!(MARKDOWN_JS.contains("markdown-it 15.0.0 + KaTeX 0.16.22"));
        assert!(MARKDOWN_JS.contains("html: false"));
        assert!(MARKDOWN_JS.contains("linkify: true"));
        assert!(MARKDOWN_JS.contains("breaks: true"));
        assert!(MARKDOWN_JS.contains("trust: false"));
        assert!(MARKDOWN_JS.contains("output: \"htmlAndMathml\""));
        assert!(MARKDOWN_JS.contains("mathInlineRule"));
        assert!(MARKDOWN_JS.contains("mathBlockRule"));
        assert!(MARKDOWN_JS.contains("task-list-item"));
        assert!(MARKDOWN_JS.contains("markdown-table-wrap"));
        assert!(MARKDOWN_JS.contains("function normalizeCjkEmphasis(source)"));
        assert!(MARKDOWN_JS.contains("insideLinkTarget(source, cursor)"));
        assert!(MARKDOWN_JS.contains("split(CJK_EMPHASIS_SENTINEL).join(\"\")"));
        assert!(TRANSCRIPT_JS.contains("createTranscriptBottomFollower"));
        assert!(TRANSCRIPT_JS.contains("reconcileHtmlChildren"));
        assert!(APP_JS.contains("return globalThis.MeMarkdown.render(source)"));
        assert!(!APP_JS.contains("function inlineMarkdown"));
        assert!(STYLE_CSS.contains(".markdown li::marker"));
        assert!(STYLE_CSS.contains(".markdown li.task-list-item.task-completed::before"));
        assert!(STYLE_CSS.contains(".markdown .math-display"));
        assert!(STYLE_CSS.contains("overflow-x: auto"));
    }

    #[test]
    fn default_webui_port_and_dynamic_page_address_are_consistent() {
        assert_eq!(DEFAULT_PORT, 38199);
        assert!(!INDEX_HTML.contains("0.0.0.0:38199"));
        assert!(!APP_JS.contains("0.0.0.0:38199"));
        assert!(APP_JS.contains("window.location.host"));
        assert!(!INDEX_HTML.contains("0.0.0.0:8189"));
        assert!(!APP_JS.contains("0.0.0.0:8189"));
    }

    #[test]
    fn occupied_webui_port_advances_to_the_next_available_port() {
        let occupied = TcpListener::bind((DEFAULT_BIND_ADDRESS, 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let (server, selected_port) = bind_first_available(occupied_port).unwrap();

        assert!(selected_port > occupied_port);
        assert_eq!(server.server_addr().to_ip().unwrap().port(), selected_port);
    }

    #[test]
    fn embedded_webui_keeps_status_and_agent_creation_copy_concise() {
        assert!(INDEX_HTML.contains("id=\"login-form\""));
        assert!(INDEX_HTML.contains("id=\"login-password\" type=\"password\""));
        assert!(APP_JS.contains("initializeAuthentication()"));
        assert!(APP_JS.contains("/api/auth/login"));
        assert!(INDEX_HTML.contains("id=\"status-model\""));
        assert!(INDEX_HTML.contains("id=\"status-effort\""));
        assert!(INDEX_HTML.contains("id=\"status-live-tokens\""));
        assert!(INDEX_HTML.contains("id=\"status-context\""));
        assert!(INDEX_HTML.contains("id=\"status-context-trigger\""));
        assert!(INDEX_HTML.contains("id=\"status-model-trigger\""));
        assert!(INDEX_HTML.contains("id=\"status-effort-trigger\""));
        assert!(!INDEX_HTML.contains("id=\"status-agent\""));
        assert!(!INDEX_HTML.contains("id=\"status-orchestrator\""));
        assert!(!INDEX_HTML.contains("<strong>me</strong>"));
        assert!(STYLE_CSS.contains("justify-content: space-between"));
        assert!(
            STYLE_CSS.contains(".status-context-selector { flex: 0 0 auto; margin-left: auto;")
        );
        assert!(APP_JS.contains("openChoice(\"创建新的会话？\""));
        assert!(APP_JS.contains("选择 Agent 类型。创建后不可更改。"));
        assert!(APP_JS.contains("标准 (main-agent)"));
        assert!(APP_JS.contains("单 Agent 模式，响应直接，Token 开销较低"));
        assert!(APP_JS.contains("协作 (manager-agent)"));
        assert!(
            APP_JS
                .contains("双 Agent 协作，适合复杂任务，减少主模型上下文占用，但总 Token 开销更高")
        );
        assert!(APP_JS.contains("聊天 (chatbot)"));
        assert!(APP_JS.contains("仅进行对话，不使用工作工具"));
        assert!(STYLE_CSS.contains(".choice { display: flex; align-items: center;"));
        assert!(STYLE_CSS.contains(".choice input { flex: 0 0 auto; margin: 0;"));
        assert!(APP_JS.contains("state.snapshot.orchestrators"));
        assert!(APP_JS.contains("state.snapshot.default_orchestrator"));
        assert!(!APP_JS.contains("创建新的独立 Agent runtime 和 EDB"));
        assert!(!APP_JS.contains("操作已提交"));
        assert!(APP_JS.contains("elements.tabs.getBoundingClientRect()"));
        assert!(APP_JS.contains("tabs.bottom + 10"));
        assert!(
            STYLE_CSS
                .contains(".toast-region { position: fixed; z-index: 80; top: 0; right: 20px;")
        );
        assert!(!STYLE_CSS.contains("bottom: 20px"));
    }

    #[test]
    fn embedded_webui_status_selectors_open_large_choice_drawers() {
        assert!(INDEX_HTML.contains("id=\"choice-drawer-backdrop\""));
        assert!(INDEX_HTML.contains("aria-labelledby=\"choice-drawer-title\""));
        assert!(
            APP_JS.contains(
                "elements.statusModelTrigger.addEventListener(\"click\", openModelDrawer)"
            )
        );
        assert!(APP_JS.contains(
            "elements.statusEffortTrigger.addEventListener(\"click\", openEffortDrawer)"
        ));
        assert!(APP_JS.contains("function openChoiceDrawer("));
        assert!(APP_JS.contains("await drawer.onSelect(value)"));
        assert!(STYLE_CSS.contains(".status-selector { min-height: 34px;"));
        assert!(
            STYLE_CSS.contains(".drawer-choice { display: grid; width: 100%; min-height: 48px;")
        );
        assert!(STYLE_CSS.contains(".drawer-choice { min-height: 52px; }"));
    }

    #[test]
    fn embedded_webui_context_usage_drawer_estimates_categories_and_confirms_clear() {
        assert!(INDEX_HTML.contains("id=\"context-drawer-backdrop\""));
        assert!(INDEX_HTML.contains("id=\"context-ring\""));
        assert!(INDEX_HTML.contains("id=\"context-percent\""));
        assert!(INDEX_HTML.contains("id=\"context-usage-text\""));
        assert!(INDEX_HTML.contains("id=\"context-clear\""));
        assert!(INDEX_HTML.contains("id=\"compact-summary-backdrop\""));
        assert!(INDEX_HTML.contains("id=\"compact-summary-content\""));
        assert!(APP_JS.contains("function estimateContextBreakdown(events, usage, memoryContent)"));
        assert!(APP_JS.contains("kind === \"ContextUsageEstimate\""));
        assert!(APP_JS.contains("value.api_state_event_id === boundaryId"));
        assert!(APP_JS.contains("系统提示词"));
        assert!(APP_JS.contains("上下文压缩"));
        assert!(APP_JS.contains("label: \"记忆\""));
        assert!(APP_JS.contains("用户消息"));
        assert!(APP_JS.contains("模型输出"));
        assert!(!APP_JS.contains("模型回复"));
        assert!(APP_JS.contains("工具调用"));
        assert!(APP_JS.contains("输出预留"));
        assert!(APP_JS.contains("model?.output_token_reservations?.[projection.effort]"));
        assert!(APP_JS.contains("category === \"reserve\" ? formatTokens(value)"));
        assert!(!APP_JS.contains("function estimateTokenWeight("));
        assert!(!INDEX_HTML.contains("分类由 WebUI 估算"));
        assert!(!APP_JS.contains("保留 system prompt"));
        assert!(APP_JS.contains(
            "elements.statusContextTrigger.addEventListener(\"click\", openContextDrawer)"
        ));
        assert!(APP_JS.contains("openConfirm(\"清空上下文？\""));
        assert!(APP_JS.contains("command: \"clear_context\""));
        assert!(APP_JS.contains("category.key !== \"compact\" || hasCompact"));
        assert!(APP_JS.contains("category.key !== \"memory\" || hasMemory"));
        assert!(APP_JS.contains("state.contextCompactContent"));
        assert!(APP_JS.contains("state.contextCompactAnalysis"));
        assert!(APP_JS.contains("state.contextMemoryContent"));
        assert!(APP_JS.contains("function latestCompactPreview(events)"));
        assert!(APP_JS.contains("value.stage === \"Analysis\""));
        assert!(APP_JS.contains("function compactPreviewMarkdown(analysis, summary)"));
        assert!(APP_JS.contains("## Analysis"));
        assert!(APP_JS.contains("## 压缩摘要"));
        assert!(APP_JS.contains("pre.textContent = content"));
        assert!(APP_JS.contains("\"上下文压缩\","));
        assert!(APP_JS.contains("openContextDetail(\"记忆\""));
        assert!(STYLE_CSS.contains(".context-ring-segment"));
        assert!(STYLE_CSS.contains(".context-percent"));
        assert!(STYLE_CSS.contains("--context-memory"));
        assert!(STYLE_CSS.contains(".context-detail-help"));
        assert!(STYLE_CSS.contains(".context-detail-raw"));
        assert!(STYLE_CSS.contains(
            "#compact-summary-backdrop { display: flex; align-items: center; justify-content: center; overflow: hidden; }"
        ));
        assert!(
            STYLE_CSS
                .contains(".compact-summary-modal { width: 100%; min-width: 0; max-width: 760px;")
        );
        assert!(
            STYLE_CSS
                .contains(".compact-summary-content { width: 100%; min-width: 0; max-width: 100%;")
        );
        assert!(STYLE_CSS.contains("#compact-summary-backdrop .compact-summary-modal"));
    }

    #[test]
    fn embedded_webui_has_compact_send_and_stop_controls() {
        assert!(INDEX_HTML.contains("id=\"stop-generation\""));
        assert!(INDEX_HTML.contains(">停止</button>"));
        assert!(INDEX_HTML.contains("primary-button composer-button"));
        assert!(INDEX_HTML.contains("id=\"send-prompt-spinner\""));
        assert!(INDEX_HTML.contains("id=\"send-prompt-label\""));
        assert!(APP_JS.contains("function stopGeneration()"));
        assert!(APP_JS.contains("function isWorkerAgent(meta = agentMeta())"));
        assert!(APP_JS.contains("meta?.orchestrator === \"worker-agent\""));
        assert!(APP_JS.contains("function canControlRuntime(meta = agentMeta())"));
        assert!(APP_JS.contains("meta.kind !== \"sub-agent\" || isWorkerAgent(meta)"));
        assert!(APP_JS.contains("elements.stop.addEventListener(\"click\", stopGeneration)"));
        assert!(APP_JS.contains("elements.stop.disabled = !canStop"));
        assert!(APP_JS.contains("elements.input.disabled = readOnly || sending"));
        assert!(APP_JS.contains("elements.send.disabled = readOnly || sending"));
        assert!(APP_JS.contains("elements.send.setAttribute(\"aria-busy\", String(sending))"));
        assert!(APP_JS.contains("pending?.status === \"confirming\" ? \"正在确认\""));
        assert!(APP_JS.contains("if (!state.selectedAgent || !canControlRuntime()) return;"));
        assert!(STYLE_CSS.contains(".composer-button {"));
        assert!(STYLE_CSS.contains(".stop-button {"));
        assert!(STYLE_CSS.contains(".send-prompt-spinner {"));
        assert!(STYLE_CSS.contains("@keyframes send-prompt-spin"));
    }

    #[test]
    fn embedded_webui_allows_worker_model_and_effort_controls_without_sidebar_metadata() {
        assert!(!APP_JS.contains("agent.orchestrator === \"worker-agent\" ? \"Worker\""));
        assert!(!APP_JS.contains("class=\"agent-secondary\""));
        assert!(APP_JS.contains("可调整模型、推理强度或停止当前任务"));
        assert!(APP_JS.contains("const canChange = canControlRuntime();"));
        assert_eq!(
            APP_JS
                .matches("if (!agentId || !canControlRuntime()) return;")
                .count(),
            2
        );
        assert!(APP_JS.contains("elements.statusModelTrigger.disabled = !canChange;"));
        assert!(APP_JS.contains(
            "elements.statusEffortTrigger.disabled = !canChange || !model?.reasoning_efforts?.length;"
        ));
    }

    #[test]
    fn embedded_webui_exposes_user_copy_and_rewind_actions() {
        assert!(INDEX_HTML.contains("id=\"user-message-menu\""));
        assert!(INDEX_HTML.contains("id=\"copy-user-message\""));
        assert!(INDEX_HTML.contains("id=\"rewind-user-message\""));
        assert!(INDEX_HTML.contains("id=\"delete-user-turn\""));
        assert!(APP_JS.contains("navigator.clipboard?.writeText"));
        assert!(APP_JS.contains("document.execCommand(\"copy\")"));
        assert!(APP_JS.contains("function observeInputDraft(meta, store)"));
        assert!(APP_JS.contains("openConfirm(\"撤回这条消息？\""));
        assert!(APP_JS.contains("openConfirm(\"删除这一轮？\""));
        assert!(STYLE_CSS.contains(".user-message-actions {"));
        assert!(STYLE_CSS.contains(".user-message-menu, .agent-menu"));
    }

    #[test]
    fn embedded_webui_exposes_clone_regenerate_and_local_clone_selection() {
        assert!(APP_JS.contains("finalAnswerEventId: value.id"));
        assert!(APP_JS.contains("class=\"clone-turn\""));
        assert!(APP_JS.contains("class=\"regenerate-turn\""));
        assert!(APP_JS.contains("command: \"clone_agent\""));
        assert!(APP_JS.contains("command: \"regenerate\""));
        assert!(APP_JS.contains("if (id) state.pendingAgentSelection = id"));
        assert!(STYLE_CSS.contains(".turn-actions button"));
        assert!(APP_JS.contains("`克隆完成。新会话：${value.title}`"));
    }

    #[test]
    fn embedded_webui_has_targeted_agent_delete_controls_with_confirmation() {
        assert!(APP_JS.contains("data-agent-delete="));
        assert!(APP_JS.contains("row.querySelector(\".agent-delete\").addEventListener"));
        assert!(APP_JS.contains("event.stopPropagation();"));
        assert!(APP_JS.contains("void openDeleteAgent(agent.id);"));
        assert!(APP_JS.contains("async function openDeleteAgent(agentId = state.selectedAgent)"));
        assert!(APP_JS.contains("openConfirm(\"删除会话？\""));
        assert!(APP_JS.contains("agent_id: agentId"));
        assert!(STYLE_CSS.contains(".agent-delete {"));
    }

    #[test]
    fn embedded_webui_synchronizes_runtime_owned_input_drafts() {
        assert!(APP_JS.contains("function observePromptSubmission(meta, store)"));
        assert!(APP_JS.contains("if (store.pendingPromptSubmission) return false;"));
        assert!(APP_JS.contains("function observeInputDraft(meta, store)"));
        assert!(APP_JS.contains("function adoptInputDraft(agentId, store, revision, content)"));
        assert!(APP_JS.contains("expected_revision: expectedRevision"));
        assert!(APP_JS.contains("if (!accepted)"));
        assert!(APP_JS.contains("command: \"update_input_draft\""));
        assert!(APP_JS.contains("function queueDraftUpdate(agentId, content)"));
        assert!(APP_JS.contains("async function pauseDraftSyncForSubmission(agentId)"));
        assert!(
            APP_JS.contains("pending?.displayContent ?? state.drafts.get(state.selectedAgent)")
        );
        assert!(APP_JS.contains("window.addEventListener(\"pagehide\", () =>"));
        assert!(APP_JS.contains("flushDraftBeforePageCloses();"));
        assert!(
            APP_JS.contains("if (state.stores.get(agentId)?.pendingPromptSubmission) continue;")
        );
        assert!(APP_JS.contains("navigator.sendBeacon?.(\"/api/command\""));
        assert!(APP_JS.contains("fetch(\"/api/command\""));
        assert!(APP_JS.contains("receipt?.prompt_submission_revision"));
        assert!(APP_JS.contains("store.promptSubmissionRevision = Math.max"));
    }

    #[test]
    fn embedded_webui_waits_for_authoritative_prompt_projection() {
        assert!(APP_JS.contains("pendingPromptSubmission: null"));
        assert!(APP_JS.contains("function promptSubmissionBoundary(meta, store)"));
        assert!(APP_JS.contains("function pendingPromptReachedProjection(store)"));
        assert!(APP_JS.contains("message.key?.startsWith(\"user:\")"));
        assert!(APP_JS.contains("Number(message.eventId) > pending.afterEventId"));
        assert!(APP_JS.contains("message.content === pending.content"));
        assert!(APP_JS.contains("function finishPendingPromptSubmission(agentId)"));
        assert!(APP_JS.contains("function cancelPendingPromptSubmission(agentId, pending)"));
        assert!(APP_JS.contains("function commandResultIsUnknown(error)"));
        assert!(APP_JS.contains("pending.status = \"confirming\""));
        assert!(APP_JS.contains("if (pending.settled) return;"));
        assert!(APP_JS.contains("const promptConfirmed = beginConfirmedPromptRender(changes)"));
        assert!(
            APP_JS.contains(
                "if (promptConfirmed) finishPendingPromptSubmission(state.selectedAgent)"
            )
        );
    }

    #[test]
    fn embedded_webui_uses_recoverable_incremental_http_polling() {
        assert!(INDEX_HTML.contains("id=\"connection-overlay\""));
        assert!(INDEX_HTML.contains("id=\"connection-retry\""));
        assert!(INDEX_HTML.contains("id=\"event-recovery-progress\""));
        assert!(INDEX_HTML.contains("role=\"progressbar\""));
        assert!(
            STYLE_CSS.contains(".connection-overlay { position: fixed; inset: 0; z-index: 120;")
        );
        assert!(STYLE_CSS.contains(".event-recovery-progress-fill"));
        assert!(APP_JS.contains("function eventRecoveryProgress(recovery, localEventCount)"));
        assert!(APP_JS.contains("api(\"/api/sync\""));
        assert!(!APP_JS.contains("new WebSocket"));
        assert!(APP_JS.contains("if (state.syncInFlight"));
        assert!(APP_JS.contains("HTTP_SYNC_TIMEOUT_MS"));
        assert!(APP_JS.contains("function failHttpSync(title, error)"));
        assert!(APP_JS.contains("function httpSyncProgressSignature()"));
        assert!(
            APP_JS.contains("const madeProgress = progressBefore !== httpSyncProgressSignature()")
        );
        assert!(
            APP_JS.contains("scheduleHttpSync(message.more_events && madeProgress ? 0 : delay)")
        );
        assert!(APP_JS.contains("HTTP_SYNC_ACTIVE_MS = 250"));
        assert!(APP_JS.contains("HTTP_SYNC_IDLE_MS = 1000"));
        assert!(APP_JS.contains("typeof PORTRAIT_LAYOUT.addListener === \"function\""));
        assert!(!APP_JS.contains(".at(-1)"));
        assert!(APP_JS.contains("elements.app.inert = true"));
        assert!(APP_JS.contains("elements.app.inert = false"));
        assert!(APP_JS.contains("snapshot_revision:"));
        assert!(APP_JS.contains("mutation_revision: store.mutationRevision"));
        assert!(APP_JS.contains("Math.min(RECONNECT_MAX_MS"));
    }

    #[test]
    fn embedded_webui_batches_drafts_and_stabilizes_transcript_layout() {
        assert!(APP_JS.contains("const DRAFT_BATCH_MS = 80;"));
        assert!(APP_JS.contains("batchTimer: null"));
        assert!(APP_JS.contains("refresh: false"));
        assert!(INDEX_HTML.contains("id=\"prompt-input-mirror\""));
        assert!(APP_JS.contains("inputMirror: $(\"#prompt-input-mirror\")"));
        assert!(APP_JS.contains("elements.inputMirror.scrollHeight"));
        assert!(!APP_JS.contains("elements.input.scrollHeight"));
        assert!(!APP_JS.contains("function inputChangeCanShrink"));
        assert!(!APP_JS.contains("elements.input.style.height = \"auto\""));
        assert!(APP_JS.contains("if (state.inputHeight !== target)"));
        assert!(STYLE_CSS.contains(".prompt-input-mirror { position: absolute;"));
        assert!(STYLE_CSS.contains("contain: layout paint style;"));
        assert!(STYLE_CSS.contains(".objective-details { position: absolute;"));
        assert!(STYLE_CSS.contains(".ios-webkit .transcript-window > .message-block"));
        assert!(APP_JS.contains("message.kind === \"notice\" || message.kind === \"session\""));
        assert!(APP_JS.contains("content.textContent = message.content;"));
        assert!(APP_JS.contains("const CONNECTION_DEGRADED_GRACE_MS = 2000;"));
        assert!(APP_JS.contains("const CONNECTION_STABILIZE_MS = 1000;"));
        assert!(APP_JS.contains("const CONNECTION_STABILIZE_SUCCESSES = 2;"));
        assert!(APP_JS.contains("connectionPhase: \"initial\""));
        assert!(APP_JS.contains("connectionOverlayMode: null"));
        assert!(
            STYLE_CSS
                .contains(".transcript-window > .message-block, .transcript-window > .tool-card")
        );
        assert!(TRANSCRIPT_JS.contains("let committedScrollHeight = viewport.scrollHeight;"));
        assert!(TRANSCRIPT_JS.contains("scrollHeight !== committedScrollHeight"));
        assert!(TRANSCRIPT_JS.contains("const applyFollowNow = (force = forcing)"));
    }

    #[test]
    fn streaming_assistant_updates_keep_the_stable_message_node() {
        assert!(
            APP_JS.contains(
                "function updateMessageNode(node, message, afterTool, followsTool, index)"
            )
        );
        assert!(APP_JS.contains("if (message.kind === \"assistant\")"));
        assert!(APP_JS.contains("MeTranscript.reconcileHtmlChildren(markdown, rendered);"));
        assert!(!APP_JS.contains("markdown.innerHTML = rendered"));
        assert!(APP_JS.contains(
            "if (node.meRenderRevision !== revision) updateMessageNode(node, message, afterTool, followsTool, index)"
        ));
        assert!(TRANSCRIPT_JS.contains("if (shouldWrite) viewport.scrollTop = scrollHeight;"));
        assert!(
            !APP_JS.contains("current.replaceWith(createMessageNode(message, afterTool, index))")
        );
    }

    #[test]
    fn embedded_webui_offers_scroll_to_latest_when_transcript_is_not_at_bottom() {
        assert!(INDEX_HTML.contains("id=\"scroll-to-bottom\""));
        assert!(INDEX_HTML.contains("aria-label=\"滚动到最新消息\""));
        assert!(APP_JS.contains("const TRANSCRIPT_BOTTOM_THRESHOLD_PX = 24;"));
        assert!(APP_JS.contains("function updateScrollToBottomButton()"));
        assert!(APP_JS.contains(
            "elements.scrollToBottom.addEventListener(\"click\", scrollTranscriptToBottomAfterLayout)"
        ));
        assert!(STYLE_CSS.contains(".scroll-to-bottom { position: absolute; left: 50%;"));
        assert!(STYLE_CSS.contains("border-radius: 50%"));
    }

    #[test]
    fn running_tool_animation_is_smooth_and_elapsed_time_refreshes_independently() {
        assert!(STYLE_CSS.contains("animation: tool-marker-breathe 900ms ease-in-out infinite"));
        assert!(STYLE_CSS.contains("@keyframes tool-marker-breathe"));
        assert!(STYLE_CSS.contains("will-change: opacity"));
        assert!(APP_JS.contains("function refreshRunningToolNodes()"));
        assert!(APP_JS.contains("function refreshUiAnimation()"));
        assert!(APP_JS.contains(
            "state.uiAnimationTimer = setTimeout(refreshUiAnimation, UI_ANIMATION_INTERVAL_MS)"
        ));
        assert!(!APP_JS.contains("setInterval(refreshRunningToolElapsed"));
        assert!(APP_JS.contains("if (document.hidden) stopUiAnimation()"));
        assert!(!APP_JS.contains("TOOL_MARKER_OPACITY"));
        assert!(!APP_JS.contains("toolAnimationTick"));
        assert!(!APP_JS.contains("node.style.opacity"));
    }

    #[test]
    fn sidebar_agent_uses_turn_lifecycle_and_stronger_three_second_sweep() {
        assert!(APP_JS.contains("if (kind === \"AgentTurn\") summary.turnState = value.state;"));
        assert!(APP_JS.contains("const active = !startupLoading && sidebarAgentActive(summary);"));
        assert!(!APP_JS.contains("const active = API_ACTIVE.has(summary?.apiState);"));
        assert!(
            !APP_JS
                .contains("else if (kind === \"ApiStateUpdate\") summary.apiState = value.state;")
        );
        assert!(APP_JS.contains("dot.classList.toggle(\"active\", active)"));
        assert_eq!(
            APP_JS
                .matches("<span class=\"agent-dot\" aria-hidden=\"true\"></span>")
                .count(),
            1
        );
        assert!(
            STYLE_CSS.contains(
                ".agent-dot.active { border-color: var(--cyan); background: var(--cyan);"
            )
        );
        assert!(STYLE_CSS.contains("animation: agent-dot-breathe 3s ease-in-out infinite"));
        assert!(STYLE_CSS.contains(
            "linear-gradient(100deg, var(--text) 0 36%, var(--activity-sweep) 46% 54%, var(--text) 64% 100%)"
        ));
        assert!(STYLE_CSS.contains("animation: agent-label-sweep 3s ease-in-out infinite"));
        assert!(STYLE_CSS.contains(
            "@keyframes agent-dot-breathe { 0%, 66.667%, 100% { opacity: 1; } 33.333% { opacity: .35; } }"
        ));
        assert!(STYLE_CSS.contains(
            "@keyframes agent-label-sweep { 0% { background-position: 100% 0; } 66.667%, 100% { background-position: 0 0; } }"
        ));
        assert!(STYLE_CSS.contains(
            "@media (prefers-reduced-motion: reduce) { .agent-dot.startup-loading, .agent-dot.active { animation: none; }"
        ));
        assert!(APP_JS.contains("row.classList.toggle(\"startup-loading\", startupLoading)"));
        assert!(APP_JS.contains("item.disabled = startupLoading"));
        assert!(APP_JS.contains("deleteButton.disabled = startupLoading"));
        assert!(STYLE_CSS.contains("animation: agent-startup-spin .8s linear infinite"));
        assert!(
            STYLE_CSS
                .contains("@keyframes agent-startup-spin { to { transform: rotate(360deg); } }")
        );
        assert!(
            THEME_CSS.contains("--activity-sweep: color-mix(in srgb, var(--text) 42%, var(--bg));")
        );
        assert!(!APP_JS.contains("agentDotAnimation"));
    }

    #[test]
    fn embedded_webui_projects_appended_events_incrementally() {
        assert!(APP_JS.contains("pendingRender: emptyRenderRequest()"));
        assert!(APP_JS.contains("projectedOrder: 0"));
        assert!(APP_JS.contains("needsReplay: true"));
        assert!(APP_JS.contains("store.events.slice(store.projectedOrder)"));
        assert!(APP_JS.contains("consumeChatEvents(store.projection, appended)"));
        assert!(APP_JS.contains("consumeWorkMapEvents(store.workmap, appended)"));
        assert!(APP_JS.contains("workmap._records.clear()"));
        assert!(APP_JS.contains("chatAppendNeedsReplay(appended)"));
        assert!(APP_JS.contains("function renderIncremental(request)"));
        assert!(APP_JS.contains("api(\"/api/sync\""));
        assert!(APP_JS.contains("method: \"POST\""));
        assert!(!APP_JS.contains("new WebSocket"));
        assert!(!APP_JS.contains("/api/api-activity/"));
        assert!(!APP_JS.contains("/api/events/"));
        assert!(!APP_JS.contains("/api/terminals/"));
        assert!(!APP_JS.contains("/api/terminal/"));
        assert!(APP_JS.contains(
            "status: !bulkRecoveryPending && !forceRecoveredReplay && apiActivityChanged"
        ));
        assert!(APP_JS.contains("receivedSseEvents"));
        assert!(APP_JS.contains("if (request.status || changes.status) renderStatus()"));
        assert!(APP_JS.contains("else if (request.workerEvents && state.view.kind === \"chat\")"));
        assert!(APP_JS.contains("function refreshWorkerActivityCards()"));
        assert!(APP_JS.contains("function showView(view)"));
        let sync_agent_events = APP_JS
            .split_once("function syncAgentEvents(meta, payload) {")
            .and_then(|(_, tail)| tail.split_once("\nfunction observeInputDraft("))
            .map(|(body, _)| body)
            .expect("syncAgentEvents function should exist");
        assert!(!sync_agent_events.contains("renderAll();"));
        let cache_hydration = APP_JS
            .split_once("async function hydrateEdbCache(snapshot) {")
            .and_then(|(_, tail)| tail.split_once("\nfunction persistAgentEdb("))
            .map(|(body, _)| body)
            .expect("hydrateEdbCache function should exist");
        assert_eq!(cache_hydration.matches("renderAll();").count(), 1);
        assert_eq!(APP_JS.matches("renderAll();").count(), 3);
        assert!(APP_JS.contains("if (changes.workmap)"));
        assert!(APP_JS.contains("while (cache.nextOrder < events.length)"));
        assert!(APP_JS.contains("store.needsReplay = true"));
        assert!(APP_JS.contains("function projectAgentSummary(events)"));
        assert!(APP_JS.contains("updateAgentSummary(store.summary, payload.events)"));
        assert!(APP_JS.contains("const summary = state.stores.get(agent.id)?.summary"));
        assert_eq!(
            APP_JS
                .matches("store.projection = projectChat(store.events)")
                .count(),
            2
        );
        assert!(!APP_JS.contains("projectionDirty"));
        assert!(!APP_JS.contains("renderPending"));
        assert!(APP_JS.contains("if (!inputHasPriority()) flushPendingRender()"));
        assert!(APP_JS.contains("return state.composing || performance.now() - state.lastInputAt"));
        assert!(APP_JS.contains("inputResizeFrame: null"));
        assert!(APP_JS.contains("state.inputResizeFrame = requestAnimationFrame"));
        assert!(APP_JS.contains("function refreshRunningToolNodes()"));
        assert!(APP_JS.contains("function refreshUiAnimation()"));
        assert!(APP_JS.contains("uiAnimationTimer: null"));
        assert!(!APP_JS.contains("setInterval(refreshRunningToolElapsed"));
        assert!(APP_JS.contains("if (!inputHasPriority()) {"));
        assert!(APP_JS.contains("transcriptFrom: null"));
        assert!(APP_JS.contains(
            "renderTranscript(Boolean(changes.fullReplay), changes.transcriptFrom ?? 0)"
        ));
        assert!(APP_JS.contains(
            "function reconcileTranscript(container, messages, start, end, previousKind = null)"
        ));
        assert!(APP_JS.contains("for (let index = start; index < end; index += 1)"));
        assert!(APP_JS.contains("transcriptVirtualizer.update(messages"));
        assert!(!APP_JS.contains("projection.messages.filter((message) =>"));
        assert!(APP_JS.contains("projection._messageByKey.get(`tool:${node.dataset.workerWait}`)"));
        assert!(APP_JS.contains("function markPendingPromptConfirmation(store, changes)"));
        assert!(APP_JS.contains("return markPendingPromptConfirmation(store, changes)"));
    }

    #[test]
    fn embedded_webui_preserves_manual_scrolling_and_stable_transcript_dom() {
        assert!(INDEX_HTML.contains("id=\"transcript-content\""));
        assert!(INDEX_HTML.contains("/transcript.js"));
        assert!(APP_JS.contains("function createTranscriptBottomFollower("));
        assert!(APP_JS.contains("return MeTranscript.createTranscriptBottomFollower("));
        assert!(TRANSCRIPT_JS.contains("new ResizeObserver(callback)"));
        assert!(TRANSCRIPT_JS.contains("resizeObserver.observe(viewport)"));
        assert!(TRANSCRIPT_JS.contains("resizeObserver.observe(content)"));
        assert!(TRANSCRIPT_JS.contains("let userScrolling = false"));
        assert!(TRANSCRIPT_JS.contains("let forcing = false"));
        assert!(TRANSCRIPT_JS.contains("if (forcing) scheduleSettling()"));
        assert!(APP_JS.contains("function suspendTranscriptAutoFollow()"));
        assert!(APP_JS.contains("typeof window.PointerEvent === \"function\""));
        assert!(APP_JS.contains("addEventListener(\"scrollend\", finishTranscriptScrolling"));
        assert!(APP_JS.contains(
            "elements.transcript.addEventListener(\"wheel\", suspendTranscriptAutoFollow"
        ));
        assert!(APP_JS.contains("function beginConfirmedPromptRender(changes,"));
        assert!(APP_JS.contains("if (!changes.promptConfirmed) return false;"));
        assert!(APP_JS.contains("bottomFollower.follow();"));
        assert!(APP_JS.contains("if (transcriptChanged || promptConfirmed)"));
        assert!(APP_JS.contains("transcriptBottomFollower.layoutChanged();"));
        assert_eq!(
            TRANSCRIPT_JS
                .matches("if (shouldWrite) viewport.scrollTop = scrollHeight;")
                .count(),
            1
        );
        assert!(APP_JS.contains("MeTranscript.reconcileHtmlChildren(markdown, rendered)"));
        assert!(APP_JS.contains("new Map([...container.children]"));
        assert!(APP_JS.contains("MeTranscript.createVirtualTranscript("));
        assert!(APP_JS.contains("transcriptVirtualizer.noteScroll();"));
        assert!(!APP_JS.contains("markdown.innerHTML = rendered"));
        assert!(
            !APP_JS.contains("if (forceFull) replaceElementChildren(elements.transcriptContent)")
        );
        assert!(STYLE_CSS.contains("overflow-anchor: none"));
        assert!(STYLE_CSS.contains(
            ".transcript-content { display: flow-root; min-height: 100%; overflow-anchor: none; }"
        ));
        assert!(STYLE_CSS.contains(".transcript-spacer {"));
        assert!(STYLE_CSS.contains(".transcript-window { display: flow-root;"));
        assert!(APP_JS.contains("function createAgentRow(agent)"));
        assert!(!APP_JS.contains("elements.agents.innerHTML = state.snapshot.agents.map"));
    }

    #[test]
    fn embedded_webui_objective_summary_uses_the_title_without_a_label() {
        assert!(
            APP_JS.contains("${active ? \"■\" : \"□\"} ${escapeHtml(current.objective.title)}")
        );
        assert!(!APP_JS.contains("目标: ${escapeHtml(current.objective.title)}"));
        assert!(STYLE_CSS.contains(
            ".objective-title { min-width: 0; flex: 1; overflow-wrap: anywhere; font-weight: 400; }"
        ));
    }

    #[test]
    fn embedded_webui_places_completed_turn_elapsed_after_the_final_answer() {
        assert!(APP_JS.contains("_turnStartedAt: new Map()"));
        assert!(APP_JS.contains("_lastAssistantByPrompt: new Map()"));
        assert!(APP_JS.contains("case \"AgentTurn\""));
        assert!(APP_JS.contains("if (stateName === \"completed\")"));
        assert!(
            APP_JS.contains("projection.messages[projection.messages.length - 1] === assistant")
        );
        assert!(APP_JS.contains("kind: \"turn-toolbar\""));
        assert!(APP_JS.contains("function formatTurnElapsed(ms)"));
        assert!(APP_JS.contains("function formatTurnCompletedAt(timestamp, now = Date.now())"));
        assert!(
            APP_JS.contains("Date.UTC(value.getFullYear(), value.getMonth(), value.getDate())")
        );
        assert!(APP_JS.contains(
            "daysAgo === 0 ? \"今天\" : daysAgo === 1 ? \"昨天\" : daysAgo === 2 ? \"前天\""
        ));
        assert!(APP_JS.contains("_turnContextBaseline: new Map()"));
        assert!(APP_JS.contains(
            "function completedTurnContextGrowth(completedApiUsage, promptId, contextBaseline)"
        ));
        assert!(APP_JS.contains("function formatTurnTokens(tokens)"));
        assert!(APP_JS.contains("tokenCount: completedTurnContextGrowth("));
        assert!(APP_JS.contains("`${hours}h ${String(minutes).padStart(2, \"0\")}m ${String(seconds).padStart(2, \"0\")}s`"));
        assert!(APP_JS.contains("return `${seconds}s`;"));
        assert!(APP_JS.contains("aria-label=\"本轮用时\""));
        assert!(APP_JS.contains("<span>▶ 用时 ${formatTurnElapsed(message.durationMs)} · ${formatTurnTokens(message.tokenCount)} · ${formatTurnCompletedAt(message.timestamp)}</span>"));
        assert!(STYLE_CSS.contains(".message-block.turn-toolbar"));
        assert!(STYLE_CSS.contains(".message-block.turn-toolbar span"));
        assert!(STYLE_CSS.contains("font-variant-numeric: tabular-nums"));
    }

    #[test]
    fn session_terminal_reconciles_agent_lifecycle_without_terminal_requests() {
        let directory = workspace();
        let workspace = Workspace::open(
            &directory,
            WorkspaceConfig {
                version: 2,
                model: "test".into(),
                effort: "unset".into(),
                orchestrator: "main-agent".into(),
            },
            vec![model()],
        )
        .unwrap();
        let (backend, commands) = workspace_ui_ports(workspace);
        let lifecycle = commands.clone();
        let server = start_from(backend, commands, 0, None).unwrap();
        let address = server
            .address()
            .replace("http://0.0.0.0:", "http://127.0.0.1:");
        let client = reqwest::blocking::Client::new();

        let UiCommandReceipt::AgentCreated(created) = lifecycle
            .submit(UiCommand::AddAgent {
                orchestrator: "main-agent".into(),
            })
            .unwrap()
        else {
            panic!("AddAgent did not create a session");
        };

        let created_id = created.id;

        // No terminal or snapshot request drives this creation. The WebUiServer
        // lifecycle loop must observe the Workspace handle on its own.
        thread::sleep(Duration::from_millis(350));
        let read = || {
            client
                .post(format!(
                    "{address}/api/session-terminal/{}/read",
                    created_id
                ))
                .json(&json!({"cursor": null}))
                .send()
                .unwrap()
        };
        assert_eq!(read().status(), reqwest::StatusCode::OK);

        lifecycle
            .submit(UiCommand::DeleteAgent {
                agent_id: created_id.clone(),
            })
            .unwrap();
        thread::sleep(Duration::from_millis(350));
        assert_eq!(read().status(), reqwest::StatusCode::NOT_FOUND);

        drop(server);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn passkey_protects_every_operational_api_with_an_http_only_session() {
        let directory = workspace();
        let workspace = Workspace::open(
            &directory,
            WorkspaceConfig {
                version: 2,
                model: "test".into(),
                effort: "unset".into(),
                orchestrator: "main-agent".into(),
            },
            vec![model()],
        )
        .unwrap();
        let (backend, commands) = workspace_ui_ports(workspace);
        let UiCommandReceipt::AgentCreated(session_agent) = commands
            .submit(UiCommand::AddAgent {
                orchestrator: "main-agent".into(),
            })
            .unwrap()
        else {
            panic!("AddAgent did not create a session");
        };
        let session_agent = session_agent.id;
        let server = start_from(backend, commands, 0, Some("correct horse")).unwrap();
        let address = server
            .address()
            .replace("http://0.0.0.0:", "http://127.0.0.1:");
        let client = reqwest::blocking::Client::new();

        assert!(
            client
                .get(format!("{address}/"))
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            client
                .get(format!("{address}/xterm.js"))
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            client
                .get(format!("{address}/xterm-addon-unicode11.js"))
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            client
                .get(format!("{address}/remote-control.js"))
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        let status: serde_json::Value = client
            .get(format!("{address}/api/auth/status"))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(status["required"], true);
        assert_eq!(status["authenticated"], false);
        assert_eq!(
            client
                .post(format!("{address}/api/sync"))
                .json(&json!({
                    "snapshot_revision": null, "agents": [], "selected_agent": null,
                    "terminal_session": null, "terminal_revision": null,
                }))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{address}/api/snapshot"))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!("{address}/api/command"))
                .json(&json!({"command": "add_agent"}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!(
                    "{address}/api/session-terminal/{session_agent}/read"
                ))
                .json(&json!({"cursor": null}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!("{address}/api/files/list"))
                .json(&json!({"path": null, "roots": false}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!("{address}/api/remote-control/status"))
                .json(&json!({"controller_token": null}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!("{address}/api/auth/login"))
                .json(&json!({"password": "wrong"}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let login = client
            .post(format!("{address}/api/auth/login"))
            .json(&json!({"password": "correct horse"}))
            .send()
            .unwrap();
        assert!(login.status().is_success());
        let set_cookie = login
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        assert!(!set_cookie.contains("correct horse"));
        let cookie = set_cookie.split(';').next().unwrap();
        let authenticated: serde_json::Value = client
            .get(format!("{address}/api/auth/status"))
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(authenticated["authenticated"], true);
        assert!(
            client
                .get(format!("{address}/api/snapshot"))
                .header(reqwest::header::COOKIE, cookie)
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        let files: serde_json::Value = client
            .post(format!("{address}/api/files/list"))
            .header(reqwest::header::COOKIE, cookie)
            .json(&json!({"path": null, "roots": false}))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(files["ok"], true);
        assert!(files["entries"].is_array());
        let remote_status: serde_json::Value = client
            .post(format!("{address}/api/remote-control/status"))
            .header(reqwest::header::COOKIE, cookie)
            .json(&json!({"controller_token": null}))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(remote_status["ok"], true);
        assert_eq!(remote_status["active"], false);
        assert!(remote_status["supported"].is_boolean());
        for path in [
            "/api/remote-control/unknown",
            "/api/remote-control/status?private=true",
            "/api/remote-control/status/extra",
        ] {
            assert_eq!(
                client
                    .post(format!("{address}{path}"))
                    .header(reqwest::header::COOKIE, cookie)
                    .json(&json!({"controller_token": null}))
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::NOT_FOUND,
                "{path}",
            );
        }
        let native_terminal: serde_json::Value = client
            .post(format!(
                "{address}/api/session-terminal/{session_agent}/read"
            ))
            .header(reqwest::header::COOKIE, cookie)
            .json(&json!({"cursor": null}))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(native_terminal["ok"], true);
        assert!(matches!(
            native_terminal["state"].as_str(),
            Some("running" | "exited" | "unavailable")
        ));
        assert!(native_terminal["events"].is_array());
        let synchronized: serde_json::Value = client
            .post(format!("{address}/api/sync"))
            .header(reqwest::header::COOKIE, cookie)
            .json(&json!({
                "snapshot_revision": null, "agents": [], "selected_agent": null,
                "terminal_session": null, "terminal_revision": null,
            }))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(synchronized["type"], "state");
        assert_eq!(
            client
                .get(format!("{address}/api/ws"))
                .header(reqwest::header::COOKIE, cookie)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );

        drop(server);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn same_host_direct_webuis_use_browser_ports_and_keep_sessions_simultaneously() {
        let start_instance = || {
            let directory = workspace();
            let workspace = Workspace::open(
                &directory,
                WorkspaceConfig {
                    version: 2,
                    model: "test".into(),
                    effort: "unset".into(),
                    orchestrator: "chatbot".into(),
                },
                vec![model()],
            )
            .unwrap();
            let (backend, commands) = workspace_ui_ports(workspace);
            let server = start_from(backend, commands, 0, Some("correct horse")).unwrap();
            let address = server
                .address()
                .replace("http://0.0.0.0:", "http://127.0.0.1:");
            (directory, server, address)
        };
        let (first_directory, first_server, first_address) = start_instance();
        let (second_directory, second_server, second_address) = start_instance();
        assert_ne!(first_server.port(), second_server.port());

        let client = reqwest::blocking::Client::new();
        let login = |address: &str, browser_port: u16| {
            client
                .post(format!("{address}/api/auth/login"))
                .json(&json!({
                    "password": "correct horse",
                    "browser_port": browser_port,
                }))
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .headers()
                .get(reqwest::header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        };
        let first_cookie = login(&first_address, 80);
        let second_cookie = login(&second_address, 443);
        assert!(first_cookie.starts_with("me_webui_session_p80="));
        assert!(second_cookie.starts_with("me_webui_session_p443="));
        assert_ne!(
            first_cookie.split_once('=').unwrap().0,
            second_cookie.split_once('=').unwrap().0
        );

        let first_token = first_cookie.split_once('=').unwrap().1;
        for invalid_name in [
            "me_webui_session",
            "me_webui_session_p0",
            "me_webui_session_p65536",
            "me_gateway_session_p80",
        ] {
            let status: serde_json::Value = client
                .get(format!("{first_address}/api/auth/status"))
                .header(
                    reqwest::header::COOKIE,
                    format!("{invalid_name}={first_token}"),
                )
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .unwrap();
            assert_eq!(status["authenticated"], false);
        }

        let browser_cookies = format!("{first_cookie}; {second_cookie}");
        for address in [&first_address, &second_address] {
            let status: serde_json::Value = client
                .get(format!("{address}/api/auth/status"))
                .header(reqwest::header::COOKIE, &browser_cookies)
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .unwrap();
            assert_eq!(status["authenticated"], true);
        }

        drop(first_server);
        drop(second_server);
        fs::remove_dir_all(first_directory).unwrap();
        fs::remove_dir_all(second_directory).unwrap();
    }

    #[test]
    fn http_sync_negotiates_gzip_without_changing_json() {
        let directory = workspace();
        let workspace = Workspace::open(
            &directory,
            WorkspaceConfig {
                version: 2,
                model: "test".into(),
                effort: "unset".into(),
                orchestrator: "chatbot".into(),
            },
            vec![model()],
        )
        .unwrap();
        let (backend, commands) = workspace_ui_ports(workspace);
        let server = start_from(backend, commands, 0, None).unwrap();
        let address = server
            .address()
            .replace("http://0.0.0.0:", "http://127.0.0.1:");
        let client = reqwest::blocking::Client::new();
        let request = json!({
            "snapshot_revision": null, "agents": [], "selected_agent": null,
            "terminal_session": null, "terminal_revision": null,
        });

        let identity = client
            .post(format!("{address}/api/sync"))
            .header(reqwest::header::ACCEPT_ENCODING, "gzip;q=0, *;q=1")
            .json(&request)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap();
        assert!(
            identity
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .is_none()
        );
        assert_eq!(
            identity.headers().get(reqwest::header::VARY).unwrap(),
            "Accept-Encoding"
        );
        let identity_body = identity.bytes().unwrap();

        let compressed = client
            .post(format!("{address}/api/sync"))
            .header(reqwest::header::ACCEPT_ENCODING, "br, gzip;q=1")
            .json(&request)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            compressed
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .unwrap(),
            "gzip"
        );
        assert_eq!(
            compressed.headers().get(reqwest::header::VARY).unwrap(),
            "Accept-Encoding"
        );
        let compressed_body = compressed.bytes().unwrap();
        let mut decoder = flate2::read::GzDecoder::new(compressed_body.as_ref());
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&decoded).unwrap(),
            serde_json::from_slice::<serde_json::Value>(&identity_body).unwrap()
        );

        drop(server);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn http_polling_resumes_from_revisions_and_synchronizes_shared_drafts() {
        let directory = workspace();
        let workspace = Workspace::open(
            &directory,
            WorkspaceConfig {
                version: 2,
                model: "test".into(),
                effort: "unset".into(),
                orchestrator: "chatbot".into(),
            },
            vec![model()],
        )
        .unwrap();
        let (backend, commands) = workspace_ui_ports(workspace);
        let server = start_from(backend, commands, 0, None).unwrap();
        let address = server
            .address()
            .replace("http://0.0.0.0:", "http://127.0.0.1:");
        let client = reqwest::blocking::Client::new();
        let sync = |client: &reqwest::blocking::Client, body: serde_json::Value| {
            client
                .post(format!("{address}/api/sync"))
                .json(&body)
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<serde_json::Value>()
                .unwrap()
        };

        let initial = sync(
            &client,
            json!({
                "snapshot_revision": null, "agents": [], "selected_agent": null,
                "terminal_session": null, "terminal_revision": null,
            }),
        );
        assert_eq!(initial["type"], "state");
        let initial_revision = initial["snapshot"]["revision"].as_u64().unwrap();

        let created: serde_json::Value = client
            .post(format!("{address}/api/command"))
            .json(&json!({"command": "add_agent", "orchestrator": "chatbot"}))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        let agent_id = created["receipt"]["agent_id"].as_str().unwrap().to_owned();

        let after_create = sync(
            &client,
            json!({
                "snapshot_revision": initial_revision, "agents": [],
                "selected_agent": agent_id, "terminal_session": null,
                "terminal_revision": null,
            }),
        );
        let snapshot_revision = after_create["snapshot"]["revision"].as_u64().unwrap();
        let agent = after_create["snapshot"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == agent_id)
            .unwrap();
        let event_count = agent["event_count"].as_u64().unwrap();
        let mutation_revision = agent["mutation_revision"].as_u64().unwrap();
        assert!(
            after_create["event_updates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|update| { update["agent_id"] == agent_id && update["reset"] == true })
        );

        let forced_replay = sync(
            &client,
            json!({
                "snapshot_revision": snapshot_revision,
                "agents": [{
                    "id": agent_id, "event_count": event_count,
                    "mutation_revision": mutation_revision + 1,
                }],
                "selected_agent": agent_id, "terminal_session": null,
                "terminal_revision": null,
            }),
        );
        assert_eq!(forced_replay["snapshot"], serde_json::Value::Null);
        assert_eq!(forced_replay["event_updates"][0]["reset"], true);

        let updated: serde_json::Value = client
            .post(format!("{address}/api/command"))
            .json(&json!({
                "command": "update_input_draft", "agent_id": agent_id,
                "expected_revision": 0, "content": "draft survives polling",
            }))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(updated["receipt"]["accepted"], true);

        // A new HTTP client resumes entirely from the caller's revision cursors.
        let reconnected_client = reqwest::blocking::Client::new();
        let recovered = sync(
            &reconnected_client,
            json!({
                "snapshot_revision": snapshot_revision,
                "agents": [{
                    "id": agent_id, "event_count": event_count,
                    "mutation_revision": mutation_revision,
                }],
                "selected_agent": agent_id, "terminal_session": null,
                "terminal_revision": null,
            }),
        );
        let recovered_agent = recovered["snapshot"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == agent_id)
            .unwrap();
        assert_eq!(recovered_agent["input_draft"], "draft survives polling");
        assert!(recovered_agent["input_draft_revision"].as_u64().unwrap() > 0);
        assert!(recovered["event_updates"].as_array().unwrap().is_empty());

        let recovered_revision = recovered["snapshot"]["revision"].as_u64().unwrap();
        let recovered_draft_revision = recovered_agent["input_draft_revision"].as_u64().unwrap();
        let observer = reqwest::blocking::Client::new();
        let observed_initial = sync(
            &observer,
            json!({
                "snapshot_revision": null, "agents": [],
                "selected_agent": agent_id, "terminal_session": null,
                "terminal_revision": null,
            }),
        );
        assert_eq!(observed_initial["snapshot"]["revision"], recovered_revision);

        let shared_update: serde_json::Value = reconnected_client
            .post(format!("{address}/api/command"))
            .json(&json!({
                "command": "update_input_draft", "agent_id": agent_id,
                "expected_revision": recovered_draft_revision,
                "content": "shared between active WebUIs",
            }))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(shared_update["receipt"]["accepted"], true);

        let observed_shared = sync(
            &observer,
            json!({
                "snapshot_revision": recovered_revision,
                "agents": [{
                    "id": agent_id, "event_count": event_count,
                    "mutation_revision": mutation_revision,
                }],
                "selected_agent": agent_id, "terminal_session": null,
                "terminal_revision": null,
            }),
        );
        let observed_agent = observed_shared["snapshot"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == agent_id)
            .unwrap();
        assert_eq!(
            observed_agent["input_draft"],
            "shared between active WebUIs"
        );

        drop(server);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn real_http_clients_create_agents_atomically_and_read_without_consuming() {
        let directory = workspace();
        let workspace = Workspace::open(
            &directory,
            WorkspaceConfig {
                version: 2,
                model: "test".into(),
                effort: "unset".into(),
                orchestrator: "chatbot".into(),
            },
            vec![model()],
        )
        .unwrap();
        let (backend, commands) = workspace_ui_ports(workspace);
        let server = start_from(backend, commands, 0, None).unwrap();
        let bind_address = server.address().to_owned();
        assert!(bind_address.starts_with("http://0.0.0.0:"));
        let address = bind_address.replace("http://0.0.0.0:", "http://127.0.0.1:");
        let mut clients = Vec::new();
        for _ in 0..6 {
            let address = address.clone();
            clients.push(thread::spawn(move || {
                reqwest::blocking::Client::new()
                    .post(format!("{address}/api/command"))
                    .json(&json!({"command": "add_agent", "orchestrator": "chatbot"}))
                    .send()
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json::<serde_json::Value>()
                    .unwrap()["receipt"]["agent_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }));
        }
        let ids = clients
            .into_iter()
            .map(|client| client.join().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 6);

        let first: serde_json::Value = reqwest::blocking::get(format!("{address}/api/snapshot"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        let second: serde_json::Value = reqwest::blocking::get(format!("{address}/api/snapshot"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(
            first["tool_visibility"],
            json!({
                "hidden_names": [crate::agent_title::TOOL_NAME],
                "hidden_prefixes": ["WorkMap.", "Worker."],
                "activity_names": [crate::agent_toolbox::WORKER_WAIT],
            })
        );
        assert_eq!(first["agents"].as_array().unwrap().len(), 6);
        assert_eq!(
            first["orchestrators"],
            json!(["main-agent", "manager-agent", "chatbot"])
        );
        assert_eq!(first["default_orchestrator"], "chatbot");
        assert_eq!(first["agents"], second["agents"]);
        assert!(first.to_string().contains("reasoning_efforts"));
        assert_eq!(
            first["models"][0]["output_token_reservations"]["unset"],
            512
        );
        assert!(first.to_string().contains("prompt_submission_revision"));
        assert!(first.to_string().contains("input_draft_revision"));
        assert!(first.to_string().contains("input_draft"));
        assert!(!first.to_string().contains("must-not-reach-webui"));

        let id = ids.first().unwrap();
        let other_id = ids.iter().nth(1).unwrap();
        let activity: serde_json::Value =
            reqwest::blocking::get(format!("{address}/api/api-activity/{id}"))
                .unwrap()
                .json()
                .unwrap();
        assert_eq!(activity["active"], false);
        assert_eq!(activity["received_sse_events"], 0);
        let draft_update: serde_json::Value = reqwest::blocking::Client::new()
            .post(format!("{address}/api/command"))
            .json(&json!({
                "command": "update_input_draft",
                "agent_id": id,
                "expected_revision": 0,
                "content": "unfinished\nshared draft",
            }))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(draft_update["receipt"]["kind"], "input_draft_updated");
        assert_eq!(draft_update["receipt"]["accepted"], true);
        let draft_revision = draft_update["receipt"]["input_draft_revision"]
            .as_u64()
            .unwrap();
        let stale_update: serde_json::Value = reqwest::blocking::Client::new()
            .post(format!("{address}/api/command"))
            .json(&json!({
                "command": "update_input_draft",
                "agent_id": id,
                "expected_revision": 0,
                "content": "stale empty draft",
            }))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(stale_update["receipt"]["accepted"], false);
        assert_eq!(
            stale_update["receipt"]["input_draft_revision"],
            draft_revision
        );
        let reconnected: serde_json::Value =
            reqwest::blocking::get(format!("{address}/api/snapshot"))
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .unwrap();
        let reconnect_agent = reconnected["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == id.as_str())
            .unwrap();
        let isolated_agent = reconnected["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == other_id.as_str())
            .unwrap();
        assert_eq!(reconnect_agent["input_draft"], "unfinished\nshared draft");
        assert!(reconnect_agent["input_draft_revision"].as_u64().unwrap() > 0);
        assert_eq!(isolated_agent["input_draft"], "");
        assert_eq!(isolated_agent["input_draft_revision"], 0);
        let events: serde_json::Value =
            reqwest::blocking::get(format!("{address}/api/events/{id}?after=0&mutation=0"))
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .unwrap();
        assert!(events["events"].as_array().unwrap().len() >= 3);
        assert_eq!(events["reset"], false);
        assert_eq!(events["turn_history_updated"], true);
        assert_eq!(events["turn_history"], serde_json::Value::Null);

        let initial_count = events["event_count"].as_u64().unwrap() as usize;
        let unchanged: serde_json::Value = reqwest::blocking::get(format!(
            "{address}/api/events/{id}?after={initial_count}&mutation=0"
        ))
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
        assert_eq!(unchanged["turn_history_updated"], false);
        assert_eq!(unchanged["turn_history"], serde_json::Value::Null);

        reqwest::blocking::Client::new()
            .post(format!("{address}/api/command"))
            .json(&json!({"command": "clear_context", "agent_id": id}))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap();
        let after_clear = (0..100)
            .find_map(|_| {
                let snapshot: serde_json::Value =
                    reqwest::blocking::get(format!("{address}/api/events/{id}?after=0&mutation=0"))
                        .unwrap()
                        .error_for_status()
                        .unwrap()
                        .json()
                        .unwrap();
                if snapshot["event_count"].as_u64()? as usize > initial_count {
                    Some(snapshot)
                } else {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("clear event should become visible");
        let clear_id = after_clear["events"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|event| event.get("ContextCleared"))
            .and_then(|event| event["id"].as_u64())
            .unwrap();
        reqwest::blocking::Client::new()
            .post(format!("{address}/api/command"))
            .json(&json!({
                "command": "rewind_context",
                "agent_id": id,
                "event_id": clear_id,
            }))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap();
        let after_rewind = (0..100)
            .find_map(|_| {
                let snapshot: serde_json::Value = reqwest::blocking::get(format!(
                    "{address}/api/events/{id}?after={}&mutation=0",
                    initial_count + 1
                ))
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .unwrap();
                if snapshot["mutation_revision"].as_u64()? > 0 {
                    Some(snapshot)
                } else {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("rewind mutation should become visible");
        assert_eq!(after_rewind["reset"], true);
        assert_eq!(after_rewind["event_count"], initial_count);
        assert_eq!(
            after_rewind["events"].as_array().unwrap().len(),
            initial_count
        );

        let html = reqwest::blocking::get(format!("{address}/"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
        assert!(html.contains("ME-S"));
        assert!(html.contains("/app.js"));

        let transcript_runtime = reqwest::blocking::get(format!("{address}/transcript.js"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
        assert!(transcript_runtime.contains("MeTranscript"));
        assert!(transcript_runtime.contains("reconcileHtmlChildren"));
        let markdown_adapter = reqwest::blocking::get(format!("{address}/markdown.js"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
        assert!(markdown_adapter.contains("MeMarkdown"));
        let markdown_engine = reqwest::blocking::get(format!("{address}/markdown-it.js"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
        assert!(markdown_engine.contains("markdownit=t()"));
        let latex_engine = reqwest::blocking::get(format!("{address}/katex.js"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
        assert!(latex_engine.contains("KaTeX"));
        let latex_style = reqwest::blocking::get(format!("{address}/katex.css"))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
        assert!(latex_style.contains("fonts/KaTeX_Main-Regular.woff2"));
        let latex_font =
            reqwest::blocking::get(format!("{address}/fonts/KaTeX_Main-Regular.woff2"))
                .unwrap()
                .error_for_status()
                .unwrap();
        assert_eq!(
            latex_font
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .unwrap(),
            "font/woff2"
        );
        assert!(latex_font.bytes().unwrap().len() > 10_000);

        let socket = address.strip_prefix("http://").unwrap().to_owned();
        drop(server);
        assert!(std::net::TcpStream::connect(socket).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
