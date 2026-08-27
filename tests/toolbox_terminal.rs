use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
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

fn temporary_workspace() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = env::temp_dir().join(format!(
        "me-terminal-toolbox-integration-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn generated_terminal_python_program_runs_as_an_independent_jsonl_toolbox() {
    let Some((python, arguments)) = python_312() else {
        panic!("Terminal toolbox integration test requires Python 3.12");
    };
    let workspace = temporary_workspace();
    let script = me::toolbox::ensure_default_toolboxes(&workspace).unwrap();
    let mut child = Command::new(python)
        .args(arguments)
        .arg(script)
        .current_dir(&workspace)
        .env("ME_TOOLBOX_HOST", env!("CARGO_BIN_EXE_me-s"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        json!({"id": 1, "cmd": "getTools"}),
        json!({"id": 2, "cmd": "getBrief"}),
        json!({"id": 3, "cmd": "getInputSchema", "tool": "Create"}),
        json!({"id": 4, "cmd": "getOutputSchema", "tool": "Create"}),
        json!({"id": 5, "cmd": "getResultTokenLimit", "tool": "Create"}),
        json!({"id": 6, "cmd": "getInstructions", "tool": "Create"}),
        json!({"id": 7, "cmd": "getRoute", "tool": "Create"}),
        json!({"id": 8, "cmd": "getExamples", "tool": "Create"}),
    ];
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let mut frames = Vec::new();
    for request in requests {
        stdin.write_all(request.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        let line = receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("Terminal.py must respond while its stdin remains open")
            .unwrap();
        frames.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    drop(stdin);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "Terminal.py failed: {}", stderr);
    assert_eq!(frames.len(), 8);
    assert!(
        frames
            .iter()
            .enumerate()
            .all(|(index, frame)| frame["id"] == (index + 1) as u64 && frame["type"] == "result")
    );
    assert!(frames[0]["output"].is_array());
    assert!(
        frames[1]["output"]
            .as_str()
            .unwrap()
            .contains("stateful PTY")
    );
    assert_eq!(frames[2]["output"]["type"], "object");
    assert_eq!(frames[3]["output"]["type"], "object");
    assert_eq!(frames[4]["output"], 32 * 1024);
    assert!(frames[5]["output"].is_string());
    assert!(frames[6]["output"].is_string());
    assert!(frames[7]["output"].is_string());
    fs::remove_dir_all(workspace).unwrap();
}
