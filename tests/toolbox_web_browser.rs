use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

fn python_312() -> Option<(OsString, Vec<OsString>)> {
    let mut candidates = Vec::new();
    candidates.push((OsString::from("python3.12"), Vec::new()));
    #[cfg(windows)]
    candidates.push((OsString::from("py"), vec![OsString::from("-3.12")]));
    if let Ok(output) = Command::new("pyenv").args(["prefix", "3.12"]).output()
        && output.status.success()
        && let Ok(prefix) = String::from_utf8(output.stdout)
    {
        let prefix = PathBuf::from(prefix.trim());
        for path in [
            prefix.join("bin/python3.12"),
            prefix.join("bin/python"),
            prefix.join("python.exe"),
        ] {
            if path.is_file() {
                candidates.push((path.into_os_string(), Vec::new()));
            }
        }
    }
    candidates.push((OsString::from("python"), Vec::new()));
    candidates.into_iter().find(|(program, arguments)| {
        Command::new(program)
            .args(arguments)
            .args([
                "-c",
                "import sys; raise SystemExit(0 if sys.version_info[:2] == (3, 12) else 1)",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn temporary_directory(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "me-web-browser-{name}-{}-{nonce}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

struct ToolboxProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl ToolboxProcess {
    fn start(workspace: &Path, script: &Path, config_home: &Path) -> Self {
        Self::start_with_environment(workspace, script, config_home, &[])
    }

    fn start_with_environment(
        workspace: &Path,
        script: &Path,
        config_home: &Path,
        extra: &[(&str, &str)],
    ) -> Self {
        let Some((python, arguments)) = python_312() else {
            panic!("WebBrowser toolbox integration test requires Python 3.12");
        };
        let mut command = Command::new(python);
        command
            .args(arguments)
            .arg(script)
            .current_dir(workspace)
            .env("ME_CONFIG_HOME", config_home)
            .env("ME_WEB_BROWSER_TEST_HEADLESS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in extra {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn request(&mut self, mut request: Value) -> (Vec<Value>, Value) {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = Value::from(id);
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let mut updates = Vec::new();
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            if line.is_empty() {
                let mut stderr = String::new();
                self.child
                    .stderr
                    .as_mut()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("WebBrowser.py closed before responding to {request}: {stderr}");
            }
            let frame: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(frame["id"], id);
            if frame["type"] == "update" {
                updates.push(frame);
                continue;
            }
            return (updates, frame);
        }
    }

    fn query(&mut self, command: &str, tool: Option<&str>) -> Value {
        let mut request = json!({"cmd": command});
        if let Some(tool) = tool {
            request["tool"] = Value::String(tool.to_owned());
        }
        self.request(request).1
    }

    fn execute(&mut self, tool: &str, input: Value) -> (Vec<Value>, Value) {
        self.request(json!({"cmd":"execute", "tool":tool, "input":input}))
    }

    fn finish(mut self) {
        drop(self.stdin);
        let status = self.child.wait().unwrap();
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(status.success(), "WebBrowser.py failed: {stderr}");
    }
}

fn generated_web_browser_toolbox(workspace: &Path) -> PathBuf {
    me::toolbox::ensure_default_toolboxes(workspace)
        .unwrap()
        .parent()
        .unwrap()
        .join("WebBrowser.py")
}

#[test]
fn web_browser_bootstrap_recovers_abandoned_lock_and_bounds_create() {
    let Some((python, arguments)) = python_312() else {
        eprintln!("skipping WebBrowser bootstrap test because Python 3.12 is unavailable");
        return;
    };
    let workspace = temporary_directory("bootstrap-workspace");
    let script = generated_web_browser_toolbox(&workspace);
    let lock_root = temporary_directory("bootstrap-lock");
    let probe = r#"
import importlib.util
import json
import os
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("me_web_browser_test", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

root = Path(sys.argv[2])
lock = root / "install.lock"
lock.mkdir()
(lock / "owner.json").write_text(json.dumps({"pid": 2147483000, "time": 0}), encoding="utf-8")
updates = []
with module.install_lock(root, updates.append):
    owner = json.loads((lock / "owner.json").read_text(encoding="utf-8"))
    assert owner["pid"] == os.getpid()
assert not lock.exists()

os.environ["ME_WEB_BROWSER_TEST_CREATE_TIMEOUT_MS"] = "321"
assert module.request_hard_timeout_ms({"cmd": "execute", "tool": "Create", "input": {}}) == 321
assert module.request_hard_timeout_ms({"cmd": "execute", "tool": "RequireHumanAction", "input": {}}) is None

os.environ["ME_CONFIG_HOME"] = str(root / "global")
runtime = module.DependencyRuntime()
runtime.browser_marker.parent.mkdir(parents=True, exist_ok=True)
runtime.browser_marker.write_text("false success", encoding="utf-8")
assert not runtime.browser_is_valid(), "a marker without a browser must not be trusted"

original_run = module.subprocess.run
module.subprocess.run = lambda *args, **kwargs: module.subprocess.CompletedProcess(
    args[0], 0, "repository sync failed", ""
)
runtime.browser_executable = lambda: None
try:
    runtime._install_browser()
except module.ToolError as error:
    assert error.code == "browser_install_failed"
    assert "reported success without installing" in error.message
    assert "repository sync failed" in error.message
else:
    raise AssertionError("a false-success Camoufox fetch was accepted")
finally:
    module.subprocess.run = original_run
print("ok")
"#;
    let output = Command::new(python)
        .args(arguments)
        .args(["-c", probe])
        .arg(&script)
        .arg(&lock_root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bootstrap probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(lock_root).unwrap();
}

fn output(frame: &Value) -> &Value {
    assert_eq!(frame["type"], "result", "{frame}");
    &frame["output"]
}

fn aria_ref(tree: &str, accessible_name: &str) -> String {
    let line = tree
        .lines()
        .find(|line| line.contains(accessible_name) && line.contains("[ref="))
        .unwrap_or_else(|| panic!("ARIA snapshot has no ref for {accessible_name:?}:\n{tree}"));
    let start = line.find("[ref=").unwrap() + 5;
    let end = line[start..].find(']').unwrap() + start;
    let value = &line[start..end];
    assert!(value.chars().last().is_some_and(|ch| ch.is_ascii_digit()));
    assert!(value.contains('e'), "unexpected ARIA ref {value:?}");
    assert!(
        value
            .chars()
            .all(|ch| ch == 'e' || ch == 'f' || ch.is_ascii_digit()),
        "unexpected ARIA ref {value:?}"
    );
    value.to_owned()
}

#[test]
fn generated_web_browser_describes_the_raw_snapshot_protocol_without_installing() {
    let workspace = temporary_directory("metadata-workspace");
    let config = temporary_directory("metadata-global");
    let script = generated_web_browser_toolbox(&workspace);
    let source = fs::read_to_string(&script).unwrap();

    assert!(source.contains("state.page.aria_snapshot("));
    assert!(source.contains("mode=\"ai\""));
    assert!(source.contains("boxes=True"));
    assert!(source.contains("aria-ref="));
    assert!(source.contains("operation_timeout"));
    assert!(source.contains("executable = self.dependencies.browser_executable()"));
    assert!(source.contains("browser=CAMOUFOX_BROWSER_VERSION"));
    assert!(source.contains("Camoufox reported success without installing"));
    assert!(!source.contains("READABLE_MARKDOWN_JS"));
    assert!(!source.contains("INTERACTIVE_SELECTOR"));
    assert!(!source.contains("save_img"));
    assert!(!source.contains("wait_stable"));
    assert!(!source.contains("\"Wait\","));

    let mut toolbox = ToolboxProcess::start(&workspace, &script, &config);
    let tools = toolbox.query("getTools", None);
    assert_eq!(tools["type"], "result");
    let names = tools["output"].as_array().unwrap();
    assert_eq!(names.len(), 11);
    assert_eq!(names[0], "Create");
    assert_eq!(names[6], "RequireHumanAction");
    assert_eq!(names[7], "Snapshot");
    assert_eq!(names[10], "Close");
    assert!(!names.iter().any(|name| name == "Wait"));

    let brief = toolbox.query("getBrief", None);
    let brief = brief["output"].as_str().unwrap();
    assert!(brief.contains("ARIA accessibility tree verbatim"));
    assert!(brief.contains("sole page-content observation tool"));
    assert!(brief.contains("Create, Navigate, Snapshot(kind=text)"));
    assert!(brief.contains("refresh the text Snapshot after navigation"));
    assert!(brief.contains("Native JavaScript dialogs are dismissed automatically"));
    assert!(brief.contains("operation_timeout"));
    assert!(brief.contains("discard every previous page_id and element_id"));
    assert!(brief.contains("Google first"));
    assert!(brief.find("Google first") < brief.find("Baidu second"));

    for name in names {
        let name = name.as_str().unwrap();
        for command in [
            "getInputSchema",
            "getOutputSchema",
            "getInstructions",
            "getRoute",
            "getExamples",
        ] {
            let frame = toolbox.query(command, Some(name));
            assert_eq!(frame["type"], "result", "{command} failed for {name}");
            if command.contains("Schema") {
                assert_eq!(frame["output"]["type"], "object");
            } else {
                assert!(!frame["output"].as_str().unwrap().is_empty());
            }
        }
    }

    let create_output = toolbox.query("getOutputSchema", Some("Create"));
    assert_eq!(
        create_output["output"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["page_id"]
    );
    for action in ["Navigate", "Click", "Type", "Press", "Scroll", "Back"] {
        let schema = toolbox.query("getInputSchema", Some(action));
        assert!(
            schema["output"]["properties"]
                .get("max_wait_time")
                .is_none(),
            "{action} retained max_wait_time"
        );
        let output_schema = toolbox.query("getOutputSchema", Some(action));
        assert!(
            output_schema["output"]["properties"]
                .get("snapshot")
                .is_none(),
            "{action} still returns a snapshot"
        );
    }
    assert_eq!(
        toolbox.query("getInputSchema", Some("Click"))["output"]["properties"]["element_id"]["pattern"],
        "^(?:f[0-9]+)*e[0-9]+$"
    );

    let snapshot_input = toolbox.query("getInputSchema", Some("Snapshot"));
    assert_eq!(
        snapshot_input["output"]["required"],
        json!(["page_id", "wait_ms", "kind"])
    );
    assert_eq!(
        snapshot_input["output"]["properties"]["wait_ms"]["minimum"],
        1_000
    );
    assert_eq!(
        snapshot_input["output"]["properties"]["wait_ms"]["maximum"],
        60_000
    );
    assert_eq!(
        snapshot_input["output"]["properties"]["kind"]["enum"],
        json!(["text", "screen", "both"])
    );
    let snapshot_output = toolbox.query("getOutputSchema", Some("Snapshot"));
    assert_eq!(
        snapshot_output["output"]["properties"]["accessibility_tree"]["type"],
        json!(["string", "object"])
    );
    assert_eq!(
        snapshot_output["output"]["properties"]["screen_path"]["type"],
        "string"
    );
    assert!(
        snapshot_output["output"]["properties"]
            .get("screen")
            .is_none()
    );
    let human_output = toolbox.query("getOutputSchema", Some("RequireHumanAction"));
    assert!(
        human_output["output"]["properties"]["target_page"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("page"))
    );
    assert!(
        human_output["output"]["properties"]["opened_pages"]["items"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("page"))
    );
    let instructions = toolbox.query("getInstructions", Some("Snapshot"));
    let instructions = instructions["output"].as_str().unwrap();
    assert!(instructions.contains("fixed delay"));
    assert!(instructions.contains("not a stability heuristic"));
    assert!(instructions.contains("verbatim"));
    assert!(instructions.contains("[ref=e"));
    assert!(instructions.contains("[box="));
    assert!(instructions.contains("only WebBrowser tool that returns page content"));
    assert!(instructions.contains("state is the sampled document.readyState"));
    assert!(instructions.contains("after navigation or a structural page change"));
    assert!(instructions.contains("returns only screen_path"));
    assert!(instructions.contains("Image.View"));
    assert!(instructions.contains("File.Stat"));
    assert!(instructions.contains("File.Delete"));
    assert!(instructions.contains("without image input may still create a screenshot"));

    let click = toolbox.query("getInstructions", Some("Click"));
    let click = click["output"].as_str().unwrap();
    assert!(click.contains("rendered DOM element once in its owning frame"));
    assert!(click.contains("not trusted human input"));
    assert!(click.contains("registered immediately"));
    assert!(click.contains("use Pages later"));
    assert!(click.contains("Native JavaScript dialogs are dismissed automatically"));

    let append = toolbox.query("getInstructions", Some("Type"));
    assert!(
        append["output"]
            .as_str()
            .unwrap()
            .contains("current caret or selection")
    );

    let pages = toolbox.query("getInstructions", Some("Pages"));
    assert!(
        pages["output"]
            .as_str()
            .unwrap()
            .contains("only currently open page identifiers")
    );

    let (_, active_pages) = toolbox.execute("__activePages", json!({}));
    assert_eq!(
        output(&active_pages),
        &json!({"pages": [], "active_page_id": null})
    );

    assert!(!config.join("runtimes").exists());
    assert!(!config.join("browsers").exists());
    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(config).unwrap();
}

#[test]
fn malformed_and_legacy_requests_fail_before_browser_installation() {
    let workspace = temporary_directory("errors-workspace");
    let config = temporary_directory("errors-global");
    let script = generated_web_browser_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script, &config);

    let unknown = toolbox.query("getInputSchema", Some("Missing"));
    assert_eq!(unknown["type"], "error");
    assert_eq!(unknown["error"]["code"], "unknown_tool");
    let wait = toolbox.query("getInputSchema", Some("Wait"));
    assert_eq!(wait["error"]["code"], "unknown_tool");
    let unsupported = toolbox.request(json!({"cmd":"unsupported"})).1;
    assert_eq!(unsupported["type"], "error");
    assert!(!config.join("runtimes").exists());

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(config).unwrap();
}

#[test]
fn web_browser_jsonl_is_utf8_even_when_the_host_requests_gbk() {
    let workspace = temporary_directory("gbk-workspace");
    let config = temporary_directory("gbk-global");
    let script = generated_web_browser_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start_with_environment(
        &workspace,
        &script,
        &config,
        &[("PYTHONIOENCODING", "gbk")],
    );
    let marker = "页面\u{e687}›";
    let (_, response) = toolbox.request(json!({
        "cmd":"getInputSchema",
        "tool":marker
    }));
    assert_eq!(response["type"], "error");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(marker)
    );
    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(config).unwrap();
}

#[test]
fn me_loads_web_browser_as_an_independent_default_toolbox() {
    let workspace = temporary_directory("runtime-workspace");
    let web_browser = generated_web_browser_toolbox(&workspace);
    fs::remove_file(web_browser.parent().unwrap().join("Terminal.py")).unwrap();
    fs::remove_file(web_browser.parent().unwrap().join("File.py")).unwrap();
    fs::write(
        web_browser.parent().unwrap().join("Desktop.py"),
        r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    output = [] if request["cmd"] == "getTools" else ""
    print(json.dumps({"id": request["id"], "type": "result", "output": output}), flush=True)
"#,
    )
    .unwrap();
    let runtime = me::toolbox::ToolboxRuntime::load(&workspace).unwrap();
    let names = runtime
        .catalog()
        .tools()
        .iter()
        .map(|tool| tool.full_name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"WebBrowser.Create"));
    assert!(names.contains(&"WebBrowser.Snapshot"));
    assert!(names.contains(&"WebBrowser.RequireHumanAction"));
    assert!(!names.contains(&"WebBrowser.Wait"));
    assert!(runtime.catalog().prompt().contains("ARIA snapshot"));
    assert!(!workspace.join(".me/browsers").exists());
    drop(runtime);
    fs::remove_dir_all(workspace).unwrap();
}

struct LocalSite {
    base_url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LocalSite {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("local browser test server failed: {error}"),
                }
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{}", address.port()),
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for LocalSite {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn serve(mut stream: TcpStream) {
    let mut request = [0_u8; 8192];
    let read = stream.read(&mut request).unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let (status, content_type, body) = match path.split('?').next().unwrap_or(path) {
        "/" => (
            "200 OK",
            "text/html; charset=utf-8",
            r#"<!doctype html>
<html><head><title>ARIA fixture</title></head>
<body>
<main>
  <h1>Rendered fixture 世界</h1>
  <label>Search field <input aria-label="Search field" value="seed" onkeydown="if(event.key==='Enter') document.getElementById('key-status').textContent='Enter received'"></label>
  <button aria-label="Change status" onclick="document.getElementById('status').textContent='Changed by click'">Change</button>
  <button aria-label="Open popup" onclick="window.open('/popup', '_blank')">Popup</button>
  <button aria-label="Show dialog" onclick="alert('fixture dialog')">Dialog</button>
  <button aria-label="Busy control" onclick="const end=Date.now()+60000; while(Date.now()<end){}">Busy</button>
  <p id="status">Initial status</p>
  <p id="key-status">No key</p>
  <p id="scroll-status">Not scrolled</p>
  <div style="height:1800px"></div>
  <iframe title="Embedded fixture" src="/frame"></iframe>
  <script>addEventListener('scroll',()=>document.getElementById('scroll-status').textContent='Scrolled viewport',{once:true})</script>
</main>
</body></html>"#,
        ),
        "/popup" => (
            "200 OK",
            "text/html; charset=utf-8",
            "<html><head><title>Popup</title></head><body><h1>Popup content</h1></body></html>",
        ),
        "/frame" => (
            "200 OK",
            "text/html; charset=utf-8",
            "<html><body><button aria-label=\"Frame action\" onclick=\"document.body.append('Frame clicked')\">Frame action</button></body></html>",
        ),
        "/second" => (
            "200 OK",
            "text/html; charset=utf-8",
            "<html><head><title>Second</title></head><body><h1>Second page</h1></body></html>",
        ),
        "/delayed" => (
            "200 OK",
            "text/html; charset=utf-8",
            "<html><body><p id=\"value\">Before delay</p><script>setTimeout(()=>value.textContent='After delay', 500)</script></body></html>",
        ),
        "/events" => (
            "200 OK",
            "text/html; charset=utf-8",
            "<html><body><h1>Events</h1><script>console.warn('fixture warning');fetch('/missing')</script></body></html>",
        ),
        "/missing" => ("404 Not Found", "text/plain", "missing"),
        _ => ("404 Not Found", "text/plain", "missing"),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn real_test_config(name: &str) -> (PathBuf, bool) {
    match env::var_os("ME_WEB_BROWSER_TEST_CONFIG") {
        Some(path) => (PathBuf::from(path), false),
        None => (temporary_directory(name), true),
    }
}

#[test]
#[ignore = "launches pinned Camoufox and exercises the real accessibility tree"]
fn real_camoufox_uses_raw_aria_snapshots_and_action_only_commands() {
    let workspace = temporary_directory("aria-workspace");
    let (config, remove_config) = real_test_config("aria-global");
    fs::create_dir_all(&config).unwrap();
    let script = generated_web_browser_toolbox(&workspace);
    let site = LocalSite::start();
    let mut toolbox = ToolboxProcess::start(&workspace, &script, &config);

    let (_, created) = toolbox.execute("Create", json!({}));
    let created = output(&created);
    assert_eq!(created, &json!({"page_id":"p0000001"}));

    let (_, navigated) = toolbox.execute(
        "Navigate",
        json!({"page_id":"p0000001","url":site.base_url}),
    );
    let navigated = output(&navigated);
    assert_eq!(navigated["page_id"], "p0000001");
    assert_eq!(navigated["navigated"], true);
    assert!(navigated.get("snapshot").is_none());

    let wait_started = Instant::now();
    let (_, snapshot) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"text"}),
    );
    assert!(wait_started.elapsed() >= Duration::from_millis(900));
    let snapshot = output(&snapshot);
    let tree = snapshot["accessibility_tree"].as_str().unwrap();
    assert!(tree.contains("Rendered fixture 世界"), "{tree}");
    assert!(tree.contains("[ref=e"), "{tree}");
    assert!(tree.contains("[box="), "{tree}");
    assert!(tree.contains("iframe"), "{tree}");
    assert!(tree.contains("Frame action"), "{tree}");
    assert!(snapshot.get("screen_path").is_none());
    assert!(snapshot.get("_me_screen_path").is_none());

    let input = aria_ref(tree, "Search field");
    let change = aria_ref(tree, "Change status");
    let popup = aria_ref(tree, "Open popup");
    let dialog = aria_ref(tree, "Show dialog");
    let frame_action = aria_ref(tree, "Frame action");

    let (_, typed) = toolbox.execute(
        "Type",
        json!({"page_id":"p0000001","element_id":input,"content":"hello 世界","mode":"replace"}),
    );
    assert_eq!(output(&typed), &json!({"page_id":"p0000001","typed":true}));

    let (_, pressed) = toolbox.execute(
        "Press",
        json!({"page_id":"p0000001","element_id":input,"key":"Enter"}),
    );
    assert_eq!(
        output(&pressed),
        &json!({"page_id":"p0000001","pressed":true})
    );

    let (_, clicked) = toolbox.execute("Click", json!({"page_id":"p0000001","element_id":change}));
    assert_eq!(output(&clicked)["clicked"], true);
    assert!(output(&clicked).get("snapshot").is_none());

    let (_, dialog_result) =
        toolbox.execute("Click", json!({"page_id":"p0000001","element_id":dialog}));
    assert_eq!(output(&dialog_result)["clicked"], true);

    let (_, frame_clicked) = toolbox.execute(
        "Click",
        json!({"page_id":"p0000001","element_id":frame_action}),
    );
    assert_eq!(output(&frame_clicked)["clicked"], true);

    let (_, scrolled) = toolbox.execute("Scroll", json!({"page_id":"p0000001","delta_y":-900}));
    assert_eq!(
        output(&scrolled),
        &json!({"page_id":"p0000001","scrolled":true})
    );

    let (_, after_actions) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"text"}),
    );
    let after_actions = output(&after_actions);
    let tree = after_actions["accessibility_tree"].as_str().unwrap();
    assert!(tree.contains("hello 世界"), "{tree}");
    assert!(tree.contains("Changed by click"), "{tree}");
    assert!(tree.contains("Frame clicked"), "{tree}");
    assert!(tree.contains("Enter received"), "{tree}");
    assert!(tree.contains("Scrolled viewport"), "{tree}");
    assert_eq!(
        after_actions["dismissed_native_dialogs"][0]["message"],
        "fixture dialog"
    );

    let (_, popup_result) =
        toolbox.execute("Click", json!({"page_id":"p0000001","element_id":popup}));
    assert_eq!(
        output(&popup_result)["opened_page_ids"],
        json!(["p0000002"])
    );
    let (_, pages) = toolbox.execute("Pages", json!({}));
    assert_eq!(output(&pages)["pages"].as_array().unwrap().len(), 2);
    let (_, observed_pages) = toolbox.execute("__activePages", json!({}));
    assert_eq!(output(&observed_pages)["pages"], output(&pages)["pages"]);

    let (_, screen) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"screen"}),
    );
    let screen = output(&screen);
    assert!(screen.get("accessibility_tree").is_none());
    let screen_path = workspace.join(screen["screen_path"].as_str().unwrap());
    assert!(
        screen["screen_path"]
            .as_str()
            .unwrap()
            .starts_with(".me/webbrowser/screenshots/web-snapshot-")
    );
    assert_eq!(&fs::read(&screen_path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");

    let (_, both) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"both"}),
    );
    let both = output(&both);
    assert!(
        both["accessibility_tree"]
            .as_str()
            .unwrap()
            .contains("Rendered fixture")
    );
    let both_path = workspace.join(both["screen_path"].as_str().unwrap());
    assert_eq!(&fs::read(&both_path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
    assert_ne!(screen_path, both_path);
    assert!(
        screen_path.exists(),
        "an earlier screenshot must remain reusable"
    );
    fs::remove_file(screen_path).unwrap();
    fs::remove_file(both_path).unwrap();

    let (_, delayed_navigation) = toolbox.execute(
        "Navigate",
        json!({"page_id":"p0000001","url":format!("{}/delayed", site.base_url)}),
    );
    assert_eq!(output(&delayed_navigation)["navigated"], true);
    let (_, delayed) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"text"}),
    );
    assert!(
        output(&delayed)["accessibility_tree"]
            .as_str()
            .unwrap()
            .contains("After delay")
    );

    let (_, events_navigation) = toolbox.execute(
        "Navigate",
        json!({"page_id":"p0000001","url":format!("{}/events", site.base_url)}),
    );
    assert_eq!(output(&events_navigation)["navigated"], true);
    let (_, events_snapshot) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"text"}),
    );
    let events = output(&events_snapshot)["browser_events"]
        .as_array()
        .unwrap();
    assert!(events.iter().any(|event| {
        event["kind"] == "console"
            && event["message"]
                .as_str()
                .is_some_and(|message| message.contains("fixture warning"))
    }));
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "http_error" && event["status"] == 404)
    );
    let (_, drained) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"text"}),
    );
    assert!(
        output(&drained)["browser_events"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let (_, second) = toolbox.execute(
        "Navigate",
        json!({"page_id":"p0000001","url":format!("{}/second", site.base_url)}),
    );
    assert_eq!(output(&second)["navigated"], true);
    let (_, stale) = toolbox.execute("Click", json!({"page_id":"p0000001","element_id":change}));
    assert_eq!(stale["type"], "error");
    assert_eq!(stale["error"]["code"], "stale_element");

    let (_, back) = toolbox.execute("Back", json!({"page_id":"p0000001"}));
    assert_eq!(output(&back)["navigated"], true);
    assert!(output(&back).get("snapshot").is_none());

    let (_, fragment_one) = toolbox.execute(
        "Navigate",
        json!({"page_id":"p0000001","url":format!("{}/second#one", site.base_url)}),
    );
    assert_eq!(output(&fragment_one)["navigated"], true);
    let (_, fragment_two) = toolbox.execute(
        "Navigate",
        json!({"page_id":"p0000001","url":format!("{}/second#two", site.base_url)}),
    );
    assert_eq!(output(&fragment_two)["navigated"], true);
    let (_, fragment_back) = toolbox.execute("Back", json!({"page_id":"p0000001"}));
    assert_eq!(output(&fragment_back)["navigated"], true);
    assert_eq!(
        output(&fragment_back)["url"],
        format!("{}/second#one", site.base_url)
    );

    let (_, invalid_wait) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":999,"kind":"text"}),
    );
    assert_eq!(invalid_wait["error"]["code"], "invalid_arguments");
    let (_, legacy) = toolbox.execute(
        "Snapshot",
        json!({"page_id":"p0000001","wait_ms":1000,"kind":"text","save_img":true}),
    );
    assert_eq!(legacy["error"]["code"], "invalid_arguments");

    for page_id in ["p0000002", "p0000001"] {
        let (_, closed) = toolbox.execute("Close", json!({"page_id":page_id}));
        assert_eq!(output(&closed)["closed"], true);
    }
    let (_, pages_after_close) = toolbox.execute("Pages", json!({}));
    assert!(
        output(&pages_after_close)["pages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (_, fresh_page) = toolbox.execute("Create", json!({}));
    assert_eq!(output(&fresh_page)["page_id"], "p0000003");
    let (_, no_history) = toolbox.execute("Back", json!({"page_id":"p0000003"}));
    assert_eq!(output(&no_history)["navigated"], false);
    let (_, fresh_closed) = toolbox.execute("Close", json!({"page_id":"p0000003"}));
    assert_eq!(output(&fresh_closed)["closed"], true);
    toolbox.finish();
    drop(site);
    fs::remove_dir_all(workspace).unwrap();
    if remove_config {
        fs::remove_dir_all(config).unwrap();
    }
}

#[test]
#[ignore = "launches Camoufox and verifies a blocked page cannot stall the toolbox"]
fn real_camoufox_hard_timeout_restarts_a_hung_browser_worker() {
    let workspace = temporary_directory("timeout-workspace");
    let (config, remove_config) = real_test_config("timeout-global");
    fs::create_dir_all(&config).unwrap();
    let script = generated_web_browser_toolbox(&workspace);
    let site = LocalSite::start();
    let mut toolbox = ToolboxProcess::start_with_environment(
        &workspace,
        &script,
        &config,
        &[
            ("ME_WEB_BROWSER_TEST_OPERATION_TIMEOUT_MS", "1000"),
            ("ME_WEB_BROWSER_TEST_HARD_TIMEOUT_GRACE_MS", "500"),
        ],
    );

    let (_, created) = toolbox.execute("Create", json!({}));
    let page_id = output(&created)["page_id"].as_str().unwrap().to_owned();
    let (_, navigated) =
        toolbox.execute("Navigate", json!({"page_id":page_id,"url":site.base_url}));
    assert_eq!(navigated["type"], "result");
    let (_, snapshot) = toolbox.execute(
        "Snapshot",
        json!({"page_id":page_id,"wait_ms":1000,"kind":"text"}),
    );
    let busy = aria_ref(
        output(&snapshot)["accessibility_tree"].as_str().unwrap(),
        "Busy control",
    );

    let started = Instant::now();
    let (_, blocked) = toolbox.execute("Click", json!({"page_id":page_id,"element_id":busy}));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "blocked click was not hard-bounded"
    );
    assert_eq!(blocked["type"], "error", "{blocked}");
    assert!(
        ["operation_timeout", "click_failed"]
            .iter()
            .any(|code| blocked["error"]["code"] == *code),
        "{blocked}"
    );

    let (_, recreated) = toolbox.execute("Create", json!({}));
    assert_eq!(recreated["type"], "result", "{recreated}");
    toolbox.finish();
    drop(site);
    fs::remove_dir_all(workspace).unwrap();
    if remove_config {
        fs::remove_dir_all(config).unwrap();
    }
}

#[test]
#[ignore = "launches Camoufox and validates human handoff reports changes without snapshots"]
fn real_camoufox_human_handoff_reports_metadata_only() {
    let workspace = temporary_directory("handoff-workspace");
    let (config, remove_config) = real_test_config("handoff-global");
    fs::create_dir_all(&config).unwrap();
    let script = generated_web_browser_toolbox(&workspace);
    let site = LocalSite::start();
    let mut toolbox = ToolboxProcess::start_with_environment(
        &workspace,
        &script,
        &config,
        &[
            ("ME_WEB_BROWSER_TEST_HUMAN_ACTION_RESULT", "completed"),
            (
                "ME_WEB_BROWSER_TEST_HUMAN_ACTION_PAGE_SCRIPT",
                "() => document.getElementById('status').textContent = 'Changed by handoff'",
            ),
        ],
    );
    let (_, created) = toolbox.execute("Create", json!({}));
    let page_id = output(&created)["page_id"].as_str().unwrap().to_owned();
    let (_, navigated) =
        toolbox.execute("Navigate", json!({"page_id":page_id,"url":site.base_url}));
    assert_eq!(navigated["type"], "result");

    let (updates, handoff) = toolbox.execute(
        "RequireHumanAction",
        json!({"page_id":page_id,"instruction":"Change the test page, then confirm."}),
    );
    let handoff = output(&handoff);
    assert_eq!(handoff["state"], "completed");
    assert_eq!(handoff["target_page"]["change"], "changed");
    assert!(handoff["target_page"].get("snapshot").is_none());
    assert!(handoff["target_page"]["page"].is_object());
    assert!(updates.iter().any(|update| {
        update["output"]["content"]
            .as_str()
            .is_some_and(|message| message.contains("Human action required"))
    }));

    let (_, observed) = toolbox.execute(
        "Snapshot",
        json!({"page_id":page_id,"wait_ms":1000,"kind":"text"}),
    );
    assert!(
        output(&observed)["accessibility_tree"]
            .as_str()
            .unwrap()
            .contains("Changed by handoff")
    );
    let (_, closed) = toolbox.execute("Close", json!({"page_id":page_id}));
    assert_eq!(output(&closed)["closed"], true);
    toolbox.finish();
    drop(site);
    fs::remove_dir_all(workspace).unwrap();
    if remove_config {
        fs::remove_dir_all(config).unwrap();
    }
}

#[test]
#[ignore = "uses live Google and Baidu through the pinned Camoufox browser"]
fn real_camoufox_google_and_baidu_search_smoke() {
    let workspace = temporary_directory("search-workspace");
    let (config, remove_config) = real_test_config("search-global");
    fs::create_dir_all(&config).unwrap();
    let script = generated_web_browser_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script, &config);

    let (_, created) = toolbox.execute("Create", json!({}));
    let page_id = output(&created)["page_id"].as_str().unwrap().to_owned();
    for (engine, url) in [
        ("Google", "https://www.google.com/search?q=camoufox"),
        ("Baidu", "https://www.baidu.com/s?wd=camoufox"),
    ] {
        let (_, navigated) = toolbox.execute("Navigate", json!({"page_id":page_id,"url":url}));
        assert_eq!(navigated["type"], "result", "{engine}: {navigated}");
        let (_, snapshot) = toolbox.execute(
            "Snapshot",
            json!({"page_id":page_id,"wait_ms":3000,"kind":"text"}),
        );
        let snapshot = output(&snapshot);
        let visible = format!(
            "{}\n{}\n{}",
            snapshot["url"].as_str().unwrap_or_default(),
            snapshot["title"].as_str().unwrap_or_default(),
            snapshot["accessibility_tree"].as_str().unwrap_or_default()
        )
        .to_lowercase();
        for marker in [
            "/sorry/",
            "wappass.baidu.com",
            "unusual traffic",
            "百度安全验证",
            "人机验证",
        ] {
            assert!(
                !visible.contains(marker),
                "{engine} entered verification containing {marker:?}: {visible}"
            );
        }
        assert!(
            visible.contains("camoufox"),
            "{engine} did not expose its search result: {visible}"
        );
    }

    let (_, closed) = toolbox.execute("Close", json!({"page_id":page_id}));
    assert_eq!(output(&closed)["closed"], true);
    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
    if remove_config {
        fs::remove_dir_all(config).unwrap();
    }
}
