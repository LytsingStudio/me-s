use std::path::Path;

use crate::{
    Result,
    config::{WorkspaceConfig, workspace_config_path, workspace_edb_path},
    event::EventDataBase,
    toolbox,
};

pub fn create(workspace: &Path, default_model: &str) -> Result<WorkspaceConfig> {
    let config_path = workspace_config_path(workspace);
    let config = WorkspaceConfig::create(&config_path, default_model.to_owned())?;
    EventDataBase::open(&workspace_edb_path(workspace))?;
    toolbox::ensure_default_toolboxes(workspace)?;
    Ok(config)
}

pub fn load_or_create(workspace: &Path, default_model: &str) -> Result<WorkspaceConfig> {
    let config_path = workspace_config_path(workspace);
    if config_path.exists() {
        WorkspaceConfig::load(&config_path)
    } else {
        create(workspace, default_model)
    }
}
