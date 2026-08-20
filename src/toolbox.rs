use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    Result, agent_title, agent_toolbox, compact,
    config::{config_home, create_private_directory},
    image_toolbox, python_runtime,
    terminal::{
        self, TerminalFrame, TerminalLineUpdate, TerminalManager, TerminalRequest,
        TerminalSessionPreview, TerminalStatus,
    },
    workmap,
};

pub const TOOLBOX_DIRECTORY: &str = ".me/tools";
pub const WORKSPACE_TEMP_DIRECTORY: &str = ".me/tmp";
pub const DEFAULT_PYTHON_MAJOR: u8 = 3;
pub const DEFAULT_PYTHON_MINOR: u8 = 12;
pub(crate) const DISABLED_FILE_APPLY_PATCH: &str = "File.ApplyPatch";
const DISABLED_FILE_APPLY_PATCH_API: &str = "File_ApplyPatch";
const DEFAULT_TERMINAL_FILE: &str = "Terminal.py";
const DEFAULT_TERMINAL_SOURCE: &str = include_str!("default_terminal_toolbox.py");
const DEFAULT_FILE_FILE: &str = "File.py";
const DEFAULT_FILE_SOURCE: &str = include_str!("default_file_toolbox.py");
const DEFAULT_WEB_BROWSER_FILE: &str = "WebBrowser.py";
const DEFAULT_WEB_BROWSER_SOURCE: &str = include_str!("default_web_browser_toolbox.py");
#[cfg(target_os = "macos")]
const DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL_FILE: &str =
    ".WebBrowser-window-control-macos.dylib";
#[cfg(target_os = "macos")]
#[path = "../.build/generated/camoufox_bridge.rs"]
mod camoufox_bridge;
#[cfg(target_os = "macos")]
const DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL: &[u8] = camoufox_bridge::BYTES;
const STDERR_HISTORY_LINES: usize = 32;
const TERMINAL_OBSERVE_ACTIVE_SESSIONS: &str = "__activeSessions";
const TERMINAL_OBSERVE_FRAME: &str = "__terminalFrame";
const TERMINAL_OBSERVE_BACKEND: &str = "__terminalBackend";
const WEB_BROWSER_OBSERVE_ACTIVE_PAGES: &str = "__activePages";
const TERMINAL_OBSERVER_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct ToolboxTool {
    pub toolbox: String,
    pub local_name: String,
    pub full_name: String,
    pub api_name: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub instructions: String,
    pub route: String,
    pub examples: String,
}

impl ToolboxTool {
    fn model_definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.api_name,
                "description": self.route,
                "parameters": self.input_schema,
            }
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolboxCatalog {
    tools: Vec<ToolboxTool>,
    briefs: Vec<(String, String)>,
    prompt: String,
    api_to_full: BTreeMap<String, String>,
}

impl ToolboxCatalog {
    pub fn tools(&self) -> &[ToolboxTool] {
        &self.tools
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn model_definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(ToolboxTool::model_definition)
            .collect()
    }

    pub fn model_definitions_excluding(&self, toolbox: &str) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|tool| tool.toolbox != toolbox)
            .map(ToolboxTool::model_definition)
            .collect()
    }

    pub fn prompt_excluding(&self, toolbox: &str) -> Result<String> {
        render_catalog_prompt(&self.tools, &self.briefs, Some(toolbox))
    }

    pub fn excluding(&self, toolbox: &str) -> Result<Self> {
        catalog_from_parts(
            self.tools
                .iter()
                .filter(|tool| tool.toolbox != toolbox)
                .cloned()
                .collect(),
            self.briefs
                .iter()
                .filter(|(name, _)| name != toolbox)
                .cloned()
                .collect(),
        )
    }

    pub fn manager_view(&self) -> Result<Self> {
        let (mut tools, worker_brief) = agent_toolbox::worker_catalog_parts();
        tools.extend(
            self.tools
                .iter()
                .filter(|tool| {
                    !matches!(
                        tool.toolbox.as_str(),
                        agent_toolbox::AGENT_TOOLBOX_NAME | agent_toolbox::WORKER_TOOLBOX_NAME
                    )
                })
                .cloned(),
        );
        let mut briefs = vec![worker_brief];
        briefs.extend(
            self.briefs
                .iter()
                .filter(|(name, _)| {
                    !matches!(
                        name.as_str(),
                        agent_toolbox::AGENT_TOOLBOX_NAME | agent_toolbox::WORKER_TOOLBOX_NAME
                    )
                })
                .cloned(),
        );
        catalog_from_parts(tools, briefs)
    }

    pub fn with_image_support(&self, image_input_supported: bool) -> Result<Self> {
        let (image_tools, image_brief) = image_toolbox::catalog_parts(image_input_supported);
        let mut tools = self
            .tools
            .iter()
            .filter(|tool| tool.toolbox != image_toolbox::TOOLBOX_NAME)
            .cloned()
            .collect::<Vec<_>>();
        tools.extend(image_tools);
        let mut briefs = self
            .briefs
            .iter()
            .filter(|(name, _)| name != image_toolbox::TOOLBOX_NAME)
            .cloned()
            .collect::<Vec<_>>();
        briefs.push(image_brief);
        catalog_from_parts(tools, briefs)
    }

    pub fn resolve_api_name(&self, name: &str) -> Option<&str> {
        self.api_to_full.get(name).map(String::as_str)
    }

    pub fn api_name(&self, full_name: &str) -> String {
        self.tools
            .iter()
            .find(|tool| tool.full_name == full_name)
            .map(|tool| tool.api_name.clone())
            .unwrap_or_else(|| api_safe_name(full_name))
    }

    #[cfg(test)]
    pub(crate) fn default_terminal_for_test() -> Self {
        let tools = terminal_local_names()
            .into_iter()
            .map(|local_name| {
                let full_name = format!("Terminal.{local_name}");
                ToolboxTool {
                    toolbox: "Terminal".into(),
                    local_name: local_name.into(),
                    api_name: api_safe_name(&full_name),
                    full_name,
                    input_schema: terminal_input_schema(local_name).unwrap(),
                    output_schema: terminal_output_schema(local_name).unwrap(),
                    instructions: terminal_instructions(local_name).unwrap().into(),
                    route: terminal_route(local_name).unwrap().into(),
                    examples: terminal_examples(local_name).unwrap().into(),
                }
            })
            .collect::<Vec<_>>();
        catalog_from_parts(
            tools,
            vec![(
                "Terminal".into(),
                terminal::tool_prompt(&terminal::shell_backend()),
            )],
        )
        .unwrap()
    }

    #[cfg(test)]
    pub(crate) fn native_for_test() -> Self {
        let (tools, briefs) = native_catalog_parts();
        catalog_from_parts(tools, briefs).unwrap()
    }
}

pub struct ToolboxRuntime {
    catalog: ToolboxCatalog,
    programs: BTreeMap<String, ToolboxClient>,
}

pub(crate) fn disabled_tool_full_name(name: &str) -> Option<&'static str> {
    matches!(
        name,
        DISABLED_FILE_APPLY_PATCH | DISABLED_FILE_APPLY_PATCH_API
    )
    .then_some(DISABLED_FILE_APPLY_PATCH)
}

impl Default for ToolboxRuntime {
    fn default() -> Self {
        Self::empty()
    }
}

impl ToolboxRuntime {
    pub fn empty() -> Self {
        Self {
            catalog: ToolboxCatalog::default(),
            programs: BTreeMap::new(),
        }
    }

    pub fn load(workspace: &Path) -> Result<Self> {
        ensure_default_toolboxes(workspace)?;
        let paths = toolbox_paths(workspace)?;
        if paths.is_empty() {
            let (tools, briefs) = native_catalog_parts();
            return Ok(Self {
                catalog: catalog_from_parts(tools, briefs)?,
                programs: BTreeMap::new(),
            });
        }
        let python = Python312::resolve()?;
        Self::load_with_python(workspace, paths, python)
    }

    fn load_with_python(workspace: &Path, paths: Vec<PathBuf>, python: Python312) -> Result<Self> {
        let host = env::current_exe()?;
        let global_home = config_home()?;
        let mut programs = BTreeMap::new();
        let (mut tools, mut briefs) = native_catalog_parts();
        let mut api_names = BTreeMap::new();
        for tool in &tools {
            api_names.insert(tool.api_name.clone(), tool.full_name.clone());
        }

        for path in paths {
            let toolbox = toolbox_name(&path)?;
            if matches!(
                toolbox.as_str(),
                agent_toolbox::AGENT_TOOLBOX_NAME
                    | agent_toolbox::WORKER_TOOLBOX_NAME
                    | agent_title::TOOLBOX_NAME
                    | workmap::WORKMAP_TOOLBOX_NAME
                    | compact::TOOLBOX_NAME
                    | image_toolbox::TOOLBOX_NAME
            ) {
                return Err(format!(
                    "toolbox namespace {toolbox} is reserved for a native me toolbox"
                )
                .into());
            }
            let client = ToolboxClient::new(ProcessSpec {
                python: python.clone(),
                script: path.clone(),
                workspace: workspace.to_owned(),
                host: host.clone(),
                global_home: global_home.clone(),
            });
            let local_names =
                required_string_array(client.query("getTools", None)?, &toolbox, "getTools")?;
            let brief = required_string(client.query("getBrief", None)?, &toolbox, "getBrief")?;
            briefs.push((toolbox.clone(), brief));

            for local_name in local_names {
                validate_tool_name(&local_name)?;
                let full_name = format!("{toolbox}.{local_name}");
                let api_name = api_safe_name(&full_name);
                if let Some(existing) = api_names.insert(api_name.clone(), full_name.clone()) {
                    return Err(format!(
                        "tool API name collision: {existing} and {full_name} both map to {api_name}"
                    )
                    .into());
                }
                let input_schema = required_schema(
                    client.query("getInputSchema", Some(&local_name))?,
                    &full_name,
                    "getInputSchema",
                )?;
                let output_schema = required_schema(
                    client.query("getOutputSchema", Some(&local_name))?,
                    &full_name,
                    "getOutputSchema",
                )?;
                let instructions = required_string(
                    client.query("getInstructions", Some(&local_name))?,
                    &full_name,
                    "getInstructions",
                )?;
                let route = required_string(
                    client.query("getRoute", Some(&local_name))?,
                    &full_name,
                    "getRoute",
                )?;
                let examples = required_string(
                    client.query("getExamples", Some(&local_name))?,
                    &full_name,
                    "getExamples",
                )?;
                tools.push(ToolboxTool {
                    toolbox: toolbox.clone(),
                    local_name,
                    full_name,
                    api_name,
                    input_schema,
                    output_schema,
                    instructions,
                    route,
                    examples,
                });
            }
            if programs.insert(toolbox.clone(), client).is_some() {
                return Err(format!("duplicate toolbox namespace {toolbox}").into());
            }
        }

        let catalog = catalog_from_parts(tools, briefs)?;
        Ok(Self { catalog, programs })
    }

    pub fn catalog(&self) -> &ToolboxCatalog {
        &self.catalog
    }

    pub fn observer(&self) -> ToolboxObserver {
        ToolboxObserver {
            terminal: self.programs.get("Terminal").cloned(),
            web_browser: self.programs.get("WebBrowser").cloned(),
            programs: self.programs.values().cloned().collect(),
        }
    }

    pub fn reset_sessions(&self) {
        for program in self.programs.values() {
            program.reset();
        }
    }

    pub fn execute(
        &self,
        full_name: &str,
        arguments: &str,
        mut on_update: impl FnMut(ToolboxUpdate) -> Result<()>,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.execute_cancellable(full_name, arguments, &mut on_update, || false)
    }

    pub fn execute_cancellable(
        &self,
        full_name: &str,
        arguments: &str,
        mut on_update: impl FnMut(ToolboxUpdate) -> Result<()>,
        mut should_cancel: impl FnMut() -> bool,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        if full_name == DISABLED_FILE_APPLY_PATCH {
            return Err(ToolboxExecutionError::Tool {
                code: "tool_disabled".into(),
                message: "File.ApplyPatch is disabled. Use File.Edit instead.".into(),
                retryable: false,
                tip: None,
            });
        }
        let tool = self
            .catalog
            .tools
            .iter()
            .find(|tool| tool.full_name == full_name)
            .ok_or_else(|| ToolboxExecutionError::Tool {
                code: "unknown_tool".into(),
                message: format!("tool {full_name} is not loaded"),
                retryable: false,
                tip: None,
            })?;
        let input =
            serde_json::from_str(arguments).map_err(|error| ToolboxExecutionError::Tool {
                code: "invalid_arguments".into(),
                message: error.to_string(),
                retryable: false,
                tip: None,
            })?;
        let program =
            self.programs
                .get(&tool.toolbox)
                .ok_or_else(|| ToolboxExecutionError::Tool {
                    code: "toolbox_unavailable".into(),
                    message: format!("toolbox {} is not running", tool.toolbox),
                    retryable: true,
                    tip: None,
                })?;
        program.execute_cancellable(&tool.local_name, input, &mut on_update, &mut should_cancel)
    }
}

impl Drop for ToolboxRuntime {
    fn drop(&mut self) {
        for program in self.programs.values() {
            program.shutdown();
        }
    }
}

fn native_catalog_parts() -> (Vec<ToolboxTool>, Vec<(String, String)>) {
    // Agent.* is intentionally kept out of the model-facing catalog.  The
    // underlying runtime remains available for the dedicated Worker surface
    // and for replaying existing EDBs, but models cannot create or control
    // general-purpose sub-Agents.
    let mut tools = Vec::new();
    let (title_tools, title_brief) = agent_title::catalog_parts();
    let (workmap_tools, workmap_brief) = workmap::catalog_parts();
    let (compact_tools, compact_brief) = compact::catalog_parts();
    let (image_tools, image_brief) = image_toolbox::catalog_parts(false);
    tools.extend(title_tools);
    tools.extend(workmap_tools);
    tools.extend(compact_tools);
    tools.extend(image_tools);
    (
        tools,
        vec![title_brief, workmap_brief, compact_brief, image_brief],
    )
}

#[derive(Clone)]
pub struct ToolboxObserver {
    terminal: Option<ToolboxClient>,
    web_browser: Option<ToolboxClient>,
    programs: Vec<ToolboxClient>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WebBrowserPagePreview {
    pub page_id: String,
    pub url: String,
    pub title: String,
    pub state: String,
}

#[derive(Deserialize)]
struct WebBrowserPagesPreview {
    pages: Vec<WebBrowserPagePreview>,
}

impl ToolboxObserver {
    pub(crate) fn shutdown(&self) {
        for program in &self.programs {
            program.shutdown();
        }
    }

    pub fn active_count(&self) -> Result<usize> {
        Ok(self.active_terminal_sessions()?.len())
    }

    pub fn active_terminal_sessions(&self) -> Result<Vec<TerminalSessionPreview>> {
        let Some(terminal) = &self.terminal else {
            return Ok(Vec::new());
        };
        let output = match terminal.internal_execute(TERMINAL_OBSERVE_ACTIVE_SESSIONS, json!({})) {
            Ok(output) => output,
            Err(ToolboxExecutionError::Interrupted(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string().into()),
        };
        Ok(serde_json::from_value(output)?)
    }

    pub fn active_web_browser_pages(&self) -> Result<Vec<WebBrowserPagePreview>> {
        let Some(web_browser) = &self.web_browser else {
            return Ok(Vec::new());
        };
        let output = match web_browser.internal_execute(WEB_BROWSER_OBSERVE_ACTIVE_PAGES, json!({}))
        {
            Ok(output) => output,
            Err(error) => return Err(error.to_string().into()),
        };
        Ok(serde_json::from_value::<WebBrowserPagesPreview>(output)?.pages)
    }

    pub fn terminal_frame(&self, session_id: &str) -> Result<Option<TerminalFrame>> {
        let Some(terminal) = &self.terminal else {
            return Ok(None);
        };
        let output = match terminal
            .internal_execute(TERMINAL_OBSERVE_FRAME, json!({"session_id": session_id}))
        {
            Ok(output) => output,
            Err(ToolboxExecutionError::Interrupted(_)) => return Ok(None),
            Err(error) => return Err(error.to_string().into()),
        };
        Ok(serde_json::from_value(output)?)
    }

    pub fn terminal_backend(&self) -> Result<Option<String>> {
        let Some(terminal) = &self.terminal else {
            return Ok(None);
        };
        let output = match terminal.internal_execute(TERMINAL_OBSERVE_BACKEND, json!({})) {
            Ok(output) => output,
            Err(ToolboxExecutionError::Interrupted(_)) => return Ok(None),
            Err(error) => return Err(error.to_string().into()),
        };
        Ok(output.as_str().map(str::to_owned))
    }

    pub fn preview_active_terminal_sessions(&self) -> Result<Vec<TerminalSessionPreview>> {
        let Some(terminal) = &self.terminal else {
            return Ok(Vec::new());
        };
        let output = terminal
            .internal_execute_timeout(
                TERMINAL_OBSERVE_ACTIVE_SESSIONS,
                json!({}),
                TERMINAL_OBSERVER_TIMEOUT,
            )
            .map_err(|error| error.to_string())?;
        Ok(serde_json::from_value(output)?)
    }

    pub fn preview_terminal_frame(&self, session_id: &str) -> Result<Option<TerminalFrame>> {
        let Some(terminal) = &self.terminal else {
            return Ok(None);
        };
        let output = terminal
            .internal_execute_timeout(
                TERMINAL_OBSERVE_FRAME,
                json!({"session_id": session_id}),
                TERMINAL_OBSERVER_TIMEOUT,
            )
            .map_err(|error| error.to_string())?;
        Ok(serde_json::from_value(output)?)
    }

    pub fn preview_terminal_backend(&self) -> Result<Option<String>> {
        let Some(terminal) = &self.terminal else {
            return Ok(None);
        };
        let output = terminal
            .internal_execute_timeout(
                TERMINAL_OBSERVE_BACKEND,
                json!({}),
                TERMINAL_OBSERVER_TIMEOUT,
            )
            .map_err(|error| error.to_string())?;
        Ok(output.as_str().map(str::to_owned))
    }
}

#[derive(Debug)]
pub enum ToolboxUpdate {
    Text { stream: String, content: String },
    Terminal(TerminalLineUpdate),
}

#[derive(Debug)]
pub enum ToolboxExecutionError {
    Interrupted(String),
    Tool {
        code: String,
        message: String,
        retryable: bool,
        tip: Option<String>,
    },
    Protocol(String),
}

impl std::fmt::Display for ToolboxExecutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted(message) => write!(formatter, "toolbox interrupted: {message}"),
            Self::Tool {
                code,
                message,
                retryable,
                tip,
            } => write!(
                formatter,
                "tool error {code}: {message} (retryable={retryable}){}",
                tip.as_ref()
                    .map(|tip| format!("; tip: {tip}"))
                    .unwrap_or_default()
            ),
            Self::Protocol(message) => write!(formatter, "toolbox protocol error: {message}"),
        }
    }
}

impl std::error::Error for ToolboxExecutionError {}

#[derive(Clone)]
struct ToolboxClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    spec: ProcessSpec,
    transport: Mutex<Option<Arc<ProcessTransport>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

#[derive(Clone)]
struct ProcessSpec {
    python: Python312,
    script: PathBuf,
    workspace: PathBuf,
    host: PathBuf,
    global_home: PathBuf,
}

struct ProcessTransport {
    writer: Mutex<ChildStdin>,
    transactions: Arc<Mutex<TransactionRegistry>>,
    child: Arc<Mutex<Child>>,
    process_tree: Arc<ProcessTree>,
    alive: Arc<AtomicBool>,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Default)]
struct TransactionRegistry {
    pending: HashMap<u64, Sender<Incoming>>,
    abandoned: HashSet<u64>,
}

enum Incoming {
    Frame(ResponseFrame),
    Interrupted(String),
}

#[derive(Debug, Deserialize)]
struct ResponseFrame {
    id: u64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    output: Value,
    #[serde(default)]
    error: Option<ToolboxErrorFrame>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ToolboxErrorFrame {
    code: String,
    message: String,
    #[serde(default)]
    retryable: bool,
    #[serde(default)]
    tip: Option<String>,
}

impl ToolboxClient {
    fn new(spec: ProcessSpec) -> Self {
        Self {
            inner: Arc::new(ClientInner {
                spec,
                transport: Mutex::new(None),
                next_id: AtomicU64::new(1),
                closed: AtomicBool::new(false),
            }),
        }
    }

    fn query(&self, cmd: &str, tool: Option<&str>) -> Result<Value> {
        let mut request = Map::from_iter([("cmd".into(), Value::String(cmd.into()))]);
        if let Some(tool) = tool {
            request.insert("tool".into(), Value::String(tool.into()));
        }
        self.call(Value::Object(request), &mut |_| Ok(()), &mut || false)
            .map_err(|error| error.to_string().into())
    }

    fn execute(
        &self,
        tool: &str,
        input: Value,
        on_update: &mut dyn FnMut(ToolboxUpdate) -> Result<()>,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.execute_cancellable(tool, input, on_update, &mut || false)
    }

    fn execute_cancellable(
        &self,
        tool: &str,
        input: Value,
        on_update: &mut dyn FnMut(ToolboxUpdate) -> Result<()>,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.call(
            json!({"cmd": "execute", "tool": tool, "input": input}),
            on_update,
            should_cancel,
        )
    }

    fn internal_execute(
        &self,
        tool: &str,
        input: Value,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.execute(tool, input, &mut |_| Ok(()))
    }

    fn internal_execute_timeout(
        &self,
        tool: &str,
        input: Value,
        timeout: Duration,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.call_with_timeout(
            json!({"cmd": "execute", "tool": tool, "input": input}),
            &mut |_| Ok(()),
            &mut || false,
            Some(timeout),
        )
    }

    fn call(
        &self,
        request: Value,
        on_update: &mut dyn FnMut(ToolboxUpdate) -> Result<()>,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.call_with_timeout(request, on_update, should_cancel, None)
    }

    fn call_with_timeout(
        &self,
        mut request: Value,
        on_update: &mut dyn FnMut(ToolboxUpdate) -> Result<()>,
        should_cancel: &mut dyn FnMut() -> bool,
        timeout: Option<Duration>,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        request
            .as_object_mut()
            .ok_or_else(|| ToolboxExecutionError::Protocol("request is not an object".into()))?
            .insert("id".into(), Value::from(id));
        let transport = self.transport()?;
        let (sender, receiver) = mpsc::channel();
        transport
            .transactions
            .lock()
            .map_err(|_| ToolboxExecutionError::Protocol("transaction lock is poisoned".into()))?
            .pending
            .insert(id, sender);
        let mut encoded = serde_json::to_vec(&request)
            .map_err(|error| ToolboxExecutionError::Protocol(error.to_string()))?;
        encoded.push(b'\n');
        let write_result = transport
            .writer
            .lock()
            .map_err(|_| ToolboxExecutionError::Protocol("writer lock is poisoned".into()))
            .and_then(|mut writer| {
                writer
                    .write_all(&encoded)
                    .and_then(|_| writer.flush())
                    .map_err(|error| ToolboxExecutionError::Interrupted(error.to_string()))
            });
        if let Err(error) = write_result {
            transport.remove_pending(id);
            transport.fail(&error.to_string());
            return Err(error);
        }
        match receive_response(id, receiver, on_update, should_cancel, &transport, timeout) {
            Err(ToolboxExecutionError::Protocol(message)) => {
                transport.fail(&message);
                Err(ToolboxExecutionError::Interrupted(message))
            }
            response => response,
        }
    }

    fn transport(&self) -> std::result::Result<Arc<ProcessTransport>, ToolboxExecutionError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(ToolboxExecutionError::Interrupted(
                "toolbox runtime closed".into(),
            ));
        }
        let mut current =
            self.inner.transport.lock().map_err(|_| {
                ToolboxExecutionError::Protocol("transport lock is poisoned".into())
            })?;
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(ToolboxExecutionError::Interrupted(
                "toolbox runtime closed".into(),
            ));
        }
        if let Some(transport) = current.as_ref()
            && transport.alive.load(Ordering::Acquire)
        {
            return Ok(Arc::clone(transport));
        }
        let transport = Arc::new(ProcessTransport::spawn(&self.inner.spec)?);
        *current = Some(Arc::clone(&transport));
        Ok(transport)
    }

    fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let transport = self
            .inner
            .transport
            .lock()
            .ok()
            .and_then(|mut transport| transport.take());
        if let Some(transport) = transport {
            transport.fail("toolbox runtime closed");
        }
    }

    fn reset(&self) {
        let transport = match self.inner.transport.lock() {
            Ok(mut transport) => transport.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(transport) = transport {
            transport.fail("toolbox sessions cleared with the Agent context");
        }
    }
}

impl ProcessTransport {
    fn spawn(spec: &ProcessSpec) -> std::result::Result<Self, ToolboxExecutionError> {
        let mut command = spec.python.command();
        let path = spec
            .python
            .augmented_path()
            .map_err(|error| ToolboxExecutionError::Interrupted(error.to_string()))?;
        command.env("PATH", path);
        command
            .arg(&spec.script)
            .current_dir(&spec.workspace)
            .env("ME_TOOLBOX_HOST", &spec.host)
            .env("ME_CONFIG_HOME", &spec.global_home)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn().map_err(|error| {
            ToolboxExecutionError::Interrupted(format!(
                "failed to start toolbox {} with Python 3.12: {error}",
                spec.script.display()
            ))
        })?;
        let process_tree = ProcessTree::attach(&mut child).map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            ToolboxExecutionError::Interrupted(format!(
                "failed to contain toolbox process tree for {}: {error}",
                spec.script.display()
            ))
        })?;
        let writer = child.stdin.take().ok_or_else(|| {
            ToolboxExecutionError::Interrupted("toolbox stdin is unavailable".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ToolboxExecutionError::Interrupted("toolbox stdout is unavailable".into())
        })?;
        let stderr_reader = child.stderr.take().ok_or_else(|| {
            ToolboxExecutionError::Interrupted("toolbox stderr is unavailable".into())
        })?;
        let transactions = Arc::new(Mutex::new(TransactionRegistry::default()));
        let alive = Arc::new(AtomicBool::new(true));
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let child = Arc::new(Mutex::new(child));
        let process_tree = Arc::new(process_tree);
        spawn_stdout_reader(
            spec.script.clone(),
            stdout,
            Arc::clone(&transactions),
            Arc::clone(&alive),
            Arc::clone(&stderr),
            Arc::clone(&child),
            Arc::clone(&process_tree),
        );
        spawn_stderr_reader(stderr_reader, Arc::clone(&stderr));
        Ok(Self {
            writer: Mutex::new(writer),
            transactions,
            child,
            process_tree,
            alive,
            stderr,
        })
    }

    fn remove_pending(&self, id: u64) {
        if let Ok(mut transactions) = self.transactions.lock() {
            transactions.pending.remove(&id);
        }
    }

    fn abandon(&self, id: u64) {
        if let Ok(mut transactions) = self.transactions.lock() {
            transactions.pending.remove(&id);
            transactions.abandoned.insert(id);
        }
    }

    fn fail(&self, message: &str) {
        if self.alive.swap(false, Ordering::AcqRel) {
            let stderr = stderr_suffix(&self.stderr);
            let message = if stderr.is_empty() {
                message.to_owned()
            } else {
                format!("{message}; stderr: {stderr}")
            };
            if let Ok(mut transactions) = self.transactions.lock() {
                for (_, sender) in transactions.pending.drain() {
                    let _ = sender.send(Incoming::Interrupted(message.clone()));
                }
                transactions.abandoned.clear();
            }
        }
        stop_child(&self.child, &self.process_tree);
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        self.fail("toolbox runtime closed");
    }
}

fn spawn_stdout_reader(
    script: PathBuf,
    stdout: impl Read + Send + 'static,
    transactions: Arc<Mutex<TransactionRegistry>>,
    alive: Arc<AtomicBool>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    child: Arc<Mutex<Child>>,
    process_tree: Arc<ProcessTree>,
) {
    thread::spawn(move || {
        let mut failure = None;
        for line in BufReader::new(stdout).lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    failure = Some(format!("failed to read toolbox stdout: {error}"));
                    break;
                }
            };
            let frame: ResponseFrame = match serde_json::from_str(&line) {
                Ok(frame) => frame,
                Err(error) => {
                    failure = Some(format!(
                        "toolbox {} wrote invalid JSONL: {error}",
                        script.display()
                    ));
                    break;
                }
            };
            let terminal = frame.kind != "update";
            let dispatch = transactions.lock().ok().and_then(|mut transactions| {
                if transactions.abandoned.contains(&frame.id) {
                    if terminal {
                        transactions.abandoned.remove(&frame.id);
                    }
                    return Some(None);
                }
                let sender = if terminal {
                    transactions.pending.remove(&frame.id)
                } else {
                    transactions.pending.get(&frame.id).cloned()
                };
                sender.map(Some)
            });
            let Some(dispatch) = dispatch else {
                failure = Some(format!(
                    "toolbox {} responded with unknown or closed id {}",
                    script.display(),
                    frame.id
                ));
                break;
            };
            let Some(sender) = dispatch else {
                continue;
            };
            if sender.send(Incoming::Frame(frame)).is_err() {
                failure = Some(format!(
                    "toolbox {} response receiver disappeared",
                    script.display()
                ));
                break;
            }
        }
        if alive.swap(false, Ordering::AcqRel) {
            let stderr = stderr_suffix(&stderr);
            let mut message =
                failure.unwrap_or_else(|| format!("toolbox {} exited", script.display()));
            if !stderr.is_empty() {
                message.push_str("; stderr: ");
                message.push_str(&stderr);
            }
            if let Ok(mut transactions) = transactions.lock() {
                for (_, sender) in transactions.pending.drain() {
                    let _ = sender.send(Incoming::Interrupted(message.clone()));
                }
                transactions.abandoned.clear();
            }
        }
        stop_child(&child, &process_tree);
    });
}

fn stop_child(child: &Mutex<Child>, process_tree: &ProcessTree) {
    process_tree.terminate();
    if let Ok(mut child) = child.lock()
        && child.try_wait().ok().flatten().is_none()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(windows)]
fn configure_process_tree(_command: &mut Command) {}

struct ProcessTree {
    #[cfg(unix)]
    process_group: i32,
    #[cfg(windows)]
    job: isize,
}

impl ProcessTree {
    #[cfg(unix)]
    fn attach(child: &mut Child) -> std::io::Result<Self> {
        Ok(Self {
            process_group: i32::try_from(child.id()).map_err(|_| {
                std::io::Error::other("toolbox process id does not fit in a process group id")
            })?,
        })
    }

    #[cfg(windows)]
    fn attach(child: &mut Child) -> std::io::Result<Self> {
        use std::{mem::size_of, os::windows::io::AsRawHandle, ptr};
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(information).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }
        let assigned = unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as _) };
        if assigned == 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }
        Ok(Self { job: job as isize })
    }

    #[cfg(unix)]
    fn terminate(&self) {
        unsafe {
            libc::kill(-self.process_group, libc::SIGKILL);
        }
    }

    #[cfg(windows)]
    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        unsafe {
            TerminateJobObject(self.job as _, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        unsafe {
            CloseHandle(self.job as _);
        }
    }
}

fn spawn_stderr_reader(stderr: impl Read + Send + 'static, history: Arc<Mutex<VecDeque<String>>>) {
    thread::spawn(move || {
        for line in BufReader::new(stderr)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Ok(mut history) = history.lock() {
                history.push_back(line);
                while history.len() > STDERR_HISTORY_LINES {
                    history.pop_front();
                }
            }
        }
    });
}

fn stderr_suffix(history: &Mutex<VecDeque<String>>) -> String {
    history
        .lock()
        .map(|history| history.iter().cloned().collect::<Vec<_>>().join(" | "))
        .unwrap_or_default()
}

fn receive_response(
    id: u64,
    receiver: Receiver<Incoming>,
    on_update: &mut dyn FnMut(ToolboxUpdate) -> Result<()>,
    should_cancel: &mut dyn FnMut() -> bool,
    transport: &ProcessTransport,
    timeout: Option<Duration>,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let started = Instant::now();
    loop {
        if should_cancel() {
            transport.remove_pending(id);
            transport.fail("toolbox request cancelled");
            return Err(ToolboxExecutionError::Interrupted(
                "toolbox request cancelled".into(),
            ));
        }
        let wait = match timeout {
            Some(timeout) => {
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    transport.abandon(id);
                    return Err(ToolboxExecutionError::Interrupted(format!(
                        "toolbox observation timed out after {} ms",
                        timeout.as_millis()
                    )));
                }
                remaining.min(Duration::from_millis(50))
            }
            None => Duration::from_millis(50),
        };
        match receiver.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => continue,
            Ok(Incoming::Interrupted(message)) => {
                return Err(ToolboxExecutionError::Interrupted(message));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ToolboxExecutionError::Interrupted(format!(
                    "toolbox response channel for id {id} closed"
                )));
            }
            Ok(Incoming::Frame(frame)) => match frame.kind.as_str() {
                "update" => {
                    let update = parse_update(frame.output)?;
                    on_update(update)
                        .map_err(|error| ToolboxExecutionError::Protocol(error.to_string()))?;
                }
                "result" => return Ok(frame.output),
                "error" => {
                    let error = frame.error.ok_or_else(|| {
                        ToolboxExecutionError::Protocol(format!(
                            "toolbox error response {id} has no error object"
                        ))
                    })?;
                    return Err(ToolboxExecutionError::Tool {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                        tip: error.tip,
                    });
                }
                kind => {
                    return Err(ToolboxExecutionError::Protocol(format!(
                        "toolbox response {id} has unsupported type {kind:?}"
                    )));
                }
            },
        }
    }
}

fn parse_update(output: Value) -> std::result::Result<ToolboxUpdate, ToolboxExecutionError> {
    let stream = output
        .get("stream")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolboxExecutionError::Protocol("update has no stream".into()))?;
    let content = output
        .get("content")
        .cloned()
        .ok_or_else(|| ToolboxExecutionError::Protocol("update has no content".into()))?;
    if stream == "terminal" {
        let update: TerminalLineUpdate = serde_json::from_value(content)
            .map_err(|error| ToolboxExecutionError::Protocol(error.to_string()))?;
        update.validate().map_err(ToolboxExecutionError::Protocol)?;
        Ok(ToolboxUpdate::Terminal(update))
    } else {
        let content = content.as_str().ok_or_else(|| {
            ToolboxExecutionError::Protocol("non-terminal update content is not a string".into())
        })?;
        Ok(ToolboxUpdate::Text {
            stream: stream.into(),
            content: content.into(),
        })
    }
}

#[derive(Clone)]
struct Python312 {
    program: PathBuf,
    path_directory: PathBuf,
}

impl Python312 {
    fn resolve() -> Result<Self> {
        Self::resolve_at(&config_home()?)
    }

    fn resolve_at(config_home: &Path) -> Result<Self> {
        let embedded = python_runtime::ensure(config_home)?;
        let candidate = Self {
            program: embedded.executable,
            path_directory: embedded.path_directory,
        };
        if !candidate.is_python312() {
            return Err("the Python 3.12 runtime embedded in me is unusable".into());
        }
        Ok(candidate)
    }

    fn command(&self) -> Command {
        Command::new(&self.program)
    }

    fn is_python312(&self) -> bool {
        self.command()
            .args([
                OsStr::new("-c"),
                OsStr::new(
                    "import sys; raise SystemExit(0 if sys.version_info[:2] == (3, 12) else 1)",
                ),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn augmented_path(&self) -> Result<OsString> {
        python_runtime::prepend_path(&self.path_directory)
    }
}

pub fn ensure_default_toolboxes(workspace: &Path) -> Result<PathBuf> {
    create_private_directory(&workspace.join(WORKSPACE_TEMP_DIRECTORY))?;
    let directory = workspace.join(TOOLBOX_DIRECTORY);
    create_private_directory(&directory)?;
    if toolbox_paths(workspace)?.is_empty() {
        let terminal = directory.join(DEFAULT_TERMINAL_FILE);
        let file = directory.join(DEFAULT_FILE_FILE);
        let web_browser = directory.join(DEFAULT_WEB_BROWSER_FILE);
        if let Err(error) = fs::write(&terminal, DEFAULT_TERMINAL_SOURCE)
            .and_then(|()| fs::write(&file, DEFAULT_FILE_SOURCE))
            .and_then(|()| fs::write(&web_browser, DEFAULT_WEB_BROWSER_SOURCE))
            .and_then(|()| ensure_web_browser_platform_assets(&directory))
        {
            let _ = fs::remove_file(&terminal);
            let _ = fs::remove_file(&file);
            let _ = fs::remove_file(&web_browser);
            let _ = remove_web_browser_platform_assets(&directory);
            return Err(error.into());
        }
        return Ok(terminal);
    }
    for (name, source) in [
        (DEFAULT_TERMINAL_FILE, DEFAULT_TERMINAL_SOURCE),
        (DEFAULT_FILE_FILE, DEFAULT_FILE_SOURCE),
        (DEFAULT_WEB_BROWSER_FILE, DEFAULT_WEB_BROWSER_SOURCE),
    ] {
        let path = directory.join(name);
        if !path.is_file() {
            continue;
        }
        let existing = fs::read_to_string(&path)?;
        let managed = is_managed_default_toolbox(&existing);
        if managed && existing != source {
            fs::write(path, source)?;
        }
    }
    ensure_web_browser_platform_assets(&directory)?;
    Ok(directory.join(DEFAULT_TERMINAL_FILE))
}

fn is_managed_default_toolbox(source: &str) -> bool {
    source
        .lines()
        .take(3)
        .any(|line| matches!(line, "# ME-S-MANAGED-TOOLBOX" | "# ME-RUST-MANAGED-TOOLBOX"))
        || source.starts_with("#!/usr/bin/env python3\n\"\"\"ME-S default ")
        || source.starts_with("#!/usr/bin/env python3\n\"\"\"ME-RUST default ")
}

#[cfg(target_os = "macos")]
fn ensure_web_browser_platform_assets(directory: &Path) -> std::io::Result<()> {
    let path = directory.join(DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL_FILE);
    let web_browser = directory.join(DEFAULT_WEB_BROWSER_FILE);
    let managed =
        fs::read_to_string(web_browser).is_ok_and(|source| is_managed_default_toolbox(&source));
    if !managed {
        return remove_web_browser_platform_assets(directory);
    }
    if fs::read(&path).is_ok_and(|content| content == DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL) {
        return Ok(());
    }
    fs::write(path, DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL)
}

#[cfg(target_os = "macos")]
fn remove_web_browser_platform_assets(directory: &Path) -> std::io::Result<()> {
    let path = directory.join(DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL_FILE);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_web_browser_platform_assets(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn remove_web_browser_platform_assets(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

fn toolbox_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    let directory = workspace.join(TOOLBOX_DIRECTORY);
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_file())
                .and_then(|_| (path.extension() == Some(OsStr::new("py"))).then_some(path))
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn toolbox_name(path: &Path) -> Result<String> {
    let name = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("toolbox {} has no UTF-8 file stem", path.display()))?
        .to_owned();
    validate_tool_name(&name)?;
    Ok(name)
}

fn validate_tool_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err("tool name cannot be empty".into());
    };
    if !first.is_ascii_alphabetic()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "tool name {name:?} must start with an ASCII letter and contain only ASCII letters, digits, or underscore"
        )
        .into());
    }
    Ok(())
}

pub fn api_safe_name(full_name: &str) -> String {
    full_name.replace('.', "_")
}

fn required_string(value: Value, owner: &str, cmd: &str) -> Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{owner} {cmd} did not return a string").into())
}

fn required_string_array(value: Value, owner: &str, cmd: &str) -> Result<Vec<String>> {
    let values = value
        .as_array()
        .ok_or_else(|| format!("{owner} {cmd} did not return an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{owner} {cmd} returned a non-string tool name").into())
        })
        .collect()
}

fn required_schema(value: Value, owner: &str, cmd: &str) -> Result<Value> {
    if !value.is_object() {
        return Err(format!("{owner} {cmd} did not return a JSON object").into());
    }
    Ok(value)
}

fn catalog_from_parts(
    mut tools: Vec<ToolboxTool>,
    briefs: Vec<(String, String)>,
) -> Result<ToolboxCatalog> {
    tools.sort_by(|left, right| left.full_name.cmp(&right.full_name));
    let mut api_to_full = BTreeMap::new();
    for tool in &tools {
        if api_to_full
            .insert(tool.api_name.clone(), tool.full_name.clone())
            .is_some()
        {
            return Err(format!("duplicate tool API name {}", tool.api_name).into());
        }
    }
    let prompt = render_catalog_prompt(&tools, &briefs, None)?;
    Ok(ToolboxCatalog {
        tools,
        briefs,
        prompt,
        api_to_full,
    })
}

fn render_catalog_prompt(
    tools: &[ToolboxTool],
    briefs: &[(String, String)],
    excluded_toolbox: Option<&str>,
) -> Result<String> {
    let mut sections = vec![r#"# Tool result envelope

Every tool response uses this runtime envelope:

```json
{
  "result": {
    "state": "succeeded",
    "exit_code": null,
    "detail": {}
  },
  "truncate": false
}
```

`result.state` reports `succeeded`, `failed`, `interrupted`, or `indeterminate`. The `Result detail schema` shown under each tool describes only `result.detail`, including its actual JSON type; it does not describe this outer runtime envelope. `detail` is omitted when a tool has no detail. Streaming or structured activity may additionally appear in top-level `terminal_updates`, `updates`, or `other_updates` arrays.

Every envelope has a top-level `truncate` boolean. `truncate:false` means the complete tool result is present. `truncate:true` means ME-S safely reduced only the tool's potentially large content before adding it to model context; read `truncate_info` for the retained and omitted original ranges. Existing tool-specific `truncated` fields have their documented collection-time meaning and are independent of this envelope.

Safe truncation never cuts serialized JSON or leaves dangling references. Ordered logs and result lists omit their oldest complete items. `File.Read` keeps its first and last numbered line entries; missing numeric keys are omitted source lines, while an oversized individual line may use `text_fragments`. `File.Search` keeps each match object coherent: it removes complete numbered context-line entries from `before` and `after` before representing an oversized `match_text` value as `text_fragments`. Other documents and long text retain exact beginning and ending fragments. When a normal string cannot remain contiguous, it is represented as a `text_fragments` object whose fragments carry exact original byte offsets; never treat separated fragments as adjacent original text. A cropped browser accessibility tree uses `aria_fragments`, whose fragments carry exact source line ranges and remain separated by omitted source ranges."#.into()];
    for (toolbox, brief) in briefs {
        if excluded_toolbox == Some(toolbox.as_str()) {
            continue;
        }
        let mut section = format!("# Toolbox {toolbox}\n\n{}", brief.trim());
        for tool in tools.iter().filter(|tool| tool.toolbox == toolbox.as_str()) {
            section.push_str(&format!(
                "\n\n## {}\n\nRoute:\n{}\n\nInstructions:\n{}\n\nResult detail schema:\n```json\n{}\n```\n\nExamples:\n{}",
                tool.full_name,
                tool.route.trim(),
                tool.instructions.trim(),
                serde_json::to_string_pretty(&tool.output_schema)?,
                tool.examples.trim(),
            ));
        }
        sections.push(section);
    }
    Ok(sections.join("\n\n"))
}

#[derive(Deserialize)]
struct WorkerRequest {
    id: u64,
    cmd: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    input: Value,
}

pub fn run_default_terminal_toolbox(
    input: impl Read,
    output: impl Write + Send + 'static,
    workspace: &Path,
) -> Result<()> {
    let state = Arc::new(Mutex::new(DefaultTerminalState::new()));
    let observer = state
        .lock()
        .map_err(|_| "Terminal toolbox state lock is poisoned")?
        .manager
        .observer();
    let output = Arc::new(Mutex::new(output));
    let (execute_sender, execute_receiver) = mpsc::channel::<WorkerRequest>();
    let execute_state = Arc::clone(&state);
    let execute_observer = observer.clone();
    let execute_workspace = workspace.to_owned();
    let execute_output = Arc::clone(&output);
    let executor = thread::spawn(move || {
        for request in execute_receiver {
            handle_terminal_execute(
                request,
                Arc::clone(&execute_state),
                execute_observer.clone(),
                &execute_workspace,
                Arc::clone(&execute_output),
            );
        }
    });
    let (observer_sender, observer_receiver) = mpsc::channel::<WorkerRequest>();
    let observer_state = Arc::clone(&state);
    let observer_terminal = observer.clone();
    let observer_workspace = workspace.to_owned();
    let observer_output = Arc::clone(&output);
    let observer_executor = thread::spawn(move || {
        for request in observer_receiver {
            handle_terminal_execute(
                request,
                Arc::clone(&observer_state),
                observer_terminal.clone(),
                &observer_workspace,
                Arc::clone(&observer_output),
            );
        }
    });
    for line in BufReader::new(input).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: WorkerRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                return Err(format!("Terminal toolbox received invalid JSONL: {error}").into());
            }
        };
        if request.cmd == "execute" {
            if request
                .tool
                .as_deref()
                .is_some_and(|tool| tool.starts_with("__"))
            {
                observer_sender
                    .send(request)
                    .map_err(|_| "Terminal toolbox observer stopped")?;
            } else {
                execute_sender
                    .send(request)
                    .map_err(|_| "Terminal toolbox executor stopped")?;
            }
        } else {
            let response = terminal_metadata_response(&request);
            write_worker_frame(&output, response)?;
        }
    }
    drop(execute_sender);
    drop(observer_sender);
    executor
        .join()
        .map_err(|_| "Terminal toolbox executor panicked")?;
    observer_executor
        .join()
        .map_err(|_| "Terminal toolbox observer panicked")?;
    Ok(())
}

struct DefaultTerminalState {
    manager: TerminalManager,
    sessions: BTreeMap<String, TerminalStatus>,
}

impl DefaultTerminalState {
    fn new() -> Self {
        Self {
            manager: TerminalManager::new(),
            sessions: BTreeMap::new(),
        }
    }

    fn remember_created(&mut self, created: &terminal::TerminalCreated) {
        self.sessions.insert(
            created.session_id.clone(),
            TerminalStatus {
                session_id: created.session_id.clone(),
                state: created.state.clone(),
                shell: created.shell.clone(),
                width: created.width,
                height: created.height,
                cwd: created.cwd.clone(),
                exit_code: None,
            },
        );
    }

    fn remember_status(&mut self, status: &TerminalStatus) {
        self.sessions
            .insert(status.session_id.clone(), status.clone());
    }
}

fn terminal_metadata_response(request: &WorkerRequest) -> Value {
    let result = match request.cmd.as_str() {
        "getTools" => Ok(
            if terminal::shell_backend() == terminal::UNAVAILABLE_BACKEND {
                json!([])
            } else {
                json!(terminal_local_names())
            },
        ),
        "getBrief" => Ok(Value::String(terminal::tool_prompt(
            &terminal::shell_backend(),
        ))),
        "getInputSchema" => metadata_tool(request).and_then(terminal_input_schema),
        "getOutputSchema" => metadata_tool(request).and_then(terminal_output_schema),
        "getInstructions" => metadata_tool(request)
            .and_then(terminal_instructions)
            .map(|value| Value::String(value.into())),
        "getRoute" => metadata_tool(request)
            .and_then(terminal_route)
            .map(|value| Value::String(value.into())),
        "getExamples" => metadata_tool(request)
            .and_then(terminal_examples)
            .map(|value| Value::String(value.into())),
        _ => Err(format!("unknown toolbox command {}", request.cmd)),
    };
    match result {
        Ok(output) => json!({"id": request.id, "type": "result", "output": output}),
        Err(message) => worker_error(request.id, "invalid_request", message, false),
    }
}

fn metadata_tool(request: &WorkerRequest) -> std::result::Result<&str, String> {
    request
        .tool
        .as_deref()
        .ok_or_else(|| format!("{} requires tool", request.cmd))
}

fn handle_terminal_execute(
    request: WorkerRequest,
    state: Arc<Mutex<DefaultTerminalState>>,
    observer: terminal::TerminalObserver,
    workspace: &Path,
    output: Arc<Mutex<impl Write>>,
) {
    let Some(tool) = request.tool.as_deref() else {
        let _ = write_worker_frame(
            &output,
            worker_error(
                request.id,
                "invalid_request",
                "execute requires tool",
                false,
            ),
        );
        return;
    };
    let result = match tool {
        TERMINAL_OBSERVE_ACTIVE_SESSIONS => observer
            .active_sessions()
            .and_then(|sessions| Ok(serde_json::to_value(sessions)?))
            .map_err(worker_execution_error),
        TERMINAL_OBSERVE_FRAME => request
            .input
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ("invalid_arguments", "missing session_id".to_owned(), false))
            .and_then(|session_id| {
                observer
                    .frame(session_id)
                    .and_then(|frame| Ok(serde_json::to_value(frame)?))
                    .map_err(worker_execution_error)
            }),
        TERMINAL_OBSERVE_BACKEND => Ok(Value::String(terminal::shell_backend())),
        _ => {
            let full_name = format!("Terminal.{tool}");
            terminal::parse_request(&full_name, &request.input.to_string())
                .map_err(|error| ("invalid_arguments", error.to_string(), false))
                .and_then(|terminal_request| {
                    execute_terminal_request(
                        request.id,
                        terminal_request,
                        &state,
                        workspace,
                        &output,
                    )
                })
        }
    };
    let frame = match result {
        Ok(value) => json!({"id": request.id, "type": "result", "output": value}),
        Err((code, message, retryable)) => worker_error(request.id, code, message, retryable),
    };
    let _ = write_worker_frame(&output, frame);
}

fn execute_terminal_request(
    id: u64,
    request: TerminalRequest,
    state: &Mutex<DefaultTerminalState>,
    workspace: &Path,
    output: &Mutex<impl Write>,
) -> std::result::Result<Value, (&'static str, String, bool)> {
    let mut state = state.lock().map_err(|_| {
        (
            "runtime_error",
            "Terminal state lock is poisoned".into(),
            false,
        )
    })?;
    match request {
        TerminalRequest::Create(request) => {
            let outcome = state
                .manager
                .create(workspace, id, &request)
                .map_err(worker_execution_error)?;
            state.remember_created(&outcome.created);
            write_terminal_update(id, &outcome.update, output)?;
            if let Some(end) = &outcome.end
                && let Some(status) = state.sessions.get_mut(&outcome.created.session_id)
            {
                status.state = end.state.to_string();
                status.exit_code = end.exit_code;
            }
            Ok(json!({
                "session_id": outcome.created.session_id,
                "state": outcome.update.state,
                "shell": outcome.created.shell,
                "cwd": outcome.created.cwd,
                "width": outcome.created.width,
                "height": outcome.created.height,
            }))
        }
        TerminalRequest::Interact(request) => {
            let outcome = state.manager.interact(&request).map_err(|error| {
                let message = error.to_string();
                let code = if message.contains("is not live") {
                    "session_not_found"
                } else {
                    "execution_error"
                };
                (code, message, false)
            })?;
            write_terminal_update(id, &outcome.update, output)?;
            if let Some(end) = &outcome.end
                && let Some(status) = state.sessions.get_mut(&request.session_id)
            {
                status.state = end.state.to_string();
                status.exit_code = end.exit_code;
            }
            Ok(outcome.update.metadata())
        }
        TerminalRequest::Status(request) => {
            if state.manager.contains(&request.session_id) {
                let outcome = state
                    .manager
                    .status(&request.session_id)
                    .map_err(worker_execution_error)?;
                state.remember_status(&outcome.status);
                Ok(serde_json::to_value(outcome.status).map_err(worker_execution_error)?)
            } else if let Some(status) = state.sessions.get(&request.session_id) {
                Ok(serde_json::to_value(status).map_err(worker_execution_error)?)
            } else {
                Err((
                    "session_not_found",
                    format!(
                        "Terminal session {} does not exist in the current tool runtime. Create a new session if needed.",
                        request.session_id
                    ),
                    false,
                ))
            }
        }
        TerminalRequest::List => Ok(json!({
            "sessions": state.sessions.values().collect::<Vec<_>>()
        })),
        TerminalRequest::Kill(request) => {
            if state.manager.contains(&request.session_id) {
                let outcome = state
                    .manager
                    .kill(&request)
                    .map_err(worker_execution_error)?;
                state.remember_status(&outcome.status);
                Ok(serde_json::to_value(outcome.status).map_err(worker_execution_error)?)
            } else if state.sessions.contains_key(&request.session_id) {
                Err((
                    "session_not_found",
                    format!(
                        "Terminal session {} is not active in the current tool runtime",
                        request.session_id
                    ),
                    false,
                ))
            } else {
                Err((
                    "session_not_found",
                    format!(
                        "Terminal session {} does not exist in the current tool runtime",
                        request.session_id
                    ),
                    false,
                ))
            }
        }
    }
}

fn write_terminal_update(
    id: u64,
    update: &TerminalLineUpdate,
    output: &Mutex<impl Write>,
) -> std::result::Result<(), (&'static str, String, bool)> {
    write_worker_frame(
        output,
        json!({
            "id": id,
            "type": "update",
            "output": {
                "stream": "terminal",
                "content": update,
            }
        }),
    )
    .map_err(worker_execution_error)
}

fn worker_execution_error(error: impl std::fmt::Display) -> (&'static str, String, bool) {
    ("execution_error", error.to_string(), false)
}

fn worker_error(
    id: u64,
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
) -> Value {
    json!({
        "id": id,
        "type": "error",
        "error": {
            "code": code.into(),
            "message": message.into(),
            "retryable": retryable,
        }
    })
}

fn write_worker_frame(output: &Mutex<impl Write>, frame: Value) -> Result<()> {
    let mut output = output
        .lock()
        .map_err(|_| "toolbox output lock is poisoned")?;
    serde_json::to_writer(&mut *output, &frame)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn terminal_local_names() -> Vec<&'static str> {
    vec!["Create", "Interact", "Status", "List", "Kill"]
}

fn terminal_input_schema(tool: &str) -> std::result::Result<Value, String> {
    let api_name = match tool {
        "Create" => terminal::API_CREATE,
        "Interact" => terminal::API_INTERACT,
        "Status" => terminal::API_STATUS,
        "List" => terminal::API_LIST,
        "Kill" => terminal::API_KILL,
        _ => return Err(format!("unknown Terminal tool {tool}")),
    };
    terminal::tool_definitions()
        .into_iter()
        .find(|definition| definition["function"]["name"] == api_name)
        .and_then(|definition| definition.pointer("/function/parameters").cloned())
        .ok_or_else(|| format!("Terminal tool {tool} has no input schema"))
}

fn terminal_output_schema(tool: &str) -> std::result::Result<Value, String> {
    match tool {
        "Create" => Ok(json!({
            "type": "object",
            "required": ["session_id", "state", "shell", "cwd", "width", "height"],
            "properties": {
                "session_id": {"type": "string"},
                "state": {"type": "string"},
                "shell": {"type": "string"},
                "cwd": {"type": "string"},
                "width": {"type": "integer"},
                "height": {"type": "integer"}
            },
            "additionalProperties": false
        })),
        "Interact" => Ok(json!({
            "type": "object",
            "required": ["session_id", "sequence", "size", "viewport", "changed_rows", "state", "exit_code", "truncated"],
            "additionalProperties": true
        })),
        "Status" | "Kill" => Ok(json!({
            "type": "object",
            "required": ["session_id", "state", "shell", "width", "height", "cwd", "exit_code"],
            "additionalProperties": false
        })),
        "List" => Ok(json!({
            "type": "object",
            "required": ["sessions"],
            "properties": {"sessions": {"type": "array"}},
            "additionalProperties": false
        })),
        _ => Err(format!("unknown Terminal tool {tool}")),
    }
}

fn terminal_route(tool: &str) -> std::result::Result<&'static str, String> {
    match tool {
        "Create" => Ok(
            "Use to create a new persistent PTY when no suitable live session exists. Do not create a replacement merely because a command is quiet.",
        ),
        "Interact" => Ok(
            "Use to send ordered text/key input to an existing PTY or poll it with an empty input list. Do not use a session_id after the tool reports that it does not exist.",
        ),
        "Status" => Ok(
            "Use to inspect the process state and metadata of one known Terminal session without reading new terminal output.",
        ),
        "List" => Ok("Use to enumerate Terminal sessions known to the current toolbox runtime."),
        "Kill" => Ok("Use only when a live Terminal session should be explicitly terminated."),
        _ => Err(format!("unknown Terminal tool {tool}")),
    }
}

fn terminal_instructions(tool: &str) -> std::result::Result<&'static str, String> {
    match tool {
        "Create" => Ok(
            "Choose a fixed PTY size and workspace-relative cwd. The call waits for stable initial output and returns the first structured terminal patch as an update.",
        ),
        "Interact" => Ok(
            "Input actions execute once in exact order. Text actions write UTF-8 text; key actions represent Enter, Escape, control chords, navigation, and other semantic keys. An empty input list polls. Interpret returned terminal patches against the previous call baseline.",
        ),
        "Status" => Ok("Status does not consume or reset the terminal patch baseline."),
        "List" => Ok(
            "List includes running and already ended sessions retained by this live toolbox process.",
        ),
        "Kill" => Ok("Kill terminates the PTY and returns its final recorded status."),
        _ => Err(format!("unknown Terminal tool {tool}")),
    }
}

fn terminal_examples(tool: &str) -> std::result::Result<&'static str, String> {
    match tool {
        "Create" => Ok(
            r#"{"width":120,"height":40,"cwd":".","wait_ms":1000,"max_wait_ms":10000,"max_output_chars":20000}"#,
        ),
        "Interact" => Ok(
            r#"Send text followed by Enter: {"session_id":"pty-10","input":[{"type":"text","text":"pwd"},{"type":"key","key":"enter"}]}. Poll without input: {"session_id":"pty-10","input":[]}."#,
        ),
        "Status" => Ok(r#"{"session_id":"pty-10"}"#),
        "List" => Ok("{}"),
        "Kill" => Ok(r#"{"session_id":"pty-10","grace_ms":1000}"#),
        _ => Err(format!("unknown Terminal tool {tool}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, io::Cursor};

    fn temporary_workspace(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "me-toolbox-{name}-{}-{}",
            std::process::id(),
            nonce
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn empty_tools_directory_gets_all_default_toolboxes() {
        let workspace = temporary_workspace("default");
        let path = ensure_default_toolboxes(&workspace).unwrap();
        assert!(workspace.join(WORKSPACE_TEMP_DIRECTORY).is_dir());
        assert_eq!(path.file_name().unwrap(), DEFAULT_TERMINAL_FILE);
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_TERMINAL_SOURCE);
        let file = path.parent().unwrap().join(DEFAULT_FILE_FILE);
        assert_eq!(fs::read_to_string(&file).unwrap(), DEFAULT_FILE_SOURCE);
        let web_browser = path.parent().unwrap().join(DEFAULT_WEB_BROWSER_FILE);
        assert_eq!(
            fs::read_to_string(&web_browser).unwrap(),
            DEFAULT_WEB_BROWSER_SOURCE
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            fs::read(
                path.parent()
                    .unwrap()
                    .join(DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL_FILE)
            )
            .unwrap(),
            DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL
        );
        ensure_default_toolboxes(&workspace).unwrap();
        assert_eq!(
            toolbox_paths(&workspace).unwrap(),
            vec![file, path, web_browser]
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn nonempty_tools_directory_is_never_modified_with_a_default() {
        let workspace = temporary_workspace("nonempty");
        let directory = workspace.join(TOOLBOX_DIRECTORY);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("Custom.py"), "print('custom')").unwrap();
        ensure_default_toolboxes(&workspace).unwrap();
        assert!(workspace.join(WORKSPACE_TEMP_DIRECTORY).is_dir());
        assert_eq!(
            toolbox_paths(&workspace).unwrap(),
            vec![directory.join("Custom.py")]
        );
        assert!(!directory.join(DEFAULT_TERMINAL_FILE).exists());
        assert!(!directory.join(DEFAULT_FILE_FILE).exists());
        assert!(!directory.join(DEFAULT_WEB_BROWSER_FILE).exists());
        #[cfg(target_os = "macos")]
        assert!(
            !directory
                .join(DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL_FILE)
                .exists()
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn managed_default_toolboxes_refresh_without_touching_custom_programs() {
        let workspace = temporary_workspace("managed-refresh");
        let directory = workspace.join(TOOLBOX_DIRECTORY);
        fs::create_dir_all(&directory).unwrap();
        let terminal = directory.join(DEFAULT_TERMINAL_FILE);
        let file = directory.join(DEFAULT_FILE_FILE);
        let web_browser = directory.join(DEFAULT_WEB_BROWSER_FILE);
        let custom = directory.join("Custom.py");
        fs::write(
            &terminal,
            "#!/usr/bin/env python3\n\"\"\"ME-RUST default Terminal toolbox.\"\"\"\nold\n",
        )
        .unwrap();
        fs::write(&file, "# custom replacement using the default filename\n").unwrap();
        fs::write(
            &web_browser,
            "#!/usr/bin/env python3\n# ME-RUST-MANAGED-TOOLBOX\nold\n",
        )
        .unwrap();
        fs::write(&custom, "custom\n").unwrap();
        #[cfg(target_os = "macos")]
        fs::write(
            directory.join(DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL_FILE),
            b"stale",
        )
        .unwrap();

        ensure_default_toolboxes(&workspace).unwrap();

        assert_eq!(
            fs::read_to_string(terminal).unwrap(),
            DEFAULT_TERMINAL_SOURCE
        );
        assert_eq!(
            fs::read_to_string(web_browser).unwrap(),
            DEFAULT_WEB_BROWSER_SOURCE
        );
        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "# custom replacement using the default filename\n"
        );
        assert_eq!(fs::read_to_string(custom).unwrap(), "custom\n");
        #[cfg(target_os = "macos")]
        assert_eq!(
            fs::read(directory.join(DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL_FILE)).unwrap(),
            DEFAULT_WEB_BROWSER_MACOS_WINDOW_CONTROL
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn external_toolbox_cannot_shadow_native_namespaces() {
        let workspace = temporary_workspace("reserved-native");
        let directory = workspace.join(TOOLBOX_DIRECTORY);
        fs::create_dir_all(&directory).unwrap();
        let source = r#"import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    command = request["cmd"]
    if command == "getTools":
        output = ["Create"]
    elif command == "getBrief":
        output = "conflicting Agent"
    elif command in ("getInputSchema", "getOutputSchema"):
        output = {"type": "object"}
    else:
        output = "metadata"
    print(json.dumps({"id": request["id"], "type": "result", "output": output}), flush=True)
"#;
        for namespace in ["Agent", "SetTitle", "WorkMap", "Compact"] {
            let script = directory.join(format!("{namespace}.py"));
            fs::write(&script, source).unwrap();
            let python = Python312::resolve_at(&workspace.join("global"))
                .expect("embedded Python 3.12 must be available");
            let error = ToolboxRuntime::load_with_python(&workspace, vec![script.clone()], python)
                .err()
                .expect("native namespace collision must fail");
            assert!(
                error
                    .to_string()
                    .contains(&format!("namespace {namespace} is reserved"))
            );
            fs::remove_file(script).unwrap();
        }
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn terminal_toolbox_metadata_is_individually_queryable() {
        let workspace = temporary_workspace("metadata");
        let input = [
            json!({"id":1,"cmd":"getTools"}),
            json!({"id":2,"cmd":"getBrief"}),
            json!({"id":3,"cmd":"getInputSchema","tool":"Create"}),
            json!({"id":4,"cmd":"getOutputSchema","tool":"Create"}),
            json!({"id":5,"cmd":"getInstructions","tool":"Create"}),
            json!({"id":6,"cmd":"getRoute","tool":"Create"}),
            json!({"id":7,"cmd":"getExamples","tool":"Create"}),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter(Arc::clone(&output));
        run_default_terminal_toolbox(Cursor::new(input), writer, &workspace).unwrap();
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let frames = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 7);
        assert_eq!(frames[0]["output"][0], "Create");
        assert!(
            frames[1]["output"]
                .as_str()
                .unwrap()
                .contains("real, stateful PTY")
        );
        assert_eq!(frames[2]["output"]["type"], "object");
        assert_eq!(frames[3]["output"]["type"], "object");
        assert!(
            frames[4]["output"]
                .as_str()
                .unwrap()
                .contains("fixed PTY size")
        );
        assert!(
            frames[5]["output"]
                .as_str()
                .unwrap()
                .contains("persistent PTY")
        );
        assert!(frames[6]["output"].as_str().unwrap().contains("\"width\""));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn catalog_builds_namespaced_model_tools_and_prompt_sections() {
        let catalog = ToolboxCatalog::default_terminal_for_test();
        assert_eq!(catalog.tools().len(), 5);
        assert_eq!(
            catalog.resolve_api_name("Terminal_Create"),
            Some("Terminal.Create")
        );
        assert_eq!(
            catalog.model_definitions()[0]["function"]["parameters"]["type"],
            "object"
        );
        assert!(catalog.prompt().contains("# Toolbox Terminal"));
        assert!(catalog.prompt().contains("## Terminal.Interact"));
        assert!(catalog.prompt().contains("Result detail schema:"));
        assert!(catalog.prompt().contains("describes only `result.detail`"));
        assert!(catalog.prompt().contains("including its actual JSON type"));
        assert!(catalog.prompt().contains("\"state\": \"succeeded\""));
        assert!(catalog.prompt().contains("\"truncate\": false"));
    }

    #[test]
    fn native_catalog_does_not_expose_disabled_agent_toolbox() {
        let catalog = ToolboxCatalog::native_for_test();
        let agent_api_names = agent_toolbox::catalog_parts()
            .0
            .into_iter()
            .map(|tool| tool.api_name)
            .collect::<BTreeSet<_>>();
        assert!(!catalog.prompt().contains("# Toolbox Agent"));
        assert!(catalog.prompt().contains("# Toolbox WorkMap"));
        assert!(catalog.prompt().contains("# Toolbox Compact"));
        assert!(catalog.model_definitions().iter().all(|definition| {
            !agent_api_names.contains(definition["function"]["name"].as_str().unwrap())
        }));
        for api_name in agent_api_names {
            assert!(catalog.resolve_api_name(&api_name).is_none());
        }
    }

    #[test]
    fn manager_catalog_exposes_low_level_tools_without_duplicate_capability_reference() {
        let source = ToolboxCatalog::default_terminal_for_test();
        let manager = source.manager_view().unwrap();
        let callable = manager
            .tools()
            .iter()
            .map(|tool| tool.full_name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            callable
                .iter()
                .copied()
                .filter(|tool| tool.starts_with("Worker."))
                .collect::<Vec<_>>(),
            vec![
                agent_toolbox::WORKER_ASK,
                agent_toolbox::WORKER_CLEAR_CONTEXT,
                agent_toolbox::WORKER_STOP,
                agent_toolbox::WORKER_WAIT,
            ]
        );
        assert!(callable.contains("Terminal.Create"));
        assert!(callable.contains("Terminal.Interact"));
        assert!(manager.prompt().contains("# Toolbox Worker"));
        assert!(manager.prompt().contains("# Toolbox Terminal"));
        assert!(!manager.prompt().contains("# Worker capability reference"));
        assert!(!manager.prompt().contains("# Worker toolbox Terminal"));
        assert!(manager.prompt().contains("## Terminal.Create"));
        assert!(manager.prompt().contains("## Terminal.Interact"));
        assert!(manager.prompt().contains("Current PTY shell backend:"));
        assert!(manager.prompt().contains("Important behavior:"));
        assert!(manager.prompt().contains("Route:\nUse to create"));
        assert!(
            manager
                .prompt()
                .contains("Instructions:\nChoose a fixed PTY size")
        );
        assert!(manager.prompt().contains("Result detail schema:\n```json"));
        assert!(manager.prompt().contains("Examples:\n"));
        assert!(manager.prompt().contains("\"max_output_chars\""));
        assert_eq!(
            manager.resolve_api_name(terminal::API_CREATE),
            Some("Terminal.Create")
        );
        assert!(
            manager
                .model_definitions()
                .iter()
                .any(|definition| definition["function"]["name"] == terminal::API_CREATE)
        );
    }

    #[test]
    fn manager_catalog_keeps_native_tools_except_agent() {
        let manager = ToolboxCatalog::native_for_test().manager_view().unwrap();

        assert!(manager.prompt().contains("# Toolbox Worker"));
        assert!(manager.prompt().contains("# Toolbox WorkMap"));
        assert!(manager.prompt().contains("# Toolbox Compact"));
        assert!(manager.prompt().contains("# Toolbox SetTitle"));
        assert!(!manager.prompt().contains("# Toolbox Agent"));
        assert!(!manager.prompt().contains("# Worker capability reference"));
        assert!(
            manager
                .tools()
                .iter()
                .all(|tool| tool.toolbox != agent_toolbox::AGENT_TOOLBOX_NAME)
        );
        assert!(manager.resolve_api_name("Agent_Create").is_none());
    }

    #[test]
    fn transaction_update_parser_preserves_typed_terminal_content() {
        let update = terminal::test_update("hello");
        let parsed = parse_update(json!({
            "stream": "terminal",
            "content": update,
        }))
        .unwrap();
        let ToolboxUpdate::Terminal(parsed) = parsed else {
            panic!("expected terminal update");
        };
        assert_eq!(parsed.rows[0].runs[0].text, "hello");
    }

    #[test]
    fn persistent_python_toolbox_correlates_concurrent_transactions_and_restarts_after_exit() {
        let workspace = temporary_workspace("persistent");
        let directory = workspace.join(TOOLBOX_DIRECTORY);
        fs::create_dir_all(&directory).unwrap();
        let script = directory.join("Probe.py");
        fs::write(
            &script,
            r#"import json
import os
import shutil
import sys
import threading
import time

lock = threading.Lock()

def send(value):
    with lock:
        sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
        sys.stdout.flush()

def execute(request):
    tool = request["tool"]
    if tool == "Exit":
        os._exit(23)
    if tool == "BadJson":
        with lock:
            sys.stdout.write("not-json\n")
            sys.stdout.flush()
        return
    if tool == "WrongId":
        send({"id": request["id"] + 1000, "type": "result", "output": {}})
        return
    if tool == "ErrorWithTip":
        send({"id": request["id"], "type": "error", "error": {
            "code": "guided_error",
            "message": "something recoverable happened",
            "retryable": True,
            "tip": "Please inspect the target and try again."
        }})
        return
    value = request["input"]["value"]
    time.sleep(request["input"].get("delay_ms", 0) / 1000)
    send({"id": request["id"], "type": "update", "output": {"stream": "stdout", "content": "update:" + value}})
    send({"id": request["id"], "type": "result", "output": {
        "value": value,
        "pid": os.getpid(),
        "python_executable": sys.executable,
        "path_python": shutil.which("python"),
        "python_utf8": os.environ.get("PYTHONUTF8"),
        "python_io_encoding": os.environ.get("PYTHONIOENCODING")
    }})

for line in sys.stdin:
    request = json.loads(line)
    cmd = request["cmd"]
    if cmd == "getTools":
        send({"id": request["id"], "type": "result", "output": ["Echo", "Exit", "BadJson", "WrongId", "ErrorWithTip"]})
    elif cmd == "getBrief":
        send({"id": request["id"], "type": "result", "output": "Probe toolbox"})
    elif cmd in ("getInputSchema", "getOutputSchema"):
        send({"id": request["id"], "type": "result", "output": {"type": "object"}})
    elif cmd in ("getInstructions", "getRoute", "getExamples"):
        send({"id": request["id"], "type": "result", "output": cmd + ":" + request["tool"]})
    elif cmd == "execute":
        threading.Thread(target=execute, args=(request,)).start()
"#,
        )
        .unwrap();

        let python = Python312::resolve_at(&workspace.join("global"))
            .expect("embedded Python 3.12 must be available");
        let embedded_executable = python.program.clone();
        let embedded_directory = python.path_directory.clone();
        let runtime =
            Arc::new(ToolboxRuntime::load_with_python(&workspace, vec![script], python).unwrap());
        assert_eq!(runtime.catalog().tools().len(), 19);
        assert!(runtime.catalog().resolve_api_name("Agent_Create").is_none());
        assert!(runtime.catalog().resolve_api_name("Agent_Stop").is_none());
        assert!(
            runtime
                .catalog()
                .resolve_api_name("Agent_ClearContext")
                .is_none()
        );
        assert_eq!(
            runtime.catalog().resolve_api_name("WorkMap_Read"),
            Some("WorkMap.Read")
        );
        let disabled_patch = runtime
            .execute("File.ApplyPatch", "{}", |_| Ok(()))
            .unwrap_err();
        assert!(matches!(
            disabled_patch,
            ToolboxExecutionError::Tool {
                ref code,
                ref message,
                retryable: false,
                ..
            } if code == "tool_disabled" && message.contains("File.Edit")
        ));
        let guided = runtime
            .execute("Probe.ErrorWithTip", "{}", |_| Ok(()))
            .unwrap_err();
        assert!(matches!(
            guided,
            ToolboxExecutionError::Tool {
                ref code,
                retryable: true,
                tip: Some(ref tip),
                ..
            } if code == "guided_error" && tip == "Please inspect the target and try again."
        ));

        let first_runtime = Arc::clone(&runtime);
        let first = thread::spawn(move || {
            let mut updates = Vec::new();
            let result = first_runtime
                .execute(
                    "Probe.Echo",
                    r#"{"value":"slow","delay_ms":80}"#,
                    |update| {
                        updates.push(update);
                        Ok(())
                    },
                )
                .unwrap();
            (result, updates)
        });
        let second_runtime = Arc::clone(&runtime);
        let second = thread::spawn(move || {
            let mut updates = Vec::new();
            let result = second_runtime
                .execute("Probe.Echo", r#"{"value":"fast","delay_ms":5}"#, |update| {
                    updates.push(update);
                    Ok(())
                })
                .unwrap();
            (result, updates)
        });
        let (slow_result, slow_updates) = first.join().unwrap();
        let (fast_result, fast_updates) = second.join().unwrap();
        assert_eq!(slow_result["value"], "slow");
        assert_eq!(fast_result["value"], "fast");
        assert_eq!(slow_result["python_utf8"], "1");
        assert_eq!(slow_result["python_io_encoding"], "utf-8");
        assert_eq!(
            PathBuf::from(slow_result["python_executable"].as_str().unwrap()),
            embedded_executable
        );
        assert!(
            PathBuf::from(slow_result["path_python"].as_str().unwrap())
                .starts_with(embedded_directory)
        );
        assert!(matches!(
            &slow_updates[0],
            ToolboxUpdate::Text { content, .. } if content == "update:slow"
        ));
        assert!(matches!(
            &fast_updates[0],
            ToolboxUpdate::Text { content, .. } if content == "update:fast"
        ));

        let original_pid = fast_result["pid"].clone();
        runtime.reset_sessions();
        let after_clear = runtime
            .execute("Probe.Echo", r#"{"value":"after-clear"}"#, |_| Ok(()))
            .unwrap();
        assert_eq!(after_clear["value"], "after-clear");
        assert_ne!(after_clear["pid"], original_pid);

        let client = runtime.programs.get("Probe").unwrap().clone();
        let timeout_started = Instant::now();
        let timed_out = client
            .internal_execute_timeout(
                "Echo",
                json!({"value": "late-observation", "delay_ms": 100}),
                Duration::from_millis(10),
            )
            .unwrap_err();
        assert!(matches!(timed_out, ToolboxExecutionError::Interrupted(_)));
        assert!(timeout_started.elapsed() < Duration::from_millis(250));
        thread::sleep(Duration::from_millis(150));
        let after_timeout = client
            .internal_execute_timeout(
                "Echo",
                json!({"value": "after-observation-timeout"}),
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(after_timeout["value"], "after-observation-timeout");
        assert_eq!(after_timeout["pid"], after_clear["pid"]);

        let cancellation_started = std::time::Instant::now();
        let cancelled = runtime
            .execute_cancellable(
                "Probe.Echo",
                r#"{"value":"cancelled","delay_ms":5000}"#,
                |_| Ok(()),
                || cancellation_started.elapsed() >= Duration::from_millis(100),
            )
            .unwrap_err();
        assert!(matches!(cancelled, ToolboxExecutionError::Interrupted(_)));
        assert!(cancellation_started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            runtime
                .execute("Probe.Echo", r#"{"value":"after-cancel"}"#, |_| Ok(()))
                .unwrap()["value"],
            "after-cancel"
        );

        let interrupted = runtime
            .execute("Probe.Exit", r#"{"value":"unused"}"#, |_| Ok(()))
            .unwrap_err();
        assert!(matches!(interrupted, ToolboxExecutionError::Interrupted(_)));
        let restarted = runtime
            .execute("Probe.Echo", r#"{"value":"after-restart"}"#, |_| Ok(()))
            .unwrap();
        assert_eq!(restarted["value"], "after-restart");

        let bad_json = runtime
            .execute("Probe.BadJson", r#"{"value":"unused"}"#, |_| Ok(()))
            .unwrap_err();
        assert!(matches!(bad_json, ToolboxExecutionError::Interrupted(_)));
        assert_eq!(
            runtime
                .execute("Probe.Echo", r#"{"value":"after-bad-json"}"#, |_| Ok(()))
                .unwrap()["value"],
            "after-bad-json"
        );

        let wrong_id = runtime
            .execute("Probe.WrongId", r#"{"value":"unused"}"#, |_| Ok(()))
            .unwrap_err();
        assert!(matches!(wrong_id, ToolboxExecutionError::Interrupted(_)));
        assert_eq!(
            runtime
                .execute("Probe.Echo", r#"{"value":"after-wrong-id"}"#, |_| Ok(()))
                .unwrap()["value"],
            "after-wrong-id"
        );

        drop(runtime);
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn shutdown_interrupts_a_stuck_toolbox_and_terminates_its_process_tree() {
        let workspace = temporary_workspace("stuck-process-tree");
        let directory = workspace.join(TOOLBOX_DIRECTORY);
        fs::create_dir_all(&directory).unwrap();
        let script = directory.join("Stuck.py");
        fs::write(
            &script,
            r#"import json
import subprocess
import sys
import time

def send(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    command = request["cmd"]
    if command == "getTools":
        send({"id": request["id"], "type": "result", "output": ["Hang"]})
    elif command == "getBrief":
        send({"id": request["id"], "type": "result", "output": "Stuck toolbox"})
    elif command in ("getInputSchema", "getOutputSchema"):
        send({"id": request["id"], "type": "result", "output": {"type": "object"}})
    elif command in ("getInstructions", "getRoute", "getExamples"):
        send({"id": request["id"], "type": "result", "output": "Hang until the host shuts down."})
    elif command == "execute":
        descendant = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        send({"id": request["id"], "type": "update", "output": {
            "stream": "stdout", "content": str(descendant.pid)
        }})
        while True:
            time.sleep(1)
"#,
        )
        .unwrap();

        let python = Python312::resolve_at(&workspace.join("global"))
            .expect("embedded Python 3.12 must be available");
        let runtime =
            Arc::new(ToolboxRuntime::load_with_python(&workspace, vec![script], python).unwrap());
        let observer = runtime.observer();
        let (pid_sender, pid_receiver) = mpsc::channel();
        let (done_sender, done_receiver) = mpsc::channel();
        let execution_runtime = Arc::clone(&runtime);
        thread::spawn(move || {
            let result = execution_runtime.execute("Stuck.Hang", "{}", |update| {
                if let ToolboxUpdate::Text { content, .. } = update {
                    let _ = pid_sender.send(content.parse::<u32>().unwrap());
                }
                Ok(())
            });
            let _ = done_sender.send(result);
        });

        let _descendant = pid_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("stuck toolbox did not report its descendant process");
        let started = Instant::now();
        observer.shutdown();
        let result = done_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("toolbox shutdown did not interrupt the stuck execute request");
        assert!(matches!(result, Err(ToolboxExecutionError::Interrupted(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            runtime.execute("Stuck.Hang", "{}", |_| Ok(())),
            Err(ToolboxExecutionError::Interrupted(_))
        ));

        #[cfg(unix)]
        {
            let deadline = Instant::now() + Duration::from_secs(2);
            while process_exists(_descendant) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !process_exists(_descendant),
                "toolbox descendant process {_descendant} survived runtime shutdown"
            );
        }

        drop(runtime);
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    fn process_exists(process_id: u32) -> bool {
        let Ok(process_id) = i32::try_from(process_id) else {
            return false;
        };
        let result = unsafe { libc::kill(process_id, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[test]
    fn default_terminal_worker_preserves_pty_state_and_rejects_stale_session_ids() {
        if terminal::shell_backend() == terminal::UNAVAILABLE_BACKEND {
            return;
        }
        let workspace = temporary_workspace("terminal-lifecycle");
        let state = Mutex::new(DefaultTerminalState::new());
        let output = Mutex::new(Vec::new());

        let create = terminal::parse_request(
            terminal::CREATE,
            r#"{"width":80,"height":20,"wait_ms":25,"max_wait_ms":1000}"#,
        )
        .unwrap();
        let created = execute_terminal_request(41, create, &state, &workspace, &output).unwrap();
        assert_eq!(created["session_id"], "pty-41");

        let interact = terminal::parse_request(
            terminal::INTERACT,
            r#"{"session_id":"pty-41","input":[{"type":"text","text":"printf 'toolbox-state-ok\\n'"},{"type":"key","key":"enter"}],"wait_ms":25,"max_wait_ms":1000}"#,
        )
        .unwrap();
        execute_terminal_request(42, interact, &state, &workspace, &output).unwrap();
        let frames = String::from_utf8(output.into_inner().unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], 41);
        assert_eq!(frames[1]["id"], 42);
        assert!(
            frames[1]["output"]["content"]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["runs"].as_array().unwrap().iter().any(|run| {
                    run["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("toolbox-state-ok"))
                }))
        );

        let kill =
            terminal::parse_request(terminal::KILL, r#"{"session_id":"pty-41","grace_ms":100}"#)
                .unwrap();
        execute_terminal_request(43, kill, &state, &workspace, &Mutex::new(Vec::new())).unwrap();

        let fresh_state = Mutex::new(DefaultTerminalState::new());
        let stale =
            terminal::parse_request(terminal::STATUS, r#"{"session_id":"pty-41"}"#).unwrap();
        let error =
            execute_terminal_request(44, stale, &fresh_state, &workspace, &Mutex::new(Vec::new()))
                .unwrap_err();
        assert_eq!(error.0, "session_not_found");
        assert!(error.1.contains("Create a new session"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn default_terminal_jsonl_worker_executes_stateful_requests_in_input_order() {
        if terminal::shell_backend() == terminal::UNAVAILABLE_BACKEND {
            return;
        }
        let workspace = temporary_workspace("terminal-jsonl-order");
        let input = [
            json!({
                "id": 1,
                "cmd": "execute",
                "tool": "Create",
                "input": {
                    "width": 80,
                    "height": 20,
                    "wait_ms": 25,
                    "max_wait_ms": 1000
                }
            }),
            json!({
                "id": 2,
                "cmd": "execute",
                "tool": "Interact",
                "input": {
                    "session_id": "pty-1",
                    "input": [
                        {"type": "text", "text": "printf 'jsonl-order-ok\\n'"},
                        {"type": "key", "key": "enter"}
                    ],
                    "wait_ms": 25,
                    "max_wait_ms": 1000
                }
            }),
            json!({
                "id": 3,
                "cmd": "execute",
                "tool": "Kill",
                "input": {"session_id": "pty-1", "grace_ms": 100}
            }),
        ]
        .into_iter()
        .map(|request| request.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let bytes = Arc::new(Mutex::new(Vec::new()));
        run_default_terminal_toolbox(
            Cursor::new(input),
            SharedWriter(Arc::clone(&bytes)),
            &workspace,
        )
        .unwrap();
        let frames = String::from_utf8(bytes.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(frames.iter().all(|frame| frame["type"] != "error"));
        let terminal_ids = frames
            .iter()
            .filter(|frame| frame["type"] == "result")
            .map(|frame| frame["id"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(terminal_ids, vec![1, 2, 3]);
        assert_eq!(frames[1]["output"]["session_id"], "pty-1");
        assert!(frames.iter().any(|frame| {
            frame["id"] == 2
                && frame["type"] == "update"
                && frame["output"]["content"]["rows"]
                    .as_array()
                    .is_some_and(|rows| {
                        rows.iter().any(|row| {
                            row["runs"].as_array().is_some_and(|runs| {
                                runs.iter().any(|run| {
                                    run["text"]
                                        .as_str()
                                        .is_some_and(|text| text.contains("jsonl-order-ok"))
                                })
                            })
                        })
                    })
        }));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn terminal_observer_worker_handles_sustained_preview_polling_in_order() {
        assert_eq!(TERMINAL_OBSERVER_TIMEOUT, Duration::from_secs(3));
        let workspace = temporary_workspace("terminal-observer-polling");
        let input = (1_u64..=512)
            .map(|id| {
                json!({
                    "id": id,
                    "cmd": "execute",
                    "tool": TERMINAL_OBSERVE_BACKEND,
                    "input": {}
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let bytes = Arc::new(Mutex::new(Vec::new()));
        run_default_terminal_toolbox(
            Cursor::new(input),
            SharedWriter(Arc::clone(&bytes)),
            &workspace,
        )
        .unwrap();
        let ids = String::from_utf8(bytes.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|frame| frame["id"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, (1_u64..=512).collect::<Vec<_>>());
        fs::remove_dir_all(workspace).unwrap();
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
