use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Client, Method, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::RwLock};

const LOCAL_GATEWAY_FIRST_PORT: u16 = 38200;
#[cfg(not(target_os = "ios"))]
const LOCAL_GATEWAY_LAST_PORT: u16 = 38231;
#[cfg(not(target_os = "ios"))]
const LOCAL_GATEWAY_PROBE_TIMEOUT: Duration = Duration::from_millis(350);
const REMEMBERED_GATEWAY_PROBE_TIMEOUT: Duration = Duration::from_millis(1200);

#[derive(Clone)]
pub struct GatewayTransport {
    connection: Arc<RwLock<Connection>>,
}

#[derive(Clone)]
struct Connection {
    endpoint: Option<String>,
    client: Client,
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
            connection: Arc::new(RwLock::new(Connection {
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
            endpoint: Some(endpoint.clone()),
            client: build_client()?,
        };
        Ok(endpoint)
    }

    async fn snapshot(&self) -> Result<(String, Client), String> {
        let connection = self.connection.read().await;
        let endpoint = connection
            .endpoint
            .clone()
            .ok_or_else(|| "请输入服务地址".to_owned())?;
        Ok((endpoint, connection.client.clone()))
    }

    pub async fn request(&self, request: GatewayRequest) -> Result<GatewayResponse, String> {
        let (endpoint, client) = self.snapshot().await?;
        let path = validate_api_path(&request.path)?;
        let url = format!("{endpoint}{path}");
        let method =
            Method::from_bytes(request.method.as_bytes()).map_err(|_| "请求方法无效".to_owned())?;
        let mut builder = client.request(method, url);
        for (name, value) in request.headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "cookie" | "host" | "content-length"
            ) {
                continue;
            }
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "请求包含无效的 header 名称".to_owned())?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| "请求包含无效的 header 内容".to_owned())?;
            builder = builder.header(name, value);
        }
        if request.body_text.is_some() && request.body_base64.is_some() {
            return Err("请求正文不能同时使用文本和二进制编码".into());
        }
        if let Some(body) = request.body_text {
            builder = builder.body(body);
        } else if let Some(body) = request.body_base64 {
            builder = builder.body(
                BASE64
                    .decode(body)
                    .map_err(|_| "请求正文编码无效".to_owned())?,
            );
        }
        let response = builder.send().await.map_err(request_error)?;
        let status = response.status().as_u16();
        let textual = textual_response(response.headers());
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let body = response.bytes().await.map_err(request_error)?;
        let (body_text, body_base64) = if textual {
            let text = String::from_utf8(body.to_vec())
                .map_err(|_| "Gateway 返回了无效的 UTF-8 文本".to_owned())?;
            (Some(text), None)
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

    pub async fn download(
        &self,
        path: &str,
        filename: &str,
        download_dir: &Path,
    ) -> Result<DownloadResult, String> {
        let (endpoint, client) = self.snapshot().await?;
        let path = validate_api_path(path)?;
        let mut response = client
            .get(format!("{endpoint}{path}"))
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("下载失败：HTTP {}", status.as_u16()));
            return Err(message);
        }
        tokio::fs::create_dir_all(download_dir)
            .await
            .map_err(|error| format!("无法创建下载目录：{error}"))?;
        let (path, mut file) = create_download_file(download_dir, filename).await?;
        let mut bytes = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(request_error)? {
            file.write_all(&chunk)
                .await
                .map_err(|error| format!("无法写入下载文件：{error}"))?;
            bytes = bytes.saturating_add(chunk.len() as u64);
        }
        file.flush()
            .await
            .map_err(|error| format!("无法完成下载文件：{error}"))?;
        Ok(DownloadResult {
            path: path.to_string_lossy().into_owned(),
            bytes,
        })
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
    let response = client
        .get(format!("{endpoint}/api/auth/status"))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let status = response.json::<LocalAuthStatus>().await.ok()?;
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
        .cookie_store(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .redirect(Policy::limited(5))
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

fn request_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "连接超时".into()
    } else if error.is_connect() {
        "无法连接到目标服务".into()
    } else {
        format!("网络请求失败：{error}")
    }
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

    #[test]
    fn local_gateway_discovery_accepts_gateway_auth_status_on_an_isolated_port() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /api/auth/status HTTP/1.1"));
            let body = r#"{"ok":true,"required":true,"authenticated":false}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let discovered = runtime.block_on(discover_local_device_from_ports([port]));
        server.join().unwrap();
        assert_eq!(
            discovered,
            LocalDevice {
                endpoint: format!("http://127.0.0.1:{port}"),
                online: true,
                requires_password: true,
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
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.contains("accept-encoding: gzip"));
            let body = [
                31_u8, 139, 8, 0, 0, 0, 0, 0, 2, 19, 171, 86, 202, 207, 86, 178, 42, 41, 42, 77,
                173, 5, 0, 144, 95, 212, 167, 11, 0, 0, 0,
            ];
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        let transport = GatewayTransport {
            connection: Arc::new(RwLock::new(Connection {
                endpoint: Some(format!("http://{address}")),
                client: build_client_with_proxy(false).unwrap(),
            })),
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let response = runtime
            .block_on(async {
                transport
                    .request(GatewayRequest {
                        path: "/api/sync".into(),
                        method: "POST".into(),
                        headers: BTreeMap::new(),
                        body_text: Some("{}".into()),
                        body_base64: None,
                    })
                    .await
            })
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body_text.as_deref(), Some("{\"ok\":true}"));
        assert!(response.body_base64.is_none());
    }
}
