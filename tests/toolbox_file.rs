use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

#[cfg(unix)]
use me::toolbox::{ToolboxExecutionError, ToolboxRuntime};
#[cfg(unix)]
use std::{
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

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
    static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let serial = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "me-file-toolbox-integration-{}-{nonce}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn utf16_le(text: &str, bom: bool) -> Vec<u8> {
    let mut bytes = if bom { vec![0xff, 0xfe] } else { Vec::new() };
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

fn utf32_be(text: &str, bom: bool) -> Vec<u8> {
    let mut bytes = if bom {
        vec![0x00, 0x00, 0xfe, 0xff]
    } else {
        Vec::new()
    };
    for character in text.chars() {
        bytes.extend_from_slice(&(character as u32).to_be_bytes());
    }
    bytes
}

fn numbered_lines(text: &str, first_line: usize) -> Value {
    let bytes = text.as_bytes();
    let mut lines = serde_json::Map::new();
    let mut start = 0;
    let mut number = first_line;
    let mut index = 0;
    while index < bytes.len() {
        let end = match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => Some(index + 2),
            b'\r' | b'\n' => Some(index + 1),
            _ => None,
        };
        if let Some(end) = end {
            let mut logical_end = end;
            if bytes[end - 1] == b'\n' {
                logical_end -= 1;
                if logical_end > start && bytes[logical_end - 1] == b'\r' {
                    logical_end -= 1;
                }
            } else if bytes[end - 1] == b'\r' {
                logical_end -= 1;
            }
            lines.insert(number.to_string(), json!(&text[start..logical_end]));
            number += 1;
            start = end;
            index = end;
        } else {
            index += 1;
        }
    }
    if start < bytes.len() {
        lines.insert(number.to_string(), json!(&text[start..]));
    }
    Value::Object(lines)
}

fn logical_lines(value: Value) -> Value {
    Value::Array(
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|line| {
                line.as_str().map_or_else(
                    || line.clone(),
                    |line| {
                        json!(
                            line.strip_suffix("\r\n")
                                .or_else(|| line.strip_suffix('\r'))
                                .or_else(|| line.strip_suffix('\n'))
                                .unwrap_or(line)
                        )
                    },
                )
            })
            .collect(),
    )
}

macro_rules! single_edit_input {
    ($path:expr, $hash:expr, $start:expr, $end:expr, $new_lines:expr) => {
        {
            let start = $start;
            let end = $end;
            let new_lines = logical_lines($new_lines);
            let edit = if start > end {
                json!({"operation":"insert","before_line":start,"new_lines":new_lines})
            } else if new_lines.as_array().is_some_and(|lines| lines.is_empty()) {
                json!({"operation":"delete","start_line":start,"end_line":end})
            } else {
                json!({"operation":"replace","start_line":start,"end_line":end,"new_lines":new_lines})
            };
            let _ = $hash;
            json!({"path":$path,"edits":[edit]})
        }
    };
}

struct ToolboxProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl ToolboxProcess {
    fn start(workspace: &Path, script: &Path) -> Self {
        Self::start_with_io_encoding(workspace, script, None)
    }

    fn start_with_io_encoding(workspace: &Path, script: &Path, io_encoding: Option<&str>) -> Self {
        Self::start_with_options(
            workspace,
            script,
            io_encoding,
            Path::new(env!("CARGO_BIN_EXE_me-s")),
        )
    }

    #[cfg(unix)]
    fn start_with_search_host(workspace: &Path, script: &Path, host: &Path) -> Self {
        Self::start_with_options(workspace, script, None, host)
    }

    fn start_with_options(
        workspace: &Path,
        script: &Path,
        io_encoding: Option<&str>,
        search_host: &Path,
    ) -> Self {
        let Some((python, arguments)) = python_312() else {
            panic!("File toolbox integration test requires Python 3.12");
        };
        let mut command = Command::new(python);
        command
            .args(arguments)
            .arg(script)
            .current_dir(workspace)
            .env("ME_TOOLBOX_HOST", search_host)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(io_encoding) = io_encoding {
            command.env("PYTHONIOENCODING", io_encoding);
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

    fn request(&mut self, mut request: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = Value::from(id);
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(
            !line.is_empty(),
            "File.py closed before responding to {request}"
        );
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["id"], id);
        frame
    }

    fn query(&mut self, command: &str, tool: Option<&str>) -> Value {
        let mut request = json!({"cmd": command});
        if let Some(tool) = tool {
            request["tool"] = Value::String(tool.to_owned());
        }
        self.request(request)
    }

    fn execute(&mut self, tool: &str, input: Value) -> Value {
        let mut input = input;
        if tool == "Edit" {
            let path = input["path"].as_str().unwrap().to_owned();
            let mut read = json!({"path": path});
            if let Some(encoding) = input.get("encoding") {
                read["encoding"] = encoding.clone();
            }
            let read_result = self.execute_raw("Read", read);
            assert_eq!(read_result["type"], "result", "edit baseline read failed");
            input.as_object_mut().unwrap().remove("expected_hash");
            if let Some(edits) = input.get_mut("edits").and_then(Value::as_array_mut) {
                for edit in edits {
                    if let Some(lines) = edit.get_mut("new_lines").and_then(Value::as_array_mut) {
                        for line in lines {
                            if let Some(text) = line.as_str() {
                                let logical = text
                                    .strip_suffix("\r\n")
                                    .or_else(|| text.strip_suffix('\r'))
                                    .or_else(|| text.strip_suffix('\n'))
                                    .unwrap_or(text)
                                    .to_owned();
                                *line = Value::String(logical);
                            }
                        }
                    }
                }
            }
        }
        self.execute_raw(tool, input)
    }

    fn execute_raw(&mut self, tool: &str, input: Value) -> Value {
        self.request(json!({"cmd":"execute", "tool":tool, "input":input}))
    }

    fn finish(mut self) {
        drop(self.stdin);
        assert!(self.child.wait().unwrap().success());
    }
}

fn generated_file_toolbox(workspace: &Path) -> PathBuf {
    me::toolbox::ensure_default_toolboxes(workspace)
        .unwrap()
        .parent()
        .unwrap()
        .join("File.py")
}

#[test]
fn generated_file_toolbox_is_self_describing_while_stdin_remains_open() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let tools = toolbox.query("getTools", None);
    assert_eq!(tools["type"], "result");
    assert_eq!(tools["output"].as_array().unwrap().len(), 15);
    assert_eq!(tools["output"][0], "Read");
    assert_eq!(tools["output"][2], "EditBytes");
    assert_eq!(tools["output"][7], "MakeDirectory");
    assert_eq!(tools["output"][12], "Copy");
    assert_eq!(tools["output"][14], "Delete");
    let tool_names = tools["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let brief = toolbox.query("getBrief", None);
    let brief = brief["output"].as_str().unwrap();
    assert!(brief.contains("Use File.Read before File.Edit"));
    assert!(brief.contains("use File.ReadBytes before File.EditBytes"));
    assert!(brief.contains("Hash-gated mutations require the current hash"));
    assert!(brief.contains("absolute paths are accepted"));
    assert!(brief.contains("PATH SUPPORT IS CAPABILITY, NOT AUTHORIZATION"));
    assert!(brief.contains("common Unicode, East Asian, and Windows text encodings"));
    assert!(brief.contains("next safe action"));
    assert!(!brief.contains("SHA-256"));
    assert!(!brief.contains("64 KiB"));
    assert!(!brief.contains("streaming transcoding"));
    let read_schema = toolbox.query("getInputSchema", Some("Read"));
    assert!(
        read_schema["output"]["properties"]["path"]["description"]
            .as_str()
            .unwrap()
            .contains("Relative paths resolve from the workspace; absolute paths are accepted")
    );
    assert!(
        read_schema["output"]["properties"]
            .get("max_lines")
            .is_none()
    );
    assert_eq!(
        read_schema["output"]["properties"]["start_line"]["minimum"],
        1
    );
    assert_eq!(
        read_schema["output"]["properties"]["end_line"]["minimum"],
        1
    );
    let read_instructions = toolbox.query("getInstructions", Some("Read"));
    let read_instructions = read_instructions["output"].as_str().unwrap();
    assert!(read_instructions.contains("omit both to read the complete file"));
    assert!(read_instructions.contains("Every successful result includes total_lines"));
    assert!(!read_instructions.contains("max_lines"));
    let read_examples = toolbox.query("getExamples", Some("Read"));
    let read_examples = read_examples["output"].as_str().unwrap();
    assert!(read_examples.contains("\"end_line\":200"));
    assert!(!read_examples.contains("max_lines"));
    let read_output_schema = toolbox.query("getOutputSchema", Some("Read"));
    assert!(
        read_output_schema["output"]["properties"]["path"]["description"]
            .as_str()
            .unwrap()
            .contains("returned relative to it")
    );
    for model_visible_read_section in [
        read_schema["output"].to_string(),
        read_output_schema["output"].to_string(),
        read_instructions.to_owned(),
        read_examples.to_owned(),
    ] {
        assert!(!model_visible_read_section.contains("beyond EOF"));
        assert!(!model_visible_read_section.contains("out of range"));
    }
    for tool in &tool_names {
        for command in [
            "getInputSchema",
            "getOutputSchema",
            "getInstructions",
            "getRoute",
            "getExamples",
        ] {
            let frame = toolbox.query(command, Some(tool));
            assert_eq!(frame["type"], "result", "{command} failed for {tool}");
            assert!(!frame["output"].is_null());
            if matches!(command, "getInputSchema" | "getOutputSchema") {
                assert_eq!(frame["output"]["type"], "object");
            } else {
                assert!(!frame["output"].as_str().unwrap().is_empty());
            }
        }
    }
    assert!(!tool_names.contains(&"ApplyPatch".to_owned()));
    assert!(tool_names.contains(&"Edit".to_owned()));
    let disabled = toolbox.execute("ApplyPatch", json!({}));
    assert_eq!(disabled["type"], "error");
    assert_eq!(disabled["error"]["code"], "tool_disabled");
    assert!(
        disabled["error"]["message"]
            .as_str()
            .unwrap()
            .contains("File.Edit")
    );
    let edit_schema = toolbox.query("getInputSchema", Some("Edit"));
    let edit_variants = edit_schema["output"]["properties"]["edits"]["items"]["oneOf"]
        .as_array()
        .unwrap();
    assert_eq!(edit_variants.len(), 3);
    assert_eq!(
        edit_variants[0]["properties"]["operation"]["enum"],
        json!(["replace"])
    );
    assert_eq!(edit_variants[0]["properties"]["start_line"]["minimum"], 1);
    assert_eq!(
        edit_variants[1]["properties"]["operation"]["enum"],
        json!(["delete"])
    );
    assert_eq!(edit_schema["output"]["properties"]["edits"]["minItems"], 1);
    assert_eq!(
        edit_variants[2]["properties"]["operation"]["enum"],
        json!(["insert"])
    );
    assert_eq!(edit_variants[2]["properties"]["before_line"]["minimum"], 1);
    assert!(
        edit_schema["output"]["properties"]
            .get("expected_hash")
            .is_none()
    );
    assert_eq!(edit_variants[0]["properties"]["new_lines"]["type"], "array");
    assert!(
        edit_variants[0]["properties"]["new_lines"]
            .get("maxItems")
            .is_none()
    );
    assert!(
        edit_variants[0]["properties"]["new_lines"]["items"]
            .get("maxLength")
            .is_none()
    );
    assert!(edit_variants[0]["properties"].get("new_text").is_none());
    assert!(edit_variants[1]["properties"].get("new_lines").is_none());
    assert!(edit_variants[2]["properties"].get("start_line").is_none());
    let edit_output_schema = toolbox.query("getOutputSchema", Some("Edit"));
    assert!(
        edit_output_schema["output"]["properties"]
            .get("hash")
            .is_none()
    );
    assert!(
        !edit_output_schema["output"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("hash"))
    );
    let edit_instructions = toolbox.query("getInstructions", Some("Edit"));
    let edit_instructions = edit_instructions["output"].as_str().unwrap();
    assert!(edit_instructions.contains("same original pre-edit snapshot"));
    assert!(edit_instructions.contains("array order is not execution order"));
    assert!(edit_instructions.contains("duplicated at the same insertion point"));
    assert!(edit_instructions.contains("MUST NOT contain LF or CR"));
    assert!(edit_instructions.contains("automatically selects and preserves"));
    assert!(edit_instructions.contains("clears every editable range"));
    assert!(edit_instructions.contains("wider continuous range"));
    assert!(edit_instructions.contains("File.Search"));
    assert!(edit_instructions.contains("later model response"));
    let edit_examples = toolbox.query("getExamples", Some("Edit"));
    let edit_examples = edit_examples["output"].as_str().unwrap();
    assert!(edit_examples.contains("original lines 1 and 3 independently"));
    assert!(edit_examples.contains("array order is irrelevant"));
    assert!(edit_examples.contains("Deletion has no new_lines field"));
    assert!(edit_examples.contains("Insert into an empty file"));
    assert!(edit_examples.contains("Common errors"));
    assert!(edit_examples.contains("do not emit Read and Edit together"));
    for example in edit_examples.lines().filter(|line| line.starts_with('{')) {
        serde_json::from_str::<Value>(example).unwrap();
    }
    let search_output = toolbox.query("getOutputSchema", Some("Search"));
    assert!(
        search_output["output"]["properties"]["matches"]["items"]["properties"]
            .get("hash")
            .is_none()
    );
    let search_instructions = toolbox.query("getInstructions", Some("Search"));
    assert!(
        search_instructions["output"]
            .as_str()
            .unwrap()
            .contains("Search results do not authorize File.Edit")
    );
    let find_input = toolbox.query("getInputSchema", Some("Find"));
    assert_eq!(find_input["output"]["properties"]["depth"]["minimum"], 1);
    assert_eq!(find_input["output"]["properties"]["depth"]["maximum"], 32);
    assert!(
        find_input["output"]["properties"]["depth"]
            .get("default")
            .is_none()
    );
    let find_instructions = toolbox.query("getInstructions", Some("Find"));
    assert!(
        find_instructions["output"]
            .as_str()
            .unwrap()
            .contains("Omit depth for unlimited recursion")
    );
    let search_input = toolbox.query("getInputSchema", Some("Search"));
    assert_eq!(search_input["output"]["properties"]["depth"]["minimum"], 1);
    assert_eq!(search_input["output"]["properties"]["depth"]["maximum"], 32);
    assert_eq!(
        search_input["output"]["properties"]["context_before"]["maximum"],
        10_000
    );
    assert_eq!(
        search_input["output"]["properties"]["context_after"]["maximum"],
        10_000
    );
    assert!(
        search_input["output"]["properties"]["depth"]
            .get("default")
            .is_none()
    );
    assert!(
        search_instructions["output"]
            .as_str()
            .unwrap()
            .contains("omitting depth searches recursively without a depth limit")
    );
    for tool in ["Find", "Search"] {
        let examples = toolbox.query("getExamples", Some(tool));
        for example in examples["output"]
            .as_str()
            .unwrap()
            .lines()
            .filter(|line| line.starts_with('{'))
        {
            serde_json::from_str::<Value>(example).unwrap();
        }
    }
    assert_eq!(
        toolbox.query("getInputSchema", Some("Read"))["output"]["properties"]["encoding"]["default"],
        "auto"
    );
    let read_instructions = toolbox.query("getInstructions", Some("Read"));
    let read_instructions_text = read_instructions["output"].as_str().unwrap();
    assert!(read_instructions_text.contains("may inspect the complete file"));
    assert!(read_instructions_text.contains("EDIT AUTHORIZATION"));
    assert!(read_instructions_text.contains("complete current set"));
    assert!(read_instructions_text.contains("Only current File.Read results establish"));
    assert!(read_instructions_text.contains("earlier model response"));
    assert!(read_instructions_text.contains("clears them"));
    assert!(read_instructions_text.contains("Automatic detection supports common Unicode"));
    assert!(!read_instructions_text.contains("strict UTF-8 first"));
    let read_output = toolbox.query("getOutputSchema", Some("Read"));
    assert_eq!(
        read_output["output"]["properties"]["lines"]["type"],
        "object"
    );
    assert!(read_output["output"]["properties"].get("content").is_none());
    let editable_ranges_description =
        read_output["output"]["properties"]["editable_ranges"]["description"]
            .as_str()
            .unwrap();
    assert!(editable_ranges_description.contains("complete current File.Edit authorization"));
    assert!(editable_ranges_description.contains("Search and other tools do not grant"));
    let read_bytes_output = toolbox.query("getOutputSchema", Some("ReadBytes"));
    assert_eq!(
        read_bytes_output["output"]["properties"]["data"]["type"],
        "string"
    );
    assert!(
        read_bytes_output["output"]["properties"]
            .get("base64")
            .is_none()
    );
    assert!(
        read_bytes_output["output"]["properties"]
            .get("chunks")
            .is_none()
    );
    let read_bytes_instructions = toolbox.query("getInstructions", Some("ReadBytes"));
    let read_bytes_instructions = read_bytes_instructions["output"].as_str().unwrap();
    assert!(read_bytes_instructions.contains("lowercase two-digit hexadecimal"));
    assert!(read_bytes_instructions.contains("retains only the earliest complete bytes"));
    assert!(read_bytes_instructions.contains("removed_offset_end_exclusive"));
    assert!(read_bytes_instructions.contains("baseline for File.EditBytes"));
    let edit_bytes_input = toolbox.query("getInputSchema", Some("EditBytes"));
    let byte_edit = &edit_bytes_input["output"]["properties"]["edits"]["items"];
    assert_eq!(byte_edit["properties"]["target_offset"]["minimum"], 0);
    assert_eq!(byte_edit["properties"]["target_length"]["minimum"], 0);
    assert_eq!(byte_edit["properties"]["data"]["type"], "string");
    assert!(byte_edit["properties"]["data"].get("maxLength").is_none());
    let edit_bytes_output = toolbox.query("getOutputSchema", Some("EditBytes"));
    assert!(
        edit_bytes_output["output"]["properties"]
            .get("hash")
            .is_none()
    );
    let edit_bytes_instructions = toolbox.query("getInstructions", Some("EditBytes"));
    let edit_bytes_instructions = edit_bytes_instructions["output"].as_str().unwrap();
    assert!(edit_bytes_instructions.contains("same original pre-edit snapshot"));
    assert!(edit_bytes_instructions.contains("half-open original range"));
    assert!(edit_bytes_instructions.contains("Call File.ReadBytes again"));
    assert!(!edit_bytes_instructions.contains("uppercase"));
    assert!(!edit_bytes_instructions.contains("multiple spaces"));
    let edit_bytes_examples = toolbox.query("getExamples", Some("EditBytes"));
    let edit_bytes_examples = edit_bytes_examples["output"].as_str().unwrap();
    assert!(edit_bytes_examples.contains("Multiple edits still use original offsets"));
    assert!(edit_bytes_examples.contains("Array order is irrelevant"));
    assert!(edit_bytes_examples.contains("Common errors"));
    for example in edit_bytes_examples
        .lines()
        .filter(|line| line.starts_with('{'))
    {
        serde_json::from_str::<Value>(example).unwrap();
    }
    let search_output = toolbox.query("getOutputSchema", Some("Search"));
    let search_match = &search_output["output"]["properties"]["matches"]["items"];
    assert!(
        search_match["required"]
            .as_array()
            .unwrap()
            .contains(&json!("match_text"))
    );
    assert!(
        !search_match["required"]
            .as_array()
            .unwrap()
            .contains(&json!("hash"))
    );
    assert!(search_match["properties"].get("hash").is_none());
    assert!(search_match["properties"].get("line").is_none());
    assert!(search_match["properties"].get("text").is_none());
    assert_eq!(search_match["properties"]["match_text"]["maxProperties"], 1);
    let search_instructions = toolbox.query("getInstructions", Some("Search"));
    let search_instructions = search_instructions["output"].as_str().unwrap();
    assert!(search_instructions.contains("before, match_text, and after"));
    assert!(search_instructions.contains("line-number-keyed objects"));
    assert!(search_instructions.contains("Search results do not authorize File.Edit"));
    assert!(search_instructions.contains("top-level truncate:true"));
    assert!(search_instructions.contains("text_fragments"));
    assert!(search_instructions.contains("Rust regular-expression syntax"));
    assert!(search_instructions.contains("look-around and backreferences return invalid_regex"));
    assert!(search_instructions.contains(".gitignore"));
    assert!(search_instructions.contains("fixed 120-second deadline"));
    assert!(search_instructions.contains("returns no partial result"));
    assert!(search_instructions.contains("search_timeout"));
    assert!(search_instructions.contains("decoded Unicode characters"));
    assert!(search_instructions.contains("GB2312/CP936/GBK/GB18030 family"));
    assert!(search_instructions.contains("Big5"));
    assert!(search_instructions.contains("EUC-JP"));
    assert!(search_instructions.contains("Windows-1251"));
    assert!(search_instructions.contains("No encoding parameter is required"));
    assert!(search_instructions.contains("can be ambiguous"));
    assert!(search_instructions.contains("neither an encoding name nor confidence"));
    assert!(search_instructions.contains("truncated=true means that limit was reached"));
    assert!(!search_instructions.contains("chardetng"));
    assert!(!search_instructions.contains("64 KiB"));
    assert!(!search_instructions.contains("integrated ripgrep engine"));
    assert!(!search_instructions.contains("another engine"));
    assert!(!search_instructions.contains("stream-decoded"));
    let list_output = toolbox.query("getOutputSchema", Some("List"));
    let list_entry = &list_output["output"]["properties"]["entries"]["items"];
    assert_eq!(
        list_entry["required"],
        json!(["path", "type", "size", "modified_ms"])
    );
    assert_eq!(
        list_entry["properties"]["type"]["enum"],
        json!(["file", "directory", "symlink", "other"])
    );
    assert!(
        list_entry["properties"]["modified_ms"]["description"]
            .as_str()
            .unwrap()
            .contains("Unix epoch milliseconds")
    );
    let stat_output = toolbox.query("getOutputSchema", Some("Stat"));
    let stat_entry = &stat_output["output"]["properties"]["entries"]["items"];
    assert_eq!(stat_entry["required"], json!(["path", "exists"]));
    assert!(stat_entry["properties"].get("type").is_some());
    assert!(stat_entry["properties"].get("size").is_some());
    assert!(stat_entry["properties"].get("modified_ms").is_some());
    assert!(stat_entry["properties"].get("readonly").is_some());
    assert!(stat_entry["properties"].get("hash").is_some());
    let create_encodings = toolbox.query("getInputSchema", Some("Create"))["output"]["properties"]
        ["encoding"]["enum"]
        .as_array()
        .unwrap()
        .clone();
    assert!(!create_encodings.contains(&json!("auto")));
    assert!(create_encodings.contains(&json!("gb18030")));
    assert_eq!(
        toolbox.query("getOutputSchema", Some("Read"))["output"]["properties"]["bom"]["type"],
        "boolean"
    );
    assert_eq!(
        toolbox.query("getInputSchema", Some("MakeDirectory"))["output"]["properties"]["parents"]["default"],
        false
    );
    let copy_input = toolbox.query("getInputSchema", Some("Copy"));
    assert_eq!(
        copy_input["output"]["required"],
        json!(["path", "destination", "expected_hash"])
    );
    assert!(
        copy_input["output"]["properties"]["expected_hash"]["description"]
            .as_str()
            .unwrap()
            .contains("mutation fails if the file has changed")
    );
    let copy_instructions = toolbox.query("getInstructions", Some("Copy"));
    assert!(
        copy_instructions["output"]
            .as_str()
            .unwrap()
            .contains("leaves the source unchanged")
    );
    assert!(
        toolbox.query("getExamples", Some("Copy"))["output"]
            .as_str()
            .unwrap()
            .contains("archive/notes.txt")
    );
    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn file_jsonl_forces_utf8_when_the_host_requests_gbk() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start_with_io_encoding(&workspace, &script, Some("gbk"));
    let marker = "文件\u{e687}›";
    let response = toolbox.request(json!({
        "cmd":"getInputSchema",
        "tool":marker
    }));
    assert_eq!(response["type"], "error");
    assert_eq!(response["error"]["code"], "unknown_tool");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(marker)
    );
    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn file_mutations_require_a_refreshed_hash_after_edit_and_never_add_implicit_text() {
    let workspace = temporary_workspace();
    fs::create_dir_all(workspace.join("archive")).unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let created = toolbox.execute(
        "Create",
        json!({"path":"notes.txt", "content":"alpha\nbeta"}),
    );
    assert_eq!(created["type"], "result");
    let hash1 = created["output"]["hash"].as_str().unwrap().to_owned();
    assert_eq!(hash1.len(), 8);
    assert!(hash1.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(created["output"]["previous_hash"], Value::Null);

    let read = toolbox.execute("Read", json!({"path":"notes.txt"}));
    assert_eq!(read["output"]["lines"], numbered_lines("alpha\nbeta", 1));
    assert!(read["output"].get("content").is_none());
    assert_eq!(read["output"]["hash"], hash1);

    let edited = toolbox.execute(
        "Edit",
        single_edit_input!("notes.txt", hash1, 1, 2, json!(["first\n", "second\n"])),
    );
    assert_eq!(edited["output"]["previous_hash"], read["output"]["hash"]);
    assert_eq!(edited["output"]["operation"], "edited");
    assert!(edited["output"].get("hash").is_none());
    let refreshed = toolbox.execute("Read", json!({"path":"notes.txt"}));
    let hash2 = refreshed["output"]["hash"].as_str().unwrap().to_owned();

    let appended = toolbox.execute(
        "Append",
        json!({"path":"notes.txt", "expected_hash":hash2, "content":"tail"}),
    );
    let hash3 = appended["output"]["hash"].as_str().unwrap().to_owned();
    assert_eq!(
        fs::read_to_string(workspace.join("notes.txt")).unwrap(),
        "first\nsecondtail"
    );
    assert_eq!(appended["output"]["appended_bytes"], 4);

    let replaced = toolbox.execute(
        "Replace",
        json!({"path":"notes.txt", "expected_hash":hash3, "content":"whole\n"}),
    );
    let hash4 = replaced["output"]["hash"].as_str().unwrap().to_owned();
    assert_eq!(
        fs::read_to_string(workspace.join("notes.txt")).unwrap(),
        "whole\n"
    );

    let copied = toolbox.execute(
        "Copy",
        json!({
            "path":"notes.txt",
            "destination":"archive/copied.txt",
            "expected_hash":hash4
        }),
    );
    assert_eq!(copied["type"], "result", "{copied}");
    assert_eq!(copied["output"]["operation"], "copied");
    assert_eq!(copied["output"]["hash"], hash4);
    assert_eq!(copied["output"]["size"], 6);
    assert_eq!(
        fs::read_to_string(workspace.join("archive/copied.txt")).unwrap(),
        "whole\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("notes.txt")).unwrap(),
        "whole\n"
    );

    let moved = toolbox.execute(
        "Move",
        json!({
            "path":"notes.txt",
            "destination":"archive/notes.txt",
            "expected_hash":hash4
        }),
    );
    assert_eq!(moved["output"]["previous_hash"], moved["output"]["hash"]);
    assert!(!workspace.join("notes.txt").exists());
    assert!(workspace.join("archive/notes.txt").is_file());

    let deleted = toolbox.execute(
        "Delete",
        json!({
            "path":"archive/notes.txt",
            "expected_hash":moved["output"]["hash"]
        }),
    );
    assert_eq!(deleted["output"]["deleted_hash"], moved["output"]["hash"]);
    assert_eq!(deleted["output"]["exists"], false);
    assert!(!workspace.join("archive/notes.txt").exists());

    let copied_deleted = toolbox.execute(
        "Delete",
        json!({
            "path":"archive/copied.txt",
            "expected_hash":copied["output"]["hash"]
        }),
    );
    assert_eq!(copied_deleted["type"], "result");

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn copy_is_atomic_preserves_the_source_and_guides_recoverable_failures() {
    let workspace = temporary_workspace();
    fs::create_dir(workspace.join("copies")).unwrap();
    fs::write(workspace.join("source.bin"), [0x00, 0x11, 0x22, 0xff]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            workspace.join("source.bin"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
    }
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let source = toolbox.execute("Stat", json!({"paths":["source.bin"]}));
    let hash = source["output"]["entries"][0]["hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let copied = toolbox.execute(
        "Copy",
        json!({"path":"source.bin","destination":"copies/target.bin","expected_hash":hash}),
    );
    assert_eq!(copied["type"], "result", "{copied}");
    assert_eq!(copied["output"]["hash"], hash);
    assert_eq!(
        fs::read(workspace.join("source.bin")).unwrap(),
        [0x00, 0x11, 0x22, 0xff]
    );
    assert_eq!(
        fs::read(workspace.join("copies/target.bin")).unwrap(),
        [0x00, 0x11, 0x22, 0xff]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(workspace.join("copies/target.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    let existing = toolbox.execute(
        "Copy",
        json!({"path":"source.bin","destination":"copies/target.bin","expected_hash":hash}),
    );
    assert_eq!(existing["error"]["code"], "already_exists");
    assert!(
        existing["error"]["tip"]
            .as_str()
            .unwrap()
            .contains("new destination")
    );

    let missing_parent = toolbox.execute(
        "Copy",
        json!({"path":"source.bin","destination":"missing/target.bin","expected_hash":hash}),
    );
    assert_eq!(missing_parent["error"]["code"], "parent_not_found");
    assert!(
        missing_parent["error"]["tip"]
            .as_str()
            .unwrap()
            .contains("MakeDirectory")
    );

    fs::write(workspace.join("source.bin"), [0xaa]).unwrap();
    let stale = toolbox.execute(
        "Copy",
        json!({"path":"source.bin","destination":"copies/stale.bin","expected_hash":hash}),
    );
    assert_eq!(stale["error"]["code"], "conflict");
    assert!(!workspace.join("copies/stale.bin").exists());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("source.bin", workspace.join("source-link")).unwrap();
        let linked = toolbox.execute(
            "Copy",
            json!({"path":"source-link","destination":"copies/link.bin","expected_hash":hash}),
        );
        assert_eq!(linked["error"]["code"], "unsupported_file_type");
        assert!(!workspace.join("copies/link.bin").exists());
    }

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn apply_patch_is_hidden_and_rejected_without_writing() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let created = toolbox.execute(
        "Create",
        json!({"path":"safe.txt", "content":"alpha\nbeta\n"}),
    );

    let rejected = toolbox.execute(
        "ApplyPatch",
        json!({
            "path":"safe.txt",
            "expected_hash":created["output"]["hash"],
            "patch":"--- a/safe.txt\n+++ b/safe.txt\n@@ -1 +1 @@\n-alpha\n+changed\n"
        }),
    );
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["error"]["code"], "tool_disabled");
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("File.Edit")
    );
    assert_eq!(
        fs::read_to_string(workspace.join("safe.txt")).unwrap(),
        "alpha\nbeta\n"
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_replaces_deletes_inserts_and_preserves_exact_line_endings() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let cases = [
        (
            "replace-one.txt",
            "alpha\nbeta\ngamma\nomega",
            2,
            2,
            json!(["updated\n"]),
            "alpha\nupdated\ngamma\nomega",
        ),
        (
            "replace-fewer.txt",
            "alpha\nbeta\ngamma\nomega",
            2,
            3,
            json!(["combined\n"]),
            "alpha\ncombined\nomega",
        ),
        (
            "replace-more.txt",
            "alpha\nbeta\ngamma",
            2,
            2,
            json!(["one\n", "two\n", "three\n"]),
            "alpha\none\ntwo\nthree\ngamma",
        ),
        (
            "delete-lines.txt",
            "alpha\nbeta\ngamma\nomega",
            2,
            3,
            json!([]),
            "alpha\nomega",
        ),
        (
            "clear-text.txt",
            "alpha\nbeta\ngamma",
            2,
            2,
            json!(["\n"]),
            "alpha\n\ngamma",
        ),
        (
            "insert-middle.txt",
            "alpha\nbeta\ngamma",
            2,
            1,
            json!(["inserted one\n", "inserted two\n"]),
            "alpha\ninserted one\ninserted two\nbeta\ngamma",
        ),
        (
            "insert-first.txt",
            "alpha\nbeta",
            1,
            0,
            json!(["header\n"]),
            "header\nalpha\nbeta",
        ),
        (
            "append-terminated.txt",
            "alpha\nomega\n",
            3,
            2,
            json!(["appended\n"]),
            "alpha\nomega\nappended\n",
        ),
        (
            "replace-unterminated-final.txt",
            "alpha\nomega",
            2,
            2,
            json!(["omega\n", "appended\n"]),
            "alpha\nomega\nappended",
        ),
        (
            "mixed-endings.txt",
            "unix\nwindows\r\nold-mac\rfinal",
            2,
            2,
            json!(["updated\r\n"]),
            "unix\nupdated\r\nold-mac\rfinal",
        ),
        (
            "old-mac-ending.txt",
            "alpha\rbeta\r",
            1,
            1,
            json!(["updated\r"]),
            "updated\rbeta\r",
        ),
        (
            "delete-all.txt",
            "alpha\nbeta\ngamma\nomega",
            1,
            4,
            json!([]),
            "",
        ),
        (
            "empty.txt",
            "",
            1,
            0,
            json!(["first line\n"]),
            "first line\n",
        ),
    ];

    for (path, initial, start, end, new_lines, expected) in cases {
        let created = toolbox.execute("Create", json!({"path":path, "content":initial}));
        assert_eq!(
            created["type"], "result",
            "create failed for {path}: {created}"
        );
        let edited = toolbox.execute(
            "Edit",
            single_edit_input!(
                path,
                created["output"]["hash"],
                start,
                end,
                new_lines.clone()
            ),
        );
        assert_eq!(edited["type"], "result", "edit failed for {path}: {edited}");
        assert_eq!(edited["output"]["operation"], "edited");
        let edit_result = &edited["output"]["edit_results"][0];
        if start > end {
            assert_eq!(edit_result["operation"], "insert");
            assert_eq!(edit_result["before_line"], start);
        } else if new_lines.as_array().is_some_and(|lines| lines.is_empty()) {
            assert_eq!(edit_result["operation"], "delete");
            assert_eq!(edit_result["start_line"], start);
            assert_eq!(edit_result["end_line"], end);
        } else {
            assert_eq!(edit_result["operation"], "replace");
            assert_eq!(edit_result["start_line"], start);
            assert_eq!(edit_result["end_line"], end);
        }
        assert_eq!(edited["output"]["edit_results"][0]["state"], "succeeded");
        assert_eq!(edited["output"]["previous_size"], initial.len());
        assert!(edited["output"].get("hash").is_none());
        assert!(
            edited["output"]["tip"]
                .as_str()
                .unwrap()
                .contains("use File.Read")
        );
        assert_eq!(fs::read_to_string(workspace.join(path)).unwrap(), expected);
        assert!(
            toolbox.execute("Read", json!({"path":path}))["output"]["hash"]
                .as_str()
                .is_some()
        );
    }

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_locates_every_operation_on_one_snapshot_and_reports_each_result() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let created = toolbox.execute(
        "Create",
        json!({"path":"batch.txt", "content":"aaa\nbbb\nccc\nddd\n"}),
    );
    let edited = toolbox.execute(
        "Edit",
        json!({
            "path":"batch.txt",
            "expected_hash":created["output"]["hash"],
            "edits":[
                {"operation":"replace","start_line":1,"end_line":1,"new_lines":["111\n","aaa\n"]},
                {"operation":"replace","start_line":3,"end_line":3,"new_lines":["333\n","ccc\n"]}
            ]
        }),
    );
    assert_eq!(edited["type"], "result", "batch edit failed: {edited}");
    assert_eq!(
        fs::read_to_string(workspace.join("batch.txt")).unwrap(),
        "111\naaa\nbbb\n333\nccc\nddd\n"
    );
    assert_eq!(edited["output"]["previous_total_lines"], 4);
    assert_eq!(edited["output"]["total_lines"], 6);
    assert_eq!(
        edited["output"]["edit_results"].as_array().unwrap().len(),
        2
    );
    assert_eq!(edited["output"]["edit_results"][0]["index"], 0);
    assert_eq!(edited["output"]["edit_results"][0]["state"], "succeeded");
    assert_eq!(edited["output"]["edit_results"][0]["operation"], "replace");
    assert_eq!(edited["output"]["edit_results"][0]["new_line_count"], 2);
    assert_eq!(edited["output"]["edit_results"][1]["start_line"], 3);
    assert!(edited["output"].get("hash").is_none());
    assert!(
        edited["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("previously readable edit range")
    );

    let search = toolbox.execute(
        "Search",
        json!({"path":"batch.txt", "query":"333", "context_before":1, "context_after":1}),
    );
    let matched = &search["output"]["matches"][0];
    assert!(matched.get("hash").is_none());
    assert_eq!(matched["before"], json!({"3":"bbb"}));
    assert_eq!(matched["match_text"], json!({"4":"333"}));
    assert_eq!(matched["after"], json!({"5":"ccc"}));

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_accepts_unordered_independent_ranges_and_rejects_every_ambiguous_batch_atomically() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let created = toolbox.execute(
        "Create",
        json!({"path":"atomic.txt", "content":"aaa\nbbb\nccc\nddd\n"}),
    );
    let original_hash = created["output"]["hash"].clone();
    let original = "aaa\nbbb\nccc\nddd\n";

    for (name, edits) in [
        (
            "overlapping replacements",
            json!([
                {"operation":"replace","start_line":1,"end_line":2,"new_lines":["left\n"]},
                {"operation":"replace","start_line":2,"end_line":3,"new_lines":["right\n"]}
            ]),
        ),
        (
            "duplicate insertion point",
            json!([
                {"operation":"insert","before_line":2,"new_lines":["first\n"]},
                {"operation":"insert","before_line":2,"new_lines":["second\n"]}
            ]),
        ),
        (
            "insertion inside replacement",
            json!([
                {"operation":"replace","start_line":1,"end_line":3,"new_lines":["block\n"]},
                {"operation":"insert","before_line":2,"new_lines":["inside\n"]}
            ]),
        ),
    ] {
        let rejected = toolbox.execute(
            "Edit",
            json!({"path":"atomic.txt", "expected_hash":original_hash, "edits":edits}),
        );
        assert_eq!(
            rejected["error"]["code"], "overlapping_edits",
            "case={name}: {rejected}"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("atomic.txt")).unwrap(),
            original,
            "case={name} mutated the file"
        );
    }

    let unexpected_nested = toolbox.execute(
        "Edit",
        json!({
            "path":"atomic.txt",
            "expected_hash":original_hash,
            "edits":[{
                "operation":"replace",
                "start_line":1,
                "end_line":1,
                "new_lines":["changed\n"],
                "find":"aaa"
            }]
        }),
    );
    assert_eq!(unexpected_nested["error"]["code"], "invalid_arguments");
    assert_eq!(
        fs::read_to_string(workspace.join("atomic.txt")).unwrap(),
        original
    );

    let edited = toolbox.execute(
        "Edit",
        json!({
            "path":"atomic.txt",
            "expected_hash":original_hash,
            "edits":[
                {"operation":"replace","start_line":4,"end_line":4,"new_lines":["last\n"]},
                {"operation":"insert","before_line":2,"new_lines":["inserted\n"]},
                {"operation":"delete","start_line":3,"end_line":3},
                {"operation":"replace","start_line":2,"end_line":2,"new_lines":["updated\n"]}
            ]
        }),
    );
    assert_eq!(edited["type"], "result", "unordered edit failed: {edited}");
    assert_eq!(
        fs::read_to_string(workspace.join("atomic.txt")).unwrap(),
        "aaa\ninserted\nupdated\nlast\n"
    );
    assert_eq!(
        edited["output"]["edit_results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| result["index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(
        edited["output"]["edit_results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| result["operation"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["replace", "insert", "delete", "replace"]
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_uses_logical_lines_and_rejects_embedded_terminators_atomically() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    toolbox.execute(
        "Create",
        json!({"path":"physical.txt", "content":"alpha\nbeta"}),
    );
    let original = "alpha\nbeta";
    let read = toolbox.execute_raw("Read", json!({"path":"physical.txt"}));
    assert_eq!(read["output"]["lines"], json!({"1":"alpha","2":"beta"}));
    assert_eq!(
        read["output"]["editable_ranges"],
        json!([{"start_line":1,"end_line":2}])
    );
    for (name, new_lines) in [
        ("LF", json!(["one\ntwo"])),
        ("CRLF", json!(["one\r\ntwo"])),
        ("CR", json!(["one\rtwo"])),
        ("non-string", json!([42])),
        ("NUL", json!(["bad\u{0}"])),
    ] {
        let rejected = toolbox.execute_raw(
            "Edit",
            json!({"path":"physical.txt","edits":[{"operation":"replace","start_line":1,"end_line":1,"new_lines":new_lines}]}),
        );
        assert_eq!(
            rejected["error"]["code"], "invalid_line_syntax",
            "case={name}: {rejected}"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("physical.txt")).unwrap(),
            original
        );
    }

    let accepted = toolbox.execute_raw(
        "Edit",
        json!({"path":"physical.txt","edits":[{"operation":"replace","start_line":1,"end_line":1,"new_lines":["changed",""]}]}),
    );
    assert_eq!(accepted["type"], "result", "{accepted}");
    assert_eq!(
        fs::read_to_string(workspace.join("physical.txt")).unwrap(),
        "changed\n\nbeta"
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_requires_visible_read_ranges_merges_them_and_clears_them_after_success() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    toolbox.execute(
        "Create",
        json!({"path":"scoped.txt", "content":"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n"}),
    );

    let without_read = toolbox.execute_raw(
        "Edit",
        json!({"path":"scoped.txt","edits":[{"operation":"replace","start_line":2,"end_line":2,"new_lines":["TWO"]}]}),
    );
    assert_eq!(without_read["error"]["code"], "read_required");
    assert!(
        without_read["error"]["tip"]
            .as_str()
            .unwrap()
            .contains("wider range")
    );

    let search = toolbox.execute_raw(
        "Search",
        json!({"path":"scoped.txt","query":"two","context_before":1,"context_after":1}),
    );
    assert_eq!(search["type"], "result");
    let after_search = toolbox.execute_raw(
        "Edit",
        json!({"path":"scoped.txt","edits":[{"operation":"replace","start_line":2,"end_line":2,"new_lines":["TWO"]}]}),
    );
    assert_eq!(after_search["error"]["code"], "read_required");

    let first = toolbox.execute_raw(
        "Read",
        json!({"path":"scoped.txt","start_line":2,"end_line":3}),
    );
    assert_eq!(first["output"]["lines"], json!({"2":"two","3":"three"}));
    assert_eq!(
        first["output"]["editable_ranges"],
        json!([{"start_line":2,"end_line":3}])
    );
    let second = toolbox.execute_raw(
        "Read",
        json!({"path":"scoped.txt","start_line":5,"end_line":5}),
    );
    assert_eq!(
        second["output"]["editable_ranges"],
        json!([
            {"start_line":2,"end_line":3},
            {"start_line":5,"end_line":5}
        ])
    );

    let unread_gap = toolbox.execute_raw(
        "Edit",
        json!({"path":"scoped.txt","edits":[{"operation":"replace","start_line":3,"end_line":5,"new_lines":["merged"]}]}),
    );
    assert_eq!(unread_gap["error"]["code"], "unread_range");
    assert!(
        unread_gap["error"]["tip"]
            .as_str()
            .unwrap()
            .contains("wider range")
    );
    assert_eq!(
        fs::read_to_string(workspace.join("scoped.txt")).unwrap(),
        "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n"
    );

    let edited = toolbox.execute_raw(
        "Edit",
        json!({
            "path":"scoped.txt",
            "edits":[
                {"operation":"replace","start_line":2,"end_line":2,"new_lines":["TWO"]},
                {"operation":"replace","start_line":5,"end_line":5,"new_lines":["FIVE"]}
            ]
        }),
    );
    assert_eq!(edited["type"], "result", "{edited}");
    assert!(
        edited["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("wider continuous range")
    );
    assert_eq!(
        fs::read_to_string(workspace.join("scoped.txt")).unwrap(),
        "one\r\nTWO\r\nthree\r\nfour\r\nFIVE\r\nsix\r\n"
    );

    let edit_again = toolbox.execute_raw(
        "Edit",
        json!({"path":"scoped.txt","edits":[{"operation":"delete","start_line":3,"end_line":3}]}),
    );
    assert_eq!(edit_again["error"]["code"], "read_required");
    assert!(
        edit_again["error"]["tip"]
            .as_str()
            .unwrap()
            .contains("wider range")
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn read_supports_optional_inclusive_bounds_and_reports_total_lines() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    toolbox.execute(
        "Create",
        json!({"path":"ranges.txt", "content":"one\ntwo\nthree\nfour\nfive\n"}),
    );

    let complete = toolbox.execute_raw("Read", json!({"path":"ranges.txt"}));
    assert_eq!(complete["type"], "result", "{complete}");
    assert_eq!(complete["output"]["total_lines"], 5);
    assert_eq!(complete["output"]["start_line"], 1);
    assert_eq!(complete["output"]["end_line"], 5);
    assert_eq!(complete["output"]["eof"], true);
    assert_eq!(complete["output"]["truncated"], false);
    assert_eq!(
        complete["output"]["lines"],
        json!({"1":"one","2":"two","3":"three","4":"four","5":"five"})
    );

    let from_start = toolbox.execute_raw("Read", json!({"path":"ranges.txt","start_line":3}));
    assert_eq!(from_start["output"]["total_lines"], 5);
    assert_eq!(from_start["output"]["start_line"], 3);
    assert_eq!(from_start["output"]["end_line"], 5);
    assert_eq!(
        from_start["output"]["lines"],
        json!({"3":"three","4":"four","5":"five"})
    );

    let through_end = toolbox.execute_raw("Read", json!({"path":"ranges.txt","end_line":3}));
    assert_eq!(through_end["output"]["total_lines"], 5);
    assert_eq!(through_end["output"]["start_line"], 1);
    assert_eq!(through_end["output"]["end_line"], 3);
    assert_eq!(through_end["output"]["eof"], false);
    assert_eq!(through_end["output"]["truncated"], true);
    assert_eq!(
        through_end["output"]["lines"],
        json!({"1":"one","2":"two","3":"three"})
    );

    let bounded = toolbox.execute_raw(
        "Read",
        json!({"path":"ranges.txt","start_line":2,"end_line":4}),
    );
    assert_eq!(bounded["output"]["total_lines"], 5);
    assert_eq!(bounded["output"]["start_line"], 2);
    assert_eq!(bounded["output"]["end_line"], 4);
    assert_eq!(
        bounded["output"]["lines"],
        json!({"2":"two","3":"three","4":"four"})
    );

    let clamped_end = toolbox.execute_raw(
        "Read",
        json!({"path":"ranges.txt","start_line":4,"end_line":700}),
    );
    assert_eq!(clamped_end["type"], "result", "{clamped_end}");
    assert_eq!(clamped_end["output"]["total_lines"], 5);
    assert_eq!(clamped_end["output"]["start_line"], 4);
    assert_eq!(clamped_end["output"]["end_line"], 5);
    assert_eq!(
        clamped_end["output"]["lines"],
        json!({"4":"four","5":"five"})
    );
    assert!(
        clamped_end["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("5 lines")
    );

    let past_eof = toolbox.execute_raw("Read", json!({"path":"ranges.txt","start_line":600}));
    assert_eq!(past_eof["type"], "result", "{past_eof}");
    assert_eq!(past_eof["output"]["total_lines"], 5);
    assert!(past_eof["output"]["start_line"].is_null());
    assert!(past_eof["output"]["end_line"].is_null());
    assert_eq!(past_eof["output"]["lines"], json!({}));
    assert_eq!(past_eof["output"]["eof"], true);
    assert!(
        past_eof["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("this range contains no lines")
    );

    let reversed = toolbox.execute_raw(
        "Read",
        json!({"path":"ranges.txt","start_line":4,"end_line":3}),
    );
    assert_eq!(reversed["type"], "error");
    assert_eq!(reversed["error"]["code"], "invalid_arguments");
    let old_parameter = toolbox.execute_raw("Read", json!({"path":"ranges.txt","max_lines":2}));
    assert_eq!(old_parameter["type"], "error");
    assert_eq!(old_parameter["error"]["code"], "invalid_arguments");

    toolbox.execute("Create", json!({"path":"empty.txt", "content":""}));
    let empty = toolbox.execute_raw("Read", json!({"path":"empty.txt"}));
    assert_eq!(empty["output"]["total_lines"], 0);
    assert!(empty["output"]["start_line"].is_null());
    assert!(empty["output"]["end_line"].is_null());
    assert_eq!(empty["output"]["lines"], json!({}));
    assert_eq!(empty["output"]["eof"], true);
    assert_eq!(
        empty["output"]["editable_ranges"],
        json!([]),
        "an empty EOF is authorized internally without inventing line ranges"
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_rejects_bad_ranges_stale_hashes_and_lossy_text_atomically() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let created = toolbox.execute(
        "Create",
        json!({"path":"safe.txt", "content":"alpha\nbeta\ngamma\n"}),
    );
    let hash = created["output"]["hash"].as_str().unwrap();
    for (name, start, end) in [
        ("reversed gap", 5, 2),
        ("replacement beyond eof", 4, 4),
        ("insertion beyond eof", 5, 4),
        ("end without start", 1, 3 + 1),
    ] {
        let rejected = toolbox.execute(
            "Edit",
            single_edit_input!("safe.txt", hash, start, end, json!(["changed\n"])),
        );
        assert_eq!(rejected["type"], "error", "{name} unexpectedly succeeded");
        assert_eq!(rejected["error"]["code"], "invalid_range", "case={name}");
        assert_eq!(
            fs::read_to_string(workspace.join("safe.txt")).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
    }

    let unexpected = toolbox.execute(
        "Edit",
        json!({
            "path":"safe.txt",
            "expected_hash":hash,
            "edits":[{"operation":"replace","start_line":2,"end_line":2,"new_lines":["changed\n"]}],
            "find":"beta"
        }),
    );
    assert_eq!(unexpected["error"]["code"], "invalid_arguments");

    toolbox.execute_raw("Read", json!({"path":"safe.txt"}));
    fs::write(workspace.join("safe.txt"), "external\n").unwrap();
    let stale = toolbox.execute_raw(
        "Edit",
        json!({"path":"safe.txt","edits":[{"operation":"replace","start_line":1,"end_line":1,"new_lines":["changed"]}]}),
    );
    assert_eq!(stale["error"]["code"], "stale_read");
    assert!(
        stale["error"]["tip"]
            .as_str()
            .unwrap()
            .contains("wider range")
    );
    assert_eq!(
        fs::read_to_string(workspace.join("safe.txt")).unwrap(),
        "external\n"
    );

    let western = toolbox.execute(
        "Create",
        json!({
            "path":"western.txt",
            "content":"Café – résumé",
            "encoding":"windows-1252"
        }),
    );
    let before = fs::read(workspace.join("western.txt")).unwrap();
    let lossy = toolbox.execute(
        "Edit",
        single_edit_input!(
            "western.txt",
            western["output"]["hash"],
            1,
            1,
            json!(["中文\n"])
        ),
    );
    assert_eq!(lossy["error"]["code"], "encoding_error");
    assert_eq!(fs::read(workspace.join("western.txt")).unwrap(), before);

    let western_batch = toolbox.execute(
        "Create",
        json!({
            "path":"western-batch.txt",
            "content":"Café\nrésumé",
            "encoding":"windows-1252"
        }),
    );
    let western_batch_before = fs::read(workspace.join("western-batch.txt")).unwrap();
    let batch_lossy = toolbox.execute(
        "Edit",
        json!({
            "path":"western-batch.txt",
            "expected_hash":western_batch["output"]["hash"],
            "edits":[
                {"operation":"replace","start_line":1,"end_line":1,"new_lines":["Cafe\n"]},
                {"operation":"replace","start_line":2,"end_line":2,"new_lines":["中文\n"]}
            ]
        }),
    );
    assert_eq!(batch_lossy["error"]["code"], "encoding_error");
    assert_eq!(
        fs::read(workspace.join("western-batch.txt")).unwrap(),
        western_batch_before
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn make_directory_supports_strict_and_recursive_creation_safely() {
    let workspace = temporary_workspace();
    let outside = workspace.parent().unwrap().join(format!(
        "me-file-directory-outside-{}",
        workspace.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(&outside).unwrap();
    fs::write(workspace.join("occupied"), "file").unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let parent = toolbox.execute("MakeDirectory", json!({"path":"generated"}));
    assert_eq!(parent["type"], "result");
    assert_eq!(parent["output"]["path"], "generated");
    assert_eq!(parent["output"]["operation"], "directory_created");
    assert_eq!(parent["output"]["exists"], true);
    assert!(workspace.join("generated").is_dir());

    let child = toolbox.execute("MakeDirectory", json!({"path":"generated/nested"}));
    assert_eq!(child["type"], "result");
    assert!(workspace.join("generated/nested").is_dir());

    let existing_directory = toolbox.execute("MakeDirectory", json!({"path":"generated"}));
    assert_eq!(existing_directory["error"]["code"], "already_exists");
    let existing_file = toolbox.execute("MakeDirectory", json!({"path":"occupied"}));
    assert_eq!(existing_file["error"]["code"], "already_exists");

    let missing_parent = toolbox.execute("MakeDirectory", json!({"path":"missing/child"}));
    assert_eq!(missing_parent["error"]["code"], "parent_not_found");
    assert!(!workspace.join("missing").exists());

    let recursive = toolbox.execute(
        "MakeDirectory",
        json!({"path":"recursive/one/two/three", "parents":true}),
    );
    assert_eq!(recursive["type"], "result");
    assert_eq!(recursive["output"]["path"], "recursive/one/two/three");
    assert!(workspace.join("recursive/one/two/three").is_dir());
    let recursive_existing = toolbox.execute(
        "MakeDirectory",
        json!({"path":"recursive/one/two/three", "parents":true}),
    );
    assert_eq!(recursive_existing["error"]["code"], "already_exists");

    let invalid_parents =
        toolbox.execute("MakeDirectory", json!({"path":"invalid", "parents":"yes"}));
    assert_eq!(invalid_parents["error"]["code"], "invalid_arguments");
    assert!(!workspace.join("invalid").exists());

    let root = toolbox.execute("MakeDirectory", json!({"path":"."}));
    assert_eq!(root["error"]["code"], "invalid_path");
    let recursive_root = toolbox.execute("MakeDirectory", json!({"path":".", "parents":true}));
    assert_eq!(recursive_root["error"]["code"], "invalid_path");
    let escaped = toolbox.execute(
        "MakeDirectory",
        json!({"path":format!("../{}/child", outside.file_name().unwrap().to_string_lossy())}),
    );
    assert_eq!(escaped["type"], "result");
    assert_eq!(
        escaped["output"]["path"],
        outside
            .join("child")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    );
    assert!(outside.join("child").is_dir());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, workspace.join("outside-link")).unwrap();
        let symlink_escape = toolbox.execute(
            "MakeDirectory",
            json!({"path":"outside-link/missing/child", "parents":true}),
        );
        assert_eq!(symlink_escape["type"], "result");
        assert!(outside.join("missing/child").is_dir());
    }

    toolbox.finish();
    fs::remove_dir_all(outside).unwrap();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn stale_hash_fails_without_mutating_the_file() {
    let workspace = temporary_workspace();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let created = toolbox.execute(
        "Create",
        json!({"path":"state.txt", "content":"same\nsame\nend\n"}),
    );
    let hash = created["output"]["hash"].as_str().unwrap();

    fs::write(workspace.join("state.txt"), "external\n").unwrap();
    let stale = toolbox.execute(
        "Append",
        json!({"path":"state.txt", "expected_hash":hash, "content":"should-not-appear"}),
    );
    assert_eq!(stale["type"], "error");
    assert_eq!(stale["error"]["code"], "conflict");
    assert!(
        stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("current_hash=")
    );
    assert!(
        stale["error"]["tip"]
            .as_str()
            .unwrap()
            .contains("current hash")
    );
    assert_eq!(
        fs::read_to_string(workspace.join("state.txt")).unwrap(),
        "external\n"
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn read_list_find_search_stat_and_bytes_have_stable_structured_results() {
    let workspace = temporary_workspace();
    fs::create_dir_all(workspace.join("src/nested")).unwrap();
    fs::write(workspace.join("src/a.txt"), "zero\nNeedle one\nlast\n").unwrap();
    fs::write(workspace.join("src/nested/b.txt"), "needle two\n").unwrap();
    fs::write(workspace.join("src/blob.bin"), [0, 1, 2, 255]).unwrap();
    fs::write(workspace.join("src/.hidden.txt"), "Needle hidden\n").unwrap();
    fs::write(
        workspace.join("src/line-endings.txt"),
        "first\r\n\rthird\u{2028}same\nlast",
    )
    .unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let read = toolbox.execute(
        "Read",
        json!({"path":"src/a.txt", "start_line":2, "end_line":2}),
    );
    assert_eq!(read["output"]["lines"], numbered_lines("Needle one\n", 2));
    assert_eq!(read["output"]["start_line"], 2);
    assert_eq!(read["output"]["end_line"], 2);
    assert_eq!(read["output"]["truncated"], true);

    let line_endings = toolbox.execute("Read", json!({"path":"src/line-endings.txt"}));
    assert_eq!(
        line_endings["output"]["lines"],
        json!({"1":"first", "2":"", "3":"third\u{2028}same", "4":"last"})
    );

    let bytes = toolbox.execute(
        "ReadBytes",
        json!({"path":"src/blob.bin", "offset":1, "length":2}),
    );
    assert_eq!(bytes["output"]["data"], "01 02");
    assert!(bytes["output"].get("base64").is_none());
    assert_eq!(bytes["output"]["length"], 2);
    assert_eq!(bytes["output"]["eof"], false);

    let all_bytes = toolbox.execute("ReadBytes", json!({"path":"src/blob.bin"}));
    assert_eq!(all_bytes["output"]["data"], "00 01 02 ff");
    assert_eq!(all_bytes["output"]["offset"], 0);
    assert_eq!(all_bytes["output"]["length"], 4);
    assert_eq!(all_bytes["output"]["size"], 4);
    assert_eq!(all_bytes["output"]["eof"], true);
    assert_eq!(all_bytes["output"]["hash"].as_str().unwrap().len(), 8);

    let past_eof = toolbox.execute(
        "ReadBytes",
        json!({"path":"src/blob.bin", "offset":99, "length":10}),
    );
    assert_eq!(past_eof["output"]["data"], "");
    assert_eq!(past_eof["output"]["offset"], 4);
    assert_eq!(past_eof["output"]["length"], 0);
    assert_eq!(past_eof["output"]["eof"], true);
    assert!(
        past_eof["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("contains no bytes")
    );

    let list = toolbox.execute(
        "List",
        json!({"path":"src", "depth":2, "include_hidden":false}),
    );
    let listed = list["output"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        vec![
            "src/a.txt",
            "src/blob.bin",
            "src/line-endings.txt",
            "src/nested",
            "src/nested/b.txt"
        ]
    );
    assert_eq!(list["output"]["returned"], 5);

    let find = toolbox.execute(
        "Find",
        json!({"patterns":["src/**/*.txt"], "include_hidden":false}),
    );
    assert_eq!(
        find["output"]["results"],
        json!(["src/a.txt", "src/line-endings.txt", "src/nested/b.txt"])
    );
    assert_eq!(find["output"]["returned"], 3);

    let search = toolbox.execute(
        "Search",
        json!({
            "path":"src",
            "query":"needle",
            "case_sensitive":false,
            "context_before":1,
            "context_after":1
        }),
    );
    assert_eq!(search["output"]["matches"].as_array().unwrap().len(), 2);
    assert_eq!(search["output"]["returned"], 2);
    assert_eq!(search["output"]["matches"][0]["column"], 1);
    assert!(search["output"]["matches"][0].get("hash").is_none());
    assert!(search["output"]["matches"][1].get("hash").is_none());
    assert_eq!(
        search["output"]["matches"][0]["before"],
        json!({"1":"zero"})
    );
    assert_eq!(
        search["output"]["matches"][0]["match_text"],
        json!({"2":"Needle one"})
    );
    assert_eq!(search["output"]["matches"][0]["after"], json!({"3":"last"}));
    assert_eq!(search["output"]["skipped_binary"], 1);

    let status = toolbox.execute(
        "Stat",
        json!({"paths":["src/a.txt", "src", "not-here.txt"]}),
    );
    assert_eq!(status["output"]["entries"][0]["type"], "file");
    assert_eq!(
        status["output"]["entries"][0]["hash"]
            .as_str()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(status["output"]["entries"][1]["type"], "directory");
    assert_eq!(status["output"]["entries"][2]["exists"], false);
    assert_eq!(status["output"]["returned"], 3);
    assert!(
        status["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("1 requested path does not exist")
    );

    fs::create_dir(workspace.join("empty-dir")).unwrap();
    let empty_list = toolbox.execute("List", json!({"path":"empty-dir"}));
    assert_eq!(empty_list["output"]["returned"], 0);
    assert!(
        empty_list["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("No entries")
    );
    let empty_find = toolbox.execute("Find", json!({"path":"src", "patterns":["*.missing"]}));
    assert_eq!(empty_find["output"]["returned"], 0);
    assert!(
        empty_find["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("No paths")
    );
    let empty_search = toolbox.execute(
        "Search",
        json!({"path":"src", "query":"definitely-not-present"}),
    );
    assert_eq!(empty_search["output"]["returned"], 0);
    assert!(
        empty_search["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("No text")
    );

    let twelve_lines = (1..=12)
        .map(|number| format!("line {number}\n"))
        .collect::<String>();
    fs::write(workspace.join("twelve.txt"), twelve_lines).unwrap();
    let numbered = toolbox.execute("Read", json!({"path":"twelve.txt"}));
    assert_eq!(numbered["output"]["lines"]["01"], "line 1");
    assert_eq!(numbered["output"]["lines"]["12"], "line 12");
    assert_eq!(
        numbered["output"]["lines"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        (1..=12)
            .map(|number| format!("{number:02}"))
            .collect::<Vec<_>>()
    );
    let padded_search = toolbox.execute(
        "Search",
        json!({
            "path":"twelve.txt",
            "query":"line 10",
            "context_before":1,
            "context_after":1
        }),
    );
    let padded_match = &padded_search["output"]["matches"][0];
    assert_eq!(padded_match["before"], json!({"09":"line 9"}));
    assert_eq!(padded_match["match_text"], json!({"10":"line 10"}));
    assert_eq!(padded_match["after"], json!({"11":"line 11"}));
    assert!(padded_match.get("line").is_none());
    assert!(padded_match.get("text").is_none());

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn integrated_search_honors_ripgrep_scope_and_match_semantics() {
    let workspace = temporary_workspace();
    let root = workspace.join("search-root");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::create_dir_all(root.join("ignored-dir")).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "ignored.txt\nignored-dir/\n").unwrap();
    fs::write(root.join(".ignore"), "custom.txt\n").unwrap();
    fs::write(root.join("a.txt"), "before\n🙂前缀 Needle42 tail\nafter\n").unwrap();
    fs::write(root.join("b.rs"), "needle7\n").unwrap();
    fs::write(root.join("sub/c.rs"), "NEEDLE8\n").unwrap();
    fs::write(root.join("ignored.txt"), "ignored-needle\n").unwrap();
    fs::write(root.join("ignored-dir/nested.txt"), "ignored-needle\n").unwrap();
    fs::write(root.join("custom.txt"), "ignored-needle\n").unwrap();
    fs::write(root.join(".hidden.txt"), "ignored-needle\n").unwrap();
    fs::write(root.join("binary.bin"), b"ignored-needle\0tail").unwrap();
    fs::write(
        root.join("legacy.txt"),
        b"\xbc\xf2\xcc\xe5\xd6\xd0\xce\xc4\xc4\xda\xc8\xdd\xa3\xac\xc4\xe3\xba\xc3\xca\xc0\xbd\xe7\xa1\xa3 ignored-needle\n",
    )
    .unwrap();
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(b"bom needle\n");
    fs::write(root.join("bom.txt"), bom).unwrap();
    fs::write(
        root.join("bom-utf16.txt"),
        utf16_le("utf16 bom needle\n", true),
    )
    .unwrap();
    let outside = workspace.join("outside-search-target");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("linked.txt"), "outside-only\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("linked-directory")).unwrap();

    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let unicode = toolbox.execute(
        "Search",
        json!({
            "path":"search-root",
            "query":r"Needle\d+",
            "regex":true,
            "case_sensitive":false,
            "globs":["**/*.txt"],
            "context_before":1,
            "context_after":1
        }),
    );
    assert_eq!(unicode["type"], "result", "regex search failed: {unicode}");
    assert_eq!(unicode["output"]["returned"], 1);
    let matched = &unicode["output"]["matches"][0];
    assert_eq!(matched["path"], "search-root/a.txt");
    assert_eq!(matched["column"], 5);
    assert_eq!(matched["match_length"], 8);
    assert_eq!(matched["before"], json!({"1":"before"}));
    assert_eq!(matched["match_text"], json!({"2":"🙂前缀 Needle42 tail"}));
    assert_eq!(matched["after"], json!({"3":"after"}));

    let ordered = toolbox.execute(
        "Search",
        json!({
            "path":"search-root",
            "query":r"needle\d+",
            "regex":true,
            "case_sensitive":false,
            "globs":["*.rs"],
            "max_matches":10
        }),
    );
    assert_eq!(
        ordered["output"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["search-root/b.rs", "search-root/sub/c.rs"]
    );
    let capped = toolbox.execute(
        "Search",
        json!({
            "path":"search-root",
            "query":r"needle\d+",
            "regex":true,
            "case_sensitive":false,
            "globs":["*.rs"],
            "max_matches":1
        }),
    );
    assert_eq!(capped["output"]["returned"], 1);
    assert_eq!(capped["output"]["truncated"], true);
    assert_eq!(capped["output"]["matches"][0]["path"], "search-root/b.rs");

    let ignored = toolbox.execute(
        "Search",
        json!({"path":"search-root", "query":"ignored-needle"}),
    );
    assert_eq!(ignored["output"]["returned"], 1);
    assert_eq!(
        ignored["output"]["matches"][0]["path"],
        "search-root/legacy.txt"
    );
    assert_eq!(ignored["output"]["skipped_binary"], 1);
    let explicit_ignored = toolbox.execute(
        "Search",
        json!({"path":"search-root/ignored.txt", "query":"ignored-needle"}),
    );
    assert_eq!(explicit_ignored["output"]["returned"], 1);
    let explicit_hidden = toolbox.execute(
        "Search",
        json!({"path":"search-root/.hidden.txt", "query":"ignored-needle"}),
    );
    assert_eq!(explicit_hidden["output"]["returned"], 1);

    let bom = toolbox.execute(
        "Search",
        json!({"path":"search-root/bom.txt", "query":"bom needle"}),
    );
    assert_eq!(bom["output"]["returned"], 1);
    assert_eq!(bom["output"]["matches"][0]["column"], 1);
    let utf16_bom = toolbox.execute(
        "Search",
        json!({"path":"search-root/bom-utf16.txt", "query":"utf16 bom needle"}),
    );
    assert_eq!(utf16_bom["output"]["returned"], 1);
    assert_eq!(utf16_bom["output"]["matches"][0]["column"], 1);
    let symlink = toolbox.execute(
        "Search",
        json!({"path":"search-root", "query":"outside-only"}),
    );
    assert_eq!(symlink["output"]["returned"], 0);

    for query in [r"(?=Needle)", r"(Needle)\1"] {
        let invalid = toolbox.execute(
            "Search",
            json!({"path":"search-root/a.txt", "query":query, "regex":true}),
        );
        assert_eq!(invalid["type"], "error");
        assert_eq!(invalid["error"]["code"], "invalid_regex");
        assert_eq!(invalid["error"]["retryable"], false);
    }

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn integrated_search_detects_common_legacy_encodings_and_late_non_utf8() {
    let workspace = temporary_workspace();
    let fixtures: Vec<(&str, &[u8], &str, &str)> = vec![
        (
            "simplified.txt",
            b"\xbc\xf2\xcc\xe5\xd6\xd0\xce\xc4\xc4\xda\xc8\xdd\xa3\xac\xc4\xe3\xba\xc3\xca\xc0\xbd\xe7\xa1\xa3",
            "你好世界",
            "简体中文内容，你好世界。",
        ),
        (
            "traditional.txt",
            b"\xc1c\xc5\xe9\xa4\xa4\xa4\xe5\xa4\xba\xaee\xa1A\xa7A\xa6n\xa5@\xac\xc9\xa1C",
            "你好世界",
            "繁體中文內容，你好世界。",
        ),
        (
            "japanese.txt",
            b"\x93\xfa\x96{\x8c\xea\x82\xcc\x95\xb6\x8f\xcd\x82\xc5\x82\xb7\x81B\x82\xb1\x82\xf1\x82\xc9\x82\xbf\x82\xcd\x90\xa2\x8aE\x81B",
            "こんにちは世界",
            "日本語の文章です。こんにちは世界。",
        ),
        (
            "korean.txt",
            b"\xc7\xd1\xb1\xb9\xbe\xee \xb9\xae\xc0\xe5\xc0\xd4\xb4\xcf\xb4\xd9. \xbe\xc8\xb3\xe7\xc7\xcf\xbc\xbc\xbf\xe4 \xbc\xbc\xb0\xe8.",
            "안녕하세요 세계",
            "한국어 문장입니다. 안녕하세요 세계.",
        ),
        (
            "western.txt",
            b"Caf\xe9 d\xe9j\xe0 vu \x96 r\xe9sum\xe9 and na\xefve.",
            "résumé",
            "Café déjà vu – résumé and naïve.",
        ),
    ];
    for (path, bytes, _, _) in &fixtures {
        fs::write(workspace.join(path), bytes).unwrap();
    }

    let japanese = "日本語の文章です。こんにちは世界。".repeat(8);
    let (euc_jp, _, euc_jp_errors) = encoding_rs::EUC_JP.encode(&japanese);
    assert!(!euc_jp_errors);
    fs::write(workspace.join("euc-jp.txt"), euc_jp.as_ref()).unwrap();

    let extended_chinese = "简体中文内容和扩展字符𠀀。".repeat(8);
    let (gb18030, _, gb18030_errors) = encoding_rs::GB18030.encode(&extended_chinese);
    assert!(!gb18030_errors);
    fs::write(workspace.join("gb18030-extended.txt"), gb18030.as_ref()).unwrap();

    let russian = "Русский текст и проверка поиска. ".repeat(8);
    let (windows_1251, _, windows_1251_errors) = encoding_rs::WINDOWS_1251.encode(&russian);
    assert!(!windows_1251_errors);
    fs::write(workspace.join("windows-1251.txt"), windows_1251.as_ref()).unwrap();

    fs::write(
        workspace.join("utf32.txt"),
        utf32_be("UTF32 中文目标\n", true),
    )
    .unwrap();

    let mut late = vec![b'a'; 64 * 1024 + 1024];
    late.push(b'\n');
    late.extend_from_slice(
        b"\xbc\xf2\xcc\xe5\xd6\xd0\xce\xc4\xc4\xda\xc8\xdd\xa3\xac\xc4\xe3\xba\xc3\xca\xc0\xbd\xe7\xa1\xa3\n",
    );
    fs::write(workspace.join("late-gb18030.txt"), late).unwrap();

    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    for (path, _, query, expected) in fixtures {
        let result = toolbox.execute("Search", json!({"path":path, "query":query}));
        assert_eq!(
            result["type"], "result",
            "search failed for {path}: {result}"
        );
        assert_eq!(result["output"]["returned"], 1, "wrong result for {path}");
        assert_eq!(result["output"]["skipped_binary"], 0);
        assert_eq!(
            result["output"]["matches"][0]["match_text"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap(),
            expected
        );
    }

    let gb_regex = toolbox.execute(
        "Search",
        json!({"path":"simplified.txt", "query":r"你.世界", "regex":true}),
    );
    assert_eq!(gb_regex["output"]["returned"], 1);
    assert_eq!(gb_regex["output"]["matches"][0]["column"], 8);
    assert_eq!(gb_regex["output"]["matches"][0]["match_length"], 4);
    let western_case = toolbox.execute(
        "Search",
        json!({"path":"western.txt", "query":"RÉSUMÉ", "case_sensitive":false}),
    );
    assert_eq!(western_case["output"]["returned"], 1);

    for (path, query) in [
        ("euc-jp.txt", "こんにちは世界"),
        ("gb18030-extended.txt", "𠀀"),
        ("windows-1251.txt", "проверка поиска"),
        ("utf32.txt", "中文目标"),
        ("late-gb18030.txt", "你好世界"),
    ] {
        let result = toolbox.execute("Search", json!({"path":path, "query":query}));
        assert_eq!(
            result["type"], "result",
            "search failed for {path}: {result}"
        );
        assert!(
            result["output"]["returned"].as_u64().unwrap() >= 1,
            "wrong result for {path}"
        );
        assert_eq!(result["output"]["skipped_binary"], 0);
    }

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[cfg(unix)]
#[test]
fn search_timeout_is_non_retryable_reaps_worker_and_preserves_file_toolbox() {
    let workspace = temporary_workspace();
    fs::write(workspace.join("target.txt"), "needle\n").unwrap();
    let script = generated_file_toolbox(&workspace);
    let source = fs::read_to_string(&script).unwrap();
    assert_eq!(source.matches("SEARCH_TIMEOUT_SECONDS = 120").count(), 1);
    fs::write(
        &script,
        source.replacen(
            "SEARCH_TIMEOUT_SECONDS = 120",
            "SEARCH_TIMEOUT_SECONDS = 5",
            1,
        ),
    )
    .unwrap();

    let marker = workspace.join("search-worker.pid");
    let host = workspace.join("slow-search-host");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();

    let mut toolbox = ToolboxProcess::start_with_search_host(&workspace, &script, &host);
    let timed_out = toolbox.execute("Search", json!({"path":"target.txt", "query":"needle"}));
    assert_eq!(timed_out["type"], "error");
    assert_eq!(timed_out["error"]["code"], "search_timeout");
    assert_eq!(timed_out["error"]["retryable"], false);
    assert_eq!(
        timed_out["error"]["message"],
        "File.Search timed out after 120 seconds."
    );
    let tip = timed_out["error"]["tip"].as_str().unwrap();
    assert!(tip.contains("smaller path"));
    assert!(tip.contains("depth"));
    assert!(tip.contains("globs"));

    let worker_pid = fs::read_to_string(&marker).unwrap();
    let worker_is_alive = Command::new("/bin/kill")
        .args(["-0", worker_pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!worker_is_alive, "timed-out search worker was not reaped");

    let recovered = toolbox.execute("Stat", json!({"paths":["target.txt"]}));
    assert_eq!(recovered["type"], "result");
    assert_eq!(recovered["output"]["entries"][0]["type"], "file");

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[cfg(unix)]
#[test]
fn cancelling_search_terminates_the_worker_tree_and_restarts_file_toolbox() {
    let workspace = temporary_workspace();
    fs::write(workspace.join("target.txt"), "needle\n").unwrap();
    let script = generated_file_toolbox(&workspace);

    let marker = workspace.join("cancelled-search-worker.pid");
    let host = workspace.join("cancelled-search-host");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();

    let source = fs::read_to_string(&script).unwrap();
    let host_assignment = format!("    host = {:?}", host.to_string_lossy().as_ref());
    let modified = source
        .replacen("# ME-S-MANAGED-TOOLBOX", "# TEST-CUSTOM-TOOLBOX", 1)
        .replacen(
            "    host = os.environ.get(\"ME_TOOLBOX_HOST\") or shutil.which(\"me-s\")",
            &host_assignment,
            1,
        );
    assert_ne!(modified, source);
    fs::write(&script, modified).unwrap();
    let tools = script.parent().unwrap();
    for name in ["Terminal.py", "WebBrowser.py", "Desktop.py"] {
        fs::remove_file(tools.join(name)).unwrap();
    }
    let runtime = ToolboxRuntime::load(&workspace).unwrap();

    let started = Instant::now();
    let mut cancellation_requested_at = None;
    let cancelled = runtime
        .execute_cancellable(
            "File.Search",
            r#"{"path":"target.txt","query":"needle"}"#,
            |_| Ok(()),
            || {
                if marker.exists() {
                    cancellation_requested_at = Some(Instant::now());
                    true
                } else {
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "search worker did not start"
                    );
                    false
                }
            },
        )
        .unwrap_err();
    assert!(matches!(cancelled, ToolboxExecutionError::Interrupted(_)));
    assert!(
        cancellation_requested_at.unwrap().elapsed() < Duration::from_secs(2),
        "search cancellation did not promptly close"
    );

    let worker_pid = fs::read_to_string(&marker).unwrap();
    let worker_is_alive = Command::new("/bin/kill")
        .args(["-0", worker_pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(
        !worker_is_alive,
        "cancelled search worker was not terminated"
    );

    let recovered = runtime
        .execute("File.Stat", r#"{"paths":["target.txt"]}"#, |_| Ok(()))
        .unwrap();
    assert_eq!(recovered["entries"][0]["type"], "file");

    drop(runtime);
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn search_accepts_ten_thousand_context_lines_on_each_side() {
    let workspace = temporary_workspace();
    let mut content = (1..=10_000)
        .map(|number| format!("before {number}\n"))
        .collect::<String>();
    content.push_str("unique needle\n");
    content.extend((1..=10_000).map(|number| format!("after {number}\n")));
    fs::write(workspace.join("large-context.txt"), content).unwrap();

    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let result = toolbox.execute(
        "Search",
        json!({
            "path":"large-context.txt",
            "query":"unique needle",
            "context_before":10_000,
            "context_after":10_000
        }),
    );
    assert_eq!(
        result["type"], "result",
        "large context search failed: {result}"
    );
    let matched = &result["output"]["matches"][0];
    assert_eq!(matched["before"].as_object().unwrap().len(), 10_000);
    assert_eq!(matched["after"].as_object().unwrap().len(), 10_000);
    assert_eq!(matched["before"]["00001"], "before 1");
    assert_eq!(matched["match_text"]["10001"], "unique needle");
    assert_eq!(matched["after"]["20001"], "after 10000");

    let rejected = toolbox.execute(
        "Search",
        json!({"path":"large-context.txt", "query":"needle", "context_before":10_001}),
    );
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["error"]["code"], "invalid_arguments");
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("0..=10000")
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn find_and_search_have_optional_bounded_recursion_depth() {
    let workspace = temporary_workspace();
    fs::create_dir_all(workspace.join("root/one/two")).unwrap();
    fs::write(workspace.join("root/direct.txt"), "needle direct\n").unwrap();
    fs::write(workspace.join("root/one/nested.txt"), "needle nested\n").unwrap();
    fs::write(workspace.join("root/one/two/deep.txt"), "needle deep\n").unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let unlimited_find = toolbox.execute("Find", json!({"path":"root", "patterns":["*.txt"]}));
    assert_eq!(
        unlimited_find["output"]["results"],
        json!([
            "root/direct.txt",
            "root/one/nested.txt",
            "root/one/two/deep.txt"
        ])
    );
    let direct_find = toolbox.execute(
        "Find",
        json!({"path":"root", "patterns":["*.txt"], "depth":1}),
    );
    assert_eq!(direct_find["output"]["results"], json!(["root/direct.txt"]));
    let two_level_find = toolbox.execute(
        "Find",
        json!({"path":"root", "patterns":["*.txt"], "depth":2}),
    );
    assert_eq!(
        two_level_find["output"]["results"],
        json!(["root/direct.txt", "root/one/nested.txt"])
    );
    let maximum_depth_find = toolbox.execute(
        "Find",
        json!({"path":"root", "patterns":["*.txt"], "depth":32}),
    );
    assert_eq!(
        maximum_depth_find["output"]["results"],
        unlimited_find["output"]["results"]
    );
    let file_find = toolbox.execute(
        "Find",
        json!({"path":"root/one/two/deep.txt", "patterns":["*.txt"], "depth":1}),
    );
    assert_eq!(
        file_find["output"]["results"],
        json!(["root/one/two/deep.txt"])
    );

    let unlimited_search = toolbox.execute("Search", json!({"path":"root", "query":"needle"}));
    assert_eq!(
        unlimited_search["output"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let direct_search = toolbox.execute(
        "Search",
        json!({"path":"root", "query":"needle", "depth":1}),
    );
    assert_eq!(
        direct_search["output"]["matches"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        direct_search["output"]["matches"][0]["path"],
        "root/direct.txt"
    );
    let two_level_search = toolbox.execute(
        "Search",
        json!({"path":"root", "query":"needle", "depth":2}),
    );
    assert_eq!(
        two_level_search["output"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        two_level_search["output"]["matches"][1]["path"],
        "root/one/nested.txt"
    );
    let maximum_depth_search = toolbox.execute(
        "Search",
        json!({"path":"root", "query":"needle", "depth":32}),
    );
    assert_eq!(
        maximum_depth_search["output"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let file_search = toolbox.execute(
        "Search",
        json!({"path":"root/one/two/deep.txt", "query":"needle", "depth":1}),
    );
    assert_eq!(
        file_search["output"]["matches"].as_array().unwrap().len(),
        1
    );

    for (tool, input) in [
        (
            "Find",
            json!({"path":"root", "patterns":["*.txt"], "depth":0}),
        ),
        (
            "Search",
            json!({"path":"root", "query":"needle", "depth":33}),
        ),
    ] {
        let invalid = toolbox.execute(tool, input);
        assert_eq!(invalid["type"], "error");
        assert_eq!(invalid["error"]["code"], "invalid_arguments");
    }

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn large_source_files_are_not_rejected_by_an_artificial_size_limit() {
    let workspace = temporary_workspace();
    let path = workspace.join("large.txt");
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(b"head\n").unwrap();
    let full_mebibyte_line = vec![b'x'; 1024 * 1024 - 1];
    for _ in 0..65 {
        file.write_all(&full_mebibyte_line).unwrap();
        file.write_all(b"\n").unwrap();
    }
    let tail = b"unique-large-file-needle\n";
    let tail_offset = file.stream_position().unwrap();
    file.write_all(tail).unwrap();
    file.flush().unwrap();
    drop(file);
    assert!(fs::metadata(&path).unwrap().len() > 64 * 1024 * 1024);

    let script = generated_file_toolbox(&workspace);
    let script_text = fs::read_to_string(&script).unwrap();
    assert!(!script_text.contains("MAX_TEXT_BYTES"));
    assert!(!script_text.contains("MAX_SEARCH_FILE_BYTES"));
    assert!(!script_text.contains("content_too_large"));
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let read = toolbox.execute(
        "Read",
        json!({"path":"large.txt", "start_line":67, "end_line":67}),
    );
    assert_eq!(read["type"], "result", "unexpected result: {read}");
    assert_eq!(
        read["output"]["lines"],
        json!({"67":"unique-large-file-needle"})
    );
    assert_eq!(read["output"]["total_lines"], 67);

    let search = toolbox.execute(
        "Search",
        json!({"path":"large.txt", "query":"unique-large-file-needle"}),
    );
    assert_eq!(search["type"], "result", "unexpected result: {search}");
    assert_eq!(search["output"]["skipped_binary"], 0);
    assert_eq!(search["output"]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        search["output"]["matches"][0]["match_text"],
        json!({"67":"unique-large-file-needle"})
    );

    let bytes = toolbox.execute(
        "ReadBytes",
        json!({"path":"large.txt", "offset":tail_offset, "length":tail.len()}),
    );
    assert_eq!(bytes["type"], "result", "unexpected result: {bytes}");
    let edited = toolbox.execute(
        "EditBytes",
        json!({
            "path":"large.txt",
            "expected_hash":bytes["output"]["hash"],
            "edits":[{"target_offset":tail_offset,"target_length":1,"data":"55"}]
        }),
    );
    assert_eq!(edited["type"], "result", "unexpected result: {edited}");
    let mut changed = fs::File::open(&path).unwrap();
    changed.seek(SeekFrom::Start(tail_offset)).unwrap();
    let mut first = [0_u8; 1];
    changed.read_exact(&mut first).unwrap();
    assert_eq!(first, [b'U']);

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_bytes_uses_one_original_snapshot_and_returns_no_chainable_hash() {
    let workspace = temporary_workspace();
    fs::write(
        workspace.join("data.bin"),
        [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
    )
    .unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let baseline = toolbox.execute("ReadBytes", json!({"path":"data.bin"}));
    let hash = baseline["output"]["hash"].as_str().unwrap().to_owned();
    let edited = toolbox.execute(
        "EditBytes",
        json!({
            "path":"data.bin",
            "expected_hash":hash,
            "edits":[
                {"target_offset":4,"target_length":1,"data":"  AA   bb  "},
                {"target_offset":1,"target_length":2,"data":"   "},
                {"target_offset":3,"target_length":0,"data":"CC"}
            ]
        }),
    );
    assert_eq!(edited["type"], "result");
    assert_eq!(
        fs::read(workspace.join("data.bin")).unwrap(),
        [0x00, 0xcc, 0x33, 0xaa, 0xbb, 0x55]
    );
    assert_eq!(edited["output"]["operation"], "bytes_edited");
    assert_eq!(edited["output"]["previous_size"], 6);
    assert_eq!(edited["output"]["size"], 6);
    assert!(edited["output"].get("hash").is_none());
    assert_eq!(edited["output"]["edit_results"][0]["kind"], "replace");
    assert_eq!(edited["output"]["edit_results"][0]["replacement_bytes"], 2);
    assert_eq!(edited["output"]["edit_results"][1]["kind"], "delete");
    assert_eq!(edited["output"]["edit_results"][2]["kind"], "insert");
    assert!(
        edited["output"]["tip"]
            .as_str()
            .unwrap()
            .contains("MUST use File.ReadBytes")
    );

    let stale = toolbox.execute(
        "EditBytes",
        json!({
            "path":"data.bin",
            "expected_hash":hash,
            "edits":[{"target_offset":6,"target_length":0,"data":"ff"}]
        }),
    );
    assert_eq!(stale["error"]["code"], "conflict");
    assert_eq!(
        fs::read(workspace.join("data.bin")).unwrap(),
        [0x00, 0xcc, 0x33, 0xaa, 0xbb, 0x55]
    );

    let refreshed = toolbox.execute("ReadBytes", json!({"path":"data.bin"}));
    assert_eq!(refreshed["output"]["data"], "00 cc 33 aa bb 55");
    let appended = toolbox.execute(
        "EditBytes",
        json!({
            "path":"data.bin",
            "expected_hash":refreshed["output"]["hash"],
            "edits":[{"target_offset":6,"target_length":0,"data":"ff"}]
        }),
    );
    assert_eq!(appended["type"], "result");
    assert_eq!(
        fs::read(workspace.join("data.bin")).unwrap(),
        [0x00, 0xcc, 0x33, 0xaa, 0xbb, 0x55, 0xff]
    );

    fs::write(workspace.join("boundary.bin"), [0x00, 0x11, 0x22, 0x33]).unwrap();
    let boundary_read = toolbox.execute("ReadBytes", json!({"path":"boundary.bin"}));
    let boundary = toolbox.execute(
        "EditBytes",
        json!({
            "path":"boundary.bin",
            "expected_hash":boundary_read["output"]["hash"],
            "edits":[
                {"target_offset":2,"target_length":1,"data":"bb"},
                {"target_offset":2,"target_length":0,"data":"aa"}
            ]
        }),
    );
    assert_eq!(boundary["type"], "result");
    assert_eq!(
        fs::read(workspace.join("boundary.bin")).unwrap(),
        [0x00, 0x11, 0xaa, 0xbb, 0x33]
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn edit_bytes_rejects_invalid_batches_without_mutating_the_file() {
    let workspace = temporary_workspace();
    let original = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
    fs::write(workspace.join("data.bin"), original).unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);
    let baseline = toolbox.execute("ReadBytes", json!({"path":"data.bin"}));
    let hash = baseline["output"]["hash"].as_str().unwrap().to_owned();

    let invalid = [
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[
                {"target_offset":1,"target_length":3,"data":"aa"},
                {"target_offset":2,"target_length":2,"data":"bb"}
            ]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[
                {"target_offset":2,"target_length":0,"data":"aa"},
                {"target_offset":2,"target_length":0,"data":"bb"}
            ]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[
                {"target_offset":1,"target_length":3,"data":"aa"},
                {"target_offset":2,"target_length":0,"data":"bb"}
            ]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[{"target_offset":5,"target_length":2,"data":"aa"}]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[{"target_offset":2,"target_length":0,"data":""}]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[{"target_offset":2,"target_length":1,"data":"0g"}]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[{"target_offset":2,"target_length":1,"data":"a"}]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[{"target_offset":2,"target_length":1,"data":"aabb"}]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[{"target_offset":2,"target_length":1}]
        }),
        json!({
            "path":"data.bin","expected_hash":hash,
            "edits":[{"target_offset":2,"target_length":1,"data":"aa","unexpected":true}]
        }),
    ];
    for input in invalid {
        let rejected = toolbox.execute("EditBytes", input);
        assert_eq!(rejected["type"], "error", "unexpected result: {rejected}");
        assert_eq!(fs::read(workspace.join("data.bin")).unwrap(), original);
    }

    let empty = workspace.join("empty.bin");
    fs::write(&empty, []).unwrap();
    let empty_read = toolbox.execute("ReadBytes", json!({"path":"empty.bin"}));
    let inserted = toolbox.execute(
        "EditBytes",
        json!({
            "path":"empty.bin",
            "expected_hash":empty_read["output"]["hash"],
            "edits":[{"target_offset":0,"target_length":0,"data":"00 ff"}]
        }),
    );
    assert_eq!(inserted["type"], "result");
    assert_eq!(fs::read(&empty).unwrap(), [0x00, 0xff]);
    let inserted_read = toolbox.execute("ReadBytes", json!({"path":"empty.bin"}));
    let deleted = toolbox.execute(
        "EditBytes",
        json!({
            "path":"empty.bin",
            "expected_hash":inserted_read["output"]["hash"],
            "edits":[{"target_offset":0,"target_length":2,"data":""}]
        }),
    );
    assert_eq!(deleted["type"], "result");
    assert!(fs::read(&empty).unwrap().is_empty());

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn file_toolbox_allows_external_reads_but_rejects_overwrite_unknown_fields_and_mutable_symlinks() {
    let workspace = temporary_workspace();
    let outside = workspace.parent().unwrap().join(format!(
        "me-file-outside-{}",
        workspace.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&outside, "outside").unwrap();
    fs::create_dir_all(workspace.join("src")).unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let escaped = toolbox.execute(
        "Read",
        json!({"path":format!("../{}", outside.file_name().unwrap().to_string_lossy())}),
    );
    assert_eq!(escaped["type"], "result");
    assert_eq!(escaped["output"]["lines"], json!({"1":"outside"}));
    assert_eq!(
        escaped["output"]["path"],
        outside
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    );

    let created = toolbox.execute("Create", json!({"path":"safe.txt", "content":"safe"}));
    let duplicate = toolbox.execute("Create", json!({"path":"safe.txt", "content":"overwrite"}));
    assert_eq!(duplicate["error"]["code"], "already_exists");
    assert_eq!(
        fs::read_to_string(workspace.join("safe.txt")).unwrap(),
        "safe"
    );

    let unknown = toolbox.execute(
        "Append",
        json!({
            "path":"safe.txt",
            "expected_hash":created["output"]["hash"],
            "content":"x",
            "surprise":true
        }),
    );
    assert_eq!(unknown["error"]["code"], "invalid_arguments");
    assert_eq!(
        fs::read_to_string(workspace.join("safe.txt")).unwrap(),
        "safe"
    );

    let lock = toolbox.execute("Stat", json!({"paths":[".me/file-toolbox.lock"]}));
    let protected = toolbox.execute(
        "Delete",
        json!({
            "path":".me/file-toolbox.lock",
            "expected_hash":lock["output"]["entries"][0]["hash"]
        }),
    );
    assert_eq!(protected["error"]["code"], "protected_path");

    let directory_delete = toolbox.execute(
        "Delete",
        json!({"path":"src", "expected_hash":created["output"]["hash"]}),
    );
    assert_eq!(directory_delete["type"], "error");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(workspace.join("safe.txt"), workspace.join("link.txt")).unwrap();
        let symlink_delete = toolbox.execute(
            "Delete",
            json!({"path":"link.txt", "expected_hash":created["output"]["hash"]}),
        );
        assert_eq!(symlink_delete["error"]["code"], "unsupported_file_type");
        assert!(workspace.join("safe.txt").is_file());
    }

    toolbox.finish();
    fs::remove_file(outside).unwrap();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn every_file_tool_supports_normalized_external_paths_without_weakening_other_guards() {
    let workspace = temporary_workspace();
    let outside = workspace.parent().unwrap().join(format!(
        "me-file-all-tools-outside-{}",
        workspace.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(&outside).unwrap();
    let display = |path: &Path| {
        let normalized = if path.exists() {
            path.canonicalize().unwrap()
        } else {
            path.parent()
                .unwrap()
                .canonicalize()
                .unwrap()
                .join(path.file_name().unwrap())
        };
        normalized.to_string_lossy().replace('\\', "/")
    };
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let nested = outside.join("nested");
    let made = toolbox.execute(
        "MakeDirectory",
        json!({"path":display(&nested), "parents":false}),
    );
    assert_eq!(made["type"], "result");
    assert_eq!(made["output"]["path"], display(&nested));

    let text_path = nested.join("text.txt");
    let created = toolbox.execute(
        "Create",
        json!({"path":display(&text_path), "content":"one\ntwo\n"}),
    );
    assert_eq!(created["type"], "result");
    assert_eq!(created["output"]["path"], display(&text_path));

    let read = toolbox.execute(
        "Read",
        json!({"path":display(&text_path), "start_line":1, "end_line":2}),
    );
    assert_eq!(read["output"]["lines"], json!({"1":"one", "2":"two"}));
    assert_eq!(
        read["output"]["editable_ranges"],
        json!([{"start_line":1,"end_line":2}])
    );
    let edited = toolbox.execute(
        "Edit",
        json!({
            "path":display(&text_path),
            "edits":[{"operation":"replace","start_line":2,"end_line":2,"new_lines":["changed"]}]
        }),
    );
    assert_eq!(edited["type"], "result");
    assert_eq!(fs::read_to_string(&text_path).unwrap(), "one\nchanged\n");

    let reread = toolbox.execute("Read", json!({"path":display(&text_path)}));
    let appended = toolbox.execute(
        "Append",
        json!({
            "path":display(&text_path),
            "expected_hash":reread["output"]["hash"],
            "content":"three\n"
        }),
    );
    assert_eq!(appended["type"], "result");
    let replaced = toolbox.execute(
        "Replace",
        json!({
            "path":display(&text_path),
            "expected_hash":appended["output"]["hash"],
            "content":"needle\nfinal\n"
        }),
    );
    assert_eq!(replaced["type"], "result");

    let listed = toolbox.execute("List", json!({"path":display(&outside), "depth":2}));
    assert_eq!(listed["type"], "result");
    assert!(
        listed["output"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == display(&text_path))
    );
    let found = toolbox.execute(
        "Find",
        json!({"path":display(&outside), "patterns":["*.txt"]}),
    );
    assert_eq!(found["output"]["results"], json!([display(&text_path)]));
    let searched = toolbox.execute(
        "Search",
        json!({"path":display(&outside), "query":"needle"}),
    );
    assert_eq!(
        searched["output"]["matches"][0]["path"],
        display(&text_path)
    );
    let stated = toolbox.execute(
        "Stat",
        json!({"paths":[display(&text_path), display(&outside.join("missing.txt"))]}),
    );
    assert_eq!(stated["output"]["entries"][0]["exists"], true);
    assert_eq!(stated["output"]["entries"][1]["exists"], false);
    assert_eq!(
        stated["output"]["entries"][1]["path"],
        display(&outside.join("missing.txt"))
    );

    let binary_path = nested.join("data.bin");
    fs::write(&binary_path, [0x01, 0x02, 0x03]).unwrap();
    let bytes = toolbox.execute("ReadBytes", json!({"path":display(&binary_path)}));
    assert_eq!(bytes["output"]["data"], "01 02 03");
    let edited_bytes = toolbox.execute(
        "EditBytes",
        json!({
            "path":display(&binary_path),
            "expected_hash":bytes["output"]["hash"],
            "edits":[{"target_offset":1,"target_length":1,"data":"ff"}]
        }),
    );
    assert_eq!(edited_bytes["type"], "result");
    assert_eq!(fs::read(&binary_path).unwrap(), [0x01, 0xff, 0x03]);

    let copied_path = nested.join("copied.txt");
    let copied = toolbox.execute(
        "Copy",
        json!({
            "path":display(&text_path),
            "destination":display(&copied_path),
            "expected_hash":replaced["output"]["hash"]
        }),
    );
    assert_eq!(copied["type"], "result");
    assert_eq!(copied["output"]["destination"], display(&copied_path));
    assert_eq!(fs::read_to_string(&copied_path).unwrap(), "needle\nfinal\n");

    let moved_path = nested.join("moved.txt");
    let moved = toolbox.execute(
        "Move",
        json!({
            "path":display(&text_path),
            "destination":display(&moved_path),
            "expected_hash":replaced["output"]["hash"]
        }),
    );
    assert_eq!(moved["type"], "result");
    assert_eq!(moved["output"]["destination"], display(&moved_path));
    let deleted = toolbox.execute(
        "Delete",
        json!({"path":display(&moved_path), "expected_hash":moved["output"]["hash"]}),
    );
    assert_eq!(deleted["type"], "result");
    assert!(!moved_path.exists());
    let copied_deleted = toolbox.execute(
        "Delete",
        json!({"path":display(&copied_path), "expected_hash":copied["output"]["hash"]}),
    );
    assert_eq!(copied_deleted["type"], "result");

    toolbox.finish();
    fs::remove_dir_all(outside).unwrap();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn detects_common_text_encodings_and_reports_bom_and_confidence() {
    let workspace = temporary_workspace();
    fs::write(
        workspace.join("simplified.txt"),
        b"\xbc\xf2\xcc\xe5\xd6\xd0\xce\xc4\xc4\xda\xc8\xdd\xa3\xac\xc4\xe3\xba\xc3\xca\xc0\xbd\xe7\xa1\xa3\r\n\xb5\xda\xb6\xfe\xd0\xd0",
    )
    .unwrap();
    fs::write(
        workspace.join("traditional.txt"),
        b"\xc1c\xc5\xe9\xa4\xa4\xa4\xe5\xa4\xba\xaee\xa1A\xa7A\xa6n\xa5@\xac\xc9\xa1C\r\n\xb2\xc4\xa4G\xa6\xe6",
    )
    .unwrap();
    fs::write(
        workspace.join("japanese.txt"),
        b"\x93\xfa\x96{\x8c\xea\x82\xcc\x95\xb6\x8f\xcd\x82\xc5\x82\xb7\x81B\x82\xb1\x82\xf1\x82\xc9\x82\xbf\x82\xcd\x90\xa2\x8aE\x81B\n\x93\xf1\x8ds\x96\xda",
    )
    .unwrap();
    fs::write(
        workspace.join("korean.txt"),
        b"\xc7\xd1\xb1\xb9\xbe\xee \xb9\xae\xc0\xe5\xc0\xd4\xb4\xcf\xb4\xd9. \xbe\xc8\xb3\xe7\xc7\xcf\xbc\xbc\xbf\xe4 \xbc\xbc\xb0\xe8.\n\xb5\xce \xb9\xf8\xc2\xb0 \xc1\xd9",
    )
    .unwrap();
    fs::write(
        workspace.join("western.txt"),
        b"Caf\xe9 d\xe9j\xe0 vu \x96 r\xe9sum\xe9 and na\xefve.",
    )
    .unwrap();
    fs::write(workspace.join("utf16.txt"), utf16_le("alpha\r\n中文", true)).unwrap();
    fs::write(
        workspace.join("utf16-no-bom.txt"),
        utf16_le("plain ASCII\r\nsecond", false),
    )
    .unwrap();
    fs::write(workspace.join("utf32.txt"), utf32_be("A中\n", true)).unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    for (path, encoding, content, bom) in [
        (
            "simplified.txt",
            "gb18030",
            "简体中文内容，你好世界。\r\n第二行",
            false,
        ),
        (
            "traditional.txt",
            "big5",
            "繁體中文內容，你好世界。\r\n第二行",
            false,
        ),
        (
            "japanese.txt",
            "shift_jis",
            "日本語の文章です。こんにちは世界。\n二行目",
            false,
        ),
        (
            "korean.txt",
            "euc_kr",
            "한국어 문장입니다. 안녕하세요 세계.\n두 번째 줄",
            false,
        ),
        (
            "western.txt",
            "windows-1252",
            "Café déjà vu – résumé and naïve.",
            false,
        ),
        ("utf16.txt", "utf-16-le", "alpha\r\n中文", true),
        (
            "utf16-no-bom.txt",
            "utf-16-le",
            "plain ASCII\r\nsecond",
            false,
        ),
        ("utf32.txt", "utf-32-be", "A中\n", true),
    ] {
        let read = toolbox.execute("Read", json!({"path":path}));
        assert_eq!(read["type"], "result", "failed to read {path}: {read}");
        assert_eq!(read["output"]["encoding"], encoding);
        assert_eq!(read["output"]["lines"], numbered_lines(content, 1));
        assert_eq!(read["output"]["bom"], bom);
        assert!(read["output"]["encoding_confidence"].as_f64().unwrap() >= 0.78);
    }

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn text_mutations_preserve_detected_encoding_bom_and_original_line_endings() {
    let workspace = temporary_workspace();
    let legacy_initial = b"\xd6\xd0\xce\xc4\r\n";
    fs::write(workspace.join("legacy.txt"), legacy_initial).unwrap();
    fs::write(
        workspace.join("unicode.txt"),
        utf16_le("alpha\r\n中文", true),
    )
    .unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let legacy = toolbox.execute("Read", json!({"path":"legacy.txt"}));
    assert_eq!(legacy["output"]["encoding"], "gb18030");
    let edited = toolbox.execute(
        "Edit",
        single_edit_input!(
            "legacy.txt",
            legacy["output"]["hash"],
            1,
            1,
            json!(["内容\r\n"])
        ),
    );
    assert_eq!(edited["output"]["encoding"], "gb18030");
    assert_eq!(edited["output"]["bom"], false);
    assert_eq!(
        fs::read(workspace.join("legacy.txt")).unwrap(),
        b"\xc4\xda\xc8\xdd\r\n"
    );
    let legacy_refreshed = toolbox.execute("Read", json!({"path":"legacy.txt"}));
    let appended = toolbox.execute(
        "Append",
        json!({
            "path":"legacy.txt",
            "expected_hash":legacy_refreshed["output"]["hash"],
            "content":"你好"
        }),
    );
    assert_eq!(appended["type"], "result", "append failed: {appended}");
    assert_eq!(appended["output"]["appended_bytes"], 4);
    assert_eq!(
        fs::read(workspace.join("legacy.txt")).unwrap(),
        b"\xc4\xda\xc8\xdd\r\n\xc4\xe3\xba\xc3"
    );
    let replaced = toolbox.execute(
        "Replace",
        json!({
            "path":"legacy.txt",
            "expected_hash":appended["output"]["hash"],
            "content":"简体中文\r\n"
        }),
    );
    assert_eq!(replaced["output"]["encoding"], "gb18030");
    assert_eq!(
        fs::read(workspace.join("legacy.txt")).unwrap(),
        b"\xbc\xf2\xcc\xe5\xd6\xd0\xce\xc4\r\n"
    );
    let search = toolbox.execute("Search", json!({"path":"legacy.txt", "query":"中文"}));
    assert_eq!(search["output"]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(search["output"]["matches"][0]["column"], 3);
    assert_eq!(search["output"]["matches"][0]["match_length"], 2);
    assert_eq!(search["output"]["skipped_binary"], 0);

    let unicode = toolbox.execute("Read", json!({"path":"unicode.txt"}));
    let unicode_edited = toolbox.execute(
        "Edit",
        single_edit_input!(
            "unicode.txt",
            unicode["output"]["hash"],
            1,
            1,
            json!(["beta\r\n"])
        ),
    );
    assert!(unicode_edited["output"].get("hash").is_none());
    let unicode_refreshed = toolbox.execute("Read", json!({"path":"unicode.txt"}));
    let unicode_appended = toolbox.execute(
        "Append",
        json!({
            "path":"unicode.txt",
            "expected_hash":unicode_refreshed["output"]["hash"],
            "content":"你好"
        }),
    );
    assert_eq!(unicode_appended["output"]["encoding"], "utf-16-le");
    assert_eq!(unicode_appended["output"]["bom"], true);
    assert_eq!(
        fs::read(workspace.join("unicode.txt")).unwrap(),
        utf16_le("beta\r\n中文你好", true)
    );

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn explicit_encoding_handles_ambiguity_and_unrepresentable_text_never_mutates() {
    let workspace = temporary_workspace();
    fs::write(workspace.join("ambiguous.txt"), b"\x81\x40").unwrap();
    let script = generated_file_toolbox(&workspace);
    let mut toolbox = ToolboxProcess::start(&workspace, &script);

    let uncertain = toolbox.execute("Read", json!({"path":"ambiguous.txt"}));
    assert_eq!(uncertain["type"], "error");
    assert_eq!(uncertain["error"]["code"], "encoding_uncertain");
    let explicit = toolbox.execute(
        "Read",
        json!({"path":"ambiguous.txt", "encoding":"gb18030"}),
    );
    assert_eq!(explicit["output"]["lines"], numbered_lines("丂", 1));
    assert_eq!(explicit["output"]["encoding_confidence"], 1.0);
    let uncertain_write = toolbox.execute(
        "Append",
        json!({
            "path":"ambiguous.txt",
            "expected_hash":explicit["output"]["hash"],
            "content":"text"
        }),
    );
    assert_eq!(uncertain_write["error"]["code"], "encoding_uncertain");
    assert_eq!(
        fs::read(workspace.join("ambiguous.txt")).unwrap(),
        b"\x81\x40"
    );

    let created = toolbox.execute(
        "Create",
        json!({
            "path":"western.txt",
            "content":"Café – résumé",
            "encoding":"windows-1252"
        }),
    );
    assert_eq!(created["type"], "result");
    let before = fs::read(workspace.join("western.txt")).unwrap();
    let rejected_replace = toolbox.execute(
        "Edit",
        single_edit_input!(
            "western.txt",
            created["output"]["hash"],
            1,
            1,
            json!(["Café – 中文\n"])
        ),
    );
    assert_eq!(rejected_replace["type"], "error");
    assert_eq!(rejected_replace["error"]["code"], "encoding_error");
    assert_eq!(fs::read(workspace.join("western.txt")).unwrap(), before);

    let rejected = toolbox.execute(
        "Append",
        json!({
            "path":"western.txt",
            "expected_hash":created["output"]["hash"],
            "content":"中文"
        }),
    );
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["error"]["code"], "encoding_error");
    assert_eq!(fs::read(workspace.join("western.txt")).unwrap(), before);

    let invalid_bom = toolbox.execute(
        "Create",
        json!({
            "path":"legacy-bom.txt",
            "content":"text",
            "encoding":"gb18030",
            "bom":true
        }),
    );
    assert_eq!(invalid_bom["error"]["code"], "invalid_encoding");
    assert!(!workspace.join("legacy-bom.txt").exists());

    let unicode = toolbox.execute(
        "Create",
        json!({
            "path":"created-utf16.txt",
            "content":"中文\r\n",
            "encoding":"utf-16-le",
            "bom":true
        }),
    );
    assert_eq!(unicode["type"], "result");
    assert_eq!(unicode["output"]["bom"], true);
    assert_eq!(
        fs::read(workspace.join("created-utf16.txt")).unwrap(),
        utf16_le("中文\r\n", true)
    );
    let mismatched = toolbox.execute(
        "Read",
        json!({"path":"created-utf16.txt", "encoding":"utf-8"}),
    );
    assert_eq!(mismatched["error"]["code"], "encoding_mismatch");

    let auto_create = toolbox.execute(
        "Create",
        json!({"path":"auto.txt", "content":"text", "encoding":"auto"}),
    );
    assert_eq!(auto_create["error"]["code"], "invalid_encoding");
    assert!(!workspace.join("auto.txt").exists());

    toolbox.finish();
    fs::remove_dir_all(workspace).unwrap();
}
