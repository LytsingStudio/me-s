use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use me::{
    config::{default_global_config, workspace_config_path, workspace_edb_path},
    managed_protocol::{
        MANAGED_PROTOCOL_VERSION, MANAGED_READY_PATH, MANAGED_SHUTDOWN_PATH, ManagedLaunchConfig,
        ManagedReadyResponse, bearer_header_value,
    },
    workspace_bootstrap,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "me-managed-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn prepare(initialize: bool) -> (TempDirectory, PathBuf, PathBuf) {
    let root = TempDirectory::new("workspace");
    let workspace = root.0.join("workspace");
    let config_home = root.0.join("config");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(config_home.join("conf.d")).unwrap();
    let global = default_global_config(&config_home).unwrap();
    global
        .save(&config_home.join("conf.d/models.toml"))
        .unwrap();
    if initialize {
        workspace_bootstrap::create_new(&workspace, &global.default_model).unwrap();
    }
    (root, workspace, config_home)
}

fn available_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_managed(
    workspace: &Path,
    config_home: &Path,
    port: u16,
) -> (Child, ChildStdin, ManagedLaunchConfig) {
    let launch = ManagedLaunchConfig {
        protocol_version: MANAGED_PROTOCOL_VERSION,
        port,
        token: "ab".repeat(32),
        instance_nonce: "cd".repeat(16),
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_me-s"));
    command
        .arg("__gateway-child")
        .current_dir(workspace)
        .env("ME_CONFIG_HOME", config_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    serde_json::to_writer(&mut input, &launch).unwrap();
    input.write_all(b"\n").unwrap();
    input.flush().unwrap();
    (child, input, launch)
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap()
}

fn wait_until_ready(
    child: &mut Child,
    port: u16,
    launch: &ManagedLaunchConfig,
) -> ManagedReadyResponse {
    let client = client();
    let address = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let stderr = child
                .stderr
                .take()
                .map(|mut stderr| {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
                    text
                })
                .unwrap_or_default();
            panic!("managed me-s exited before readiness: {status}: {stderr}");
        }
        if let Ok(response) = client
            .get(format!("{address}{MANAGED_READY_PATH}"))
            .header(
                reqwest::header::AUTHORIZATION,
                bearer_header_value(&launch.token),
            )
            .send()
            && response.status().is_success()
        {
            return response.json().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "managed me-s readiness timed out"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "managed me-s did not exit");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn managed_child_loads_authenticates_reports_identity_and_shuts_down() {
    let (_root, workspace, config_home) = prepare(true);
    let port = available_port();
    let (mut child, input, launch) = spawn_managed(&workspace, &config_home, port);
    let ready = wait_until_ready(&mut child, port, &launch);
    let address = format!("http://127.0.0.1:{port}");
    let client = client();

    assert!(ready.ok && ready.ready);
    assert_eq!(ready.protocol_version, MANAGED_PROTOCOL_VERSION);
    assert_eq!(ready.product_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(ready.instance_nonce, launch.instance_nonce);
    assert_eq!(
        PathBuf::from(ready.workspace_path),
        fs::canonicalize(&workspace).unwrap()
    );
    assert_eq!(
        client
            .get(format!("{address}{MANAGED_READY_PATH}"))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{address}/"))
            .header(
                reqwest::header::AUTHORIZATION,
                bearer_header_value(&launch.token),
            )
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    for path in [
        "/api/health",
        "/api/api-activity/main",
        "/api/terminal-backend/main",
        "/api/terminal/main/session",
    ] {
        assert_eq!(
            client
                .get(format!("{address}{path}"))
                .header(
                    reqwest::header::AUTHORIZATION,
                    bearer_header_value(&launch.token),
                )
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND,
            "managed route {path} must stay private",
        );
    }
    let sync: serde_json::Value = client
        .post(format!("{address}/api/sync"))
        .header(
            reqwest::header::AUTHORIZATION,
            bearer_header_value(&launch.token),
        )
        .json(&serde_json::json!({
            "snapshot_revision": null,
            "agents": [],
            "selected_agent": null,
            "terminal_session": null,
            "terminal_revision": null,
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(sync["ok"], true);
    let agent_id = sync["snapshot"]["agents"][0]["id"].as_str().unwrap();
    assert_eq!(
        client
            .post(format!("{address}/api/session-terminal/{agent_id}/read"))
            .json(&serde_json::json!({"cursor": null}))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let native_terminal: serde_json::Value = client
        .post(format!("{address}/api/session-terminal/{agent_id}/read"))
        .header(
            reqwest::header::AUTHORIZATION,
            bearer_header_value(&launch.token),
        )
        .json(&serde_json::json!({"cursor": null}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(native_terminal["ok"], true);
    assert!(native_terminal["events"].is_array());
    assert_eq!(
        client
            .post(format!(
                "{address}/api/session-terminal/{agent_id}/read?cursor=0"
            ))
            .header(
                reqwest::header::AUTHORIZATION,
                bearer_header_value(&launch.token),
            )
            .json(&serde_json::json!({"cursor": null}))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert!(workspace_config_path(&workspace).exists());
    assert!(workspace_edb_path(&workspace).exists());

    let response = client
        .post(format!("{address}{MANAGED_SHUTDOWN_PATH}"))
        .header(
            reqwest::header::AUTHORIZATION,
            bearer_header_value(&launch.token),
        )
        .send()
        .unwrap();
    assert!(response.status().is_success());
    drop(input);
    assert!(wait_for_exit(&mut child).success());
}

#[test]
fn managed_child_exits_when_the_parent_control_pipe_closes() {
    let (_root, workspace, config_home) = prepare(true);
    let port = available_port();
    let (mut child, input, launch) = spawn_managed(&workspace, &config_home, port);
    wait_until_ready(&mut child, port, &launch);
    drop(input);
    assert!(wait_for_exit(&mut child).success());
}

#[test]
fn managed_child_rejects_an_uninitialized_workspace_without_creating_it() {
    let (_root, workspace, config_home) = prepare(false);
    let port = available_port();
    let (mut child, _input, _launch) = spawn_managed(&workspace, &config_home, port);
    let status = wait_for_exit(&mut child);
    let mut stderr = String::new();
    if let Some(mut output) = child.stderr.take() {
        std::io::Read::read_to_string(&mut output, &mut stderr).unwrap();
    }
    assert!(!status.success());
    assert!(!workspace.join(".me").exists());
    assert!(stderr.contains("workspace is not initialized"), "{stderr}");
}

#[test]
fn managed_child_never_drifts_from_or_exposes_an_occupied_exact_port() {
    let (_root, workspace, config_home) = prepare(true);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (mut child, input, _launch) = spawn_managed(&workspace, &config_home, port);
    drop(input);
    let status = wait_for_exit(&mut child);
    let mut stderr = String::new();
    if let Some(mut output) = child.stderr.take() {
        std::io::Read::read_to_string(&mut output, &mut stderr).unwrap();
    }
    assert!(!status.success());
    assert_eq!(listener.local_addr().unwrap().port(), port);
    assert!(!stderr.contains(&port.to_string()), "{stderr}");
}
