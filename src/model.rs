use std::io::{BufRead, BufReader};

use reqwest::blocking::{Client, Response};
use serde_json::{Map, Value, json};

use crate::{
    Result, codex_oauth,
    config::{ModelConfig, ProviderType, UNSET_EFFORT},
};

#[derive(Clone, Debug, Default)]
pub struct ModelContext {
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
}

impl ModelContext {
    pub fn user(prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![json!({"role": "user", "content": prompt.into()})],
            tools: Vec::new(),
        }
    }

    pub fn push(&mut self, role: &str, content: impl Into<String>) {
        self.messages
            .push(json!({"role": role, "content": content.into()}));
    }

    pub fn push_value(&mut self, message: Value) {
        self.messages.push(message);
    }
}

pub struct ModelApi {
    model: ModelConfig,
    client: Client,
}

pub struct ModelRuntime {
    models: Vec<ModelConfig>,
    active: ModelApi,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAiToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OpenAiStreamEvent {
    Delta {
        content: Option<String>,
        tool_calls: Vec<OpenAiToolCallDelta>,
    },
    ProviderContextItem {
        provider: String,
        item: Value,
    },
    Done,
    Other,
}

impl ModelApi {
    pub fn new(model: ModelConfig) -> Result<Self> {
        if !matches!(
            model.provider,
            ProviderType::OpenaiCompatible | ProviderType::CodexOauth
        ) {
            return Err(format!("provider {} is not implemented", model.provider).into());
        }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(model.timeout_seconds))
            .build()?;
        Ok(Self { model, client })
    }

    pub fn complete(&self, context: &ModelContext, effort: Option<&str>) -> Result<Value> {
        let body = self.request_body(context, effort, false)?;
        let response = self.send(body)?;
        Ok(serde_json::from_str(&response.text()?)?)
    }

    pub fn validate_effort(&self, effort: &str) -> Result<()> {
        self.model.validate_effort(effort)
    }

    pub fn model_name(&self) -> &str {
        &self.model.name
    }

    pub fn complete_stream(
        &self,
        context: &ModelContext,
        effort: Option<&str>,
        mut on_line: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        self.complete_stream_with_output_limit(context, effort, None, &mut on_line)
    }

    pub fn complete_stream_with_output_limit(
        &self,
        context: &ModelContext,
        effort: Option<&str>,
        output_limit: Option<u64>,
        mut on_line: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let mut body = self.request_body(context, effort, true)?;
        if self.model.provider != ProviderType::CodexOauth
            && let Some(output_limit) = output_limit
        {
            clamp_output_limit(&mut body, output_limit)?;
        }
        let response = self.send(body)?;
        let mut codex_completed = false;
        for line in BufReader::new(response).lines() {
            let line = line?;
            if !line.is_empty() {
                if self.model.provider == ProviderType::CodexOauth
                    && matches!(openai_stream_event(&line)?, OpenAiStreamEvent::Done)
                {
                    codex_completed = true;
                }
                on_line(&line)?;
            }
        }
        if self.model.provider == ProviderType::CodexOauth && !codex_completed {
            return Err("Codex Responses stream closed before response.completed".into());
        }
        Ok(())
    }

    pub fn output_token_reservation(&self, effort: Option<&str>) -> u64 {
        self.model.output_token_reservation(effort)
    }

    fn send(&self, body: Value) -> Result<Response> {
        if self.model.provider == ProviderType::CodexOauth {
            return self.send_codex(body);
        }
        let mut request = self.client.post(self.endpoint()).json(&body);
        if let Some(api_key) = self.model.request_api_key()? {
            request = request.bearer_auth(api_key);
        }
        let response = request.send()?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let message = response.text()?;
        Err(format!("model request failed with {status}: {message}").into())
    }

    fn send_codex(&self, body: Value) -> Result<Response> {
        let credential_file = self
            .model
            .credential_file
            .as_deref()
            .ok_or_else(|| format!("model {} has no credential file", self.model.name))?;
        let credential_file = crate::config::expand_home(credential_file);
        let credential = codex_oauth::request_credential(&credential_file, &self.client, None)?;
        let mut response = self.send_codex_once(&body, &credential)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let refreshed = codex_oauth::request_credential(
                &credential_file,
                &self.client,
                Some(&credential.access_token),
            )?;
            response = self.send_codex_once(&body, &refreshed)?;
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let message = response.text()?;
        Err(format!("Codex model request failed with {status}: {message}").into())
    }

    fn send_codex_once(
        &self,
        body: &Value,
        credential: &codex_oauth::CodexRequestCredential,
    ) -> Result<Response> {
        Ok(self
            .client
            .post(self.endpoint())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(
                reqwest::header::USER_AGENT,
                concat!("me/", env!("CARGO_PKG_VERSION")),
            )
            .header("originator", "me")
            .header("ChatGPT-Account-ID", &credential.account_id)
            .bearer_auth(&credential.access_token)
            .json(body)
            .send()?)
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/{}",
            self.model.base_url.trim_end_matches('/'),
            self.model.endpoint.trim_start_matches('/')
        )
    }

    fn request_body(
        &self,
        context: &ModelContext,
        effort: Option<&str>,
        stream: bool,
    ) -> Result<Value> {
        if self.model.provider == ProviderType::CodexOauth {
            return self.codex_request_body(context, effort, stream);
        }
        let mut body = toml_table_to_json(&self.model.parameters)?;

        if let Some(effort) = effort {
            self.model.validate_effort(effort)?;
            if effort != UNSET_EFFORT
                && let Some(parameters) = self.model.effort_parameters.get(effort)
            {
                body.extend(toml_table_to_json(parameters)?);
            }
        }

        body.insert("model".into(), Value::String(self.model.model.clone()));
        body.insert(
            "messages".into(),
            Value::Array(
                context
                    .messages
                    .iter()
                    .filter(|message| message.get("_me_provider").is_none())
                    .cloned()
                    .collect(),
            ),
        );
        body.insert("stream".into(), Value::Bool(stream));
        if stream {
            let stream_options = body
                .entry("stream_options")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or("model parameter stream_options must be an object")?;
            stream_options.insert("include_usage".into(), Value::Bool(true));
        }
        if !context.tools.is_empty() {
            body.insert("tools".into(), Value::Array(context.tools.clone()));
            body.insert("parallel_tool_calls".into(), Value::Bool(true));
        }
        Ok(Value::Object(body))
    }

    fn codex_request_body(
        &self,
        context: &ModelContext,
        effort: Option<&str>,
        stream: bool,
    ) -> Result<Value> {
        let mut body = toml_table_to_json(&self.model.parameters)?;
        let (instructions, input) = responses_input(&context.messages)?;
        let tools = responses_tools(&context.tools)?;

        if let Some(effort) = effort {
            self.model.validate_effort(effort)?;
            if effort != UNSET_EFFORT {
                body.insert(
                    "reasoning".into(),
                    json!({"effort": effort, "summary": "auto"}),
                );
            }
        }
        body.insert("model".into(), Value::String(self.model.model.clone()));
        body.insert("instructions".into(), Value::String(instructions));
        body.insert("input".into(), Value::Array(input));
        body.insert("stream".into(), Value::Bool(stream));
        body.insert("store".into(), Value::Bool(false));
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
            body.insert("tool_choice".into(), Value::String("auto".into()));
            body.insert("parallel_tool_calls".into(), Value::Bool(true));
        }
        Ok(Value::Object(body))
    }
}

fn clamp_output_limit(body: &mut Value, limit: u64) -> Result<()> {
    let body = body
        .as_object_mut()
        .ok_or("model request body must be an object")?;
    for name in ["max_output_tokens", "max_completion_tokens", "max_tokens"] {
        let Some(current) = body.get(name).and_then(Value::as_u64) else {
            continue;
        };
        if current > limit {
            body.insert(name.into(), Value::from(limit));
        }
    }
    Ok(())
}

impl ModelRuntime {
    pub fn new(models: Vec<ModelConfig>, initial_model: &str) -> Result<Self> {
        let model = models
            .iter()
            .find(|model| model.name == initial_model)
            .cloned()
            .ok_or_else(|| format!("model {initial_model} does not exist"))?;
        Ok(Self {
            models,
            active: ModelApi::new(model)?,
        })
    }

    pub fn single(active: ModelApi) -> Self {
        Self {
            models: vec![active.model.clone()],
            active,
        }
    }

    pub fn activate(&mut self, name: &str) -> Result<()> {
        if self.active.model.name == name {
            return Ok(());
        }
        let model = self
            .models
            .iter()
            .find(|model| model.name == name)
            .cloned()
            .ok_or_else(|| format!("model {name} does not exist"))?;
        self.active = ModelApi::new(model)?;
        Ok(())
    }

    pub fn validate_activation(&self, name: &str) -> Result<()> {
        let model = self
            .models
            .iter()
            .find(|model| model.name == name)
            .cloned()
            .ok_or_else(|| format!("model {name} does not exist"))?;
        ModelApi::new(model).map(|_| ())
    }

    pub fn model(&self, name: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|model| model.name == name)
    }

    pub fn active_model(&self) -> &ModelConfig {
        &self.active.model
    }

    pub fn api(&self) -> &ModelApi {
        &self.active
    }
}

impl From<ModelApi> for ModelRuntime {
    fn from(api: ModelApi) -> Self {
        Self::single(api)
    }
}

pub fn openai_stream_event(line: &str) -> Result<OpenAiStreamEvent> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(OpenAiStreamEvent::Other);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(OpenAiStreamEvent::Done);
    }
    let value: Value = serde_json::from_str(data)?;
    if value.get("type").is_some() {
        return responses_stream_event(&value);
    }
    if value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        == Some("length")
    {
        return Err("model response was truncated at the output token limit".into());
    }
    let content = value
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let tool_calls = value
        .pointer("/choices/0/delta/tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    Some(OpenAiToolCallDelta {
                        index: usize::try_from(call.get("index")?.as_u64()?).ok()?,
                        id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                        name: call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        arguments: call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if content.is_none() && tool_calls.is_empty() {
        Ok(OpenAiStreamEvent::Other)
    } else {
        Ok(OpenAiStreamEvent::Delta {
            content,
            tool_calls,
        })
    }
}

pub fn openai_stream_usage(line: &str) -> Result<Option<ModelUsage>> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(data)?;
    let usage = value
        .pointer("/response/usage")
        .or_else(|| value.get("usage"));
    let Some(usage) = usage else {
        return Ok(None);
    };
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_u64);
    let total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
    match (input_tokens, output_tokens) {
        (Some(input_tokens), Some(output_tokens)) => Ok(Some(ModelUsage {
            input_tokens,
            output_tokens,
            total_tokens: total_tokens.unwrap_or(input_tokens.saturating_add(output_tokens)),
        })),
        _ => Ok(None),
    }
}

fn responses_stream_event(value: &Value) -> Result<OpenAiStreamEvent> {
    match value.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => Ok(OpenAiStreamEvent::Delta {
            content: value
                .get("delta")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool_calls: Vec::new(),
        }),
        Some("response.output_item.done")
            if value.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
        {
            let item = &value["item"];
            Ok(OpenAiStreamEvent::Delta {
                content: None,
                tool_calls: vec![OpenAiToolCallDelta {
                    index: value
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .and_then(|index| usize::try_from(index).ok())
                        .unwrap_or(0),
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }],
            })
        }
        Some("response.output_item.done")
            if value.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") =>
        {
            Ok(OpenAiStreamEvent::ProviderContextItem {
                provider: "codex-oauth".into(),
                item: value["item"].clone(),
            })
        }
        Some("response.completed") => Ok(OpenAiStreamEvent::Done),
        Some("response.failed" | "response.incomplete" | "error") => {
            let message = value
                .pointer("/response/error/message")
                .or_else(|| value.pointer("/error/message"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex Responses stream failed");
            Err(message.to_owned().into())
        }
        _ => Ok(OpenAiStreamEvent::Other),
    }
}

fn responses_input(messages: &[Value]) -> Result<(String, Vec<Value>)> {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        if let Some(provider) = message.get("_me_provider").and_then(Value::as_str) {
            if provider == "codex-oauth" {
                input.push(
                    message
                        .get("item")
                        .cloned()
                        .ok_or("Codex provider context message has no item")?,
                );
            }
            continue;
        }
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or("ModelContext message has no role")?;
        match role {
            "system" => {
                if let Some(content) = message.get("content").and_then(Value::as_str)
                    && !content.is_empty()
                {
                    instructions.push(content.to_owned());
                }
            }
            "user" | "assistant" => {
                let mut content_parts = Vec::new();
                if let Some(content) = message.get("content").and_then(Value::as_str) {
                    if !content.is_empty() {
                        let content_type = if role == "assistant" {
                            "output_text"
                        } else {
                            "input_text"
                        };
                        content_parts.push(json!({"type": content_type, "text": content}));
                    }
                } else if let Some(parts) = message.get("content").and_then(Value::as_array) {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("text" | "input_text" | "output_text") => {
                                let text = required_json_string(part, "/text")?;
                                if !text.is_empty() {
                                    content_parts.push(json!({
                                        "type": if role == "assistant" {"output_text"} else {"input_text"},
                                        "text": text,
                                    }));
                                }
                            }
                            Some("image_url") if role == "user" => {
                                content_parts.push(json!({
                                    "type": "input_image",
                                    "image_url": required_json_string(part, "/image_url/url")?,
                                }));
                            }
                            Some(kind) => {
                                return Err(format!(
                                    "unsupported Codex Responses {role} content type {kind}"
                                )
                                .into());
                            }
                            None => return Err("ModelContext content part has no type".into()),
                        }
                    }
                }
                if !content_parts.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": content_parts,
                    }));
                }
                if role == "assistant"
                    && let Some(calls) = message.get("tool_calls").and_then(Value::as_array)
                {
                    for call in calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": required_json_string(call, "/id")?,
                            "name": required_json_string(call, "/function/name")?,
                            "arguments": required_json_string(call, "/function/arguments")?,
                        }));
                    }
                }
            }
            "tool" => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": required_json_string(message, "/tool_call_id")?,
                    "output": required_json_string(message, "/content")?,
                }));
            }
            _ => return Err(format!("unsupported ModelContext role {role}").into()),
        }
    }
    Ok((instructions.join("\n\n"), input))
}

fn responses_tools(tools: &[Value]) -> Result<Vec<Value>> {
    tools
        .iter()
        .map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err("Codex Responses only supports function tools".into());
            }
            let function = tool
                .get("function")
                .and_then(Value::as_object)
                .ok_or("function tool has no function object")?;
            Ok(json!({
                "type": "function",
                "name": function.get("name").cloned().ok_or("function tool has no name")?,
                "description": function.get("description").cloned().unwrap_or(Value::String(String::new())),
                "parameters": function.get("parameters").cloned().ok_or("function tool has no parameters")?,
                "strict": function.get("strict").cloned().unwrap_or(Value::Bool(false)),
            }))
        })
        .collect()
}

fn required_json_string<'a>(value: &'a Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("ModelContext item is missing {pointer}").into())
}

fn toml_table_to_json(table: &toml::Table) -> Result<Map<String, Value>> {
    let value = serde_json::to_value(table)?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "model parameters must be a table".into())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use crate::config::{ModelCapabilities, ProviderType};

    use super::*;

    fn config() -> ModelConfig {
        let mut effort_parameters = BTreeMap::new();
        effort_parameters.insert(
            "high".into(),
            toml::from_str("reasoning_effort = \"high\"").unwrap(),
        );
        ModelConfig {
            name: "test".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "https://example.com/v1".into(),
            endpoint: "/chat/completions".into(),
            api_key: Some("key".into()),
            api_key_env: None,
            credential_file: None,
            model: "api-model".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: ModelCapabilities {
                reasoning_efforts: vec!["high".into()],
                ..ModelCapabilities::default()
            },
            parameters: toml::from_str("max_tokens = 32").unwrap(),
            effort_parameters,
        }
    }

    fn codex_config() -> ModelConfig {
        ModelConfig {
            name: "gpt-5.6-sol".into(),
            provider: ProviderType::CodexOauth,
            reserve_output_context: false,
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            endpoint: "/responses".into(),
            api_key: None,
            api_key_env: None,
            credential_file: Some("/config/me/codex/auth.json".into()),
            model: "gpt-5.6-sol".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: ModelCapabilities {
                context_window: 512_000,
                max_output_tokens: Some(128_000),
                reasoning_efforts: vec!["high".into()],
                ..ModelCapabilities::default()
            },
            parameters: toml::Table::new(),
            effort_parameters: BTreeMap::new(),
        }
    }

    #[test]
    fn request_body_merges_parameters() {
        let api = ModelApi::new(config()).unwrap();
        let body = api
            .request_body(&ModelContext::user("hello"), Some("high"), true)
            .unwrap();
        assert_eq!(body["model"], "api-model");
        assert_eq!(body["max_tokens"], 32);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn credentialless_openai_compatible_request_omits_authorization() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(!headers.contains("\r\nauthorization:"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                )
                .unwrap();
        });

        let mut configured = config();
        configured.base_url = format!("http://{address}");
        configured.endpoint = "/v1/chat/completions".into();
        configured.api_key = None;
        configured.api_key_env = None;
        configured.credential_file = None;
        let response = ModelApi::new(configured)
            .unwrap()
            .complete(&ModelContext::user("hello"), None)
            .unwrap();

        assert_eq!(response["ok"], true);
        server.join().unwrap();
    }

    #[test]
    fn output_reservation_uses_the_actual_request_parameter_and_explicit_policy() {
        let mut configured = config();
        configured
            .effort_parameters
            .insert("high".into(), toml::from_str("max_tokens = 16").unwrap());
        let api = ModelApi::new(configured).unwrap();
        assert_eq!(api.output_token_reservation(None), 32);
        assert_eq!(api.output_token_reservation(Some("high")), 16);

        let codex = codex_config();
        assert_eq!(
            ModelApi::new(codex)
                .unwrap()
                .output_token_reservation(Some("high")),
            0
        );

        let mut capability_reserved = codex_config();
        capability_reserved.reserve_output_context = true;
        assert_eq!(
            ModelApi::new(capability_reserved)
                .unwrap()
                .output_token_reservation(Some("high")),
            128_000
        );

        let mut explicitly_reserved = codex_config();
        explicitly_reserved.reserve_output_context = true;
        explicitly_reserved.parameters = toml::from_str("max_output_tokens = 999").unwrap();
        assert_eq!(
            ModelApi::new(explicitly_reserved)
                .unwrap()
                .output_token_reservation(Some("high")),
            999
        );
    }

    #[test]
    fn output_limit_only_clamps_existing_request_parameters() {
        let mut body = json!({"max_tokens": 393216, "temperature": 0.2});
        clamp_output_limit(&mut body, 233_221).unwrap();
        assert_eq!(body["max_tokens"], 233_221);
        assert_eq!(body["temperature"], 0.2);

        let mut body = json!({"temperature": 0.2});
        clamp_output_limit(&mut body, 100).unwrap();
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn multimodal_messages_translate_to_codex_responses() {
        let data_url = format!("data:image/png;base64,{}", "A".repeat(200_000));
        let context = ModelContext {
            messages: vec![json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "inspect"},
                    {"type": "image_url", "image_url": {"url": data_url}}
                ]
            })],
            tools: Vec::new(),
        };
        let (_, input) = responses_input(&context.messages).unwrap();
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert!(
            input[0]["content"][1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn openai_compatible_allows_multiple_tool_calls_per_response() {
        let api = ModelApi::new(config()).unwrap();
        let mut context = ModelContext::user("run both");
        context.tools.push(json!({
            "type": "function",
            "function": {
                "name": "First",
                "description": "first",
                "parameters": {"type": "object"}
            }
        }));

        let body = api.request_body(&context, None, true).unwrap();
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn streaming_usage_option_preserves_other_configured_options() {
        let mut config = config();
        config.parameters.insert(
            "stream_options".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "vendor_flag".into(),
                toml::Value::Boolean(true),
            )])),
        );
        let api = ModelApi::new(config).unwrap();
        let body = api
            .request_body(&ModelContext::user("hello"), None, true)
            .unwrap();
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stream_options"]["vendor_flag"], true);
    }

    #[test]
    fn non_streaming_request_does_not_request_stream_usage() {
        let api = ModelApi::new(config()).unwrap();
        let body = api
            .request_body(&ModelContext::user("hello"), None, false)
            .unwrap();
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn unset_effort_omits_provider_reasoning_parameters() {
        let api = ModelApi::new(config()).unwrap();
        let body = api
            .request_body(&ModelContext::user("hello"), Some(UNSET_EFFORT), true)
            .unwrap();
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["max_tokens"], 32);

        let api = ModelApi::new(codex_config()).unwrap();
        let body = api
            .request_body(&ModelContext::user("hello"), Some(UNSET_EFFORT), true)
            .unwrap();
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["model"], "gpt-5.6-sol");
    }

    #[test]
    fn model_runtime_switches_only_to_catalog_models() {
        let mut second = config();
        second.name = "second".into();
        second.model = "api-second".into();
        let mut runtime = ModelRuntime::new(vec![config(), second], "test").unwrap();
        assert_eq!(runtime.active_model().name, "test");
        runtime.activate("second").unwrap();
        assert_eq!(runtime.active_model().model, "api-second");
        assert!(runtime.activate("missing").is_err());
        assert_eq!(runtime.active_model().name, "second");
    }

    #[test]
    fn parses_openai_stream_event() {
        let line = r#"data: {"choices":[{"delta":{"content":"ok"}}]}"#;
        assert_eq!(
            openai_stream_event(line).unwrap(),
            OpenAiStreamEvent::Delta {
                content: Some("ok".into()),
                tool_calls: Vec::new(),
            }
        );
        assert_eq!(
            openai_stream_event("data: [DONE]").unwrap(),
            OpenAiStreamEvent::Done
        );
        let error =
            openai_stream_event(r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#)
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "model response was truncated at the output token limit"
        );
    }

    #[test]
    fn parses_chat_and_responses_stream_usage() {
        assert_eq!(
            openai_stream_usage(
                r#"data: {"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#
            )
            .unwrap(),
            Some(ModelUsage {
                input_tokens: 8,
                output_tokens: 2,
                total_tokens: 10,
            })
        );
        assert_eq!(
            openai_stream_usage(
                r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":11,"output_tokens":3,"total_tokens":14}}}"#
            )
            .unwrap(),
            Some(ModelUsage {
                input_tokens: 11,
                output_tokens: 3,
                total_tokens: 14,
            })
        );
        assert_eq!(
            openai_stream_usage(r#"data: {"usage":{"input_tokens":5,"output_tokens":1}}"#).unwrap(),
            Some(ModelUsage {
                input_tokens: 5,
                output_tokens: 1,
                total_tokens: 6,
            })
        );
        assert_eq!(openai_stream_usage("data: [DONE]").unwrap(), None);
        assert_eq!(
            openai_stream_usage(r#"data: {"choices":[{"delta":{"content":"OK"}}]}"#).unwrap(),
            None
        );
    }

    #[test]
    fn parses_streamed_tool_call_delta() {
        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"Terminal_Interact","arguments":"{\"session_id\":"}}]}}]}"#;
        assert_eq!(
            openai_stream_event(line).unwrap(),
            OpenAiStreamEvent::Delta {
                content: None,
                tool_calls: vec![OpenAiToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("Terminal_Interact".into()),
                    arguments: Some("{\"session_id\":".into()),
                }],
            }
        );
    }

    #[test]
    fn codex_request_translates_messages_tools_and_effort_to_responses() {
        let api = ModelApi::new(codex_config()).unwrap();
        let mut context = ModelContext::default();
        context.push("system", "system");
        context.push("user", "run it");
        context.push_value(json!({
            "role": "assistant",
            "content": "working",
            "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {
                    "name": "Terminal_Interact",
                    "arguments": "{\"input\":\"pwd\"}"
                }
            }]
        }));
        context.push_value(json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": "/workspace"
        }));
        context.tools.push(json!({
            "type": "function",
            "function": {
                "name": "Terminal_Interact",
                "description": "Interact with a terminal",
                "parameters": {"type": "object"},
            }
        }));

        let body = api.request_body(&context, Some("high"), true).unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][2]["call_id"], "call-1");
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "Terminal_Interact");
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn parses_codex_responses_text_tool_completion_and_failure() {
        assert_eq!(
            openai_stream_event(r#"data: {"type":"response.output_text.delta","delta":"hello"}"#)
                .unwrap(),
            OpenAiStreamEvent::Delta {
                content: Some("hello".into()),
                tool_calls: Vec::new(),
            }
        );
        assert_eq!(
            openai_stream_event(
                r#"data: {"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call-9","name":"Terminal_Create","arguments":"{\"command\":\"pwd\"}"}}"#
            )
            .unwrap(),
            OpenAiStreamEvent::Delta {
                content: None,
                tool_calls: vec![OpenAiToolCallDelta {
                    index: 2,
                    id: Some("call-9".into()),
                    name: Some("Terminal_Create".into()),
                    arguments: Some(r#"{"command":"pwd"}"#.into()),
                }],
            }
        );
        assert_eq!(
            openai_stream_event(r#"data: {"type":"response.completed","response":{"id":"r"}}"#)
                .unwrap(),
            OpenAiStreamEvent::Done
        );
        assert_eq!(
            openai_stream_event(
                r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"opaque","summary":[]}}"#
            )
            .unwrap(),
            OpenAiStreamEvent::ProviderContextItem {
                provider: "codex-oauth".into(),
                item: json!({
                    "type": "reasoning",
                    "encrypted_content": "opaque",
                    "summary": []
                }),
            }
        );
        let error = openai_stream_event(
            r#"data: {"type":"response.failed","response":{"error":{"message":"denied"}}}"#,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "denied");
    }

    #[test]
    fn codex_context_keeps_system_only_in_instructions() {
        let api = ModelApi::new(codex_config()).unwrap();
        let mut context = ModelContext::default();
        context.push("system", "one");
        context.push("system", "two");
        context.push("user", "hello");
        let body = api.request_body(&context, None, true).unwrap();
        assert_eq!(body["instructions"], "one\n\ntwo");
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["role"] != "system")
        );
    }

    #[test]
    fn openai_compatible_filters_codex_provider_context_items() {
        let api = ModelApi::new(config()).unwrap();
        let mut context = ModelContext::user("hello");
        context.push_value(json!({
            "_me_provider": "codex-oauth",
            "item": {
                "type": "reasoning",
                "encrypted_content": "opaque",
                "summary": []
            }
        }));
        let body = api.request_body(&context, None, true).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["content"], "hello");
    }
}
