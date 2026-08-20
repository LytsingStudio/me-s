use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{Result, config::write_private_atomic};

pub const GATEWAY_STATE_VERSION: u32 = 1;
pub const BUILTIN_WORKSPACE_ID: &str = "chat";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRecord {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayState {
    pub version: u32,
    #[serde(default)]
    pub external_workspaces: Vec<WorkspaceRecord>,
    pub selected_workspace_id: Option<String>,
    pub selected_agent_id: Option<String>,
}

impl Default for GatewayState {
    fn default() -> Self {
        Self {
            version: GATEWAY_STATE_VERSION,
            external_workspaces: Vec::new(),
            selected_workspace_id: Some(BUILTIN_WORKSPACE_ID.to_owned()),
            selected_agent_id: None,
        }
    }
}

impl GatewayState {
    pub fn load(gateway_root: &Path) -> Result<Self> {
        let path = state_path(gateway_root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let state: Self = serde_json::from_slice(&fs::read(&path)?)?;
        state.validate()?;
        Ok(state)
    }

    pub fn save(&self, gateway_root: &Path) -> Result<()> {
        self.validate()?;
        let mut content = serde_json::to_vec_pretty(self)?;
        content.push(b'\n');
        write_private_atomic(&state_path(gateway_root), &content)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != GATEWAY_STATE_VERSION {
            return Err(format!("unsupported me-gateway state version {}", self.version).into());
        }
        let mut ids = HashSet::new();
        let mut paths = HashSet::new();
        for workspace in &self.external_workspaces {
            if workspace.id.is_empty() || workspace.id == BUILTIN_WORKSPACE_ID {
                return Err("external Workspace has an invalid gateway identity".into());
            }
            if !ids.insert(workspace.id.as_str()) {
                return Err(
                    format!("duplicate gateway Workspace identity {}", workspace.id).into(),
                );
            }
            if !workspace.path.is_absolute() {
                return Err(format!(
                    "gateway Workspace path is not absolute: {}",
                    workspace.path.display()
                )
                .into());
            }
            if !paths.insert(workspace.path.as_path()) {
                return Err(format!(
                    "duplicate gateway Workspace path {}",
                    workspace.path.display()
                )
                .into());
            }
        }
        Ok(())
    }
}

pub fn state_directory(gateway_root: &Path) -> PathBuf {
    gateway_root.join(".me-gateway")
}

pub fn state_path(gateway_root: &Path) -> PathBuf {
    state_directory(gateway_root).join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temporary() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "me-gateway-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn missing_state_defaults_and_round_trips_in_stable_order() {
        let root = temporary();
        let mut state = GatewayState::load(&root).unwrap();
        assert_eq!(state.selected_workspace_id.as_deref(), Some("chat"));
        state.external_workspaces.push(WorkspaceRecord {
            id: "w-a".into(),
            path: root.join("a"),
        });
        state.external_workspaces.push(WorkspaceRecord {
            id: "w-b".into(),
            path: root.join("b"),
        });
        state.save(&root).unwrap();
        let loaded = GatewayState::load(&root).unwrap();
        assert_eq!(loaded.external_workspaces[0].id, "w-a");
        assert_eq!(loaded.external_workspaces[1].id, "w-b");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_paths_and_builtin_identity_are_rejected() {
        let root = temporary();
        let path = root.join("workspace");
        let state = GatewayState {
            version: GATEWAY_STATE_VERSION,
            external_workspaces: vec![
                WorkspaceRecord {
                    id: "chat".into(),
                    path: path.clone(),
                },
                WorkspaceRecord {
                    id: "w-b".into(),
                    path,
                },
            ],
            selected_workspace_id: None,
            selected_agent_id: None,
        };
        assert!(state.validate().is_err());
        let _ = fs::remove_dir_all(root);
    }
}
