use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Client, Method, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::OpenOptions,
    io::AsyncWriteExt,
    sync::{RwLock, watch},
};

const LOCAL_GATEWAY_FIRST_PORT: u16 = 38200;
#[cfg(not(target_os = "ios"))]
const LOCAL_GATEWAY_LAST_PORT: u16 = 38231;
#[cfg(not(target_os = "ios"))]
const LOCAL_GATEWAY_PROBE_TIMEOUT: Duration = Duration::from_millis(350);
const REMEMBERED_GATEWAY_PROBE_TIMEOUT: Duration = Duration::from_millis(1200);

#[derive(Clone)]
pub struct GatewayTransport {
    connection: Arc<RwLock<Connection>>,
    downloads: Arc<Mutex<BTreeMap<String, watch::Sender<bool>>>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub request_id: String,
    pub bytes: u64,
}

#[derive(Clone)]
struct Connection {
    endpoint: Option<String>,
    client: Client,
    transport: Option<Arc<me_transport::async_http::Transport>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRequest {
    pub path: String,
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body_text: Option<String>,
    pub body_base64: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body_text: Option<String>,
    pub body_base64: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadResult {
    pub path: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalDevice {
    pub endpoint: String,
    pub online: bool,
    pub requires_password: bool,
}

#[derive(Deserialize)]
struct LocalAuthStatus {
    ok: bool,
    #[cfg_attr(target_os = "ios", allow(dead_code))]
    required: bool,
    #[serde(rename = "authenticated")]
    _authenticated: bool,
}

impl GatewayTransport {
    pub fn new(endpoint: Option<String>) -> Result<Self, String> {
        let endpoint = endpoint
            .map(|value| normalize_endpoint(&value))
            .transpose()?;
        Ok(Self {
            downloads: Arc::new(Mutex::new(BTreeMap::new())),
            connection: Arc::new(RwLock::new(Connection {
                transport: endpoint
                    .as_ref()
                    .map(|value| Arc::new(me_transport::async_http::Transport::new(value))),
                endpoint,
                client: build_client()?,
            })),
        })
    }

    pub async fn endpoint(&self) -> Option<String> {
        self.connection.read().await.endpoint.clone()
    }

    pub async fn configure(&self, endpoint: &str) -> Result<String, String> {
        let endpoint = normalize_endpoint(endpoint)?;
        let mut connection = self.connection.write().await;
        *connection = Connection {
            transport: Some(Arc::new(me_transport::async_http::Transport::new(
                &endpoint,
            ))),
            endpoint: Some(endpoint.clone()),
            client: build_client()?,
        };
        Ok(endpoint)
    }

    async fn snapshot(&self) -> Result<(Arc<me_transport::async_http::Transport>, Client), String> {
        let connection = self.connection.read().await;
        let transport = connection
            .transport
            .clone()
            .ok_or_else(|| "请输入服务地址".to_owned())?;
        Ok((transport, connection.client.clone()))
    }

    pub async fn request(&self, request: GatewayRequest) -> Result<GatewayResponse, String> {
        let (transport, client) = self.snapshot().await?;
        let path = validate_api_path(&request.path)?;
        let method =
            Method::from_bytes(request.method.as_bytes()).map_err(|_| "请求方法无效".to_owned())?;
        let mut headers = Vec::new();
        for (name, value) in request.headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "cookie" | "host" | "content-length" | "accept-encoding"
            ) {
                continue;
            }
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "请求内容无效".to_owned())?;
            reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| "请求内容无效".to_owned())?;
            headers.push((name, value));
        }
        headers.push(("Accept-Encoding".into(), "gzip".into()));
        if request.body_text.is_some() && request.body_base64.is_some() {
            return Err("请求正文不能同时使用文本和二进制编码".into());
        }
        let body = if let Some(body) = request.body_text {
            body.into_bytes()
        } else if let Some(body) = request.body_base64 {
            BASE64
                .decode(body)
                .map_err(|_| "请求正文编码无效".to_owned())?
        } else {
            Vec::new()
        };
        let response = transport
            .request(
                &client,
                &me_transport::RequestHead {
                    method: method.as_str().to_owned(),
                    url: path.to_owned(),
                    headers,
                    body_length: body.len(),
                },
                &body,
            )
            .await
            .map_err(request_error)?;
        let status = response.head.status;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &response.head.headers {
            headers.append(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| "响应内容无效".to_owned())?,
                reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| "响应内容无效".to_owned())?,
            );
        }
        let textual = textual_response(&headers);
        let mut body = response.bytes().await.map_err(request_error)?;
        if let Some(encoding) = headers.remove(reqwest::header::CONTENT_ENCODING) {
            if encoding != "gzip" {
                return Err("响应内容无效".into());
            }
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(body.as_slice())
                .read_to_end(&mut decoded)
                .map_err(request_error)?;
            body = decoded;
            headers.remove(reqwest::header::CONTENT_LENGTH);
        }
        headers.remove(reqwest::header::TRANSFER_ENCODING);
        let headers = headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let (body_text, body_base64) = if textual {
            (
                Some(String::from_utf8(body).map_err(|_| "响应文本无效".to_owned())?),
                None,
            )
        } else {
            (None, Some(BASE64.encode(body)))
        };
        Ok(GatewayResponse {
            status,
            headers,
            body_text,
            body_base64,
        })
    }

    pub fn cancel_download(&self, request_id: &str) {
        if let Ok(downloads) = self.downloads.lock() {
            if let Some(cancel) = downloads.get(request_id) {
                let _ = cancel.send(true);
            }
        }
    }

    pub async fn download(
        &self,
        path: &str,
        filename: &str,
        download_dir: &Path,
        progress: impl Fn(DownloadProgress) -> Result<(), String>,
    ) -> Result<DownloadResult, String> {
        let mut random = [0; 16];
        me_transport::random_fill(&mut random).map_err(request_error)?;
        let id = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let (cancel, mut cancelled) = watch::channel(false);
        self.downloads
            .lock()
            .map_err(|_| "无法启动下载")?
            .insert(id.clone(), cancel);
        let notify = |bytes| {
            progress(DownloadProgress {
                request_id: id.clone(),
                bytes,
            })
        };
        // Acknowledge registration before network I/O, so cancellation cannot overtake registration.
        let result = match notify(0) {
            Ok(()) => {
                self.download_inner(path, filename, download_dir, &mut cancelled, notify)
                    .await
            }
            Err(error) => Err(error),
        };
        if let Ok(mut downloads) = self.downloads.lock() {
            downloads.remove(&id);
        }
        result
    }

    async fn download_inner(
        &self,
        path: &str,
        filename: &str,
        download_dir: &Path,
        cancelled: &mut watch::Receiver<bool>,
        progress: impl Fn(u64) -> Result<(), String>,
    ) -> Result<DownloadResult, String> {
        if *cancelled.borrow() {
            return Err("下载已取消".into());
        }
        let (transport, client) = self.snapshot().await?;
        let path = validate_api_path(path)?;
        let request = me_transport::RequestHead {
            method: "GET".into(),
            url: path.to_owned(),
            headers: vec![("Accept-Encoding".into(), "identity".into())],
            body_length: 0,
        };
        let mut response = tokio::select! {
            response = transport.request(&client, &request, &[]) => response.map_err(request_error)?,
            _ = cancelled.changed() => return Err("下载已取消".into()),
        };
        if !(200..300).contains(&response.head.status) {
            let status = response.head.status;
            let body = response.bytes().await.map_err(request_error)?;
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("下载失败：HTTP {status}"));
            return Err(message);
        }
        tokio::fs::create_dir_all(download_dir)
            .await
            .map_err(|error| format!("无法创建下载目录：{error}"))?;
        let (path, mut file) = create_download_file(download_dir, filename).await?;
        let result = {
            let transfer = async {
                let mut bytes = 0_u64;
                let mut last_progress = Instant::now();
                while let Some(chunk) = response.chunk().await.map_err(request_error)? {
                    file.write_all(&chunk)
                        .await
                        .map_err(|error| format!("无法写入下载文件：{error}"))?;
                    bytes += chunk.len() as u64;
                    if last_progress.elapsed() >= Duration::from_millis(100) {
                        progress(bytes)?;
                        last_progress = Instant::now();
                    }
                }
                file.flush()
                    .await
                    .map_err(|error| format!("无法完成下载文件：{error}"))?;
                progress(bytes)?;
                Ok(DownloadResult {
                    path: path.to_string_lossy().into_owned(),
                    bytes,
                })
            };
            if *cancelled.borrow() {
                Err("下载已取消".into())
            } else {
                tokio::select! {
                    result = transfer => result,
                    _ = cancelled.changed() => Err("下载已取消".into()),
                }
            }
        };
        drop(file);
        if let Err(error) = &result {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|cleanup| format!("{error}；无法清理未完成文件：{cleanup}"))?;
        }
        result
    }
}

#[cfg(not(target_os = "ios"))]
pub async fn discover_local_device() -> LocalDevice {
    discover_local_device_from_ports(LOCAL_GATEWAY_FIRST_PORT..=LOCAL_GATEWAY_LAST_PORT).await
}

#[cfg(target_os = "ios")]
pub async fn discover_local_device() -> LocalDevice {
    LocalDevice {
        endpoint: format!("http://127.0.0.1:{LOCAL_GATEWAY_FIRST_PORT}"),
        online: false,
        requires_password: false,
    }
}

#[cfg(not(target_os = "ios"))]
async fn discover_local_device_from_ports(ports: impl IntoIterator<Item = u16>) -> LocalDevice {
    let offline = LocalDevice {
        endpoint: format!("http://127.0.0.1:{LOCAL_GATEWAY_FIRST_PORT}"),
        online: false,
        requires_password: false,
    };
    let Ok(client) = Client::builder()
        .no_proxy()
        .connect_timeout(LOCAL_GATEWAY_PROBE_TIMEOUT)
        .timeout(LOCAL_GATEWAY_PROBE_TIMEOUT)
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .build()
    else {
        return offline;
    };
    let tasks = ports
        .into_iter()
        .map(|port| {
            let client = client.clone();
            tokio::spawn(async move { probe_local_gateway_port(&client, port).await })
        })
        .collect::<Vec<_>>();
    let mut discovered: Option<LocalDevice> = None;
    for task in tasks {
        let Ok(Some(candidate)) = task.await else {
            continue;
        };
        if discovered
            .as_ref()
            .is_none_or(|current| candidate.endpoint < current.endpoint)
        {
            discovered = Some(candidate);
        }
    }
    discovered.unwrap_or(offline)
}

#[cfg(not(target_os = "ios"))]
async fn probe_local_gateway_port(client: &Client, port: u16) -> Option<LocalDevice> {
    let endpoint = format!("http://127.0.0.1:{port}");
    let status = probe_gateway_status(client, &endpoint).await?;
    Some(LocalDevice {
        endpoint,
        online: true,
        requires_password: status.required,
    })
}

pub async fn online_remembered_devices(endpoints: Vec<String>) -> BTreeSet<String> {
    let Ok(client) = Client::builder()
        .connect_timeout(REMEMBERED_GATEWAY_PROBE_TIMEOUT)
        .timeout(REMEMBERED_GATEWAY_PROBE_TIMEOUT)
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .build()
    else {
        return BTreeSet::new();
    };
    let tasks = endpoints
        .into_iter()
        .map(|endpoint| {
            let client = client.clone();
            tokio::spawn(async move {
                probe_gateway_status(&client, &endpoint)
                    .await
                    .map(|_| endpoint)
            })
        })
        .collect::<Vec<_>>();
    let mut online = BTreeSet::new();
    for task in tasks {
        if let Ok(Some(endpoint)) = task.await {
            online.insert(endpoint);
        }
    }
    online
}

async fn probe_gateway_status(client: &Client, endpoint: &str) -> Option<LocalAuthStatus> {
    let transport = me_transport::async_http::Transport::new(endpoint);
    let response = transport
        .request(
            client,
            &me_transport::RequestHead {
                method: "GET".into(),
                url: "/api/auth/status".into(),
                headers: Vec::new(),
                body_length: 0,
            },
            &[],
        )
        .await
        .ok()?;
    if !(200..300).contains(&response.head.status) {
        return None;
    }
    let status: LocalAuthStatus = serde_json::from_slice(&response.bytes().await.ok()?).ok()?;
    status.ok.then_some(status)
}

pub fn normalize_endpoint(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("请输入服务地址".into());
    }
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let mut url = Url::parse(&candidate).map_err(|_| "服务地址格式无效".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("服务地址必须使用 HTTP 或 HTTPS".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("服务地址不能包含用户名或密码".into());
    }
    url.set_query(None);
    url.set_fragment(None);
    let normalized = url.as_str().trim_end_matches('/').to_owned();
    if normalized.is_empty() {
        return Err("服务地址格式无效".into());
    }
    Ok(normalized)
}

fn validate_api_path(path: &str) -> Result<&str, String> {
    if !path.starts_with("/api/") || path.contains("\r") || path.contains("\n") {
        return Err("客户端只允许访问 Gateway API".into());
    }
    Ok(path)
}

fn textual_response(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            let media_type = value.split(';').next().unwrap_or_default().trim();
            media_type.starts_with("text/")
                || media_type == "application/json"
                || media_type.ends_with("+json")
        })
        .unwrap_or(false)
}

fn build_client() -> Result<Client, String> {
    build_client_with_proxy(true)
}

fn build_client_with_proxy(system_proxy: bool) -> Result<Client, String> {
    let builder = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .user_agent(format!("me-client/{}", env!("CARGO_PKG_VERSION")));
    let builder = if system_proxy {
        builder
    } else {
        builder.no_proxy()
    };
    builder
        .build()
        .map_err(|error| format!("无法初始化网络客户端：{error}"))
}

fn request_error(error: std::io::Error) -> String {
    if let Some(source) = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<reqwest::Error>())
    {
        if source.is_timeout() {
            return "连接超时".into();
        }
        if source.is_connect() {
            return "无法连接到目标服务".into();
        }
    }
    "安全连接未能完成，请确认服务地址与版本".into()
}

async fn create_download_file(
    directory: &Path,
    filename: &str,
) -> Result<(PathBuf, tokio::fs::File), String> {
    let sanitized = Path::new(filename)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && *value != "." && *value != "..")
        .unwrap_or("download");
    let path = Path::new(sanitized);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = path.extension().and_then(|value| value.to_str());
    for index in 0..10_000_u32 {
        let candidate_name = if index == 0 {
            sanitized.to_owned()
        } else if let Some(extension) = extension {
            format!("{stem} ({index}).{extension}")
        } else {
            format!("{stem} ({index})")
        };
        let candidate = directory.join(candidate_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("无法创建下载文件：{error}")),
        }
    }
    Err("无法为下载文件分配名称".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalization_accepts_host_and_rejects_credentials() {
        assert_eq!(
            normalize_endpoint("127.0.0.1:38201/").unwrap(),
            "http://127.0.0.1:38201"
        );
        assert_eq!(
            normalize_endpoint("https://example.com/base/").unwrap(),
            "https://example.com/base"
        );
        assert!(normalize_endpoint("ftp://example.com").is_err());
        assert!(normalize_endpoint("http://user:pass@example.com").is_err());
    }

    fn encrypted_server(
        requests: usize,
        mut handle: impl FnMut(
            me_transport::RequestHead,
            Vec<u8>,
        ) -> (me_transport::ResponseHead, Vec<u8>, bool)
        + Send
        + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let mut channels = BTreeMap::new();
            let mut completed = 0;
            while completed < requests {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                assert!(line.starts_with("POST "));
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.to_ascii_lowercase().starts_with("cookie:"));
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let reply = if path == me_transport::HANDSHAKE_PATH {
                    let (channel, message) = me_transport::respond(&body).unwrap();
                    let mut id = [0; 16];
                    me_transport::random_fill(&mut id).unwrap();
                    channels.insert(id, channel);
                    [id.as_slice(), &message].concat()
                } else {
                    assert_eq!(path, me_transport::REQUEST_PATH);
                    let id: [u8; 16] = body[..16].try_into().unwrap();
                    let number = u32::from_be_bytes(body[16..20].try_into().unwrap());
                    let channel = Arc::clone(channels.get(&id).unwrap());
                    let (head, body) =
                        me_transport::decode_request(Arc::clone(&channel), number, &body[20..])
                            .unwrap();
                    let (head, body, truncate) = handle(head, body);
                    completed += 1;
                    let mut encrypted = me_transport::EncryptReader::new(
                        channel,
                        number,
                        &serde_json::to_vec(&head).unwrap(),
                        std::io::Cursor::new(body),
                    )
                    .unwrap();
                    let mut reply = Vec::new();
                    encrypted.read_to_end(&mut reply).unwrap();
                    if truncate {
                        reply.truncate(reply.len() - 1);
                    }
                    reply
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len()).unwrap();
                for part in reply.chunks(8192) {
                    if stream.write_all(part).is_err() {
                        break;
                    }
                    if reply.len() > 1024 * 1024 {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        });
        (endpoint, worker)
    }

    fn transport_for_test(endpoint: &str) -> GatewayTransport {
        GatewayTransport {
            downloads: Arc::new(Mutex::new(BTreeMap::new())),
            connection: Arc::new(RwLock::new(Connection {
                endpoint: Some(endpoint.to_owned()),
                transport: Some(Arc::new(me_transport::async_http::Transport::new(endpoint))),
                client: build_client_with_proxy(false).unwrap(),
            })),
        }
    }

    #[test]
    fn local_gateway_discovery_accepts_gateway_auth_status_on_an_isolated_port() {
        let (endpoint, server) = encrypted_server(1, |request, _| {
            assert_eq!(request.url, "/api/auth/status");
            let body = br#"{"ok":true,"required":true,"authenticated":false}"#.to_vec();
            (
                me_transport::ResponseHead {
                    status: 200,
                    headers: Vec::new(),
                    body_length: Some(body.len() as u64),
                },
                body,
                false,
            )
        });
        let port = Url::parse(&endpoint).unwrap().port().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let discovered = runtime.block_on(discover_local_device_from_ports([port]));
        server.join().unwrap();
        assert_eq!(
            discovered,
            LocalDevice {
                endpoint,
                online: true,
                requires_password: true
            }
        );
    }

    #[test]
    fn api_path_rejects_non_gateway_targets() {
        assert_eq!(
            validate_api_path("/api/gateway/state").unwrap(),
            "/api/gateway/state"
        );
        assert!(validate_api_path("https://example.com/api/gateway/state").is_err());
        assert!(validate_api_path("/theme.js").is_err());
    }

    #[test]
    fn json_transport_negotiates_and_decodes_gzip_without_base64() {
        let mut calls = 0;
        let (endpoint, server) = encrypted_server(2, move |request, body| {
            assert_eq!(body, b"{}");
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("accept-encoding") && v == "gzip")
            );
            if calls > 0 {
                assert!(
                    request
                        .headers
                        .iter()
                        .any(|(k, v)| k.eq_ignore_ascii_case("cookie")
                            && v == "session=private-token")
                );
            }
            calls += 1;
            let body = vec![
                31_u8, 139, 8, 0, 0, 0, 0, 0, 2, 19, 171, 86, 202, 207, 86, 178, 42, 41, 42, 77,
                173, 5, 0, 144, 95, 212, 167, 11, 0, 0, 0,
            ];
            let head = me_transport::ResponseHead {
                status: 200,
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Content-Encoding".into(), "gzip".into()),
                    (
                        "Set-Cookie".into(),
                        "session=private-token; HttpOnly; Path=/".into(),
                    ),
                ],
                body_length: Some(body.len() as u64),
            };
            (head, body, false)
        });
        let transport = transport_for_test(&endpoint);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for _ in 0..2 {
            let response = runtime
                .block_on(transport.request(GatewayRequest {
                    path: "/api/sync".into(),
                    method: "POST".into(),
                    headers: BTreeMap::new(),
                    body_text: Some("{}".into()),
                    body_base64: None,
                }))
                .unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body_text.as_deref(), Some("{\"ok\":true}"));
            assert!(response.body_base64.is_none());
            assert!(
                !response
                    .headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("set-cookie")
                        || key.eq_ignore_ascii_case("content-encoding"))
            );
        }
        server.join().unwrap();
    }

    #[test]
    fn encrypted_download_preserves_binary_and_removes_truncated_partial_file() {
        let expected: Vec<u8> = (0..2 * 1024 * 1024).map(|n| n as u8).collect();
        let outgoing = expected.clone();
        let mut calls = 0;
        let (endpoint, server) = encrypted_server(2, move |request, _| {
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("accept-encoding") && v == "identity")
            );
            calls += 1;
            (
                me_transport::ResponseHead {
                    status: 200,
                    headers: Vec::new(),
                    body_length: Some(outgoing.len() as u64),
                },
                outgoing.clone(),
                calls == 2,
            )
        });
        let directory = std::env::temp_dir().join(format!(
            "me-encrypted-download-{}-{}",
            std::process::id(),
            Url::parse(&endpoint).unwrap().port().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let transport = transport_for_test(&endpoint);
        let saved = runtime
            .block_on(transport.download(
                "/api/files/downloads/x/content",
                "original.bin",
                &directory,
                |_| Ok(()),
            ))
            .unwrap();
        assert_eq!(saved.bytes, expected.len() as u64);
        assert_eq!(std::fs::read(&saved.path).unwrap(), expected);
        assert!(
            runtime
                .block_on(transport.download(
                    "/api/files/downloads/x/content",
                    "broken.bin",
                    &directory,
                    |_| Ok(())
                ))
                .is_err()
        );
        server.join().unwrap();
        assert!(!directory.join("broken.bin").exists());
        std::fs::remove_file(saved.path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn download_cancel_before_network_cannot_overtake_registration() {
        let transport = transport_for_test("http://127.0.0.1:9");
        let directory =
            std::env::temp_dir().join(format!("me-cancelled-download-{}", std::process::id()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(transport.download(
            "/api/files/downloads/x/content",
            "cancelled.bin",
            &directory,
            |event| {
                transport.cancel_download(&event.request_id);
                Ok(())
            },
        ));
        assert_eq!(result.unwrap_err(), "下载已取消");
        assert!(!directory.exists());
        assert!(transport.downloads.lock().unwrap().is_empty());
    }

    #[test]
    fn download_cancel_during_stream_removes_only_its_partial_file() {
        let (endpoint, server) = encrypted_server(1, |_, _| {
            let bytes = vec![37; 2 * 1024 * 1024];
            (
                me_transport::ResponseHead {
                    status: 200,
                    headers: Vec::new(),
                    body_length: Some(bytes.len() as u64),
                },
                bytes,
                false,
            )
        });
        let directory =
            std::env::temp_dir().join(format!("me-stream-cancel-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let untouched = directory.join("existing.txt");
        std::fs::write(&untouched, "keep").unwrap();
        let transport = transport_for_test(&endpoint);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(transport.download(
            "/api/files/downloads/x/content",
            "partial.bin",
            &directory,
            |event| {
                if event.bytes > 0 {
                    transport.cancel_download(&event.request_id);
                }
                Ok(())
            },
        ));
        assert_eq!(result.unwrap_err(), "下载已取消");
        server.join().unwrap();
        assert!(!directory.join("partial.bin").exists());
        assert_eq!(std::fs::read_to_string(&untouched).unwrap(), "keep");
        assert!(transport.downloads.lock().unwrap().is_empty());
        std::fs::remove_file(untouched).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
