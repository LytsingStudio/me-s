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

use crate::encrypted_http::{EncryptedHttp, Request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::{
    Result,
    gateway::{Gateway, OpenWorkspaceOutcome, is_ui_projection_path},
    gateway_settings::GatewaySettings,
    web_auth::WebSessionAuth,
};

pub const DEFAULT_GATEWAY_PORT: u16 = 38200;
pub const GATEWAY_BIND_ADDRESS: &str = "0.0.0.0";
const INDEX_HTML: &str = include_str!("webui/index.html");
#[cfg(test)]
const APP_JS: &str = include_str!("webui/app.js");
const RUNTIME_JS: &str = include_str!("gateway_webui/runtime.js");
const SESSION_COOKIE_PREFIX: &str = "me_gateway_session";
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
    #[cfg(debug_assertions)]
    let first_port = match std::env::var("ME_GATEWAY_TEST_PORT") {
        Ok(value) => {
            let port = value
                .parse::<u16>()
                .map_err(|_| "ME_GATEWAY_TEST_PORT must be a valid nonzero port")?;
            if port == 0 {
                return Err("ME_GATEWAY_TEST_PORT must be a valid nonzero port".into());
            }
            port
        }
        Err(std::env::VarError::NotPresent) => DEFAULT_GATEWAY_PORT,
        Err(error) => return Err(error.into()),
    };
    #[cfg(not(debug_assertions))]
    let first_port = DEFAULT_GATEWAY_PORT;
    start_from(gateway, first_port, passkey)
}

fn start_from(
    gateway: Arc<Gateway>,
    first_port: u16,
    passkey: Option<&str>,
) -> Result<GatewayWebUiServer> {
    let (server, port) = bind_first_available(first_port)?;
    let address = format!("http://{GATEWAY_BIND_ADDRESS}:{port}");
    let auth = Arc::new(WebSessionAuth::new(SESSION_COOKIE_PREFIX, port, passkey)?);
    let encrypted_http = Arc::new(EncryptedHttp::default());
    let codex_usage = Arc::new(crate::codex_usage::CodexUsage::start(true)?);
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
                        let encrypted_http = Arc::clone(&encrypted_http);
                        let codex_usage = Arc::clone(&codex_usage);
                        let _ = thread::Builder::new()
                            .name("me-gateway-request".into())
                            .spawn(move || {
                                serve(request, gateway, auth, codex_usage, encrypted_http)
                            });
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

fn serve(
    request: tiny_http::Request,
    gateway: Arc<Gateway>,
    auth: Arc<WebSessionAuth>,
    codex_usage: Arc<crate::codex_usage::CodexUsage>,
    encrypted_http: Arc<EncryptedHttp>,
) {
    encrypted_http.serve(request, public_asset, |request| {
        let result = route(
            request,
            gateway.as_ref(),
            auth.as_ref(),
            codex_usage.as_ref(),
        );
        match result {
            Ok(response) => response,
            Err(error) => {
                eprintln!("warning: me-gateway request failed: {error}");
                json_response(
                    StatusCode(500),
                    &json!({"ok": false, "error": "请求未能完成"}),
                )
            }
        }
    });
}

type HttpResponse = Response<Box<dyn Read + Send>>;

fn public_asset(path: &str) -> Option<HttpResponse> {
    match path {
        "/" => Some(text_response("text/html; charset=utf-8", INDEX_HTML)),
        "/runtime.js" => Some(text_response("text/javascript; charset=utf-8", RUNTIME_JS)),
        _ => crate::webui::shared_public_asset(path),
    }
}

fn route(
    request: &mut Request,
    gateway: &Gateway,
    auth: &WebSessionAuth,
    codex_usage: &crate::codex_usage::CodexUsage,
) -> Result<HttpResponse> {
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or(url.as_str()).to_owned();
    match (request.method(), path.as_str()) {
        (&Method::Get, "/api/auth/status") => return auth_status(request, auth),
        (&Method::Post, "/api/auth/login") => return login(request, auth),
        _ => {}
    }
    if !auth.authorized_any(request_session_tokens(request, auth.cookie_prefix())) {
        return Ok(json_response(
            StatusCode(401),
            &json!({"ok": false, "error": "需要登录"}),
        ));
    }

    match (request.method(), path.as_str()) {
        (&Method::Get, "/api/codex/usage") => {
            Ok(json_response(StatusCode(200), &codex_usage.snapshot()))
        }
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
        (&Method::Post, "/api/gateway/directories/create") => {
            let input: CreateDirectoryRequest = read_json(request, MAX_BODY_BYTES)?;
            match gateway.create_directory(&PathBuf::from(input.parent), &input.name) {
                Ok(path) => Ok(json_response(
                    StatusCode(200),
                    &json!({"ok": true, "path": crate::host_path::public_host_path(&path)}),
                )),
                Err(error) => {
                    eprintln!("warning: create host directory failed: {error}");
                    Ok(json_response(
                        StatusCode(400),
                        &json!({"ok": false, "error": "无法创建文件夹"}),
                    ))
                }
            }
        }
        (&Method::Post, "/api/gateway/workspaces/open") => {
            let input: OpenWorkspaceRequest = read_json(request, MAX_BODY_BYTES)?;
            match gateway.open_workspace(&PathBuf::from(input.path), input.initialize) {
                Ok(OpenWorkspaceOutcome::Opened(id)) => Ok(json_response(
                    StatusCode(200),
                    &json!({"ok": true, "status": "opened", "workspace_id": id}),
                )),
                Ok(OpenWorkspaceOutcome::RequiresInitialization { path }) => Ok(json_response(
                    StatusCode(200),
                    &json!({"ok": true, "status": "requires_initialization", "path": path}),
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
        (method, _) if workspace_proxy_request(&url).is_some() => {
            let (workspace_id, child_path, query) = workspace_proxy_request(&url).unwrap();
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
            let range = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Range"))
                .map(|header| header.value.as_str().to_owned());
            let body = read_body(request, MAX_BODY_BYTES)?;
            match gateway.proxy(
                workspace_id,
                method,
                child_path,
                query,
                content_type.as_deref(),
                accept_encoding.as_deref(),
                range.as_deref(),
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
    #[serde(default)]
    browser_port: Option<u16>,
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
struct CreateDirectoryRequest {
    parent: String,
    name: String,
}

#[derive(Deserialize)]
struct OpenWorkspaceRequest {
    path: String,
    #[serde(default)]
    initialize: bool,
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
            "product_version": env!("CARGO_PKG_VERSION"),
            "required": auth.required(),
            "authenticated": auth.authorized_any(request_session_tokens(
                request,
                auth.cookie_prefix(),
            )),
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

fn workspace_proxy_request(url: &str) -> Option<(&str, &str, Option<&str>)> {
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    };
    let (workspace_id, child_path) = workspace_proxy_path(path)?;
    if query.is_some() && !is_ui_projection_path(child_path) {
        return None;
    }
    Some((workspace_id, child_path, query))
}

fn workspace_proxy_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/api/workspaces/")?;
    let (workspace_id, child_path) = rest.split_once('/')?;
    (!workspace_id.is_empty() && !child_path.is_empty()).then_some((workspace_id, child_path))
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
    .expect("gateway session cookie is valid")
}

fn proxy_response(response: crate::gateway::ProxyResponse) -> HttpResponse {
    let crate::gateway::ProxyResponse {
        status,
        content_type,
        content_encoding,
        content_disposition,
        accept_ranges,
        content_range,
        vary,
        remote_sequence,
        screen_width,
        screen_height,
        frame_width,
        frame_height,
        content_length,
        body,
    } = response;
    let mut headers = vec![no_store()];
    for (name, value) in [
        ("Content-Type", content_type),
        ("Content-Encoding", content_encoding),
        ("Content-Disposition", content_disposition),
        ("Accept-Ranges", accept_ranges),
        ("Content-Range", content_range),
        ("Vary", vary),
        ("X-Me-Remote-Sequence", remote_sequence),
        ("X-Me-Screen-Width", screen_width),
        ("X-Me-Screen-Height", screen_height),
        ("X-Me-Frame-Width", frame_width),
        ("X-Me-Frame-Height", frame_height),
    ] {
        if let Some(value) = value
            && let Ok(header) = Header::from_bytes(name, value)
        {
            headers.push(header);
        }
    }
    Response::new(StatusCode(status), headers, body, content_length, None)
}

fn json_response(status: StatusCode, value: &impl Serialize) -> HttpResponse {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"请求未能完成"}"#.as_bytes().to_vec());
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
    Header::from_bytes("Content-Type", value).expect("static Content-Type is valid")
}

fn no_store() -> Header {
    Header::from_bytes("Cache-Control", "no-store").expect("static Cache-Control is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_reports_running_product_version() {
        let request = Request::new(
            me_transport::RequestHead {
                method: "GET".into(),
                url: "/api/auth/status".into(),
                headers: Vec::new(),
                body_length: 0,
            },
            Vec::new(),
        )
        .unwrap();
        let auth = WebSessionAuth::new("me_gateway_session", 38199, Some("test")).unwrap();
        let response = auth_status(&request, &auth).unwrap();
        let payload: serde_json::Value = serde_json::from_reader(response.into_reader()).unwrap();
        assert_eq!(payload["product_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(payload["required"], true);
        assert_eq!(payload["authenticated"], false);
    }

    #[test]
    fn embedded_gateway_webui_uses_the_shared_projection_frontend() {
        assert!(!INDEX_HTML.contains("/edb-cache.js"));
        assert!(APP_JS.contains("ui_projection: true"));
        assert!(APP_JS.contains("/api/ui-projections/"));
        assert!(RUNTIME_JS.contains("/api/ui-projections/"));
        assert!(!APP_JS.contains("cache_metadata_only"));
        assert!(!RUNTIME_JS.contains("createEdbCache"));
    }

    #[test]
    fn workspace_projection_proxy_preserves_query() {
        let (workspace_id, child_path, query) = workspace_proxy_request(
            "/api/workspaces/chat/ui-projections/main/range?start=0&end=5&revision=known",
        )
        .unwrap();
        assert_eq!(workspace_id, "chat");
        assert_eq!(child_path, "ui-projections/main/range");
        assert_eq!(query, Some("start=0&end=5&revision=known"));
        assert!(
            workspace_proxy_request("/api/workspaces/chat/remote-control/status?private=true")
                .is_none()
        );
    }

    #[test]
    fn image_original_proxy_preserves_download_headers_and_bytes() {
        let response = proxy_response(crate::gateway::ProxyResponse {
            status: 200,
            content_type: Some("image/png".into()),
            content_disposition: Some("attachment; filename=\"image-test.png\"".into()),
            content_encoding: None,
            accept_ranges: None,
            content_range: None,
            vary: None,
            remote_sequence: None,
            screen_width: None,
            screen_height: None,
            frame_width: None,
            frame_height: None,
            content_length: Some(4),
            body: Box::new(std::io::Cursor::new(vec![0x89, b'P', b'N', b'G'])),
        });
        let header = |name: &'static str| {
            response
                .headers()
                .iter()
                .find(|header| header.field.equiv(name))
                .map(|header| header.value.as_str())
        };
        assert_eq!(header("Content-Type"), Some("image/png"));
        assert_eq!(
            header("Content-Disposition"),
            Some("attachment; filename=\"image-test.png\"")
        );
        assert_eq!(header("Cache-Control"), Some("no-store"));
        assert_eq!(response.data_length(), Some(4));
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader(), &mut body).unwrap();
        assert_eq!(body, [0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn remote_frame_proxy_preserves_binary_body_and_geometry_headers() {
        let response = proxy_response(crate::gateway::ProxyResponse {
            status: 200,
            content_type: Some("image/jpeg".into()),
            content_encoding: None,
            content_disposition: None,
            accept_ranges: None,
            content_range: None,
            vary: None,
            remote_sequence: Some("42".into()),
            screen_width: Some("1920".into()),
            screen_height: Some("1080".into()),
            frame_width: Some("960".into()),
            frame_height: Some("540".into()),
            content_length: Some(5),
            body: Box::new(std::io::Cursor::new(vec![0xff, 0xd8, 1, 0xff, 0xd9])),
        });
        let header = |name: &'static str| {
            response
                .headers()
                .iter()
                .find(|header| header.field.equiv(name))
                .map(|header| header.value.as_str().to_owned())
        };
        assert_eq!(response.status_code(), StatusCode(200));
        assert_eq!(response.data_length(), Some(5));
        assert_eq!(header("Content-Type").as_deref(), Some("image/jpeg"));
        assert_eq!(header("X-Me-Remote-Sequence").as_deref(), Some("42"));
        assert_eq!(header("X-Me-Screen-Width").as_deref(), Some("1920"));
        assert_eq!(header("X-Me-Screen-Height").as_deref(), Some("1080"));
        assert_eq!(header("X-Me-Frame-Width").as_deref(), Some("960"));
        assert_eq!(header("X-Me-Frame-Height").as_deref(), Some("540"));
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader(), &mut body).unwrap();
        assert_eq!(body, [0xff, 0xd8, 1, 0xff, 0xd9]);
    }
}
