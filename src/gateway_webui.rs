use std::{
    io::Read,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::{
    Result, gateway::Gateway, gateway_settings::GatewaySettings, web_auth::WebSessionAuth,
};

pub const DEFAULT_GATEWAY_PORT: u16 = 38200;
pub const GATEWAY_BIND_ADDRESS: &str = "0.0.0.0";
const INDEX_HTML: &str = include_str!("gateway_webui/index.html");
const APP_JS: &str = include_str!("gateway_webui/app.js");
const STYLE_CSS: &str = include_str!("gateway_webui/style.css");
const THEME_JS: &str = include_str!("webui/theme.js");
const THEME_CSS: &str = include_str!("webui/theme.css");
const TRANSCRIPT_JS: &str = include_str!("webui/transcript.js");
const MARKDOWN_JS: &str = include_str!("webui/markdown.js");
const MARKDOWN_IT_JS: &str = include_str!("webui/vendor/markdown-it.min.js");
const KATEX_JS: &str = include_str!("webui/vendor/katex.min.js");
const KATEX_CSS: &str = include_str!("webui/vendor/katex.min.css");
const SESSION_COOKIE: &str = "me_gateway_session";
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_LOGIN_BYTES: usize = 4096;

pub struct GatewayWebUiServer {
    address: String,
    port: u16,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl GatewayWebUiServer {
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for GatewayWebUiServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start(gateway: Arc<Gateway>, passkey: Option<&str>) -> Result<GatewayWebUiServer> {
    start_from(gateway, DEFAULT_GATEWAY_PORT, passkey)
}

fn start_from(
    gateway: Arc<Gateway>,
    first_port: u16,
    passkey: Option<&str>,
) -> Result<GatewayWebUiServer> {
    let (server, port) = bind_first_available(first_port)?;
    let address = format!("http://{GATEWAY_BIND_ADDRESS}:{port}");
    let auth = Arc::new(WebSessionAuth::new(passkey)?);
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker = thread::Builder::new()
        .name("me-gateway-webui".into())
        .spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(request)) => {
                        let gateway = Arc::clone(&gateway);
                        let auth = Arc::clone(&auth);
                        let _ = thread::Builder::new()
                            .name("me-gateway-request".into())
                            .spawn(move || serve(request, gateway, auth));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("warning: me-gateway WebUI listener stopped: {error}");
                        break;
                    }
                }
            }
        })?;
    Ok(GatewayWebUiServer {
        address,
        port,
        shutdown,
        worker: Some(worker),
    })
}

fn bind_first_available(first_port: u16) -> Result<(Server, u16)> {
    for port in first_port..=u16::MAX {
        match Server::http((GATEWAY_BIND_ADDRESS, port)) {
            Ok(server) => return Ok((server, port)),
            Err(error)
                if bind_error_kind(error.as_ref()) == Some(std::io::ErrorKind::AddrInUse) => {}
            Err(error) => {
                return Err(format!("failed to bind me-gateway port {port}: {error}").into());
            }
        }
    }
    Err(format!("no available me-gateway port at or above {first_port}").into())
}

fn bind_error_kind(error: &(dyn std::error::Error + 'static)) -> Option<std::io::ErrorKind> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            return Some(error.kind());
        }
        current = error.source();
    }
    None
}

fn serve(mut request: Request, gateway: Arc<Gateway>, auth: Arc<WebSessionAuth>) {
    let result = route(&mut request, gateway.as_ref(), auth.as_ref());
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            eprintln!("warning: me-gateway request failed: {error}");
            json_response(
                StatusCode(500),
                &json!({"ok": false, "error": "请求未能完成"}),
            )
        }
    };
    let _ = request.respond(response);
}

type HttpResponse = Response<std::io::Cursor<Vec<u8>>>;

fn route(request: &mut Request, gateway: &Gateway, auth: &WebSessionAuth) -> Result<HttpResponse> {
    let url = request.url();
    let has_query = url.contains('?');
    let path = url.split('?').next().unwrap_or(url).to_owned();
    if request.method() == &Method::Get
        && let Some(font) = crate::webui::shared_katex_font(&path)
    {
        return Ok(bytes_response("font/woff2", font));
    }
    if request.method() == &Method::Get
        && let Some((content_type, content)) = crate::webui::shared_session_terminal_asset(&path)
    {
        return Ok(text_response(content_type, content));
    }
    match (request.method(), path.as_str()) {
        (&Method::Get, "/") => return Ok(text_response("text/html; charset=utf-8", INDEX_HTML)),
        (&Method::Get, "/theme.js") => {
            return Ok(text_response("text/javascript; charset=utf-8", THEME_JS));
        }
        (&Method::Get, "/app.js") => {
            return Ok(text_response("text/javascript; charset=utf-8", APP_JS));
        }
        (&Method::Get, "/transcript.js") => {
            return Ok(text_response(
                "text/javascript; charset=utf-8",
                TRANSCRIPT_JS,
            ));
        }
        (&Method::Get, "/style.css") => {
            return Ok(text_response("text/css; charset=utf-8", STYLE_CSS));
        }
        (&Method::Get, "/theme.css") => {
            return Ok(text_response("text/css; charset=utf-8", THEME_CSS));
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
        (&Method::Get, "/api/auth/status") => return auth_status(request, auth),
        (&Method::Post, "/api/auth/login") => return login(request, auth),
        _ => {}
    }
    if !auth.authorized(request_cookie(request, SESSION_COOKIE)) {
        return Ok(json_response(
            StatusCode(401),
            &json!({"ok": false, "error": "需要登录"}),
        ));
    }

    match (request.method(), path.as_str()) {
        (&Method::Get, "/api/gateway/state") => {
            Ok(json_response(StatusCode(200), &gateway.snapshot()?))
        }
        (&Method::Post, "/api/gateway/selection") => {
            let selection: SelectionRequest = read_json(request, MAX_BODY_BYTES)?;
            gateway.select(&selection.workspace_id, selection.agent_id)?;
            Ok(json_response(StatusCode(200), &json!({"ok": true})))
        }
        (&Method::Post, "/api/gateway/directories") => {
            let input: DirectoryRequest = read_json(request, MAX_BODY_BYTES)?;
            let path = input.path.map(PathBuf::from);
            let listing = if input.roots {
                gateway.list_directory_roots()
            } else {
                gateway.list_directories(path.as_deref())
            };
            match listing {
                Ok(listing) => Ok(json_response(StatusCode(200), &listing)),
                Err(error) => {
                    eprintln!("warning: host directory listing failed: {error}");
                    Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": "无法读取该位置"}),
                    ))
                }
            }
        }
        (&Method::Post, "/api/gateway/workspaces/open") => {
            let input: OpenWorkspaceRequest = read_json(request, MAX_BODY_BYTES)?;
            match gateway.open_workspace(&PathBuf::from(input.path)) {
                Ok(id) => Ok(json_response(
                    StatusCode(200),
                    &json!({"ok": true, "workspace_id": id}),
                )),
                Err(error) => {
                    eprintln!("warning: open Workspace failed: {error}");
                    Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": "无法打开该工作区"}),
                    ))
                }
            }
        }
        (&Method::Post, "/api/gateway/workspaces/create") => {
            let input: CreateWorkspaceRequest = read_json(request, MAX_BODY_BYTES)?;
            match gateway.create_workspace(&PathBuf::from(input.parent), &input.name) {
                Ok(id) => Ok(json_response(
                    StatusCode(200),
                    &json!({"ok": true, "workspace_id": id}),
                )),
                Err(error) => {
                    eprintln!("warning: create Workspace failed: {error}");
                    Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": "无法创建该工作区"}),
                    ))
                }
            }
        }
        (&Method::Post, path) if workspace_close_id(path).is_some() => {
            let id = workspace_close_id(path).unwrap();
            match gateway.close_workspace(id) {
                Ok(()) => Ok(json_response(StatusCode(200), &json!({"ok": true}))),
                Err(error) => {
                    eprintln!("warning: close Workspace failed: {error}");
                    Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": "无法关闭该工作区"}),
                    ))
                }
            }
        }
        (&Method::Get, "/api/gateway/settings") => {
            Ok(json_response(StatusCode(200), &gateway.settings()?))
        }
        (&Method::Post, "/api/gateway/settings") => {
            let settings: GatewaySettings = read_json(request, MAX_BODY_BYTES)?;
            match gateway.save_settings(settings) {
                Ok(settings) => Ok(json_response(StatusCode(200), &settings)),
                Err(error) => {
                    eprintln!("warning: save global settings failed: {error}");
                    Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": "无法保存设置"}),
                    ))
                }
            }
        }
        (method, path) if !has_query && workspace_proxy_path(path).is_some() => {
            let (workspace_id, child_path) = workspace_proxy_path(path).unwrap();
            let method = match method {
                &Method::Get => reqwest::Method::GET,
                &Method::Post => reqwest::Method::POST,
                _ => {
                    return Ok(json_response(
                        StatusCode(405),
                        &json!({"ok": false, "error": "不支持的操作"}),
                    ));
                }
            };
            let content_type = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Content-Type"))
                .map(|header| header.value.as_str().to_owned());
            let accept_encoding = request
                .headers()
                .iter()
                .filter(|header| header.field.equiv("Accept-Encoding"))
                .map(|header| header.value.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let accept_encoding = (!accept_encoding.is_empty()).then_some(accept_encoding);
            let body = read_body(request, MAX_BODY_BYTES)?;
            match gateway.proxy(
                workspace_id,
                method,
                child_path,
                content_type.as_deref(),
                accept_encoding.as_deref(),
                body,
            ) {
                Ok(response) => Ok(proxy_response(response)),
                Err(error) => {
                    eprintln!("warning: Workspace request failed: {error}");
                    Ok(json_response(
                        StatusCode(502),
                        &json!({"ok": false, "error": "该工作区已停止运行"}),
                    ))
                }
            }
        }
        _ => Ok(json_response(
            StatusCode(404),
            &json!({"ok": false, "error": "未找到该页面"}),
        )),
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

#[derive(Deserialize)]
struct SelectionRequest {
    workspace_id: String,
    agent_id: Option<String>,
}

#[derive(Deserialize)]
struct DirectoryRequest {
    path: Option<String>,
    #[serde(default)]
    roots: bool,
}

#[derive(Deserialize)]
struct OpenWorkspaceRequest {
    path: String,
}

#[derive(Deserialize)]
struct CreateWorkspaceRequest {
    parent: String,
    name: String,
}

fn auth_status(request: &Request, auth: &WebSessionAuth) -> Result<HttpResponse> {
    Ok(json_response(
        StatusCode(200),
        &json!({
            "ok": true,
            "required": auth.required(),
            "authenticated": auth.authorized(request_cookie(request, SESSION_COOKIE)),
        }),
    ))
}

fn login(request: &mut Request, auth: &WebSessionAuth) -> Result<HttpResponse> {
    if !auth.required() {
        return Ok(json_response(
            StatusCode(200),
            &json!({"ok": true, "authenticated": true}),
        ));
    }
    let login: LoginRequest = read_json(request, MAX_LOGIN_BYTES)?;
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

fn read_json<T: DeserializeOwned>(request: &mut Request, limit: usize) -> Result<T> {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str());
    if content_type.is_none_or(|value| !value.starts_with("application/json")) {
        return Err("Content-Type must be application/json".into());
    }
    Ok(serde_json::from_slice(&read_body(request, limit)?)?)
}

fn read_body(request: &mut Request, limit: usize) -> Result<Vec<u8>> {
    let length = request.body_length().unwrap_or(0);
    if length > limit {
        return Err("request body is too large".into());
    }
    let mut body = Vec::with_capacity(length.min(limit));
    request
        .as_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > limit {
        return Err("request body is too large".into());
    }
    Ok(body)
}

fn workspace_close_id(path: &str) -> Option<&str> {
    let id = path
        .strip_prefix("/api/gateway/workspaces/")?
        .strip_suffix("/close")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

fn workspace_proxy_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/api/workspaces/")?;
    let (workspace_id, child_path) = rest.split_once('/')?;
    (!workspace_id.is_empty() && !child_path.is_empty()).then_some((workspace_id, child_path))
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
    .expect("gateway session cookie is valid")
}

fn proxy_response(response: crate::gateway::ProxyResponse) -> HttpResponse {
    let mut response_body =
        Response::from_data(response.body).with_status_code(StatusCode(response.status));
    for (name, value) in [
        ("Content-Type", response.content_type),
        ("Content-Encoding", response.content_encoding),
        ("Vary", response.vary),
    ] {
        if let Some(value) = value
            && let Ok(header) = Header::from_bytes(name, value)
        {
            response_body = response_body.with_header(header);
        }
    }
    response_body.with_header(no_store())
}

fn json_response(status: StatusCode, value: &impl Serialize) -> HttpResponse {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"请求未能完成"}"#.as_bytes().to_vec());
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
    Header::from_bytes("Content-Type", value).expect("static Content-Type is valid")
}

fn no_store() -> Header {
    Header::from_bytes("Cache-Control", "no-store").expect("static Cache-Control is valid")
}
