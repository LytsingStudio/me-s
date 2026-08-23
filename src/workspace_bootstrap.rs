use std::{fs, io::ErrorKind, path::Path};

use crate::{
    Result,
    config::{WorkspaceConfig, workspace_config_path, workspace_edb_path},
    event::EventDataBase,
    toolbox,
};

pub enum WorkspaceBootstrap {
    Missing,
    Ready(WorkspaceConfig),
}

pub fn inspect(workspace: &Path) -> Result<WorkspaceBootstrap> {
    let marker = workspace.join(".me");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(WorkspaceBootstrap::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("workspace metadata {} is not a directory", marker.display()).into());
    }
    WorkspaceConfig::load(&workspace_config_path(workspace)).map(WorkspaceBootstrap::Ready)
}

pub fn load(workspace: &Path) -> Result<WorkspaceConfig> {
    match inspect(workspace)? {
        WorkspaceBootstrap::Missing => Err(format!(
            "workspace is not initialized: {}",
            workspace.join(".me").display()
        )
        .into()),
        WorkspaceBootstrap::Ready(config) => Ok(config),
    }
}

pub fn initialize_if_missing(workspace: &Path, default_model: &str) -> Result<WorkspaceConfig> {
    match inspect(workspace)? {
        WorkspaceBootstrap::Missing => create_new(workspace, default_model),
        WorkspaceBootstrap::Ready(config) => Ok(config),
    }
}

pub fn create(workspace: &Path, default_model: &str) -> Result<WorkspaceConfig> {
    create_contents(workspace, default_model)
}

pub fn create_new(workspace: &Path, default_model: &str) -> Result<WorkspaceConfig> {
    let marker = workspace.join(".me");
    if let Err(error) = fs::create_dir(&marker) {
        if error.kind() == ErrorKind::AlreadyExists {
            return Err(format!("workspace metadata {} already exists", marker.display()).into());
        }
        return Err(error.into());
    }
    create_contents(workspace, default_model)
}

fn create_contents(workspace: &Path, default_model: &str) -> Result<WorkspaceConfig> {
    let config_path = workspace_config_path(workspace);
    let config = WorkspaceConfig::create(&config_path, default_model.to_owned())?;
    EventDataBase::open(&workspace_edb_path(workspace))?;
    toolbox::ensure_default_toolboxes(workspace)?;
    Ok(config)
}
