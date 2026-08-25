use std::{
    fs,
    io::{BufRead, BufReader, Read as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use me::{
    config::{default_global_config, workspace_config_path},
    workspace_bootstrap,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "me-gateway-test-{}-{}",
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

fn prepare(with_config: bool) -> (TempDirectory, PathBuf, PathBuf) {
    let temporary = TempDirectory::new();
    let root = temporary.0.join("root");
    let config_home = temporary.0.join("config");
    fs::create_dir_all(&root).unwrap();
    if with_config {
        fs::create_dir_all(config_home.join("conf.d")).unwrap();
        let mut config = default_global_config(&config_home).unwrap();
        config.models[0].api_key = Some("gateway-visible-inline-key".into());
        config
            .save(&config_home.join("conf.d/models.toml"))
            .unwrap();
    }
    (temporary, root, config_home)
}

fn spawn_gateway(root: &Path, config_home: &Path) -> (Child, String) {
    spawn_gateway_with_me_s(
        root,
        config_home,
        Path::new(env!("CARGO_BIN_EXE_me-s")),
        Duration::from_secs(20),
    )
}

fn spawn_gateway_with_me_s(
    root: &Path,
    config_home: &Path,
    me_s: &Path,
    startup_timeout: Duration,
) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_me-gateway"))
        .arg("--webui-passkey")
        .arg("secret")
        .current_dir(root)
        .env("ME_CONFIG_HOME", config_home)
        .env("ME_GATEWAY_ME_S", me_s)
        .env(
            "ME_GATEWAY_TEST_PORT",
            (42_000 + (std::process::id() % 10_000) as u16).to_string(),
        )
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    let deadline = Instant::now() + startup_timeout;
    let mut address = None;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let lines = receiver.try_iter().collect::<Vec<_>>().join("\n");
            panic!("me-gateway exited before WebUI startup: {status}: {lines}");
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                assert!(
                    line.starts_with("ME Gateway: ")
                        || line.starts_with("warning:")
                        || line.starts_with("error:"),
                    "me-gateway emitted non-debug CLI output: {line}"
                );
                if let Some(value) = line.strip_prefix("ME Gateway: ") {
                    address = Some(value.replace("http://0.0.0.0:", "http://127.0.0.1:"));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(address) = address {
                    return (child, address);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("me-gateway closed stderr before WebUI startup");
            }
        }
        assert!(Instant::now() < deadline, "me-gateway startup timed out");
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn login(address: &str) -> String {
    let browser_port = address.rsplit(':').next().unwrap().parse().unwrap();
    login_from_browser_port(address, browser_port)
}

fn login_from_browser_port(address: &str, browser_port: u16) -> String {
    let response = client()
        .post(format!("{address}/api/auth/login"))
        .json(&serde_json::json!({
            "password": "secret",
            "browser_port": browser_port,
        }))
        .send()
        .unwrap();
    assert!(response.status().is_success());
    response
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn get_state(address: &str, cookie: &str) -> serde_json::Value {
    client()
        .get(format!("{address}/api/gateway/state"))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .unwrap()
        .json()
        .unwrap()
}

fn post_json(
    address: &str,
    path: &str,
    cookie: &str,
    value: serde_json::Value,
) -> serde_json::Value {
    client()
        .post(format!("{address}{path}"))
        .header(reqwest::header::COOKIE, cookie)
        .json(&value)
        .send()
        .unwrap()
        .json()
        .unwrap()
}

fn stop_gateway(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    child.kill().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "me-gateway exited with {status}");
            return;
        }
        assert!(Instant::now() < deadline, "me-gateway did not stop");
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn gateway_waits_for_one_slow_but_healthy_managed_child() {
    use std::os::unix::fs::PermissionsExt;

    let (temporary, root, config_home) = prepare(true);
    let wrapper = temporary.0.join("delayed-me-s.sh");
    let executable = env!("CARGO_BIN_EXE_me-s").replace('\'', "'\"'\"'");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\nsleep 16\nexec '{executable}' \"$@\"\n"),
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let started = Instant::now();
    let (mut gateway, address) =
        spawn_gateway_with_me_s(&root, &config_home, &wrapper, Duration::from_secs(30));
    assert!(started.elapsed() >= Duration::from_secs(15));
    let _cookie = login(&address);
    stop_gateway(&mut gateway);
}

#[test]
fn same_host_gateways_use_browser_ports_and_keep_sessions_simultaneously() {
    let (_first_temporary, first_root, first_config_home) = prepare(true);
    let (_second_temporary, second_root, second_config_home) = prepare(true);
    let (mut first_gateway, first_address) = spawn_gateway(&first_root, &first_config_home);
    let (mut second_gateway, second_address) = spawn_gateway(&second_root, &second_config_home);
    assert_ne!(first_address, second_address);

    let first_cookie = login_from_browser_port(&first_address, 80);
    let second_cookie = login_from_browser_port(&second_address, 443);
    assert!(first_cookie.starts_with("me_gateway_session_p80="));
    assert!(second_cookie.starts_with("me_gateway_session_p443="));
    assert_ne!(
        first_cookie.split_once('=').unwrap().0,
        second_cookie.split_once('=').unwrap().0
    );

    let first_token = first_cookie.split_once('=').unwrap().1;
    for invalid_name in [
        "me_gateway_session",
        "me_gateway_session_p0",
        "me_gateway_session_p65536",
        "me_webui_session_p80",
    ] {
        let status: serde_json::Value = client()
            .get(format!("{first_address}/api/auth/status"))
            .header(
                reqwest::header::COOKIE,
                format!("{invalid_name}={first_token}"),
            )
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(status["authenticated"], false);
    }

    let browser_cookies = format!("{first_cookie}; {second_cookie}");
    for address in [&first_address, &second_address] {
        let status: serde_json::Value = client()
            .get(format!("{address}/api/auth/status"))
            .header(reqwest::header::COOKIE, &browser_cookies)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(status["authenticated"], true);
    }

    stop_gateway(&mut first_gateway);
    stop_gateway(&mut second_gateway);
}

#[test]
fn gateway_authenticates_manages_persists_and_restores_workspaces() {
    let (_temporary, root, config_home) = prepare(true);
    let (mut gateway, address) = spawn_gateway(&root, &config_home);
    let http = client();

    let transcript_runtime = http
        .get(format!("{address}/transcript.js"))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .unwrap();
    assert!(transcript_runtime.contains("MeTranscript"));
    assert!(transcript_runtime.contains("reconcileHtmlChildren"));

    let session_terminal_runtime = http
        .get(format!("{address}/session-terminal.js"))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .unwrap();
    assert!(session_terminal_runtime.contains("MeSessionTerminal"));
    assert!(session_terminal_runtime.contains("/api/session-terminal/"));

    let remote_control_runtime = http
        .get(format!("{address}/remote-control.js"))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .unwrap();
    assert!(remote_control_runtime.contains("MeRemoteControl"));
    assert!(remote_control_runtime.contains("X-Me-Remote-Sequence"));
    assert!(!remote_control_runtime.contains("WebSocket"));

    assert_eq!(
        http.get(format!("{address}/api/gateway/state"))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.post(format!(
            "{address}/api/workspaces/chat/remote-control/status"
        ))
        .json(&serde_json::json!({"controller_token": null}))
        .send()
        .unwrap()
        .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let cookie = login(&address);
    let private_route = http
        .get(format!("{address}/api/workspaces/chat/managed/ready"))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .unwrap();
    assert_eq!(private_route.status(), reqwest::StatusCode::BAD_GATEWAY);
    let private_body = private_route.text().unwrap();
    assert!(!private_body.contains("nonce"));
    assert!(!private_body.contains("127.0.0.1"));
    let initial = get_state(&address, &cookie);
    assert_eq!(initial["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(initial["workspaces"][0]["id"], "chat");
    assert!(root.join(".me-gateway/state.json").exists());
    assert!(workspace_config_path(&root).exists());

    let remote_status: serde_json::Value = http
        .post(format!(
            "{address}/api/workspaces/chat/remote-control/status"
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&serde_json::json!({"controller_token": null}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(remote_status["ok"], true);
    assert_eq!(remote_status["active"], false);
    assert!(remote_status["supported"].is_boolean());
    assert_eq!(
        http.post(format!(
            "{address}/api/workspaces/chat/remote-control/status?private=true"
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&serde_json::json!({"controller_token": null}))
        .send()
        .unwrap()
        .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let unknown_remote = http
        .post(format!(
            "{address}/api/workspaces/chat/remote-control/unknown"
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&serde_json::json!({}))
        .send()
        .unwrap();
    assert_eq!(unknown_remote.status(), reqwest::StatusCode::BAD_GATEWAY);
    let unknown_remote_body = unknown_remote.text().unwrap();
    assert!(!unknown_remote_body.contains("127.0.0.1"));
    assert!(!unknown_remote_body.contains("token"));

    let child_snapshot: serde_json::Value = http
        .get(format!("{address}/api/workspaces/chat/snapshot"))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    let session_agent = child_snapshot["agents"][0]["id"].as_str().unwrap();
    let sync_request = serde_json::json!({
        "snapshot_revision": null, "agents": [], "selected_agent": session_agent,
        "terminal_session": null, "terminal_revision": null,
    });
    let identity = http
        .post(format!("{address}/api/workspaces/chat/sync"))
        .header(reqwest::header::COOKIE, &cookie)
        .header(reqwest::header::ACCEPT_ENCODING, "gzip;q=0, *;q=1")
        .json(&sync_request)
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        identity
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_none()
    );
    assert_eq!(
        identity.headers().get(reqwest::header::VARY).unwrap(),
        "Accept-Encoding"
    );
    let identity_body = identity.bytes().unwrap();
    let compressed = http
        .post(format!("{address}/api/workspaces/chat/sync"))
        .header(reqwest::header::COOKIE, &cookie)
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .json(&sync_request)
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        compressed
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .unwrap(),
        "gzip"
    );
    assert_eq!(
        compressed.headers().get(reqwest::header::VARY).unwrap(),
        "Accept-Encoding"
    );
    let compressed_body = compressed.bytes().unwrap();
    let mut decoder = flate2::read::GzDecoder::new(compressed_body.as_ref());
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&decoded).unwrap(),
        serde_json::from_slice::<serde_json::Value>(&identity_body).unwrap()
    );

    assert_eq!(
        http.post(format!(
            "{address}/api/workspaces/chat/session-terminal/{session_agent}/read"
        ))
        .json(&serde_json::json!({"cursor": null}))
        .send()
        .unwrap()
        .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let native_terminal: serde_json::Value = http
        .post(format!(
            "{address}/api/workspaces/chat/session-terminal/{session_agent}/read"
        ))
        .header(reqwest::header::COOKIE, &cookie)
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
        http.post(format!(
            "{address}/api/workspaces/chat/session-terminal/{session_agent}/read?cursor=0"
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&serde_json::json!({"cursor": null}))
        .send()
        .unwrap()
        .status(),
        reqwest::StatusCode::NOT_FOUND
    );

    let settings: serde_json::Value = http
        .get(format!("{address}/api/gateway/settings"))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        settings["models"][0]["api_key"],
        "gateway-visible-inline-key"
    );
    assert!(settings["models"][0].get("has_inline_api_key").is_none());
    assert!(settings["models"][0].get("clear_inline_api_key").is_none());

    let default_model = settings["default_model"].as_str().unwrap().to_owned();
    fs::write(root.join("not-a-directory"), b"file").unwrap();
    fs::create_dir(root.join("folder-2")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("folder-2"), root.join("folder-link")).unwrap();
    let listing = post_json(
        &address,
        "/api/gateway/directories",
        &cookie,
        serde_json::json!({"path": root}),
    );
    assert_eq!(listing["ok"], true);
    let entries = listing["entries"].as_array().unwrap();
    let file = entries
        .iter()
        .find(|entry| entry["name"] == "not-a-directory")
        .unwrap();
    assert_eq!(file["kind"], "file");
    assert_eq!(file["size_bytes"], 4);
    assert!(file["modified_at_ms"].is_number());
    let folder = entries
        .iter()
        .find(|entry| entry["name"] == "folder-2")
        .unwrap();
    assert_eq!(folder["kind"], "directory");
    assert!(folder["size_bytes"].is_null());
    #[cfg(unix)]
    assert!(entries.iter().all(|entry| entry["name"] != "folder-link"));

    let roots = post_json(
        &address,
        "/api/gateway/directories",
        &cookie,
        serde_json::json!({"roots": true}),
    );
    assert_eq!(roots["ok"], true);
    assert_eq!(roots["root_selector"], true);
    assert!(roots["path"].is_null());
    let root_entries = roots["entries"].as_array().unwrap();
    assert!(!root_entries.is_empty());
    assert!(
        root_entries
            .iter()
            .all(|entry| { entry["kind"] == "drive" && entry["size_bytes"].is_null() })
    );
    #[cfg(not(windows))]
    assert_eq!(root_entries[0]["path"], "/");
    #[cfg(windows)]
    {
        let readable_root = root_entries
            .iter()
            .find(|entry| {
                entry["path"]
                    .as_str()
                    .is_some_and(|path| fs::read_dir(path).is_ok())
            })
            .expect("Windows must expose at least one readable logical drive");
        let drive_listing = post_json(
            &address,
            "/api/gateway/directories",
            &cookie,
            serde_json::json!({"path": readable_root["path"]}),
        );
        assert_eq!(drive_listing["ok"], true);
        assert_eq!(drive_listing["root_selector"], false);
        assert_eq!(drive_listing["parent_is_root_selector"], true);
    }

    let rejected_file = post_json(
        &address,
        "/api/gateway/directories",
        &cookie,
        serde_json::json!({"path": root.join("not-a-directory")}),
    );
    assert_eq!(rejected_file["ok"], false);
    let new_folder = post_json(
        &address,
        "/api/gateway/directories/create",
        &cookie,
        serde_json::json!({"parent": root, "name": "created-folder"}),
    );
    assert_eq!(new_folder["ok"], true);
    assert!(root.join("created-folder").is_dir());
    let rejected_folder = post_json(
        &address,
        "/api/gateway/directories/create",
        &cookie,
        serde_json::json!({"parent": root, "name": "nested/folder"}),
    );
    assert_eq!(rejected_folder["ok"], false);

    let ordinary = root.join("ordinary");
    fs::create_dir(&ordinary).unwrap();
    let requires_initialization = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": ordinary}),
    );
    assert_eq!(requires_initialization["status"], "requires_initialization");
    assert!(!ordinary.join(".me").exists());
    assert_eq!(
        get_state(&address, &cookie)["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let initialized = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": ordinary, "initialize": true}),
    );
    assert_eq!(initialized["status"], "opened");
    assert!(workspace_config_path(&ordinary).exists());
    let ordinary_id = initialized["workspace_id"].as_str().unwrap();
    assert_eq!(
        get_state(&address, &cookie)["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        post_json(
            &address,
            &format!("/api/gateway/workspaces/{ordinary_id}/close"),
            &cookie,
            serde_json::json!({}),
        )["ok"],
        true
    );

    let invalid = root.join("invalid");
    fs::create_dir_all(invalid.join(".me")).unwrap();
    let invalid_open = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": invalid}),
    );
    assert_eq!(invalid_open["ok"], false);
    let invalid_initialize = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": invalid, "initialize": true}),
    );
    assert_eq!(invalid_initialize["ok"], false);
    assert!(!workspace_config_path(&invalid).exists());

    let invalid_file = root.join("invalid-file");
    fs::create_dir(&invalid_file).unwrap();
    fs::write(invalid_file.join(".me"), b"preserve").unwrap();
    let invalid_file_open = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": invalid_file, "initialize": true}),
    );
    assert_eq!(invalid_file_open["ok"], false);
    assert_eq!(fs::read(invalid_file.join(".me")).unwrap(), b"preserve");

    let raced = root.join("raced");
    fs::create_dir(&raced).unwrap();
    let raced_missing = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": raced}),
    );
    assert_eq!(raced_missing["status"], "requires_initialization");
    workspace_bootstrap::create(&raced, &default_model).unwrap();
    let raced_open = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": raced, "initialize": true}),
    );
    assert_eq!(raced_open["status"], "opened");
    let raced_id = raced_open["workspace_id"].as_str().unwrap();
    assert_eq!(
        post_json(
            &address,
            &format!("/api/gateway/workspaces/{raced_id}/close"),
            &cookie,
            serde_json::json!({}),
        )["ok"],
        true
    );

    let created = post_json(
        &address,
        "/api/gateway/workspaces/create",
        &cookie,
        serde_json::json!({"parent": root, "name": "work-a"}),
    );
    assert_eq!(created["ok"], true);
    let workspace_id = created["workspace_id"].as_str().unwrap().to_owned();
    let workspace_path = root.join("work-a");
    assert!(workspace_config_path(&workspace_path).exists());

    let duplicate = post_json(
        &address,
        "/api/gateway/workspaces/open",
        &cookie,
        serde_json::json!({"path": workspace_path}),
    );
    assert_eq!(duplicate["status"], "opened");
    assert_eq!(duplicate["workspace_id"], workspace_id);
    assert_eq!(
        get_state(&address, &cookie)["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    drop(http);
    thread::sleep(Duration::from_millis(200));
    assert!(gateway.try_wait().unwrap().is_none());
    stop_gateway(&mut gateway);

    let (mut restarted, address) = spawn_gateway(&root, &config_home);
    let cookie = login(&address);
    let restored = get_state(&address, &cookie);
    assert_eq!(restored["workspaces"].as_array().unwrap().len(), 2);
    assert_eq!(restored["workspaces"][1]["id"], workspace_id);
    let closed = post_json(
        &address,
        &format!("/api/gateway/workspaces/{workspace_id}/close"),
        &cookie,
        serde_json::json!({}),
    );
    assert_eq!(closed["ok"], true);
    assert!(workspace_path.is_dir());
    assert_eq!(
        get_state(&address, &cookie)["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    stop_gateway(&mut restarted);
}

#[test]
fn missing_global_configuration_fails_before_gateway_state_or_webui() {
    let (_temporary, root, config_home) = prepare(false);
    let mut child = Command::new(env!("CARGO_BIN_EXE_me-gateway"))
        .current_dir(&root)
        .env("ME_CONFIG_HOME", &config_home)
        .env("ME_GATEWAY_ME_S", env!("CARGO_BIN_EXE_me-s"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "invalid gateway startup did not fail"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    assert!(!root.join(".me-gateway").exists());
    assert!(!workspace_config_path(&root).exists());
}
