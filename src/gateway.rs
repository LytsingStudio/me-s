use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

use serde::Serialize;

use crate::{
    Result,
    config::{GlobalConfig, global_config_path},
    gateway_process::{ManagedProcess, ProcessRoute, me_s_executable},
    gateway_settings::GatewaySettings,
    gateway_state::{BUILTIN_WORKSPACE_ID, GatewayState, WorkspaceRecord},
    managed_protocol::bearer_header_value,
};

const MAX_NOTICES: usize = 32;

#[derive(Clone, Serialize)]
pub struct GatewayWorkspace {
    pub id: String,
    pub name: String,
    pub path: String,
    pub builtin: bool,
}

#[derive(Clone, Serialize)]
pub struct GatewayNotice {
    pub id: u64,
    pub message: String,
}

#[derive(Serialize)]
pub struct GatewaySnapshot {
    pub ok: bool,
    pub version: &'static str,
    pub gateway_root: String,
    pub workspaces: Vec<GatewayWorkspace>,
    pub selected_workspace_id: Option<String>,
    pub selected_agent_id: Option<String>,
    pub notices: Vec<GatewayNotice>,
}

pub struct ProxyResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub vary: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Serialize)]
pub struct DirectoryListing {
    pub ok: bool,
    pub path: Option<String>,
    pub parent: Option<String>,
    pub root_selector: bool,
    pub parent_is_root_selector: bool,
    pub directories: Vec<DirectoryEntry>,
}

#[derive(Serialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

pub struct Gateway {
    root: PathBuf,
    me_s: PathBuf,
    global_config: PathBuf,
    state: Mutex<GatewayState>,
    processes: Mutex<HashMap<String, ManagedProcess>>,
    notices: Mutex<VecDeque<GatewayNotice>>,
    next_notice: AtomicU64,
    lifecycle: Mutex<()>,
    shutting_down: AtomicBool,
    proxy_client: reqwest::blocking::Client,
}

impl Gateway {
    pub fn start(root: &Path) -> Result<Arc<Self>> {
        let global_config = global_config_path()?;
        GlobalConfig::load(&global_config)?;
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(format!("me-gateway root is not a directory: {}", root.display()).into());
        }
        let me_s = me_s_executable()?;
        let mut state = GatewayState::load(&root)?;
        let builtin = ManagedProcess::start(&me_s, &root)
            .map_err(|error| format!("无法启动聊天工作区：{error}"))?;
        let gateway = Arc::new(Self {
            root: root.clone(),
            me_s,
            global_config,
            state: Mutex::new(GatewayState::default()),
            processes: Mutex::new(HashMap::from([(BUILTIN_WORKSPACE_ID.to_owned(), builtin)])),
            notices: Mutex::new(VecDeque::new()),
            next_notice: AtomicU64::new(1),
            lifecycle: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            proxy_client: reqwest::blocking::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(60))
                .build()?,
        });

        let mut restored = Vec::new();
        let mut paths = HashSet::from([root]);
        for record in state.external_workspaces.drain(..) {
            let path = match canonical_directory(&record.path) {
                Ok(path) => path,
                Err(_) => {
                    gateway
                        .push_notice(format!("无法打开工作区“{}”", workspace_name(&record.path)));
                    continue;
                }
            };
            if !paths.insert(path.clone()) {
                continue;
            }
            match ManagedProcess::start(&gateway.me_s, &path) {
                Ok(process) => {
                    gateway
                        .processes
                        .lock()
                        .map_err(|_| "gateway process registry is unavailable")?
                        .insert(record.id.clone(), process);
                    restored.push(WorkspaceRecord {
                        id: record.id,
                        path,
                    });
                }
                Err(error) => {
                    eprintln!(
                        "warning: failed to restore Workspace {}: {error}",
                        path.display()
                    );
                    gateway.push_notice(format!("无法打开工作区“{}”", workspace_name(&path)));
                }
            }
        }
        state.external_workspaces = restored;
        if state
            .selected_workspace_id
            .as_deref()
            .is_none_or(|selected| {
                selected != BUILTIN_WORKSPACE_ID
                    && !state
                        .external_workspaces
                        .iter()
                        .any(|item| item.id == selected)
            })
        {
            state.selected_workspace_id = Some(BUILTIN_WORKSPACE_ID.to_owned());
            state.selected_agent_id = None;
        }
        state.save(&gateway.root)?;
        *gateway
            .state
            .lock()
            .map_err(|_| "gateway state is unavailable")? = state;
        Ok(gateway)
    }

    pub fn snapshot(&self) -> Result<GatewaySnapshot> {
        let state = self
            .state
            .lock()
            .map_err(|_| "gateway state is unavailable")?
            .clone();
        let mut workspaces = vec![GatewayWorkspace {
            id: BUILTIN_WORKSPACE_ID.to_owned(),
            name: "聊天".into(),
            path: self.root.to_string_lossy().into_owned(),
            builtin: true,
        }];
        workspaces.extend(
            state
                .external_workspaces
                .iter()
                .map(|record| GatewayWorkspace {
                    id: record.id.clone(),
                    name: workspace_name(&record.path),
                    path: record.path.to_string_lossy().into_owned(),
                    builtin: false,
                }),
        );
        let notices = self
            .notices
            .lock()
            .map_err(|_| "gateway notice store is unavailable")?
            .iter()
            .cloned()
            .collect();
        Ok(GatewaySnapshot {
            ok: true,
            version: env!("CARGO_PKG_VERSION"),
            gateway_root: self.root.to_string_lossy().into_owned(),
            workspaces,
            selected_workspace_id: state.selected_workspace_id,
            selected_agent_id: state.selected_agent_id,
            notices,
        })
    }

    pub fn open_workspace(&self, path: &Path) -> Result<String> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "gateway lifecycle is unavailable")?;
        let path = canonical_directory(path)?;
        if path == self.root {
            return Ok(BUILTIN_WORKSPACE_ID.to_owned());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "gateway state is unavailable")?;
        if let Some(record) = state
            .external_workspaces
            .iter()
            .find(|record| record.path == path)
        {
            return Ok(record.id.clone());
        }
        let id = self.new_workspace_id(&state)?;
        let process = ManagedProcess::start(&self.me_s, &path)?;
        let mut updated = state.clone();
        updated.external_workspaces.push(WorkspaceRecord {
            id: id.clone(),
            path,
        });
        updated.selected_workspace_id = Some(id.clone());
        updated.selected_agent_id = None;
        updated.save(&self.root)?;
        self.processes
            .lock()
            .map_err(|_| "gateway process registry is unavailable")?
            .insert(id.clone(), process);
        *state = updated;
        Ok(id)
    }

    pub fn create_workspace(&self, parent: &Path, name: &str) -> Result<String> {
        validate_directory_name(name)?;
        let parent = canonical_directory(parent)?;
        let path = parent.join(name);
        fs::create_dir(&path)?;
        self.open_workspace(&path)
    }

    pub fn close_workspace(&self, id: &str) -> Result<()> {
        if id == BUILTIN_WORKSPACE_ID {
            return Err("聊天工作区不能关闭".into());
        }
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "gateway lifecycle is unavailable")?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "gateway state is unavailable")?;
        let index = state
            .external_workspaces
            .iter()
            .position(|record| record.id == id)
            .ok_or("工作区当前未打开")?;
        let mut updated = state.clone();
        updated.external_workspaces.remove(index);
        if updated.selected_workspace_id.as_deref() == Some(id) {
            updated.selected_workspace_id = Some(BUILTIN_WORKSPACE_ID.to_owned());
            updated.selected_agent_id = None;
        }
        updated.save(&self.root)?;
        *state = updated;
        drop(state);
        self.processes
            .lock()
            .map_err(|_| "gateway process registry is unavailable")?
            .remove(id);
        Ok(())
    }

    pub fn select(&self, workspace_id: &str, agent_id: Option<String>) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "gateway state is unavailable")?;
        if workspace_id != BUILTIN_WORKSPACE_ID
            && !state
                .external_workspaces
                .iter()
                .any(|record| record.id == workspace_id)
        {
            return Err("工作区当前未打开".into());
        }
        let mut updated = state.clone();
        updated.selected_workspace_id = Some(workspace_id.to_owned());
        updated.selected_agent_id = agent_id;
        updated.save(&self.root)?;
        *state = updated;
        Ok(())
    }

    pub fn poll(&self) -> Result<()> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Ok(());
        }
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| "gateway lifecycle is unavailable")?;
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| "gateway process registry is unavailable")?;
        if processes
            .get_mut(BUILTIN_WORKSPACE_ID)
            .is_none_or(|process| process.has_exited().unwrap_or(true))
        {
            return Err("聊天工作区已停止运行".into());
        }
        let failed = processes
            .iter_mut()
            .filter_map(|(id, process)| {
                (id != BUILTIN_WORKSPACE_ID && process.has_exited().unwrap_or(true))
                    .then_some(id.clone())
            })
            .collect::<Vec<_>>();
        if failed.is_empty() {
            return Ok(());
        }
        for id in &failed {
            processes.remove(id);
        }
        drop(processes);
        let mut state = self
            .state
            .lock()
            .map_err(|_| "gateway state is unavailable")?;
        let mut updated = state.clone();
        for id in failed {
            if let Some(index) = updated
                .external_workspaces
                .iter()
                .position(|record| record.id == id)
            {
                let record = updated.external_workspaces.remove(index);
                self.push_notice(format!(
                    "工作区“{}”已停止运行",
                    workspace_name(&record.path)
                ));
            }
            if updated.selected_workspace_id.as_deref() == Some(&id) {
                updated.selected_workspace_id = Some(BUILTIN_WORKSPACE_ID.to_owned());
                updated.selected_agent_id = None;
            }
        }
        updated.save(&self.root)?;
        *state = updated;
        Ok(())
    }

    pub fn proxy(
        &self,
        workspace_id: &str,
        method: reqwest::Method,
        child_path: &str,
        content_type: Option<&str>,
        accept_encoding: Option<&str>,
        body: Vec<u8>,
    ) -> Result<ProxyResponse> {
        validate_proxy_path(child_path)?;
        let route = self.process_route(workspace_id)?;
        let url = format!("{}/api/{child_path}", route.address);
        let mut request = self
            .proxy_client
            .request(method, url)
            .header(
                reqwest::header::AUTHORIZATION,
                bearer_header_value(&route.token),
            )
            .body(body);
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        if let Some(accept_encoding) = accept_encoding {
            request = request.header(reqwest::header::ACCEPT_ENCODING, accept_encoding);
        }
        let response = request.send().map_err(|_| "工作区请求未能完成")?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_encoding = response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let vary = response
            .headers()
            .get(reqwest::header::VARY)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().map_err(|_| "工作区响应未能完成")?.to_vec();
        Ok(ProxyResponse {
            status,
            content_type,
            content_encoding,
            vary,
            body,
        })
    }

    pub fn list_directory_roots(&self) -> Result<DirectoryListing> {
        Ok(DirectoryListing {
            ok: true,
            path: None,
            parent: None,
            root_selector: true,
            parent_is_root_selector: false,
            directories: host_directory_roots()?,
        })
    }

    pub fn list_directories(&self, path: Option<&Path>) -> Result<DirectoryListing> {
        let path = canonical_directory(path.unwrap_or(&self.root))?;
        let mut directories = fs::read_dir(&path)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .map(|_| DirectoryEntry {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        path: entry.path().to_string_lossy().into_owned(),
                    })
            })
            .collect::<Vec<_>>();
        directories.sort_by_key(|entry| entry.name.to_lowercase());
        let parent = path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned());
        Ok(DirectoryListing {
            ok: true,
            path: Some(path.to_string_lossy().into_owned()),
            parent_is_root_selector: cfg!(windows) && parent.is_none(),
            parent,
            root_selector: false,
            directories,
        })
    }

    pub fn settings(&self) -> Result<GatewaySettings> {
        GatewaySettings::load(&self.global_config)
    }

    pub fn save_settings(&self, settings: GatewaySettings) -> Result<GatewaySettings> {
        settings.save(&self.global_config)
    }

    pub fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let _lifecycle = self.lifecycle.lock();
        if let Ok(mut processes) = self.processes.lock() {
            let drained = processes
                .drain()
                .map(|(_, process)| process)
                .collect::<Vec<_>>();
            drop(processes);
            drop(drained);
        }
    }

    fn process_route(&self, id: &str) -> Result<ProcessRoute> {
        self.processes
            .lock()
            .map_err(|_| "gateway process registry is unavailable")?
            .get(id)
            .map(ManagedProcess::route)
            .ok_or_else(|| "工作区当前未运行".into())
    }

    fn new_workspace_id(&self, state: &GatewayState) -> Result<String> {
        loop {
            let id = format!("w-{}", crate::managed_protocol::random_hex_secret(16)?);
            if !state
                .external_workspaces
                .iter()
                .any(|record| record.id == id)
            {
                return Ok(id);
            }
        }
    }

    fn push_notice(&self, message: String) {
        let id = self.next_notice.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut notices) = self.notices.lock() {
            notices.push_back(GatewayNotice { id, message });
            while notices.len() > MAX_NOTICES {
                notices.pop_front();
            }
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(format!("不是目录：{}", path.display()).into());
    }
    fs::read_dir(&path)?;
    Ok(path)
}

#[cfg(windows)]
fn host_directory_roots() -> Result<Vec<DirectoryEntry>> {
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(windows_drive_entries(mask))
}

#[cfg(not(windows))]
fn host_directory_roots() -> Result<Vec<DirectoryEntry>> {
    Ok(vec![DirectoryEntry {
        name: "/".to_owned(),
        path: "/".to_owned(),
    }])
}

#[cfg(any(windows, test))]
fn windows_drive_entries(mask: u32) -> Vec<DirectoryEntry> {
    (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| {
            let letter = char::from(b'A' + index as u8);
            DirectoryEntry {
                name: format!("{letter}:"),
                path: format!("{letter}:\\"),
            }
        })
        .collect()
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn validate_directory_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    if name.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err("新工作区名称必须是一个有效的目录名".into());
    }
    Ok(())
}

fn validate_proxy_path(path: &str) -> Result<()> {
    let valid_agent = |agent: &str| {
        !agent.is_empty()
            && agent
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    let deletion_blocker = path
        .strip_prefix("deletion-blocker/")
        .is_some_and(valid_agent);
    let session_terminal = path
        .strip_prefix("session-terminal/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(agent, action)| {
            valid_agent(agent) && matches!(action, "read" | "input" | "resize")
        });
    if !matches!(path, "sync" | "snapshot" | "command") && !deletion_blocker && !session_terminal {
        return Err("不支持的工作区接口".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_allowlist_exposes_only_formal_workspace_routes() {
        for path in [
            "sync",
            "snapshot",
            "command",
            "deletion-blocker/main",
            "deletion-blocker/worker_1",
            "session-terminal/main/read",
            "session-terminal/main/input",
            "session-terminal/worker_1/resize",
        ] {
            validate_proxy_path(path).unwrap();
        }
        for path in [
            "health",
            "managed/ready",
            "managed/shutdown",
            "terminal/main/session",
            "deletion-blocker/",
            "deletion-blocker/main/extra",
            "deletion-blocker/..",
            "session-terminal//read",
            "session-terminal/main",
            "session-terminal/main/close",
            "session-terminal/main/read/extra",
            "session-terminal/../read",
            "session-terminal/main/read?cursor=1",
            "sync?private=true",
        ] {
            assert!(validate_proxy_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn windows_drive_mask_maps_to_sorted_root_entries() {
        let roots = windows_drive_entries((1 << 2) | (1 << 3) | (1 << 25));
        assert_eq!(
            roots
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["C:", "D:", "Z:"]
        );
        assert_eq!(
            roots
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            [r"C:\", r"D:\", r"Z:\"]
        );
    }

    #[test]
    fn workspace_names_are_single_normal_path_components() {
        for name in ["project", "项目", "project-name_1"] {
            validate_directory_name(name).unwrap();
        }
        for name in ["", ".", "..", "nested/project", "/absolute"] {
            assert!(validate_directory_name(name).is_err(), "{name}");
        }
    }
}
