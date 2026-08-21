use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use me::config::{default_global_config, workspace_config_path};

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_me-gateway"))
        .arg("--webui-passkey")
        .arg("secret")
        .current_dir(root)
        .env("ME_CONFIG_HOME", config_home)
        .env("ME_GATEWAY_ME_S", env!("CARGO_BIN_EXE_me-s"))
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
    let deadline = Instant::now() + Duration::from_secs(20);
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
    let response = client()
        .post(format!("{address}/api/auth/login"))
        .json(&serde_json::json!({"password": "secret"}))
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

    assert_eq!(
        http.get(format!("{address}/api/gateway/state"))
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

    fs::write(root.join("not-a-directory"), b"file").unwrap();
    let listing = post_json(
        &address,
        "/api/gateway/directories",
        &cookie,
        serde_json::json!({"path": root}),
    );
    assert_eq!(listing["ok"], true);
    assert!(
        listing["directories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["name"] != "not-a-directory")
    );
    let roots = post_json(
        &address,
        "/api/gateway/directories",
        &cookie,
        serde_json::json!({"roots": true}),
    );
    assert_eq!(roots["ok"], true);
    assert_eq!(roots["root_selector"], true);
    assert!(roots["path"].is_null());
    let root_entries = roots["directories"].as_array().unwrap();
    assert!(!root_entries.is_empty());
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
