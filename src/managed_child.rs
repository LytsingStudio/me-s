use std::{
    fs,
    io::{self, BufRead, Read},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    Result, codex_oauth,
    config::{GlobalConfig, global_config_path},
    managed_protocol::ManagedLaunchConfig,
    termination::TerminationSignals,
    ui_backend::workspace_ui_ports,
    webui::{self, ManagedWebAccess},
    workspace::Workspace,
    workspace_bootstrap,
};

const MAX_LAUNCH_CONFIG_BYTES: usize = 4096;

pub fn run(workspace_root: &Path) -> Result<()> {
    let stdin = io::stdin();
    let launch = read_launch_config(&mut stdin.lock())?;
    let termination = Arc::new(AtomicBool::new(false));
    watch_parent_pipe(Arc::clone(&termination))?;
    let signals = TerminationSignals::install()?;

    let global_path = global_config_path()?;
    let mut global = GlobalConfig::load(&global_path).map_err(|error| {
        format!(
            "global configuration is unavailable; run `me-s init` before starting me-gateway: {error}"
        )
    })?;
    codex_oauth::add_models_if_logged_in(&mut global)?;
    let local = workspace_bootstrap::load_or_create(workspace_root, &global.default_model)?;
    let canonical_workspace = fs::canonicalize(workspace_root)?;
    let workspace = Workspace::open(workspace_root, local, global.models)?;
    let (backend, commands) = workspace_ui_ports(workspace);
    let server = webui::start_managed(
        backend,
        commands,
        launch.port,
        ManagedWebAccess {
            token: launch.token,
            instance_nonce: launch.instance_nonce,
            workspace_path: canonical_workspace.to_string_lossy().into_owned(),
            terminate: Arc::clone(&termination),
        },
    )?;

    while !termination.load(Ordering::Acquire) && !signals.requested() {
        thread::park_timeout(Duration::from_millis(100));
    }
    drop(server);
    Ok(())
}

fn read_launch_config(input: &mut impl BufRead) -> Result<ManagedLaunchConfig> {
    let mut line = String::new();
    input
        .take((MAX_LAUNCH_CONFIG_BYTES + 1) as u64)
        .read_line(&mut line)?;
    if line.len() > MAX_LAUNCH_CONFIG_BYTES {
        return Err("managed launch configuration is too large".into());
    }
    if line.is_empty() {
        return Err("managed launch configuration is missing".into());
    }
    let launch: ManagedLaunchConfig = serde_json::from_str(line.trim_end())
        .map_err(|error| format!("invalid managed launch configuration: {error}"))?;
    launch.validate()?;
    Ok(launch)
}

fn watch_parent_pipe(termination: Arc<AtomicBool>) -> Result<()> {
    thread::Builder::new()
        .name("me-gateway-parent".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            let mut byte = [0_u8; 1];
            let _ = input.read(&mut byte);
            termination.store(true, Ordering::Release);
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_protocol::MANAGED_PROTOCOL_VERSION;
    use std::io::Cursor;

    #[test]
    fn launch_configuration_is_bounded_versioned_and_secret_bearing() {
        let json = serde_json::json!({
            "protocol_version": MANAGED_PROTOCOL_VERSION,
            "port": 41001,
            "token": "ab".repeat(32),
            "instance_nonce": "cd".repeat(16),
        });
        let launch = read_launch_config(&mut Cursor::new(format!("{json}\n"))).unwrap();
        assert_eq!(launch.port, 41001);

        assert!(read_launch_config(&mut Cursor::new("\n")).is_err());
        assert!(read_launch_config(&mut Cursor::new("x".repeat(5000))).is_err());
    }
}
