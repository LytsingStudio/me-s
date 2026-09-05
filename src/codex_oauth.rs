use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::{
    Result,
    config::{GlobalConfig, ModelCapabilities, ModelConfig, ProviderType, config_home},
};

const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REVOKE_TOKEN_URL: &str = "https://auth.openai.com/oauth/revoke";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_WINDOW: Duration = Duration::from_secs(5 * 60);
const DEVICE_AUTH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MODEL_SOURCE_URL: &str =
    "https://developers.openai.com/api/docs/guides/latest-model?model=gpt-5.6-sol";

const BASE_MODEL_NAMES: [&str; 4] = [
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-6-astra",
];
const LEGACY_MODEL_NAMES: [&str; 4] = [
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-6-astra",
];
pub const MODEL_NAMES: [&str; 12] = [
    "gpt-5.6-sol-272k",
    "gpt-5.6-sol-512k",
    "gpt-5.6-sol-1000k",
    "gpt-5.6-terra-272k",
    "gpt-5.6-terra-512k",
    "gpt-5.6-terra-1000k",
    "gpt-5.6-luna-272k",
    "gpt-5.6-luna-512k",
    "gpt-5.6-luna-1000k",
    "gpt-6-astra-272k",
    "gpt-6-astra-512k",
    "gpt-6-astra-1000k",
];

pub fn is_legacy_model_name(name: &str) -> bool {
    LEGACY_MODEL_NAMES.contains(&name)
}

pub fn is_hidden_legacy_model(model: &ModelConfig) -> bool {
    model.provider == ProviderType::CodexOauth && is_legacy_model_name(&model.name)
}

pub struct CodexRequestCredential {
    pub access_token: String,
    pub account_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexStatus {
    pub credential_file: PathBuf,
    pub logged_in: bool,
    pub auth_mode: Option<String>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub expires_at: Option<u64>,
    pub error: Option<String>,
}

pub struct LogoutResult {
    pub removed: bool,
    pub revoke_warning: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: Value,
}

#[derive(Debug, serde::Deserialize)]
struct DeviceAuthorizationResponse {
    authorization_code: String,
    code_verifier: String,
    #[serde(rename = "code_challenge")]
    _code_challenge: String,
}

#[derive(Debug, serde::Deserialize)]
struct OAuthTokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

pub fn codex_home() -> Result<PathBuf> {
    Ok(config_home()?.join("codex"))
}

pub fn credential_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("auth.json"))
}

pub fn status() -> Result<CodexStatus> {
    status_at(&credential_path()?)
}

pub fn login() -> Result<CodexStatus> {
    let home = codex_home()?;
    create_private_directory(&home)?;
    let client = oauth_client()?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    login_at(
        &home.join("auth.json"),
        &client,
        AUTH_BASE_URL,
        DEVICE_AUTH_TIMEOUT,
        &mut output,
    )?;
    let result = status_at(&home.join("auth.json"))?;
    if !result.logged_in {
        return Err(result
            .error
            .unwrap_or_else(|| "Codex OAuth login did not produce usable credentials".to_owned())
            .into());
    }
    Ok(result)
}

pub fn logout() -> Result<LogoutResult> {
    let home = codex_home()?;
    let auth_file = home.join("auth.json");
    let mut revoke_warning = None;
    let _lock = lock_auth(&auth_file)?;
    let was_present = auth_file.exists();
    if was_present {
        match read_auth(&auth_file)
            .and_then(|document| revoke_at(&oauth_client()?, &document, REVOKE_TOKEN_URL))
        {
            Ok(()) => {}
            Err(error) => {
                revoke_warning = Some(format!(
                    "Codex token revocation failed: {error}; local credential was still deleted"
                ));
            }
        }
    }
    let removed_locally = match fs::remove_file(&auth_file) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    Ok(LogoutResult {
        removed: was_present || removed_locally,
        revoke_warning,
    })
}

pub fn add_models_if_logged_in(global: &mut GlobalConfig) -> Result<()> {
    add_models_if_logged_in_at(global, &credential_path()?)
}

fn add_models_if_logged_in_at(global: &mut GlobalConfig, path: &Path) -> Result<()> {
    if !status_at(path)?.logged_in {
        return Ok(());
    }
    global.models.retain(|model| {
        model.provider != ProviderType::CodexOauth
            || (!MODEL_NAMES.contains(&model.name.as_str())
                && !LEGACY_MODEL_NAMES.contains(&model.name.as_str()))
    });
    let existing_names = global
        .models
        .iter()
        .map(|model| model.name.clone())
        .collect::<HashSet<_>>();
    global.models.extend(
        model_configs(path.to_path_buf())
            .into_iter()
            .filter(|model| !existing_names.contains(&model.name)),
    );
    global.validate()
}

pub fn request_credential(
    path: &Path,
    client: &Client,
    rejected_access_token: Option<&str>,
) -> Result<CodexRequestCredential> {
    let endpoint = std::env::var("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
        .unwrap_or_else(|_| REFRESH_TOKEN_URL.to_owned());
    let client_id = std::env::var("CODEX_APP_SERVER_LOGIN_CLIENT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_owned());
    request_credential_at(path, client, rejected_access_token, &endpoint, &client_id)
}

fn request_credential_at(
    path: &Path,
    client: &Client,
    rejected_access_token: Option<&str>,
    endpoint: &str,
    client_id: &str,
) -> Result<CodexRequestCredential> {
    let mut document = read_auth(path)?;
    let current_token = required_string(&document, "/tokens/access_token")?;
    let must_refresh = credential_requires_refresh(current_token, rejected_access_token);
    if must_refresh {
        let _lock = lock_auth(path)?;
        document = read_auth(path)?;
        let latest_token = required_string(&document, "/tokens/access_token")?;
        let still_requires_refresh =
            credential_requires_refresh(latest_token, rejected_access_token);
        if still_requires_refresh {
            document = refresh_at(path, client, document, endpoint, client_id)?;
        }
    }
    request_credential_from_document(&document)
}

fn credential_requires_refresh(current_token: &str, rejected_access_token: Option<&str>) -> bool {
    match rejected_access_token {
        Some(rejected) => rejected == current_token,
        None => token_expires_soon(current_token),
    }
}

fn refresh_at(
    path: &Path,
    client: &Client,
    mut document: Value,
    endpoint: &str,
    client_id: &str,
) -> Result<Value> {
    let refresh_token = required_string(&document, "/tokens/refresh_token")?.to_owned();
    if refresh_token.is_empty() {
        return Err("Codex OAuth refresh token is empty; run `me codex login` again".into());
    }
    let response = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&json!({
            "client_id": client_id,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        let detail = refresh_error_detail(&body);
        return Err(format!(
            "Codex OAuth token refresh failed with {status}: {detail}; run `me codex login` again if the failure persists"
        )
        .into());
    }
    let refreshed: Value = response.json()?;
    let tokens = document
        .pointer_mut("/tokens")
        .and_then(Value::as_object_mut)
        .ok_or("Codex OAuth credential has no tokens object")?;
    for key in ["id_token", "access_token", "refresh_token"] {
        if let Some(value) = refreshed.get(key).and_then(Value::as_str) {
            tokens.insert(key.to_owned(), Value::String(value.to_owned()));
        }
    }
    document["last_refresh"] = Value::String(chrono::Utc::now().to_rfc3339());
    write_auth(path, &document)?;
    Ok(document)
}

fn request_credential_from_document(document: &Value) -> Result<CodexRequestCredential> {
    if document.pointer("/auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return Err("Codex credential is not a ChatGPT OAuth login; run `me codex login`".into());
    }
    let access_token = required_string(document, "/tokens/access_token")?.to_owned();
    let account_id = document
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            document
                .pointer("/tokens/id_token")
                .and_then(Value::as_str)
                .and_then(jwt_payload)
                .and_then(|claims| {
                    claims
                        .get("https://api.openai.com/auth")
                        .and_then(|auth| auth.get("chatgpt_account_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        })
        .ok_or("Codex OAuth credential has no ChatGPT account ID")?;
    if access_token.is_empty() {
        return Err("Codex OAuth access token is empty; run `me codex login` again".into());
    }
    Ok(CodexRequestCredential {
        access_token,
        account_id,
    })
}

fn status_at(path: &Path) -> Result<CodexStatus> {
    let mut result = CodexStatus {
        credential_file: path.to_path_buf(),
        logged_in: false,
        auth_mode: None,
        account_id: None,
        email: None,
        plan: None,
        expires_at: None,
        error: None,
    };
    if !path.exists() {
        return Ok(result);
    }
    let document = match read_auth(path) {
        Ok(document) => document,
        Err(error) => {
            result.error = Some(error.to_string());
            return Ok(result);
        }
    };
    result.auth_mode = document
        .pointer("/auth_mode")
        .and_then(Value::as_str)
        .map(str::to_owned);
    result.account_id = document
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id_claims = document
        .pointer("/tokens/id_token")
        .and_then(Value::as_str)
        .and_then(jwt_payload);
    result.email = id_claims
        .as_ref()
        .and_then(|claims| claims.get("email"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    result.plan = id_claims
        .as_ref()
        .and_then(|claims| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_plan_type"))
                .or_else(|| claims.get("chatgpt_plan_type"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned);
    result.expires_at = document
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .and_then(jwt_expiration);
    match request_credential_from_document(&document) {
        Ok(_) => result.logged_in = true,
        Err(error) => result.error = Some(error.to_string()),
    }
    Ok(result)
}

fn model_configs(credential_file: PathBuf) -> Vec<ModelConfig> {
    let mut models = Vec::with_capacity(MODEL_NAMES.len() + LEGACY_MODEL_NAMES.len());
    for base_name in BASE_MODEL_NAMES {
        for (suffix, context_window, reserve_output_context) in [
            ("272k", 272_000, false),
            ("512k", 512_000, false),
            ("1000k", 1_000_000, true),
        ] {
            models.push(model_config(
                format!("{base_name}-{suffix}"),
                base_name,
                context_window,
                reserve_output_context,
                &credential_file,
            ));
        }
    }
    for name in LEGACY_MODEL_NAMES {
        models.push(model_config(
            name.to_owned(),
            name,
            512_000,
            false,
            &credential_file,
        ));
    }
    models
}

fn model_config(
    name: String,
    api_model: &str,
    context_window: u64,
    reserve_output_context: bool,
    credential_file: &Path,
) -> ModelConfig {
    let reasoning_efforts = if api_model == "gpt-5.6-luna" {
        ["unset", "low", "medium", "high", "xhigh", "max"]
            .map(str::to_owned)
            .to_vec()
    } else {
        ["unset", "low", "medium", "high", "xhigh", "max", "ultra"]
            .map(str::to_owned)
            .to_vec()
    };
    ModelConfig {
        name,
        provider: ProviderType::CodexOauth,
        reserve_output_context,
        base_url: CODEX_BASE_URL.to_owned(),
        endpoint: "/responses".to_owned(),
        api_key: None,
        api_key_env: None,
        credential_file: Some(credential_file.to_string_lossy().into_owned()),
        model: api_model.to_owned(),
        source_url: Some(MODEL_SOURCE_URL.to_owned()),
        timeout_seconds: 300,
        capabilities: ModelCapabilities {
            context_window,
            max_output_tokens: Some(128_000),
            input_modalities: vec!["text".into(), "image".into()],
            output_modalities: vec!["text".into()],
            reasoning_modes: Vec::new(),
            reasoning_efforts,
            tools: true,
            streaming: true,
        },
        parameters: toml::Table::new(),
        effort_parameters: Default::default(),
    }
}

fn oauth_client() -> Result<Client> {
    Ok(Client::builder().timeout(Duration::from_secs(30)).build()?)
}

fn login_at(
    path: &Path,
    client: &Client,
    auth_base_url: &str,
    timeout: Duration,
    output: &mut impl Write,
) -> Result<()> {
    let auth_base_url = auth_base_url.trim_end_matches('/');
    let device_endpoint = format!("{auth_base_url}/api/accounts/deviceauth/usercode");
    let response = client
        .post(&device_endpoint)
        .header("Content-Type", "application/json")
        .json(&json!({ "client_id": CLIENT_ID }))
        .send()?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = oauth_response_detail(response.text().unwrap_or_default().as_str());
        return Err(format!("Codex device login could not start ({status}): {detail}").into());
    }
    let device: DeviceCodeResponse = response.json()?;
    if device.device_auth_id.trim().is_empty() || device.user_code.trim().is_empty() {
        return Err("Codex device login returned an invalid device code".into());
    }
    let interval = device_poll_interval(&device.interval)?;
    let verification_url = format!("{auth_base_url}/codex/device");
    writeln!(
        output,
        "请在浏览器中打开：\n{verification_url}\n\n输入设备码：\n{}\n\n设备码 15 分钟内有效，正在等待登录完成……",
        device.user_code
    )?;
    output.flush()?;

    let authorization =
        poll_device_authorization(client, auth_base_url, &device, interval, timeout)?;
    let token_endpoint = format!("{auth_base_url}/oauth/token");
    let redirect_uri = format!("{auth_base_url}/deviceauth/callback");
    let response = client
        .post(&token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", authorization.authorization_code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", CLIENT_ID),
            ("code_verifier", authorization.code_verifier.as_str()),
        ])
        .send()?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = oauth_response_detail(response.text().unwrap_or_default().as_str());
        return Err(
            format!("Codex device login token exchange failed ({status}): {detail}").into(),
        );
    }
    let tokens: OAuthTokenResponse = response.json()?;
    let document = auth_document_from_tokens(tokens)?;
    let _lock = lock_auth(path)?;
    write_auth(path, &document)?;
    writeln!(output, "Codex 登录成功。")?;
    Ok(())
}

fn poll_device_authorization(
    client: &Client,
    auth_base_url: &str,
    device: &DeviceCodeResponse,
    interval: Duration,
    timeout: Duration,
) -> Result<DeviceAuthorizationResponse> {
    let endpoint = format!("{auth_base_url}/api/accounts/deviceauth/token");
    let started = Instant::now();
    loop {
        let response = client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .json(&json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()?;
        if response.status().is_success() {
            return Ok(response.json()?);
        }
        let status = response.status();
        if status != reqwest::StatusCode::FORBIDDEN && status != reqwest::StatusCode::NOT_FOUND {
            let detail = oauth_response_detail(response.text().unwrap_or_default().as_str());
            return Err(format!("Codex device login failed ({status}): {detail}").into());
        }
        if started.elapsed() >= timeout {
            return Err("Codex device login timed out".into());
        }
        thread::sleep(interval.min(timeout.saturating_sub(started.elapsed())));
    }
}

fn device_poll_interval(value: &Value) -> Result<Duration> {
    let seconds = match value {
        Value::Null => Some(5),
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
    .ok_or("Codex device login returned an invalid polling interval")?;
    Ok(Duration::from_secs(seconds))
}

fn auth_document_from_tokens(tokens: OAuthTokenResponse) -> Result<Value> {
    if tokens.id_token.is_empty()
        || tokens.access_token.is_empty()
        || tokens.refresh_token.is_empty()
    {
        return Err("Codex device login returned incomplete credentials".into());
    }
    let account_id = jwt_payload(&tokens.id_token).and_then(|claims| {
        claims
            .get("https://api.openai.com/auth")
            .and_then(|auth| auth.get("chatgpt_account_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let mut token_document = json!({
        "id_token": tokens.id_token,
        "access_token": tokens.access_token,
        "refresh_token": tokens.refresh_token,
    });
    if let Some(account_id) = account_id {
        token_document["account_id"] = Value::String(account_id);
    }
    Ok(json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": token_document,
        "last_refresh": chrono::Utc::now().to_rfc3339(),
    }))
}

fn revoke_at(client: &Client, document: &Value, endpoint: &str) -> Result<()> {
    let refresh_token = required_string(document, "/tokens/refresh_token")?;
    let response = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&json!({
            "token": refresh_token,
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        }))
        .send()?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let detail = oauth_response_detail(response.text().unwrap_or_default().as_str());
    Err(format!("OAuth server returned {status}: {detail}").into())
}

fn lock_auth(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        create_private_directory(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path.with_extension("lock"))?;
    file.lock()?;
    Ok(file)
}

fn oauth_response_detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/code")
                .or_else(|| value.pointer("/error/message"))
                .or_else(|| value.get("code"))
                .or_else(|| value.get("error").filter(|error| error.is_string()))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "OAuth server rejected the request".to_owned())
}

fn read_auth(path: &Path) -> Result<Value> {
    let value: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    if !value.is_object() {
        return Err("Codex OAuth credential root must be a JSON object".into());
    }
    Ok(value)
}

fn write_auth(path: &Path, document: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_directory(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(serde_json::to_string_pretty(document)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn required_string<'a>(document: &'a Value, pointer: &str) -> Result<&'a str> {
    document
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Codex OAuth credential is missing {pointer}").into())
}

fn jwt_payload(token: &str) -> Option<Value> {
    let encoded = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_expiration(token: &str) -> Option<u64> {
    jwt_payload(token)?.get("exp")?.as_u64()
}

fn token_expires_soon(token: &str) -> bool {
    let Some(expiration) = jwt_expiration(token) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    expiration <= now.saturating_add(REFRESH_WINDOW.as_secs())
}

fn refresh_error_detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/code")
                .or_else(|| value.get("code"))
                .or_else(|| value.get("error").filter(|error| error.is_string()))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "OAuth server rejected the refresh token".to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Barrier},
        thread,
    };

    use super::*;

    fn jwt(payload: Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    fn auth_document(expiration: u64) -> Value {
        json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": jwt(json!({
                    "email": "person@example.com",
                    "https://api.openai.com/auth": {
                        "chatgpt_plan_type": "plus"
                    }
                })),
                "access_token": jwt(json!({"exp": expiration})),
                "refresh_token": "refresh-token",
                "account_id": "account-123"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        })
    }

    fn respond_json(request: tiny_http::Request, status: u16, body: Value) {
        request
            .respond(
                tiny_http::Response::from_string(body.to_string())
                    .with_status_code(status)
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    ),
            )
            .unwrap();
    }

    fn read_request_body(request: &mut tiny_http::Request) -> String {
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        body
    }

    #[test]
    fn status_exposes_metadata_but_not_secrets() {
        let directory = std::env::temp_dir().join(format!(
            "me-codex-status-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("auth.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, auth_document(u64::MAX).to_string()).unwrap();

        let status = status_at(&path).unwrap();
        assert!(status.logged_in);
        assert_eq!(status.auth_mode.as_deref(), Some("chatgpt"));
        assert_eq!(status.account_id.as_deref(), Some("account-123"));
        assert_eq!(status.email.as_deref(), Some("person@example.com"));
        assert_eq!(status.plan.as_deref(), Some("plus"));
        assert!(!format!("{status:?}").contains("refresh-token"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_or_non_oauth_credentials_are_not_logged_in() {
        let directory =
            std::env::temp_dir().join(format!("me-codex-invalid-{}", std::process::id()));
        let path = directory.join("auth.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, r#"{"auth_mode":"apikey"}"#).unwrap();

        let status = status_at(&path).unwrap();
        assert!(!status.logged_in);
        assert!(status.error.unwrap().contains("not a ChatGPT OAuth"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn hidden_legacy_models_are_scoped_to_codex_oauth() {
        let codex_legacy = model_config(
            "gpt-5.6-sol".into(),
            "gpt-5.6-sol",
            512_000,
            false,
            Path::new("/config/me/codex/auth.json"),
        );
        let mut custom_same_name = codex_legacy.clone();
        custom_same_name.provider = ProviderType::OpenaiCompatible;
        let codex_current = model_config(
            "gpt-5.6-sol-512k".into(),
            "gpt-5.6-sol",
            512_000,
            false,
            Path::new("/config/me/codex/auth.json"),
        );

        assert!(is_hidden_legacy_model(&codex_legacy));
        assert!(!is_hidden_legacy_model(&custom_same_name));
        assert!(!is_hidden_legacy_model(&codex_current));
    }

    #[test]
    fn automatic_models_have_official_capabilities_and_credential_path() {
        let path = PathBuf::from("/config/me/codex/auth.json");
        let models = model_configs(path.clone());
        assert_eq!(
            models
                .iter()
                .filter(|model| !is_hidden_legacy_model(model))
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            MODEL_NAMES
        );
        assert_eq!(models.len(), 16);
        assert!(
            models
                .iter()
                .all(|model| model.capabilities.max_output_tokens == Some(128_000))
        );
        for models in models[..12].chunks_exact(3) {
            assert_eq!(models[0].capabilities.context_window, 272_000);
            assert_eq!(models[1].capabilities.context_window, 512_000);
            assert_eq!(models[2].capabilities.context_window, 1_000_000);
            assert!(!models[0].reserve_output_context);
            assert!(!models[1].reserve_output_context);
            assert!(models[2].reserve_output_context);
            assert_eq!(models[0].output_token_reservation(Some("unset")), 0);
            assert_eq!(models[1].output_token_reservation(Some("unset")), 0);
            assert_eq!(models[2].output_token_reservation(Some("unset")), 128_000);
            assert!(models.iter().all(|model| model.model == models[0].model));
        }
        assert!(models[12..].iter().all(|model| {
            is_hidden_legacy_model(model)
                && model.capabilities.context_window == 512_000
                && !model.reserve_output_context
        }));
        assert!(models.iter().all(|model| model.parameters.is_empty()));
        assert!(
            models[0]
                .capabilities
                .reasoning_efforts
                .contains(&"ultra".to_owned())
        );
        assert!(
            !models[6]
                .capabilities
                .reasoning_efforts
                .contains(&"ultra".to_owned())
        );
        assert!(models.iter().all(|model| {
            model
                .capabilities
                .reasoning_efforts
                .first()
                .map(String::as_str)
                == Some(crate::config::UNSET_EFFORT)
        }));
        assert!(
            models
                .iter()
                .all(|model| model.provider == ProviderType::CodexOauth
                    && model.credential_file.as_deref() == path.to_str())
        );
    }

    #[test]
    fn logged_in_state_adds_runtime_models_without_persisting_secrets_or_overwriting_custom_names()
    {
        let directory =
            std::env::temp_dir().join(format!("me-codex-models-{}", std::process::id()));
        let path = directory.join("auth.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, auth_document(u64::MAX).to_string()).unwrap();
        let existing = ModelConfig {
            name: "default".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "https://example.com".into(),
            endpoint: "/chat/completions".into(),
            api_key: Some("test".into()),
            api_key_env: None,
            credential_file: None,
            model: "default".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: ModelCapabilities::default(),
            parameters: toml::Table::new(),
            effort_parameters: Default::default(),
        };
        let mut custom_same_name = existing.clone();
        custom_same_name.name = "gpt-5.6-sol".into();
        custom_same_name.model = "custom-gpt-5.6-sol".into();
        let mut global = GlobalConfig {
            version: 1,
            default_model: "default".into(),
            models: vec![existing, custom_same_name],
        };

        add_models_if_logged_in_at(&mut global, &path).unwrap();
        assert_eq!(global.models.len(), 17);
        assert!(MODEL_NAMES.iter().all(|name| global.model(name).is_some()));
        assert_eq!(
            global
                .models
                .iter()
                .filter(|model| model.name == "gpt-5.6-sol")
                .count(),
            1
        );
        assert_eq!(
            global.model("gpt-5.6-sol").unwrap().provider,
            ProviderType::OpenaiCompatible
        );
        assert_eq!(
            global.model("gpt-5.6-sol").unwrap().model,
            "custom-gpt-5.6-sol"
        );
        assert!(
            LEGACY_MODEL_NAMES
                .iter()
                .all(|name| global.model(name).is_some())
        );
        assert!(
            global
                .models
                .iter()
                .filter(|model| model.provider == ProviderType::CodexOauth)
                .all(|model| model.api_key.is_none())
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refresh_rotates_tokens_and_keeps_auth_file_private() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n")
                    && request
                        .windows("refresh-token".len())
                        .any(|window| window == b"refresh-token")
                {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&request).contains("\"grant_type\":\"refresh_token\""));
            let body = r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let directory =
            std::env::temp_dir().join(format!("me-codex-refresh-{}", std::process::id()));
        let path = directory.join("auth.json");
        let document = auth_document(1);
        let client = Client::builder().no_proxy().build().unwrap();

        let refreshed = refresh_at(&path, &client, document, &endpoint, "official-client").unwrap();
        server.join().unwrap();
        assert_eq!(
            refreshed.pointer("/tokens/access_token"),
            Some(&Value::String("fresh-access".into()))
        );
        assert_eq!(
            refreshed.pointer("/tokens/refresh_token"),
            Some(&Value::String("fresh-refresh".into()))
        );
        assert_eq!(
            read_auth(&path).unwrap().pointer("/tokens/access_token"),
            Some(&Value::String("fresh-access".into()))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_refreshes_share_the_single_rotated_credential() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request
                    .windows("stale-refresh".len())
                    .any(|window| window == b"stale-refresh")
                {
                    break;
                }
            }
            let body = json!({
                "access_token": jwt(json!({"exp": u64::MAX})),
                "refresh_token": "fresh-refresh"
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let directory = std::env::temp_dir().join(format!(
            "me-codex-refresh-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = Arc::new(directory.join("auth.json"));
        let mut document = auth_document(1);
        document["tokens"]["refresh_token"] = Value::String("stale-refresh".into());
        write_auth(path.as_ref(), &document).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                let endpoint = endpoint.clone();
                thread::spawn(move || {
                    let client = Client::builder().no_proxy().build().unwrap();
                    barrier.wait();
                    let credential = request_credential_at(
                        path.as_ref(),
                        &client,
                        None,
                        &endpoint,
                        "official-client",
                    )
                    .unwrap();
                    assert!(credential.access_token.contains('.'));
                    let stored = read_auth(path.as_ref()).unwrap();
                    assert_eq!(
                        stored
                            .pointer("/tokens/refresh_token")
                            .and_then(Value::as_str),
                        Some("fresh-refresh")
                    );
                    endpoint
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), endpoint);
        }
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn built_in_device_login_prints_code_exchanges_tokens_and_persists_auth() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let auth_base_url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let id_token = jwt(json!({
            "email": "device@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-device",
                "chatgpt_plan_type": "plus"
            }
        }));
        let access_token = jwt(json!({"exp": u64::MAX}));
        let server_thread = thread::spawn(move || {
            let mut request = server.recv().unwrap();
            assert_eq!(request.url(), "/api/accounts/deviceauth/usercode");
            assert_eq!(
                serde_json::from_str::<Value>(&read_request_body(&mut request)).unwrap(),
                json!({"client_id": CLIENT_ID})
            );
            respond_json(
                request,
                200,
                json!({
                    "device_auth_id": "device-123",
                    "user_code": "ABCD-EFGH",
                    "interval": "0"
                }),
            );

            let mut request = server.recv().unwrap();
            assert_eq!(request.url(), "/api/accounts/deviceauth/token");
            assert_eq!(
                serde_json::from_str::<Value>(&read_request_body(&mut request)).unwrap(),
                json!({"device_auth_id": "device-123", "user_code": "ABCD-EFGH"})
            );
            respond_json(request, 403, json!({"error": "authorization_pending"}));

            let mut request = server.recv().unwrap();
            assert_eq!(request.url(), "/api/accounts/deviceauth/token");
            assert_eq!(
                serde_json::from_str::<Value>(&read_request_body(&mut request)).unwrap(),
                json!({"device_auth_id": "device-123", "user_code": "ABCD-EFGH"})
            );
            respond_json(
                request,
                200,
                json!({
                    "authorization_code": "authorization-code",
                    "code_challenge": "challenge",
                    "code_verifier": "verifier"
                }),
            );

            let mut request = server.recv().unwrap();
            assert_eq!(request.url(), "/oauth/token");
            let body = read_request_body(&mut request);
            assert!(body.contains("grant_type=authorization_code"));
            assert!(body.contains("code=authorization-code"));
            assert!(body.contains("code_verifier=verifier"));
            respond_json(
                request,
                200,
                json!({
                    "id_token": id_token,
                    "access_token": access_token,
                    "refresh_token": "device-refresh"
                }),
            );
        });

        let directory = std::env::temp_dir().join(format!(
            "me-codex-device-login-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("auth.json");
        let client = Client::builder().no_proxy().build().unwrap();
        let mut output = Vec::new();
        login_at(
            &path,
            &client,
            &auth_base_url,
            Duration::from_secs(2),
            &mut output,
        )
        .unwrap();
        server_thread.join().unwrap();

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains(&format!("{auth_base_url}/codex/device")));
        assert!(rendered.contains("ABCD-EFGH"));
        assert!(rendered.contains("Codex 登录成功"));
        let status = status_at(&path).unwrap();
        assert!(status.logged_in);
        assert_eq!(status.account_id.as_deref(), Some("account-device"));
        assert_eq!(status.email.as_deref(), Some("device@example.com"));
        assert_eq!(status.plan.as_deref(), Some("plus"));
        assert_eq!(
            read_auth(&path)
                .unwrap()
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("device-refresh")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refresh_error_never_echoes_raw_body() {
        assert_eq!(
            refresh_error_detail(
                r#"{"error":{"code":"refresh_token_expired","message":"secret echo"}}"#
            ),
            "refresh_token_expired"
        );
        assert_eq!(
            refresh_error_detail("<html>secret infrastructure detail</html>"),
            "OAuth server rejected the refresh token"
        );
    }
}
