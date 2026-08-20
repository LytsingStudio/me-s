use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::{
    Result,
    event::{Event, EventId},
    turn_history,
    ui_backend::{
        CHAT_ACTIVITY_TOOL_NAMES, CHAT_HIDDEN_TOOL_NAMES, CHAT_HIDDEN_TOOL_PREFIXES, UiBackend,
        UiCommand, UiCommandGateway, UiCommandReceipt, UiModelOption, UiSnapshot,
    },
    workspace::AgentId,
};

pub const DEFAULT_PORT: u16 = 38199;
pub const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0";
const INDEX_HTML: &str = include_str!("webui/index.html");
const APP_JS: &str = include_str!("webui/app.js");
const MARKDOWN_JS: &str = include_str!("webui/markdown.js");
const MARKDOWN_IT_JS: &str = include_str!("webui/vendor/markdown-it.min.js");
const KATEX_JS: &str = include_str!("webui/vendor/katex.min.js");
const KATEX_CSS: &str = include_str!("webui/vendor/katex.min.css");
const STYLE_CSS: &str = include_str!("webui/style.css");
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
const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BATCH_BYTES: usize = 512 * 1024;
const MAX_LOGIN_BYTES: usize = 4096;
const SESSION_COOKIE: &str = "me_webui_session";

struct WebAuth {
    password_hash: Option<String>,
    sessions: Mutex<HashSet<String>>,
}

impl WebAuth {
    fn new(passkey: Option<&str>) -> Result<Self> {
        let password_hash = passkey
            .map(|passkey| -> Result<String> {
                let mut salt = [0_u8; 16];
                getrandom::fill(&mut salt)
                    .map_err(|error| format!("failed to generate WebUI password salt: {error}"))?;
                let salt = SaltString::encode_b64(&salt)
                    .map_err(|error| format!("failed to encode WebUI password salt: {error}"))?;
                let hash = Argon2::default()
                    .hash_password(passkey.as_bytes(), &salt)
                    .map_err(|error| format!("failed to hash WebUI password: {error}"))?;
                Ok(hash.to_string())
            })
            .transpose()?;
        Ok(Self {
            password_hash,
            sessions: Mutex::new(HashSet::new()),
        })
    }

    fn required(&self) -> bool {
        self.password_hash.is_some()
    }

    fn authorized(&self, request: &Request) -> bool {
        if !self.required() {
            return true;
        }
        let Some(token) = request_cookie(request, SESSION_COOKIE) else {
            return false;
        };
        self.sessions
            .lock()
            .is_ok_and(|sessions| sessions.contains(token))
    }

    fn login(&self, passkey: &str) -> Result<Option<String>> {
        let Some(encoded) = &self.password_hash else {
            return Ok(Some(String::new()));
        };
        let parsed = PasswordHash::new(encoded)
            .map_err(|error| format!("stored WebUI password hash is invalid: {error}"))?;
        if Argon2::default()
            .verify_password(passkey.as_bytes(), &parsed)
            .is_err()
        {
            return Ok(None);
        }
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| format!("failed to generate WebUI session: {error}"))?;
        let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        self.sessions
            .lock()
            .map_err(|_| "WebUI session store is unavailable")?
            .insert(token.clone());
        Ok(Some(token))
    }
}

pub struct WebUiServer {
    address: String,
    port: u16,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
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
    start_with_server(backend, commands, server, port, passkey)
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
    let commands: Arc<dyn UiCommandGateway> = Arc::new(commands);
    let auth = Arc::new(WebAuth::new(passkey)?);
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker = thread::Builder::new()
        .name("me-webui".into())
        .spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(request)) => {
                        let backend = Arc::clone(&backend);
                        let commands = Arc::clone(&commands);
                        let auth = Arc::clone(&auth);
                        let _ = thread::Builder::new()
                            .name("me-webui-request".into())
                            .spawn(move || {
                                serve(request, backend.as_ref(), commands.as_ref(), auth.as_ref());
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
}

#[derive(Deserialize)]
struct SyncAgentCursor {
    id: String,
    event_count: usize,
    mutation_revision: u64,
}

fn sync_state_payload(
    backend: &dyn UiBackend,
    snapshot_revision: Option<u64>,
    cursors: Vec<SyncAgentCursor>,
    selected_agent: Option<String>,
    terminal_session: Option<String>,
    terminal_revision: Option<u64>,
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
    let mut agent_indexes = (0..snapshot.agents.len()).collect::<Vec<_>>();
    agent_indexes.sort_by_key(|index| {
        usize::from(selected_agent.as_ref() != Some(&snapshot.agents[*index].id))
    });
    for index in agent_indexes {
        let agent = &snapshot.agents[index];
        let cursor = cursors.get(agent.id.as_str());
        if !snapshot_changed
            && cursor.is_some_and(|cursor| {
                cursor.event_count == agent.events.len()
                    && cursor.mutation_revision == agent.mutation_revision
            })
        {
            continue;
        }
        let reset = cursor.is_none_or(|cursor| {
            cursor.mutation_revision != agent.mutation_revision
                || cursor.event_count > agent.events.len()
        });
        let start = if reset {
            0
        } else {
            cursor.map_or(0, |cursor| cursor.event_count)
        };
        if !reset && start == agent.events.len() {
            continue;
        }
        let available = &agent.events[start..];
        if available.is_empty() {
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
        let turn_history_updated = turn_history_needs_refresh(reset, start, events);
        let turn_history = if turn_history_updated && agent.orchestrator_name != "worker-agent" {
            turn_history::latest_snapshot(&agent.events)?
        } else {
            None
        };
        event_updates.push(json!({
            "agent_id": agent.id.to_string(),
            "reset": reset,
            "event_count": agent.events.len(),
            "mutation_revision": agent.mutation_revision,
            "turn_history_updated": turn_history_updated,
            "turn_history": turn_history,
            "events": events,
        }));
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
    let snapshot_payload = snapshot_changed.then(|| snapshot_metadata(snapshot));
    let selected_agent_id = selected_agent.as_ref().map(ToString::to_string);
    Ok(json!({
        "ok": true,
        "type": "state",
        "snapshot": snapshot_payload,
        "event_updates": event_updates,
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
    auth: &WebAuth,
) {
    let result = route(&mut request, backend, commands, auth);
    let response = match result {
        Ok(response) => response,
        Err(error) => json_response(
            StatusCode(500),
            &json!({"ok": false, "error": error.to_string()}),
        ),
    };
    let _ = request.respond(response);
}

type HttpResponse = Response<std::io::Cursor<Vec<u8>>>;

fn route(
    request: &mut Request,
    backend: &dyn UiBackend,
    commands: &dyn UiCommandGateway,
    auth: &WebAuth,
) -> Result<HttpResponse> {
    let (path, query) = split_url(request.url());
    if request.method() == &Method::Get
        && let Some((_, font)) = KATEX_FONTS.iter().find(|(font_path, _)| *font_path == path)
    {
        return Ok(bytes_response("font/woff2", font));
    }
    match (request.method(), path) {
        (&Method::Get, "/") => {
            return Ok(text_response("text/html; charset=utf-8", INDEX_HTML));
        }
        (&Method::Get, "/app.js") => {
            return Ok(text_response("text/javascript; charset=utf-8", APP_JS));
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
        (&Method::Get, "/api/auth/status") => return auth_status_response(request, auth),
        (&Method::Post, "/api/auth/login") => return login_response(request, auth),
        _ => {}
    }
    if !auth.authorized(request) {
        return Ok(unauthorized_response());
    }
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
        (&Method::Post, "/api/command") => command_response(request, commands),
        _ => Ok(json_response(
            StatusCode(404),
            &json!({"ok": false, "error": "not found"}),
        )),
    }
}

fn sync_response(request: &mut Request, backend: &dyn UiBackend) -> Result<HttpResponse> {
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
    )?;
    Ok(json_response(StatusCode(200), &payload))
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
        &snapshot_metadata(backend.snapshot()?),
    ))
}

fn snapshot_metadata(snapshot: UiSnapshot) -> SnapshotResponse {
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
            edb_path: agent.edb_path.display().to_string(),
            edb_size_bytes: agent.edb_size_bytes,
            event_count: agent.events.len(),
            last_event_id: agent.events.last().map(Event::id),
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
            workspace: environment.workspace.display().to_string(),
            system: format!("{}/{}", environment.os, environment.arch),
        },
        agents,
        models,
        orchestrators: orchestrators.to_vec(),
        default_orchestrator,
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
            "edb_path": draft.edb_path.display().to_string(),
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
}

fn auth_status_response(request: &Request, auth: &WebAuth) -> Result<HttpResponse> {
    Ok(json_response(
        StatusCode(200),
        &json!({
            "ok": true,
            "required": auth.required(),
            "authenticated": auth.authorized(request),
        }),
    ))
}

fn login_response(request: &mut Request, auth: &WebAuth) -> Result<HttpResponse> {
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
    let Some(token) = auth.login(&login.password)? else {
        return Ok(json_response(
            StatusCode(401),
            &json!({"ok": false, "error": "密码错误"}),
        ));
    };
    Ok(
        json_response(StatusCode(200), &json!({"ok": true, "authenticated": true}))
            .with_header(session_cookie(&token)),
    )
}

fn request_cookie<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("Cookie"))
        .flat_map(|header| header.value.as_str().split(';'))
        .find_map(|cookie| {
            let (candidate, value) = cookie.trim().split_once('=')?;
            (candidate == name).then_some(value)
        })
}

fn session_cookie(token: &str) -> Header {
    Header::from_bytes(
        "Set-Cookie",
        format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict"),
    )
    .expect("session cookie is generated from ASCII bytes")
}

fn unauthorized_response() -> HttpResponse {
    json_response(
        StatusCode(401),
        &json!({"ok": false, "error": "WebUI authentication required"}),
    )
}

fn json_response(status: StatusCode, value: &impl Serialize) -> HttpResponse {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|error| format!(r#"{{"ok":false,"error":"{error}"}}"#).into_bytes());
    Response::from_data(body)
        .with_status_code(status)
        .with_header(content_type("application/json; charset=utf-8"))
        .with_header(no_store())
}

fn text_response(content_type_value: &'static str, content: &'static str) -> HttpResponse {
    Response::from_data(content.as_bytes().to_vec())
        .with_status_code(StatusCode(200))
        .with_header(content_type(content_type_value))
        .with_header(no_store())
}

fn bytes_response(content_type_value: &'static str, content: &'static [u8]) -> HttpResponse {
    Response::from_data(content.to_vec())
        .with_status_code(StatusCode(200))
        .with_header(content_type(content_type_value))
        .with_header(no_store())
}

fn content_type(value: &'static str) -> Header {
    Header::from_bytes("Content-Type", value).expect("static Content-Type header is valid")
}

fn no_store() -> Header {
    Header::from_bytes("Cache-Control", "no-store").expect("static Cache-Control header is valid")
}

#[cfg(test)]
mod tests {
    use std::{fs, net::TcpListener, path::PathBuf, thread};

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
    fn http_sync_event_batches_are_bounded_without_splitting_events() {
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
        assert!(APP_JS.contains("const PORTRAIT_LAYOUT = matchMedia(\"(orientation: portrait)\")"));
        assert!(APP_JS.contains("agent.title || agent.id"));
        assert!(APP_JS.contains("function toolIsChatVisible(name)"));
    }

    #[test]
    fn embedded_webui_offers_a_cookie_backed_send_shortcut_preference() {
        assert!(INDEX_HTML.contains("Enter 换行 · Shift/Alt+Enter 发送"));
        assert!(APP_JS.contains("const SEND_SHORTCUT_COOKIE = \"me_send_shortcut\""));
        assert!(APP_JS.contains("Max-Age=31536000; Path=/; SameSite=Lax"));
        assert!(APP_JS.contains("openChoiceDrawer(\"发送设置\""));
        assert!(
            APP_JS.contains("elements.send.addEventListener(\"click\", submitOrOpenSendSettings)")
        );
        assert!(APP_JS.contains("visible && enterSubmitsPrompt(event)"));
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
        assert!(APP_JS.contains("if (typeof ResizeObserver === \"function\")"));
        assert!(APP_JS.contains("return { observe() {}, disconnect() {} };"));
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
        assert!(APP_JS.contains("return parts.join(\" \");"));
        assert!(!APP_JS.contains("return parts.join(\" · \");"));
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
    fn ordinary_tool_cards_are_single_line_summaries_until_expanded() {
        assert!(APP_JS.contains("function toolCardView(tool)"));
        assert!(APP_JS.contains("rows: expanded ? toolRows(tool, true) : []"));
        assert!(APP_JS.contains("view.expanded ? renderToolDetails(view.rows) : \"\""));
        assert!(APP_JS.contains("function updateToolCardNode(node, tool, followsTool ="));
        assert!(APP_JS.contains("if (message.kind === \"tool\")"));
        assert!(APP_JS.contains("class=\"tool-name\""));
        assert!(APP_JS.contains("class=\"tool-brief\""));
        assert!(STYLE_CSS.contains(
            ".tool-header { display: grid; grid-template-columns: 15px max-content minmax(0, 1fr)"
        ));
        assert!(STYLE_CSS.contains(".tool-brief { min-width: 0; overflow: hidden;"));
        assert!(STYLE_CSS.contains("text-overflow: ellipsis; white-space: nowrap;"));
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
        let latex_style = INDEX_HTML.find("/katex.css").unwrap();
        let application_style = INDEX_HTML.find("/style.css").unwrap();
        assert!(engine < latex && latex < adapter && adapter < application);
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
    fn embedded_webui_allows_worker_model_and_effort_controls() {
        assert!(APP_JS.contains("agent.orchestrator === \"worker-agent\" ? \"Worker\""));
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
    fn embedded_webui_has_targeted_agent_action_menus_with_delete_confirmation() {
        assert!(INDEX_HTML.contains("id=\"agent-menu\""));
        assert!(INDEX_HTML.contains("id=\"delete-agent-menu\""));
        assert!(APP_JS.contains("data-agent-menu="));
        assert!(APP_JS.contains("function openAgentMenu(trigger, agentId)"));
        assert!(APP_JS.contains("async function openDeleteAgent(agentId = state.selectedAgent)"));
        assert!(APP_JS.contains("openConfirm(\"删除会话？\""));
        assert!(APP_JS.contains("agent_id: agentId"));
        assert!(STYLE_CSS.contains(".user-message-menu, .agent-menu"));
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
        assert!(
            STYLE_CSS.contains(".connection-overlay { position: fixed; inset: 0; z-index: 120;")
        );
        assert!(APP_JS.contains("api(\"/api/sync\""));
        assert!(!APP_JS.contains("new WebSocket"));
        assert!(APP_JS.contains("if (state.syncInFlight"));
        assert!(APP_JS.contains("HTTP_SYNC_TIMEOUT_MS"));
        assert!(APP_JS.contains("function failHttpSync(title, error)"));
        assert!(APP_JS.contains("scheduleHttpSync(message.more_events ? 0"));
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
    fn streaming_assistant_updates_keep_the_stable_message_node() {
        assert!(
            APP_JS.contains(
                "function updateMessageNode(node, message, afterTool, followsTool, index)"
            )
        );
        assert!(APP_JS.contains("if (message.kind === \"assistant\")"));
        assert!(
            APP_JS.contains("if (markdown.innerHTML !== rendered) markdown.innerHTML = rendered")
        );
        assert!(APP_JS.contains(
            "if (current.meRenderRevision !== revision) updateMessageNode(current, message, afterTool, followsTool, index)"
        ));
        assert!(APP_JS.contains("viewport.scrollTop = viewport.scrollHeight"));
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
        assert!(APP_JS.contains("function refreshRunningToolElapsed()"));
        assert!(APP_JS.contains("setInterval(refreshRunningToolElapsed, 100)"));
        assert!(!APP_JS.contains("TOOL_MARKER_OPACITY"));
        assert!(!APP_JS.contains("toolAnimationTick"));
        assert!(!APP_JS.contains("node.style.opacity"));
    }

    #[test]
    fn active_sidebar_agent_dot_breathes_without_javascript_animation() {
        assert!(
            APP_JS
                .contains("row.querySelector(\".agent-dot\").classList.toggle(\"active\", active)")
        );
        assert_eq!(
            APP_JS.matches("<span class=\"agent-dot\"></span>").count(),
            1
        );
        assert!(
            STYLE_CSS.contains(
                ".agent-dot.active { border-color: var(--cyan); background: var(--cyan);"
            )
        );
        assert!(STYLE_CSS.contains("animation: agent-dot-breathe 1.4s ease-in-out infinite"));
        assert!(STYLE_CSS.contains("@keyframes agent-dot-breathe"));
        assert!(STYLE_CSS.contains(
            "@media (prefers-reduced-motion: reduce) { .agent-dot.active { animation: none; } }"
        ));
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
        assert!(APP_JS.contains("status: apiActivityChanged"));
        assert!(APP_JS.contains("receivedSseEvents"));
        assert!(APP_JS.contains("if (request.status || changes.status) renderStatus()"));
        assert!(APP_JS.contains("else if (request.workerEvents && state.view.kind === \"chat\")"));
        assert!(APP_JS.contains("function refreshWorkerActivityCards()"));
        assert!(APP_JS.contains("function showView(view)"));
        assert_eq!(APP_JS.matches("renderAll();").count(), 2);
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
        assert!(APP_JS.contains("function refreshRunningToolElapsed()"));
        assert!(APP_JS.contains("if (inputHasPriority()) return"));
        assert!(APP_JS.contains("transcriptFrom: null"));
        assert!(APP_JS.contains(
            "renderTranscript(Boolean(changes.fullReplay), changes.transcriptFrom ?? 0)"
        ));
        assert!(APP_JS.contains("function reconcileTranscript(messages, changedFrom = 0)"));
        assert!(APP_JS.contains("for (let index = start; index < messages.length; index += 1)"));
        assert!(!APP_JS.contains("projection.messages.filter((message) =>"));
        assert!(APP_JS.contains("projection._messageByKey.get(`tool:${node.dataset.workerWait}`)"));
        assert!(APP_JS.contains("function markPendingPromptConfirmation(store, changes)"));
        assert!(APP_JS.contains("return markPendingPromptConfirmation(store, changes)"));
    }

    #[test]
    fn embedded_webui_does_not_fight_manual_scrolling() {
        assert!(INDEX_HTML.contains("id=\"transcript-content\""));
        assert!(APP_JS.contains("function createTranscriptBottomFollower("));
        assert!(APP_JS.contains("new ResizeObserver(callback)"));
        assert!(APP_JS.contains("resizeObserver.observe(viewport)"));
        assert!(APP_JS.contains("resizeObserver.observe(content)"));
        assert!(APP_JS.contains("function suspendTranscriptAutoFollow()"));
        assert!(APP_JS.contains("if (interacting) following = isNearBottom()"));
        assert!(APP_JS.contains(
            "elements.transcript.addEventListener(\"wheel\", suspendTranscriptAutoFollow"
        ));
        assert!(APP_JS.contains("function beginConfirmedPromptRender(changes,"));
        assert!(APP_JS.contains("if (!changes.promptConfirmed) return false;"));
        assert!(APP_JS.contains("bottomFollower.follow();"));
        assert!(APP_JS.contains("if (transcriptChanged || promptConfirmed)"));
        assert!(APP_JS.contains("transcriptBottomFollower.layoutChanged();"));
        assert_eq!(
            APP_JS
                .matches("viewport.scrollTop = viewport.scrollHeight")
                .count(),
            1
        );
        assert!(!APP_JS.contains("elements.transcript.scrollTop = previousScrollTop"));
        assert!(STYLE_CSS.contains("overflow-anchor: auto"));
        assert!(
            STYLE_CSS.contains(".transcript-content { display: flow-root; min-height: 100%; }")
        );
        assert!(APP_JS.contains("function createAgentRow(agent)"));
        assert!(!APP_JS.contains("elements.agents.innerHTML = state.snapshot.agents.map"));
    }

    #[test]
    fn embedded_webui_objective_summary_uses_the_title_without_a_label() {
        assert!(
            APP_JS.contains("${active ? \"■\" : \"□\"} ${escapeHtml(current.objective.title)}")
        );
        assert!(!APP_JS.contains("目标: ${escapeHtml(current.objective.title)}"));
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
    fn passkey_protects_every_operational_api_with_an_http_only_session() {
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
        let client = reqwest::blocking::Client::new();

        assert!(
            client
                .get(format!("{address}/"))
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
