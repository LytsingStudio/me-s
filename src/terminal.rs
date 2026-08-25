use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::env;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

use crate::{
    Result,
    event::{EventId, TerminalSessionState},
};

pub const CREATE: &str = "Terminal.Create";
pub const INTERACT: &str = "Terminal.Interact";
pub const STATUS: &str = "Terminal.Status";
pub const LIST: &str = "Terminal.List";
pub const KILL: &str = "Terminal.Kill";

pub const API_CREATE: &str = "Terminal_Create";
pub const API_INTERACT: &str = "Terminal_Interact";
pub const API_STATUS: &str = "Terminal_Status";
pub const API_LIST: &str = "Terminal_List";
pub const API_KILL: &str = "Terminal_Kill";
pub const TOOL_VERSION: &str = "8";

const DEFAULT_WIDTH: u16 = 120;
const DEFAULT_HEIGHT: u16 = 40;
const DEFAULT_WAIT_MS: u32 = 1_000;
const DEFAULT_MAX_WAIT_MS: u32 = 10_000;
const DEFAULT_MAX_OUTPUT_CHARS: u32 = 20_000;
const MAX_DIMENSION: u16 = 500;
const MAX_WAIT_MS: u32 = 60_000;
const MAX_OUTPUT_CHARS: u32 = 200_000;
const MAX_INPUT_ACTIONS: usize = 256;
const MAX_KEY_REPEAT: u16 = 1_000;
const READER_CHUNK: usize = 4_096;
const PTY_SCROLLBACK_ROWS: usize = 100_000;
const EXIT_DRAIN_IDLE_MS: u64 = 50;
const _: () = assert!(PTY_SCROLLBACK_ROWS >= 100_000);

#[cfg(unix)]
const UNIX_BASH_BACKEND: &str = "/bin/bash";
#[cfg(windows)]
const WINDOWS_INITIAL_CURSOR_RESPONSE: &[u8] = b"\x1b[1;1R";
pub const UNAVAILABLE_BACKEND: &str = "unavailable (PowerShell not found)";

pub fn shell_backend() -> String {
    detect_terminal_backend()
        .map(|backend| backend.program)
        .unwrap_or_else(|| UNAVAILABLE_BACKEND.to_owned())
}

pub fn tool_prompt(shell: &str) -> String {
    if shell == UNAVAILABLE_BACKEND {
        return "Terminal is unavailable on this Windows system because neither powershell.exe nor pwsh.exe could be found. No Terminal functions are available. Do not attempt Terminal tool calls.".to_owned();
    }
    let shell_rule = shell_rule(shell);
    format!(
        r##"Terminal provides real, stateful PTY shell sessions.

Current PTY shell backend: `{shell}`. {shell_rule}
ME-S's bundled Python 3.12 is available as `python` in every Terminal session. Do not probe for or install Python before using it.

Available actions:
- Terminal.Create(width=120, height=40, cwd=".", wait_ms=1000, max_wait_ms=10000, max_output_chars=20000): create a persistent PTY shell, wait for its initial output to become idle, and return a session_id plus its initial structured terminal patch.
- Terminal.Interact(session_id, input=[], wait_ms=1000, max_wait_ms=10000, max_output_chars=20000): apply an ordered list of semantic text/key actions, or poll with an empty list.
- Terminal.Status(session_id): inspect a session known to the current Terminal toolbox process.
- Terminal.List(): list all sessions recorded by the current Terminal toolbox process.
- Terminal.Kill(session_id, grace_ms=1000): terminate a live session.

Important behavior:
- A session preserves shell state, including cwd, environment variables, jobs, and interactive program state, until it exits or is killed.
- Use the same session_id for follow-up interaction. Create a new session only when isolation is useful or the previous one ended.
- Create waits for the initial shell output with the same idle/maximum wait rules as Interact. That initial output is returned as tool output, so inspect it before the next action.
- Every operation that reads a PTY session uses the same output state and idle/maximum wait rules. PTY stdout and stderr are one terminal stream.
- `wait_ms` is the required quiet interval after the latest received PTY output; any newer output restarts it. `max_wait_ms` is the hard upper bound for that tool call.
- For a non-empty `input`, the quiet interval never starts before all input bytes have been successfully written. Old unread output cannot make newly submitted input return immediately. For an empty polling input, already-pending output that has been quiet long enough may return immediately.
- An idle return only says the PTY stopped producing output for `wait_ms`; it does not prove a foreground command finished. Confirm a shell prompt or the interactive program's known state. If output is inconclusive, poll with `input:[]`; do not submit a second shell command into a possibly running foreground process.
- The PTY size is fixed for the whole session. Terminal resize is intentionally unavailable.
- Terminal rows use permanent absolute numbers that increase as normal output scrolls, so the current viewport may begin above row 0.
- Every tool result is one JSON object. Terminal reads appear in its `terminal_updates` array as structured line-level `terminal_patch` version 2 objects; the tool call outcome appears separately in `result`. Terminal patches are objects, never JSON encoded again inside a string field, and never cell matrices.
- `viewport.first_terminal_row` and `viewport.last_terminal_row` identify the current visible absolute terminal row range. `rows` contains only rows whose final rendered cells differ from the previous baseline. Every included object completely replaces its `terminal_row`; `text:""` explicitly clears that terminal row.
- Changes are compared only after the PTY becomes idle or reaches the maximum wait. Intermediate redraws are never reported separately, and a row that returns to its baseline is omitted.
- `terminal_row` is patch metadata, not terminal text, a source-file line number, a menu choice, a command argument, or any other application value. Only each row object's `text` is visible terminal content. Never reuse a `terminal_row` number as application input or infer application meaning from its numeric value.
- A row's `text` is the complete visible text with literal spaces preserved. `wrapped:true` marks terminal autowrap. Trailing default blanks are omitted; all remaining columns before the fixed terminal width are default blank cells. A row absent from `rows` is unchanged, not blank; `rows:[]` is a valid cursor-, viewport-, or state-only update.
- Style 0 is the implicit default and is omitted from `styles`. Every object in `styles` defines one non-default style ID used by this patch. Missing foreground/background means terminal default; missing attributes means none. Colors are `indexed(N)` or `#rrggbb`; attributes are `bold`, `dim`, `italic`, `underline`, and `inverse`.
- A row's optional `style_spans` describes only its non-default styled terminal-column ranges using `start_column`, `width`, and `style`. The complete visible content remains in `text`; no style markers are inserted into it and terminal text needs no custom escaping beyond ordinary JSON escaping. Treat inverse as a visual fact, not proof that text is selected.
- `cursor` reports the real cursor separately without replacing its `underlying` character. `wide_continuation:true` means the cursor is on the continuation cell of a two-column character.
- `sequence` increases once for every captured patch in one session. Use `session_id` plus `sequence` to order patches; sequence numbers from different sessions are unrelated.
- `truncated:true` means only the newest complete changed rows that fit the requested limit are included. Never infer omitted rows. The live baseline still advances, so omitted changes are not automatically returned by a later poll; choose a larger `max_output_chars` before a read when losing changed rows would be unacceptable. `result.state` describes completion of this tool call; do not confuse it with the PTY process `pty_state`.
- A patch reports only the final changed rows on the active terminal screen; transient primary/alternate-screen switching is not reported as a separate event.
- The comparison baseline belongs to the live session. Conversation rewind, clearing, or context replacement does not restore or reset it.
- `input` actions execute once in exact array order before Terminal waits for output. Never guess an implied Enter.
- A text action is exactly `{{"type":"text","text":"..."}}`. It writes ordinary UTF-8 text verbatim; newline and tab are allowed. Do not put Escape, Ctrl keys, JSON Unicode control escapes, Base64, or key names inside text.
- A key action is exactly `{{"type":"key","key":"...","modifiers":[],"repeat":1}}`. `modifiers` and `repeat` are optional. Supported modifiers are `ctrl`, `alt`, and `shift`; they describe one simultaneous chord. `repeat` repeats that complete key chord and is between 1 and 1000.
- Supported named keys are `enter`, `escape`, `tab`, `backspace`, `insert`, `delete`, `up`, `down`, `left`, `right`, `home`, `end`, `page_up`, `page_down`, `f1` through `f12`, and `space`. A single printable character such as `c` can be a key when modifiers are needed.
- For printable keys, `ctrl` supports ASCII letters, Space/@, `[`, `\`, `]`, `^`, `_`, and `?`; `shift` is accepted only with ASCII letters. To enter a visible symbol such as `!`, send that resulting character directly as text or as `key:"!"` without guessing a physical keyboard layout.
- Use multiple array elements for sequential actions. Sequential actions are not one simultaneous key chord or an escaped text string.
- The tool converts semantic keys to canonical terminal bytes and VT sequences. Ctrl+I is indistinguishable from Tab, Ctrl+M from Enter, and Ctrl+[ from Escape at the PTY boundary. Host GUI shortcuts such as Command+C or a terminal application's paste shortcut are not PTY input.
- A session exists only while its current Terminal toolbox process remains alive. If me, the toolbox, or its process restarts or fails, that process's PTYs are gone. Any unfinished tool call is reported as `interrupted`; no separate session-loss result is synthesized.
- A session_id from an earlier toolbox process does not exist in a restarted process. A later attempt to use it fails with `session_not_found`; do not keep retrying that ID. Call Terminal.Create and continue in a new session if needed.
- Replaying earlier conversation history does not recreate a PTY or repeat terminal side effects.
- Follow the governing external-path safety rule for every command: external reads must be materially relevant, and no content outside the workspace may be modified without the actual user's explicit authorization for the exact operation and target scope. Do not expose secrets or perform destructive actions without exact authorization.

The following examples progress from simple to complex and only explain how to interpret structured Terminal results. Neutral commands may appear solely to make returned text understandable; the examples do not prescribe tool input or a workflow for any task.

Example 1, the first observed terminal state:
```json
{{
  "terminal_updates": [{{
    "type": "terminal_patch",
    "version": 2,
    "session_id": "pty-10",
    "sequence": 1,
    "terminal_size": {{"columns": 120, "rows": 40}},
    "viewport": {{"first_terminal_row": 0, "last_terminal_row": 39}},
    "styles": [],
    "rows": [{{"terminal_row": 0, "text": "Prompt> "}}],
    "cursor": {{"terminal_row": 0, "column": 8, "visible": true, "width": 1, "wide_continuation": false, "underlying": ""}},
    "pty_state": "running",
    "exit_code": null,
    "truncated": false
  }}],
  "result": {{"state": "succeeded", "exit_code": null}}
}}
```
The other 39 viewport rows are unchanged/default; they were not returned.

Example 2, the returned patch after a simple `echo hello`:
```json
{{
  "terminal_updates": [{{
    "type": "terminal_patch",
    "version": 2,
    "session_id": "pty-10",
    "sequence": 2,
    "terminal_size": {{"columns": 120, "rows": 40}},
    "viewport": {{"first_terminal_row": 0, "last_terminal_row": 39}},
    "styles": [],
    "rows": [
      {{"terminal_row": 0, "text": "Prompt> echo hello"}},
      {{"terminal_row": 1, "text": "hello"}},
      {{"terminal_row": 2, "text": "Prompt> "}}
    ],
    "cursor": {{"terminal_row": 2, "column": 8, "visible": true, "width": 1, "wide_continuation": false, "underlying": ""}},
    "pty_state": "running",
    "exit_code": null,
    "truncated": false
  }}],
  "result": {{"state": "succeeded", "exit_code": null}}
}}
```
Sequence 2 follows sequence 1 for `pty-10`. Rows 0 through 2 are renderer coordinates, not content or command arguments. Each object completely replaces its terminal row; keep every other previously known row unchanged.

Example 3, one existing row changes and another is explicitly cleared:
```json
{{
  "terminal_updates": [{{
    "type": "terminal_patch",
    "version": 2,
    "session_id": "pty-10",
    "sequence": 3,
    "terminal_size": {{"columns": 120, "rows": 40}},
    "viewport": {{"first_terminal_row": 0, "last_terminal_row": 39}},
    "styles": [],
    "rows": [
      {{"terminal_row": 1, "text": "hello again"}},
      {{"terminal_row": 2, "text": ""}}
    ],
    "cursor": {{"terminal_row": 1, "column": 11, "visible": true, "width": 1, "wide_continuation": false, "underlying": ""}},
    "pty_state": "running",
    "exit_code": null,
    "truncated": false
  }}],
  "result": {{"state": "succeeded", "exit_code": null}}
}}
```
Terminal row 1 now contains the complete replacement `hello again`; do not append it to the previous `hello`. Terminal row 2 is now explicitly empty. Terminal row 0 and every unlisted row retain their previous content.

Example 4, cursor movement without a row redraw:
```json
{{
  "terminal_updates": [{{
    "type": "terminal_patch",
    "version": 2,
    "session_id": "pty-10",
    "sequence": 4,
    "terminal_size": {{"columns": 120, "rows": 40}},
    "viewport": {{"first_terminal_row": 0, "last_terminal_row": 39}},
    "styles": [],
    "rows": [],
    "cursor": {{"terminal_row": 1, "column": 6, "visible": true, "width": 1, "wide_continuation": false, "underlying": "a"}},
    "pty_state": "running",
    "exit_code": null,
    "truncated": false
  }}],
  "result": {{"state": "succeeded", "exit_code": null}}
}}
```
Do not treat `rows:[]` as missing output and do not ask for a full screen merely because only the cursor moved.

Example 5, a styled redraw:
```json
{{
  "terminal_updates": [{{
    "type": "terminal_patch",
    "version": 2,
    "session_id": "pty-10",
    "sequence": 5,
    "terminal_size": {{"columns": 120, "rows": 40}},
    "viewport": {{"first_terminal_row": 40, "last_terminal_row": 79}},
    "styles": [
      {{"id": 4, "foreground": "#00ff88", "attributes": ["bold"]}},
      {{"id": 9, "background": "indexed(4)", "attributes": ["inverse"]}}
    ],
    "rows": [
      {{"terminal_row": 47, "text": "Progress 80%", "style_spans": [{{"start_column": 9, "width": 3, "style": 4}}]}},
      {{"terminal_row": 48, "text": "Choice: Continue", "style_spans": [{{"start_column": 8, "width": 8, "style": 9}}]}}
    ],
    "cursor": {{"terminal_row": 48, "column": 8, "visible": true, "width": 1, "wide_continuation": false, "underlying": "C"}},
    "pty_state": "running",
    "exit_code": null,
    "truncated": false
  }}],
  "result": {{"state": "succeeded", "exit_code": null}}
}}
```
Only `80%` uses style 4 and only `Continue` uses style 9. Style spans use terminal columns, while `text` remains the complete readable line. Inverse is only a visual fact; combine it with the cursor and application state instead of assuming what an input action would do.

Example 6, autowrap, wide characters, and a scrolled viewport:
```json
{{
  "terminal_updates": [{{
    "type": "terminal_patch",
    "version": 2,
    "session_id": "pty-10",
    "sequence": 6,
    "terminal_size": {{"columns": 12, "rows": 3}},
    "viewport": {{"first_terminal_row": 80, "last_terminal_row": 82}},
    "styles": [],
    "rows": [
      {{"terminal_row": 80, "text": "1234567890你", "wrapped": true}},
      {{"terminal_row": 81, "text": "好"}}
    ],
    "cursor": {{"terminal_row": 81, "column": 2, "visible": true, "width": 1, "wide_continuation": false, "underlying": ""}},
    "pty_state": "running",
    "exit_code": null,
    "truncated": false
  }}],
  "result": {{"state": "succeeded", "exit_code": null}}
}}
```
The viewport now covers absolute rows 80 through 82. The wide character `你` occupies two terminal columns, so `wrapped:true` means the terminal continued onto row 81; do not concatenate rows merely because their absolute numbers are consecutive unless the wrap flag establishes that relationship.

Example 7, truncated changed rows and an exited PTY:
```json
{{
  "terminal_updates": [{{
    "type": "terminal_patch",
    "version": 2,
    "session_id": "pty-10",
    "sequence": 7,
    "terminal_size": {{"columns": 120, "rows": 40}},
    "viewport": {{"first_terminal_row": 160, "last_terminal_row": 199}},
    "styles": [],
    "rows": [
      {{"terminal_row": 198, "text": "last retained changed row"}},
      {{"terminal_row": 199, "text": ""}}
    ],
    "cursor": {{"terminal_row": 199, "column": 0, "visible": true, "width": 1, "wide_continuation": false, "underlying": ""}},
    "pty_state": "exited",
    "exit_code": 0,
    "truncated": true
  }}],
  "result": {{"state": "succeeded", "exit_code": null}}
}}
```
Only the newest complete changed rows that fit the output limit are present. Do not reconstruct omitted rows, and do not expect a later poll to resend them because the session baseline has advanced. `result.state:"succeeded"` means the Terminal tool call completed successfully; the patch's `pty_state:"exited"` and `exit_code:0` mean the PTY process itself ended normally. These are separate lifecycles, so the tool result's `exit_code` remains null here.

"##
    )
}

#[cfg(unix)]
fn shell_rule(_shell: &str) -> &'static str {
    "On Unix, Terminal.Create always starts interactive Bash with `--noprofile --norc -i`; it does not use the user's default shell. Commands must use Bash syntax."
}

#[cfg(windows)]
fn shell_rule(_shell: &str) -> &'static str {
    "On Windows, Terminal.Create always starts PowerShell with `-NoLogo -NoProfile`; it never starts cmd.exe or the user's default shell. Commands must use PowerShell syntax."
}

#[cfg(not(any(unix, windows)))]
fn shell_rule(_shell: &str) -> &'static str {
    "Commands must use the syntax of this exact shell backend."
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    #[cfg(unix)]
    Bash,
    #[cfg(windows)]
    PowerShell,
}

#[derive(Clone, Debug)]
struct TerminalBackend {
    program: String,
    kind: ShellKind,
}

fn detect_terminal_backend() -> Option<TerminalBackend> {
    #[cfg(unix)]
    {
        Some(TerminalBackend {
            program: UNIX_BASH_BACKEND.to_owned(),
            kind: ShellKind::Bash,
        })
    }
    #[cfg(windows)]
    {
        windows_powershell_backend().map(|program| TerminalBackend {
            program,
            kind: ShellKind::PowerShell,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(windows)]
fn windows_powershell_backend() -> Option<String> {
    let system_root = env::var_os("SystemRoot").or_else(|| env::var_os("WINDIR"));
    if let Some(system_root) = system_root {
        let powershell = PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if powershell.is_file() {
            return Some(powershell.to_string_lossy().into_owned());
        }
    }

    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        for executable in ["powershell.exe", "pwsh.exe"] {
            let candidate = directory.join(executable);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            API_CREATE,
            "Create a persistent PTY, wait for stable initial output, and return one structured line-level terminal patch.",
            json!({
                "type": "object",
                "properties": {
                    "width": {"type": "integer", "minimum": 1, "maximum": MAX_DIMENSION, "default": DEFAULT_WIDTH},
                    "height": {"type": "integer", "minimum": 1, "maximum": MAX_DIMENSION, "default": DEFAULT_HEIGHT},
                    "cwd": {"type": "string", "default": "."},
                    "wait_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": DEFAULT_WAIT_MS},
                    "max_wait_ms": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_MS, "default": DEFAULT_MAX_WAIT_MS},
                    "max_output_chars": {"type": "integer", "minimum": 1, "maximum": MAX_OUTPUT_CHARS, "default": DEFAULT_MAX_OUTPUT_CHARS}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            API_INTERACT,
            "Apply ordered semantic text/key actions to a persistent PTY, or poll with input=[], then return one structured line-level terminal patch.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "input": {
                        "type": "array",
                        "description": "Ordered actions. Use {type:\"text\",text:\"...\"} for ordinary UTF-8 text and {type:\"key\",key:\"...\",modifiers:[...],repeat:N} for terminal keys or simultaneous key chords. An empty array only polls.",
                        "default": [],
                        "maxItems": MAX_INPUT_ACTIONS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "description": "Action discriminator. text requires text; key requires key.",
                                    "enum": ["text", "key"]
                                },
                                "text": {
                                    "type": "string",
                                    "description": "Only for type=text. Ordinary UTF-8 text; newline and tab are allowed. Control keys must use type=key."
                                },
                                "key": {
                                    "type": "string",
                                    "description": "Only for type=key. One named key (enter, escape, tab, backspace, insert, delete, arrows, home, end, page_up, page_down, f1..f12, space) or one printable character. Send a visible symbol directly instead of guessing a keyboard-layout Shift chord."
                                },
                                "modifiers": {
                                    "type": "array",
                                    "description": "Only for type=key. Keys listed here are held simultaneously with key; separate actions are sequential.",
                                    "default": [],
                                    "uniqueItems": true,
                                    "items": {"type": "string", "enum": ["ctrl", "alt", "shift"]}
                                },
                                "repeat": {
                                    "type": "integer",
                                    "description": "Only for type=key. Repeat the complete key chord this many times.",
                                    "minimum": 1,
                                    "maximum": MAX_KEY_REPEAT,
                                    "default": 1
                                }
                            },
                            "required": ["type"],
                            "additionalProperties": false
                        }
                    },
                    "wait_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": DEFAULT_WAIT_MS},
                    "max_wait_ms": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_MS, "default": DEFAULT_MAX_WAIT_MS},
                    "max_output_chars": {"type": "integer", "minimum": 1, "maximum": MAX_OUTPUT_CHARS, "default": DEFAULT_MAX_OUTPUT_CHARS}
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        ),
        tool(
            API_STATUS,
            "Inspect one live, exited, or killed PTY session without consuming terminal output.",
            object_with_session_id(),
        ),
        tool(
            API_LIST,
            "List all PTY sessions recorded for the current agent.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        tool(
            API_KILL,
            "Terminate a live PTY session.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "grace_ms": {"type": "integer", "minimum": 0, "maximum": 10_000, "default": 1_000}
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

fn object_with_session_id() -> Value {
    json!({
        "type": "object",
        "properties": {"session_id": {"type": "string"}},
        "required": ["session_id"],
        "additionalProperties": false
    })
}

pub fn normalize_api_name(name: &str) -> &str {
    match name {
        API_CREATE => CREATE,
        API_INTERACT => INTERACT,
        API_STATUS => STATUS,
        API_LIST => LIST,
        API_KILL => KILL,
        _ => name,
    }
}

pub fn api_name(name: &str) -> &str {
    match name {
        CREATE => API_CREATE,
        INTERACT => API_INTERACT,
        STATUS => API_STATUS,
        LIST => API_LIST,
        KILL => API_KILL,
        _ => name,
    }
}

pub fn is_terminal_tool(name: &str) -> bool {
    matches!(name, CREATE | INTERACT | STATUS | LIST | KILL)
}

#[derive(Debug)]
pub enum TerminalRequest {
    Create(CreateRequest),
    Interact(InteractRequest),
    Status(SessionRequest),
    List,
    Kill(KillRequest),
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    #[serde(default = "default_width")]
    pub width: u16,
    #[serde(default = "default_height")]
    pub height: u16,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_wait_ms")]
    pub wait_ms: u32,
    #[serde(default = "default_max_wait_ms")]
    pub max_wait_ms: u32,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractRequest {
    pub session_id: String,
    #[serde(default)]
    pub input: Vec<TerminalInputAction>,
    #[serde(default = "default_wait_ms")]
    pub wait_ms: u32,
    #[serde(default = "default_max_wait_ms")]
    pub max_wait_ms: u32,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalInputAction {
    Text {
        text: String,
    },
    Key {
        key: String,
        #[serde(default)]
        modifiers: Vec<TerminalKeyModifier>,
        #[serde(default = "default_key_repeat")]
        repeat: u16,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TerminalKeyModifier {
    Ctrl,
    Alt,
    Shift,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KillRequest {
    pub session_id: String,
    #[serde(default = "default_grace_ms")]
    pub grace_ms: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyRequest {}

fn default_width() -> u16 {
    DEFAULT_WIDTH
}

fn default_height() -> u16 {
    DEFAULT_HEIGHT
}

fn default_cwd() -> String {
    ".".into()
}

fn default_wait_ms() -> u32 {
    DEFAULT_WAIT_MS
}

fn default_max_wait_ms() -> u32 {
    DEFAULT_MAX_WAIT_MS
}

fn default_max_output_chars() -> u32 {
    DEFAULT_MAX_OUTPUT_CHARS
}

fn default_grace_ms() -> u32 {
    1_000
}

fn default_key_repeat() -> u16 {
    1
}

pub fn parse_request(name: &str, arguments: &str) -> Result<TerminalRequest> {
    let request = match name {
        CREATE => TerminalRequest::Create(serde_json::from_str(arguments)?),
        INTERACT => TerminalRequest::Interact(serde_json::from_str(arguments)?),
        STATUS => TerminalRequest::Status(serde_json::from_str(arguments)?),
        LIST => {
            let _: EmptyRequest = serde_json::from_str(arguments)?;
            TerminalRequest::List
        }
        KILL => TerminalRequest::Kill(serde_json::from_str(arguments)?),
        _ => return Err(format!("unknown terminal tool {name}").into()),
    };
    validate_request(&request)?;
    Ok(request)
}

fn validate_request(request: &TerminalRequest) -> Result<()> {
    match request {
        TerminalRequest::Create(request) => {
            validate_size(request.width, request.height)?;
            if request.cwd.trim().is_empty() {
                return Err("Terminal.Create cwd must not be empty".into());
            }
            validate_wait(
                "Terminal.Create",
                request.wait_ms,
                request.max_wait_ms,
                request.max_output_chars,
            )?;
        }
        TerminalRequest::Interact(request) => {
            validate_session_id(&request.session_id)?;
            request.input()?;
            validate_wait(
                "Terminal.Interact",
                request.wait_ms,
                request.max_wait_ms,
                request.max_output_chars,
            )?;
        }
        TerminalRequest::Status(request) => validate_session_id(&request.session_id)?,
        TerminalRequest::List => {}
        TerminalRequest::Kill(request) => {
            validate_session_id(&request.session_id)?;
            if request.grace_ms > 10_000 {
                return Err("Terminal.Kill grace_ms must not exceed 10000".into());
            }
        }
    }
    Ok(())
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty() {
        Err("terminal session_id must not be empty".into())
    } else {
        Ok(())
    }
}

fn validate_size(width: u16, height: u16) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        Err(format!("terminal size must be between 1 and {MAX_DIMENSION}").into())
    } else {
        Ok(())
    }
}

fn validate_wait(
    operation: &str,
    wait_ms: u32,
    max_wait_ms: u32,
    max_output_chars: u32,
) -> Result<()> {
    if wait_ms > max_wait_ms {
        return Err(format!("{operation} wait_ms must not exceed max_wait_ms").into());
    }
    if max_wait_ms == 0 || max_wait_ms > MAX_WAIT_MS {
        return Err(format!("{operation} max_wait_ms must be between 1 and {MAX_WAIT_MS}").into());
    }
    if max_output_chars == 0 || max_output_chars > MAX_OUTPUT_CHARS {
        return Err(format!(
            "{operation} max_output_chars must be between 1 and {MAX_OUTPUT_CHARS}"
        )
        .into());
    }
    Ok(())
}

impl InteractRequest {
    fn input(&self) -> Result<Vec<u8>> {
        if self.input.len() > MAX_INPUT_ACTIONS {
            return Err(format!(
                "Terminal.Interact input must contain at most {MAX_INPUT_ACTIONS} actions"
            )
            .into());
        }
        let mut input = Vec::new();
        for action in &self.input {
            match action {
                TerminalInputAction::Text { text } => {
                    if text.chars().any(|character| {
                        character.is_control() && !matches!(character, '\n' | '\t')
                    }) {
                        return Err(
                            "Terminal.Interact text actions may contain newlines and tabs, but control keys must use key actions"
                                .into(),
                        );
                    }
                    input.extend_from_slice(text.as_bytes());
                }
                TerminalInputAction::Key {
                    key,
                    modifiers,
                    repeat,
                } => {
                    if *repeat == 0 || *repeat > MAX_KEY_REPEAT {
                        return Err(format!(
                            "Terminal.Interact key repeat must be between 1 and {MAX_KEY_REPEAT}"
                        )
                        .into());
                    }
                    let key = terminal_key_bytes(key, modifiers)?;
                    for _ in 0..*repeat {
                        input.extend_from_slice(&key);
                    }
                }
            }
        }
        Ok(input)
    }
}

fn terminal_key_bytes(key: &str, modifiers: &[TerminalKeyModifier]) -> Result<Vec<u8>> {
    let modifier_set = modifiers.iter().copied().collect::<BTreeSet<_>>();
    if modifier_set.len() != modifiers.len() {
        return Err("Terminal.Interact key modifiers must not contain duplicates".into());
    }
    let ctrl = modifier_set.contains(&TerminalKeyModifier::Ctrl);
    let alt = modifier_set.contains(&TerminalKeyModifier::Alt);
    let shift = modifier_set.contains(&TerminalKeyModifier::Shift);
    let modifier_code = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);

    let navigation = match key {
        "up" => Some(("A", None)),
        "down" => Some(("B", None)),
        "right" => Some(("C", None)),
        "left" => Some(("D", None)),
        "home" => Some(("H", None)),
        "end" => Some(("F", None)),
        "insert" => Some(("", Some(2))),
        "delete" => Some(("", Some(3))),
        "page_up" => Some(("", Some(5))),
        "page_down" => Some(("", Some(6))),
        "f1" => Some(("P", None)),
        "f2" => Some(("Q", None)),
        "f3" => Some(("R", None)),
        "f4" => Some(("S", None)),
        "f5" => Some(("", Some(15))),
        "f6" => Some(("", Some(17))),
        "f7" => Some(("", Some(18))),
        "f8" => Some(("", Some(19))),
        "f9" => Some(("", Some(20))),
        "f10" => Some(("", Some(21))),
        "f11" => Some(("", Some(23))),
        "f12" => Some(("", Some(24))),
        _ => None,
    };
    if let Some((final_character, tilde_code)) = navigation {
        let sequence = if let Some(code) = tilde_code {
            if modifiers.is_empty() {
                format!("\u{1b}[{code}~")
            } else {
                format!("\u{1b}[{code};{modifier_code}~")
            }
        } else if matches!(key, "f1" | "f2" | "f3" | "f4") && modifiers.is_empty() {
            format!("\u{1b}O{final_character}")
        } else if modifiers.is_empty() {
            format!("\u{1b}[{final_character}")
        } else {
            format!("\u{1b}[1;{modifier_code}{final_character}")
        };
        return Ok(sequence.into_bytes());
    }

    let base = match key {
        "enter" => {
            reject_modifiers(key, ctrl, shift)?;
            vec![b'\r']
        }
        "escape" => {
            reject_modifiers(key, ctrl, shift)?;
            vec![0x1b]
        }
        "backspace" => {
            reject_modifiers(key, ctrl, shift)?;
            vec![0x7f]
        }
        "tab" => {
            if ctrl {
                return Err("Terminal.Interact does not define Ctrl+Tab".into());
            }
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        "space" => printable_key_bytes(' ', ctrl, shift)?,
        _ => {
            let mut characters = key.chars();
            let Some(character) = characters.next() else {
                return Err("Terminal.Interact key must not be empty".into());
            };
            if characters.next().is_some() {
                return Err(format!("Terminal.Interact does not support key {key:?}").into());
            }
            printable_key_bytes(character, ctrl, shift)?
        }
    };
    if alt {
        let mut prefixed = Vec::with_capacity(base.len() + 1);
        prefixed.push(0x1b);
        prefixed.extend_from_slice(&base);
        Ok(prefixed)
    } else {
        Ok(base)
    }
}

fn reject_modifiers(key: &str, ctrl: bool, shift: bool) -> Result<()> {
    if ctrl || shift {
        Err(format!("Terminal.Interact does not define Ctrl or Shift modifiers for {key}").into())
    } else {
        Ok(())
    }
}

fn printable_key_bytes(character: char, ctrl: bool, shift: bool) -> Result<Vec<u8>> {
    if character.is_control() {
        return Err("Terminal.Interact key characters must be printable".into());
    }
    let character = if shift {
        if character.is_ascii_alphabetic() {
            character.to_ascii_uppercase()
        } else {
            return Err(
                "Terminal.Interact Shift on a printable key is only defined for ASCII letters"
                    .into(),
            );
        }
    } else {
        character
    };
    if ctrl {
        let control = match character {
            ' ' | '@' => 0,
            'a'..='z' | 'A'..='Z' => character.to_ascii_uppercase() as u8 & 0x1f,
            '['..='_' => character as u8 & 0x1f,
            '?' => 0x7f,
            _ => {
                return Err(format!("Terminal.Interact does not define Ctrl+{character}").into());
            }
        };
        Ok(vec![control])
    } else {
        let mut encoded = [0; 4];
        Ok(character.encode_utf8(&mut encoded).as_bytes().to_vec())
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TerminalCreated {
    pub session_id: String,
    pub state: String,
    pub shell: String,
    pub width: u16,
    pub height: u16,
    pub cwd: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum TerminalColor {
    Indexed(u8),
    Rgb([u8; 3]),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct TerminalStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<TerminalColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<TerminalColor>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inverse: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalStyleDefinition {
    pub id: u32,
    pub style: TerminalStyle,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalRowRun {
    pub col: u16,
    pub width: u16,
    pub text: String,
    pub style: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalRowUpdate {
    pub row: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub wrapped: bool,
    pub runs: Vec<TerminalRowRun>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalCursor {
    pub row: u64,
    pub col: u16,
    pub visible: bool,
    pub underlying: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub wide: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub wide_continuation: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalLineUpdate {
    pub session_id: String,
    pub sequence: u64,
    pub width: u16,
    pub height: u16,
    pub viewport: [u64; 2],
    pub style_count: u32,
    pub style_defs: Vec<TerminalStyleDefinition>,
    pub rows: Vec<TerminalRowUpdate>,
    pub cursor: TerminalCursor,
    pub state: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}

impl TerminalLineUpdate {
    pub fn plain_text(&self) -> String {
        self.rows
            .iter()
            .map(|row| format!("{:06}: {}", row.row, row.plain_text()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn metadata(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "sequence": self.sequence,
            "size": [self.width, self.height],
            "viewport": self.viewport,
            "changed_rows": self.rows.len(),
            "state": self.state,
            "exit_code": self.exit_code,
            "truncated": self.truncated,
        })
    }

    pub fn model_value(&self) -> Value {
        let styles = self
            .style_defs
            .iter()
            .map(model_style_value)
            .collect::<Vec<_>>();
        let rows = self
            .rows
            .iter()
            .map(TerminalRowUpdate::model_value)
            .collect::<Vec<_>>();
        json!({
            "type": "terminal_patch",
            "version": 2,
            "session_id": self.session_id,
            "sequence": self.sequence,
            "terminal_size": {
                "columns": self.width,
                "rows": self.height,
            },
            "viewport": {
                "first_terminal_row": self.viewport[0],
                "last_terminal_row": self.viewport[1],
            },
            "styles": styles,
            "rows": rows,
            "cursor": {
                "terminal_row": self.cursor.row,
                "column": self.cursor.col,
                "visible": self.cursor.visible,
                "width": if self.cursor.wide { 2 } else { 1 },
                "wide_continuation": self.cursor.wide_continuation,
                "underlying": self.cursor.underlying,
            },
            "pty_state": self.state,
            "exit_code": self.exit_code,
            "truncated": self.truncated,
        })
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.session_id.is_empty() || self.session_id.contains(['\n', '\r']) {
            return Err("terminal line update has an invalid session id".into());
        }
        if self.width == 0 || self.height == 0 {
            return Err("terminal line update has an empty viewport".into());
        }
        if self.viewport[1] < self.viewport[0]
            || self.viewport[1] - self.viewport[0] + 1 != u64::from(self.height)
        {
            return Err("terminal viewport does not match its fixed height".into());
        }
        if self.cursor.row < self.viewport[0]
            || self.cursor.row > self.viewport[1]
            || self.cursor.col >= self.width
        {
            return Err(format!(
                "terminal cursor {}:{} is outside viewport {}-{} at width {}",
                self.cursor.row, self.cursor.col, self.viewport[0], self.viewport[1], self.width
            ));
        }
        if self.style_count == 0 {
            return Err("terminal style catalog is empty".into());
        }
        let mut previous_definition = None;
        let mut defined_styles = BTreeSet::new();
        for definition in &self.style_defs {
            if definition.id == 0
                || definition.id >= self.style_count
                || previous_definition.is_some_and(|id| id >= definition.id)
            {
                return Err("terminal style definitions are invalid or unordered".into());
            }
            previous_definition = Some(definition.id);
            defined_styles.insert(definition.id);
        }
        let mut previous_row = None;
        let mut referenced_styles = BTreeSet::new();
        for row in &self.rows {
            if previous_row.is_some_and(|value| value >= row.row) {
                return Err("terminal changed rows are not strictly ordered".into());
            }
            previous_row = Some(row.row);
            let mut end = 0;
            for run in &row.runs {
                if run.style >= self.style_count
                    || run.width == 0
                    || run.col < end
                    || run.col.saturating_add(run.width) > self.width
                {
                    return Err("terminal row run overlaps or exceeds the fixed width".into());
                }
                if run.style != 0 {
                    referenced_styles.insert(run.style);
                }
                if run.text.contains(['\n', '\r', '\x1b'])
                    || UnicodeWidthStr::width(run.text.as_str()) != usize::from(run.width)
                {
                    return Err("terminal row run has invalid text width".into());
                }
                end = run.col + run.width;
            }
        }
        if defined_styles != referenced_styles {
            return Err(
                "terminal style definitions must exactly describe styles used by this patch".into(),
            );
        }
        Ok(())
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn model_color_text(color: &TerminalColor) -> String {
    match color {
        TerminalColor::Indexed(value) => format!("indexed({value})"),
        TerminalColor::Rgb([red, green, blue]) => {
            format!("#{red:02x}{green:02x}{blue:02x}")
        }
    }
}

fn model_style_value(definition: &TerminalStyleDefinition) -> Value {
    let mut value = json!({"id": definition.id});
    let object = value
        .as_object_mut()
        .expect("terminal model style is an object");
    if let Some(foreground) = &definition.style.foreground {
        object.insert(
            "foreground".into(),
            Value::String(model_color_text(foreground)),
        );
    }
    if let Some(background) = &definition.style.background {
        object.insert(
            "background".into(),
            Value::String(model_color_text(background)),
        );
    }
    let attributes = [
        (definition.style.bold, "bold"),
        (definition.style.dim, "dim"),
        (definition.style.italic, "italic"),
        (definition.style.underline, "underline"),
        (definition.style.inverse, "inverse"),
    ]
    .into_iter()
    .filter_map(|(enabled, name)| enabled.then_some(name))
    .collect::<Vec<_>>();
    if !attributes.is_empty() {
        object.insert("attributes".into(), json!(attributes));
    }
    value
}

impl TerminalRowUpdate {
    fn plain_text(&self) -> String {
        let mut output = String::new();
        let mut col = 0;
        for run in &self.runs {
            if run.col > col {
                output.extend(std::iter::repeat_n(' ', usize::from(run.col - col)));
            }
            output.push_str(&run.text);
            col = run.col.saturating_add(run.width);
        }
        output
    }

    fn model_value(&self) -> Value {
        let mut value = json!({
            "terminal_row": self.row,
            "text": self.plain_text(),
        });
        let object = value
            .as_object_mut()
            .expect("terminal model row is an object");
        if self.wrapped {
            object.insert("wrapped".into(), Value::Bool(true));
        }
        let style_spans = self
            .runs
            .iter()
            .filter(|run| run.style != 0)
            .map(|run| {
                json!({
                    "start_column": run.col,
                    "width": run.width,
                    "style": run.style,
                })
            })
            .collect::<Vec<_>>();
        if !style_spans.is_empty() {
            object.insert("style_spans".into(), Value::Array(style_spans));
        }
        value
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TerminalStatus {
    pub session_id: String,
    pub state: String,
    pub shell: String,
    pub width: u16,
    pub height: u16,
    pub cwd: String,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEnd {
    pub state: TerminalSessionState,
    pub exit_code: Option<i32>,
    pub detail: String,
}

pub struct InteractOutcome {
    pub update: TerminalLineUpdate,
    pub end: Option<SessionEnd>,
}

pub struct CreateOutcome {
    pub created: TerminalCreated,
    pub update: TerminalLineUpdate,
    pub end: Option<SessionEnd>,
}

pub struct StatusOutcome {
    pub status: TerminalStatus,
    pub end: Option<SessionEnd>,
}

pub struct KillOutcome {
    pub status: TerminalStatus,
    pub end: SessionEnd,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalSessionPreview {
    pub session_id: String,
    pub creation_order: EventId,
    pub width: u16,
    pub height: u16,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalFrame {
    pub session_id: String,
    pub revision: u64,
    pub width: u16,
    pub height: u16,
    pub viewport: [u64; 2],
    pub style_defs: Vec<TerminalStyleDefinition>,
    pub rows: Vec<TerminalRowUpdate>,
    pub cursor: TerminalCursor,
}

#[derive(Clone, Default)]
pub struct TerminalObserver {
    sessions: Arc<Mutex<BTreeMap<String, ObservedTerminal>>>,
}

#[derive(Clone)]
struct ObservedTerminal {
    order: EventId,
    created: TerminalCreated,
    output: Arc<SharedTerminalOutput>,
}

impl TerminalObserver {
    pub fn active_count(&self) -> Result<usize> {
        Ok(self.active_sessions()?.len())
    }

    pub fn active_sessions(&self) -> Result<Vec<TerminalSessionPreview>> {
        let observed = self
            .sessions
            .lock()
            .map_err(|_| "terminal observer lock is poisoned")?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut active = Vec::with_capacity(observed.len());
        for session in observed {
            let activity = session.output.activity()?;
            if !activity.closed {
                active.push((
                    session.order,
                    TerminalSessionPreview {
                        session_id: session.created.session_id,
                        creation_order: session.order,
                        width: session.created.width,
                        height: session.created.height,
                        revision: activity.revision,
                    },
                ));
            }
        }
        active.sort_by_key(|(order, _)| *order);
        Ok(active.into_iter().map(|(_, session)| session).collect())
    }

    pub fn frame(&self, session_id: &str) -> Result<Option<TerminalFrame>> {
        let observed = self
            .sessions
            .lock()
            .map_err(|_| "terminal observer lock is poisoned")?
            .get(session_id)
            .cloned();
        let Some(observed) = observed else {
            return Ok(None);
        };
        if observed.output.activity()?.closed {
            return Ok(None);
        }
        Ok(Some(observed.output.frame(&observed.created)?))
    }

    fn insert(
        &self,
        order: EventId,
        created: TerminalCreated,
        output: Arc<SharedTerminalOutput>,
    ) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "terminal observer lock is poisoned")?;
        if sessions.contains_key(&created.session_id) {
            return Err("terminal observer session already exists".into());
        }
        sessions.insert(
            created.session_id.clone(),
            ObservedTerminal {
                order,
                created,
                output,
            },
        );
        Ok(())
    }

    fn remove(&self, session_id: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(session_id);
        }
    }

    fn clear(&self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.clear();
        }
    }
}

pub struct TerminalManager {
    shell: String,
    shell_kind: Option<ShellKind>,
    sessions: BTreeMap<String, PtySession>,
    observer: TerminalObserver,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        let backend = detect_terminal_backend();
        Self {
            shell: backend
                .as_ref()
                .map(|backend| backend.program.clone())
                .unwrap_or_else(|| UNAVAILABLE_BACKEND.to_owned()),
            shell_kind: backend.map(|backend| backend.kind),
            sessions: BTreeMap::new(),
            observer: TerminalObserver::default(),
        }
    }

    pub fn shell_backend(&self) -> &str {
        &self.shell
    }

    pub fn is_available(&self) -> bool {
        self.shell_kind.is_some()
    }

    pub fn observer(&self) -> TerminalObserver {
        self.observer.clone()
    }

    pub fn create(
        &mut self,
        workspace: &Path,
        tool_call_id: EventId,
        request: &CreateRequest,
    ) -> Result<CreateOutcome> {
        let session_id = format!("pty-{tool_call_id}");
        if self.sessions.contains_key(&session_id) {
            return Err(format!("terminal session {session_id} already exists").into());
        }
        let shell_kind = self.shell_kind.ok_or(
            "Terminal is unavailable because the required PowerShell backend was not found",
        )?;
        #[cfg(unix)]
        if !Path::new(&self.shell).is_file() {
            return Err(format!("required Bash backend {} does not exist", self.shell).into());
        }
        let cwd = resolve_cwd(workspace, &request.cwd)?;
        let size = pty_size(request.width, request.height);
        let pair = native_pty_system().openpty(size)?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        #[cfg(windows)]
        let writer = {
            let mut writer = writer;
            if shell_kind == ShellKind::PowerShell {
                // portable-pty creates ConPTY with PSEUDOCONSOLE_INHERIT_CURSOR.
                // ConPTY emits ESC[6n and waits for a cursor-position response
                // before it finishes initializing the attached shell.
                writer.write_all(WINDOWS_INITIAL_CURSOR_RESPONSE)?;
                writer.flush()?;
            }
            writer
        };
        let mut command = CommandBuilder::new(&self.shell);
        match shell_kind {
            #[cfg(unix)]
            ShellKind::Bash => {
                command.arg("--noprofile");
                command.arg("--norc");
                command.arg("-i");
            }
            #[cfg(windows)]
            ShellKind::PowerShell => {
                command.arg("-NoLogo");
                command.arg("-NoProfile");
            }
        }
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        #[cfg(unix)]
        command.env("SHELL", &self.shell);
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);

        let output = Arc::new(SharedTerminalOutput::new(request.height, request.width));
        let reader_discard = spawn_reader(reader, Arc::clone(&output));
        let created = TerminalCreated {
            session_id: session_id.clone(),
            state: TerminalSessionState::Running.to_string(),
            shell: self.shell.clone(),
            width: request.width,
            height: request.height,
            cwd: crate::host_path::public_host_path(&cwd),
        };
        self.observer
            .insert(tool_call_id, created.clone(), Arc::clone(&output))?;
        self.sessions.insert(
            session_id.clone(),
            PtySession {
                created: created.clone(),
                sequence: 0,
                writer: Some(writer),
                child,
                output,
                observed_output_revision: 0,
                reader_discard,
                // Close the input writer first, then keep the output reader
                // draining until after the ConPTY master has closed.
                _master: pair.master,
            },
        );
        let initial = InteractRequest {
            session_id: session_id.clone(),
            input: Vec::new(),
            wait_ms: request.wait_ms,
            max_wait_ms: request.max_wait_ms,
            max_output_chars: request.max_output_chars,
        };
        let initial = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| format!("terminal session {session_id} is not live"))?;
            session.interact(&initial, &[], true)
        };
        match initial {
            Ok((update, end)) => {
                if end.is_some() {
                    self.sessions.remove(&session_id);
                    self.observer.remove(&session_id);
                }
                Ok(CreateOutcome {
                    created,
                    update,
                    end,
                })
            }
            Err(error) => {
                if let Some(mut session) = self.sessions.remove(&session_id) {
                    let _ = session.child.kill();
                }
                self.observer.remove(&session_id);
                Err(error)
            }
        }
    }

    pub fn interact(&mut self, request: &InteractRequest) -> Result<InteractOutcome> {
        self.shell_kind.ok_or(
            "Terminal is unavailable because the required PowerShell backend was not found",
        )?;
        let input = request.input()?;
        let (update, end) = {
            let session = self
                .sessions
                .get_mut(&request.session_id)
                .ok_or_else(|| format!("terminal session {} is not live", request.session_id))?;
            session.interact(request, &input, false)?
        };
        if end.is_some() {
            self.sessions.remove(&request.session_id);
            self.observer.remove(&request.session_id);
        }
        Ok(InteractOutcome { update, end })
    }

    pub fn status(&mut self, session_id: &str) -> Result<StatusOutcome> {
        let (status, end) = {
            let session = self
                .sessions
                .get_mut(session_id)
                .ok_or_else(|| format!("terminal session {session_id} is not live"))?;
            if let Some(end) = session.try_exit()? {
                (session.status(end.state, end.exit_code), Some(end))
            } else {
                (session.status(TerminalSessionState::Running, None), None)
            }
        };
        if end.is_some() {
            self.sessions.remove(session_id);
            self.observer.remove(session_id);
        }
        Ok(StatusOutcome { status, end })
    }

    pub fn kill(&mut self, request: &KillRequest) -> Result<KillOutcome> {
        let mut session = self
            .sessions
            .remove(&request.session_id)
            .ok_or_else(|| format!("terminal session {} is not live", request.session_id))?;
        self.observer.remove(&request.session_id);
        if let Some(end) = session.try_exit()? {
            let status = session.status(end.state, end.exit_code);
            return Ok(KillOutcome { status, end });
        }

        session.child.kill()?;
        let deadline = Instant::now() + Duration::from_millis(u64::from(request.grace_ms));
        let exit_code = loop {
            if let Some(end) = session.try_exit()? {
                break end.exit_code;
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let end = SessionEnd {
            state: TerminalSessionState::Killed,
            exit_code,
            detail: "terminal session was killed".into(),
        };
        let status = session.status(end.state, end.exit_code);
        Ok(KillOutcome { status, end })
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        self.observer.clear();
        for session in self.sessions.values_mut() {
            let _ = session.child.kill();
        }
    }
}

struct PtySession {
    created: TerminalCreated,
    sequence: u64,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<SharedTerminalOutput>,
    observed_output_revision: u64,
    reader_discard: Arc<AtomicBool>,
    _master: Box<dyn MasterPty + Send>,
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        drop(self.writer.take());
        self.reader_discard.store(true, Ordering::Release);
    }
}

impl PtySession {
    fn interact(
        &mut self,
        request: &InteractRequest,
        input: &[u8],
        wait_for_first_output: bool,
    ) -> Result<(TerminalLineUpdate, Option<SessionEnd>)> {
        let start = Instant::now();
        let mut local_error = None;
        let mut end = self.try_exit()?;
        let mut input_written_at = None;
        if end.is_none() && !input.is_empty() {
            let writer = self.writer.as_mut().ok_or("PTY writer is closed")?;
            if let Err(error) = writer.write_all(input).and_then(|()| writer.flush()) {
                local_error = Some(format!("PTY write failed: {error}"));
            } else {
                input_written_at = Some(Instant::now());
            }
        }

        let mut last_activity = input_written_at.unwrap_or(start);
        let idle = Duration::from_millis(u64::from(request.wait_ms));
        let maximum = Duration::from_millis(u64::from(request.max_wait_ms));
        let mut saw_output = false;

        loop {
            let activity = self.output.activity()?;
            if activity.output_revision > self.observed_output_revision {
                saw_output = true;
                last_activity = input_written_at
                    .map(|written_at| activity.last_activity.max(written_at))
                    .unwrap_or(activity.last_activity);
            }
            if end.is_none() {
                end = self.try_exit()?;
            }
            let now = Instant::now();
            let elapsed = now.duration_since(start);
            let reader_closed = activity.closed || local_error.is_some();
            let exit_drain_idle = idle.max(Duration::from_millis(EXIT_DRAIN_IDLE_MS));
            if elapsed >= maximum
                || (end.is_some()
                    && (reader_closed || now.duration_since(last_activity) >= exit_drain_idle))
                || (end.is_none()
                    && !reader_closed
                    && (!wait_for_first_output || saw_output)
                    && now.duration_since(last_activity) >= idle)
            {
                break;
            }
            let until_maximum = maximum.saturating_sub(elapsed);
            let until_idle = if end.is_some() {
                exit_drain_idle.saturating_sub(now.duration_since(last_activity))
            } else if reader_closed || (wait_for_first_output && !saw_output) {
                until_maximum
            } else {
                idle.saturating_sub(now.duration_since(last_activity))
            };
            let timeout = until_idle.min(until_maximum).min(Duration::from_millis(20));
            if !timeout.is_zero() {
                thread::sleep(timeout);
            }
        }

        if end.is_none() {
            end = self.try_exit()?;
        }
        let activity = self.output.activity()?;
        let reader_error = local_error.or(activity.reader_error.clone());
        if end.is_none() && (activity.closed || reader_error.is_some()) {
            let _ = self.child.kill();
            end = Some(SessionEnd {
                state: TerminalSessionState::Lost,
                exit_code: None,
                detail: reader_error
                    .map(|error| format!("PTY transport lost: {error}"))
                    .unwrap_or_else(|| "PTY reader closed before process status was known".into()),
            });
        }

        let (rendered, activity) = self.output.capture(request.max_output_chars as usize)?;
        self.observed_output_revision = activity.output_revision;
        self.sequence += 1;
        let state = end
            .as_ref()
            .map(|end| end.state)
            .unwrap_or(TerminalSessionState::Running);
        let exit_code = end.as_ref().and_then(|end| end.exit_code);
        let update = TerminalLineUpdate {
            session_id: self.created.session_id.clone(),
            sequence: self.sequence,
            width: self.created.width,
            height: self.created.height,
            viewport: rendered.viewport,
            style_count: rendered.style_count,
            style_defs: rendered.style_defs,
            rows: rendered.rows,
            cursor: rendered.cursor,
            state: state.to_string(),
            exit_code,
            truncated: rendered.truncated,
        };
        Ok((update, end))
    }

    fn try_exit(&mut self) -> Result<Option<SessionEnd>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        let exit_code = i32::try_from(status.exit_code()).ok();
        let detail = status
            .signal()
            .map(|signal| format!("terminal shell exited from signal {signal}"))
            .unwrap_or_else(|| format!("terminal shell exited with code {}", status.exit_code()));
        Ok(Some(SessionEnd {
            state: TerminalSessionState::Exited,
            exit_code,
            detail,
        }))
    }

    fn status(&self, state: TerminalSessionState, exit_code: Option<i32>) -> TerminalStatus {
        TerminalStatus {
            session_id: self.created.session_id.clone(),
            state: state.to_string(),
            shell: self.created.shell.clone(),
            width: self.created.width,
            height: self.created.height,
            cwd: self.created.cwd.clone(),
            exit_code,
        }
    }
}

#[derive(Clone)]
struct ReaderActivity {
    revision: u64,
    output_revision: u64,
    last_activity: Instant,
    closed: bool,
    reader_error: Option<String>,
}

struct SharedTerminalOutput {
    state: Mutex<SharedTerminalOutputState>,
}

struct SharedTerminalOutputState {
    renderer: TerminalRenderer,
    revision: u64,
    output_revision: u64,
    last_activity: Instant,
    saw_eof: bool,
    reader_error: Option<String>,
}

impl SharedTerminalOutput {
    fn new(height: u16, width: u16) -> Self {
        Self {
            state: Mutex::new(SharedTerminalOutputState {
                renderer: TerminalRenderer::new(height, width),
                revision: 0,
                output_revision: 0,
                last_activity: Instant::now(),
                saw_eof: false,
                reader_error: None,
            }),
        }
    }

    fn process(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.renderer.process(data);
            state.revision = state.revision.wrapping_add(1);
            state.output_revision = state.output_revision.wrapping_add(1);
            state.last_activity = Instant::now();
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.saw_eof = true;
            state.revision = state.revision.wrapping_add(1);
        }
    }

    fn fail(&self, error: String) {
        if let Ok(mut state) = self.state.lock() {
            state.reader_error = Some(error);
            state.revision = state.revision.wrapping_add(1);
        }
    }

    fn activity(&self) -> Result<ReaderActivity> {
        let state = self
            .state
            .lock()
            .map_err(|_| "terminal output lock is poisoned")?;
        Ok(state.activity())
    }

    fn capture(&self, maximum_chars: usize) -> Result<(RenderedOutput, ReaderActivity)> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "terminal output lock is poisoned")?;
        let rendered = state.renderer.capture(maximum_chars);
        Ok((rendered, state.activity()))
    }

    fn frame(&self, created: &TerminalCreated) -> Result<TerminalFrame> {
        let state = self
            .state
            .lock()
            .map_err(|_| "terminal output lock is poisoned")?;
        Ok(state.renderer.frame(created, state.revision))
    }
}

impl SharedTerminalOutputState {
    fn activity(&self) -> ReaderActivity {
        ReaderActivity {
            revision: self.revision,
            output_revision: self.output_revision,
            last_activity: self.last_activity,
            closed: self.saw_eof || self.reader_error.is_some(),
            reader_error: self.reader_error.clone(),
        }
    }
}

// Owned only by one live PtySession. This comparison state is deliberately
// neither serializable nor connected to the EDB/model-context timeline.
struct TerminalRenderer {
    parser: vt100::Parser,
    baseline_rows: BTreeMap<u64, InternalRow>,
    viewport_start: u64,
    primary_origin: u64,
    primary_scrollback: usize,
    next_row: u64,
    styles: StyleTable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct InternalRow {
    wrapped: bool,
    runs: Vec<TerminalRowRun>,
}

struct RenderedOutput {
    viewport: [u64; 2],
    style_count: u32,
    style_defs: Vec<TerminalStyleDefinition>,
    rows: Vec<TerminalRowUpdate>,
    cursor: TerminalCursor,
    truncated: bool,
}

#[cfg(test)]
impl RenderedOutput {
    fn plain_text(&self) -> String {
        self.rows
            .iter()
            .map(|row| row.plain_text())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct StyleTable {
    styles: Vec<TerminalStyle>,
    ids: HashMap<TerminalStyle, u32>,
}

impl Default for StyleTable {
    fn default() -> Self {
        let default = TerminalStyle::default();
        let mut ids = HashMap::new();
        ids.insert(default.clone(), 0);
        Self {
            styles: vec![default],
            ids,
        }
    }
}

impl StyleTable {
    fn id(&mut self, style: TerminalStyle) -> u32 {
        if let Some(id) = self.ids.get(&style) {
            return *id;
        }
        let id = u32::try_from(self.styles.len()).expect("terminal style table overflow");
        self.styles.push(style.clone());
        self.ids.insert(style, id);
        id
    }
}

impl TerminalRenderer {
    fn new(height: u16, width: u16) -> Self {
        let parser = vt100::Parser::new(height, width, PTY_SCROLLBACK_ROWS);
        let baseline_rows = (0..u64::from(height))
            .map(|row| (row, InternalRow::default()))
            .collect();
        Self {
            parser,
            baseline_rows,
            viewport_start: 0,
            primary_origin: 0,
            primary_scrollback: 0,
            next_row: u64::from(height),
            styles: StyleTable::default(),
        }
    }

    fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    fn frame(&self, created: &TerminalCreated, revision: u64) -> TerminalFrame {
        let screen = self.parser.screen();
        let mut styles = StyleTable::default();
        let (viewport_start, rendered_rows) = if screen.alternate_screen() {
            (0, render_visible_rows(screen, 0, &mut styles))
        } else {
            let (scrollback, rows) = render_primary_rows(screen, 0, &mut styles);
            (
                u64::try_from(scrollback).expect("terminal scrollback fits u64"),
                rows.into_iter()
                    .map(|(row, rendered)| {
                        (
                            u64::try_from(row).expect("terminal preview row fits u64"),
                            rendered,
                        )
                    })
                    .collect(),
            )
        };
        let rows = rendered_rows
            .into_iter()
            .map(|(row, rendered)| TerminalRowUpdate {
                row,
                wrapped: rendered.wrapped,
                runs: rendered.runs,
            })
            .collect();
        let style_defs = styles
            .styles
            .into_iter()
            .enumerate()
            .skip(1)
            .map(|(id, style)| TerminalStyleDefinition {
                id: u32::try_from(id).expect("terminal style id fits u32"),
                style,
            })
            .collect();
        TerminalFrame {
            session_id: created.session_id.clone(),
            revision,
            width: created.width,
            height: created.height,
            viewport: [
                viewport_start,
                viewport_start
                    .saturating_add(u64::from(created.height))
                    .saturating_sub(1),
            ],
            style_defs,
            rows,
            cursor: cursor_at(screen, viewport_start),
        }
    }

    fn capture(&mut self, maximum_chars: usize) -> RenderedOutput {
        let screen = self.parser.screen();
        let alternate_screen = screen.alternate_screen();
        let (height, _) = screen.size();
        let (viewport_start, rendered_rows) = if alternate_screen {
            (
                self.viewport_start,
                render_visible_rows(screen, self.viewport_start, &mut self.styles),
            )
        } else {
            let (scrollback, retained) =
                render_primary_rows(screen, self.primary_scrollback, &mut self.styles);
            if scrollback < self.primary_scrollback {
                self.primary_origin = self.next_row;
            }
            let viewport_start = self
                .primary_origin
                .saturating_add(u64::try_from(scrollback).expect("scrollback fits u64"));
            let rows = retained
                .into_iter()
                .map(|(offset, row)| {
                    (
                        self.primary_origin
                            .saturating_add(u64::try_from(offset).expect("row offset fits u64")),
                        row,
                    )
                })
                .collect::<Vec<_>>();
            self.primary_scrollback = scrollback;
            self.viewport_start = viewport_start;
            self.next_row = self.next_row.max(
                self.primary_origin
                    .saturating_add(u64::try_from(scrollback).expect("scrollback fits u64"))
                    .saturating_add(u64::from(height)),
            );
            (viewport_start, rows)
        };

        let mut changed = Vec::new();
        for (absolute_row, row) in rendered_rows {
            if self.baseline_rows.get(&absolute_row) != Some(&row) {
                changed.push(TerminalRowUpdate {
                    row: absolute_row,
                    wrapped: row.wrapped,
                    runs: row.runs.clone(),
                });
            }
            self.baseline_rows.insert(absolute_row, row);
        }
        let cursor = cursor_at(screen, viewport_start);
        let style_count =
            u32::try_from(self.styles.styles.len()).expect("terminal style table fits u32");
        let (rows, truncated) = truncate_changed_rows(changed, maximum_chars);
        let referenced_styles = rows
            .iter()
            .flat_map(|row| row.runs.iter().map(|run| run.style))
            .filter(|style| *style != 0)
            .collect::<BTreeSet<_>>();
        let style_defs = referenced_styles
            .into_iter()
            .map(|id| TerminalStyleDefinition {
                id,
                style: self.styles.styles[usize::try_from(id).expect("style id fits usize")]
                    .clone(),
            })
            .collect();
        RenderedOutput {
            viewport: [
                viewport_start,
                viewport_start
                    .saturating_add(u64::from(height))
                    .saturating_sub(1),
            ],
            style_count,
            style_defs,
            rows,
            cursor,
            truncated,
        }
    }
}

fn render_primary_rows(
    screen: &vt100::Screen,
    previous_scrollback: usize,
    styles: &mut StyleTable,
) -> (usize, Vec<(usize, InternalRow)>) {
    let mut view = screen.clone();
    let (height, _) = view.size();
    view.set_scrollback(usize::MAX);
    let scrollback = view.scrollback();
    let first_changed = if scrollback < previous_scrollback {
        0
    } else {
        previous_scrollback
    };
    let mut rows = Vec::with_capacity(
        scrollback
            .saturating_sub(first_changed)
            .saturating_add(usize::from(height)),
    );
    let mut start = first_changed;
    while start < scrollback {
        view.set_scrollback(scrollback - start);
        let take = usize::from(height).min(scrollback - start);
        for row in 0..u16::try_from(take).expect("visible terminal height fits u16") {
            rows.push((start + usize::from(row), render_row(&view, row, styles)));
        }
        start += take;
    }
    view.set_scrollback(0);
    for row in 0..height {
        rows.push((
            scrollback + usize::from(row),
            render_row(&view, row, styles),
        ));
    }
    (scrollback, rows)
}

fn render_visible_rows(
    screen: &vt100::Screen,
    viewport_start: u64,
    styles: &mut StyleTable,
) -> Vec<(u64, InternalRow)> {
    let (height, _) = screen.size();
    let mut rows = Vec::with_capacity(usize::from(height));
    for row in 0..height {
        rows.push((
            viewport_start.saturating_add(u64::from(row)),
            render_row(screen, row, styles),
        ));
    }
    rows
}

fn render_row(screen: &vt100::Screen, row: u16, styles: &mut StyleTable) -> InternalRow {
    let (_, width) = screen.size();
    let mut runs = Vec::new();
    let mut col = 0;
    while col < width {
        let Some(cell) = screen.cell(row, col) else {
            col += 1;
            continue;
        };
        if cell.is_wide_continuation() {
            col += 1;
            continue;
        }
        let cell_width = if cell.is_wide() { 2 } else { 1 };
        let style = style_from_cell(cell);
        if cell.has_contents() || style != TerminalStyle::default() {
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            append_row_piece(&mut runs, col, cell_width, text, styles.id(style));
        }
        col = col.saturating_add(cell_width);
    }
    InternalRow {
        wrapped: screen.row_wrapped(row),
        runs,
    }
}

fn append_row_piece(runs: &mut Vec<TerminalRowRun>, col: u16, width: u16, text: &str, style: u32) {
    if let Some(last) = runs.last_mut()
        && last.style == style
        && last.col.saturating_add(last.width) == col
    {
        last.width = last.width.saturating_add(width);
        last.text.push_str(text);
        return;
    }
    runs.push(TerminalRowRun {
        col,
        width,
        text: text.to_owned(),
        style,
    });
}

fn cursor_at(screen: &vt100::Screen, viewport_start: u64) -> TerminalCursor {
    let (row, raw_col) = screen.cursor_position();
    let (_, width) = screen.size();
    // vt100 keeps the cursor one cell past the right margin while a delayed
    // autowrap is pending. Humans still see it on the last rendered cell.
    let col = raw_col.min(width.saturating_sub(1));
    let cell = screen.cell(row, col);
    let underlying = cell
        .filter(|cell| cell.has_contents())
        .or_else(|| {
            cell.filter(|cell| cell.is_wide_continuation())
                .and_then(|_| col.checked_sub(1))
                .and_then(|previous| screen.cell(row, previous))
                .filter(|cell| cell.has_contents())
        })
        .map(|cell| cell.contents().to_owned())
        .unwrap_or_default();
    TerminalCursor {
        row: viewport_start.saturating_add(u64::from(row)),
        col,
        visible: !screen.hide_cursor(),
        underlying,
        wide: cell.is_some_and(vt100::Cell::is_wide)
            || cell.is_some_and(vt100::Cell::is_wide_continuation),
        wide_continuation: cell.is_some_and(vt100::Cell::is_wide_continuation),
    }
}

fn truncate_changed_rows(
    mut rows: Vec<TerminalRowUpdate>,
    maximum_chars: usize,
) -> (Vec<TerminalRowUpdate>, bool) {
    let mut total = rows
        .iter()
        .map(|row| {
            1 + row
                .runs
                .iter()
                .map(|run| run.text.chars().count())
                .sum::<usize>()
        })
        .sum::<usize>();
    if total <= maximum_chars {
        return (rows, false);
    }
    let mut remove = 0;
    while remove < rows.len() && total > maximum_chars {
        total = total.saturating_sub(
            1 + rows[remove]
                .runs
                .iter()
                .map(|run| run.text.chars().count())
                .sum::<usize>(),
        );
        remove += 1;
    }
    rows.drain(..remove);
    (rows, true)
}

fn style_from_cell(cell: &vt100::Cell) -> TerminalStyle {
    TerminalStyle {
        foreground: color_from_vt100(cell.fgcolor()),
        background: color_from_vt100(cell.bgcolor()),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
    }
}

fn color_from_vt100(color: vt100::Color) -> Option<TerminalColor> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(value) => Some(TerminalColor::Indexed(value)),
        vt100::Color::Rgb(red, green, blue) => Some(TerminalColor::Rgb([red, green, blue])),
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    output: Arc<SharedTerminalOutput>,
) -> Arc<AtomicBool> {
    let discard = Arc::new(AtomicBool::new(false));
    let thread_discard = discard.clone();
    thread::spawn(move || read_pty(&mut reader, &output, &thread_discard));
    discard
}

fn read_pty(reader: &mut dyn Read, output: &SharedTerminalOutput, discard: &AtomicBool) {
    let mut buffer = [0_u8; READER_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                if !discard.load(Ordering::Acquire) {
                    output.close();
                }
                return;
            }
            Ok(length) => {
                if !discard.load(Ordering::Acquire) {
                    output.process(&buffer[..length]);
                }
            }
            Err(error) => {
                if !discard.load(Ordering::Acquire) {
                    output.fail(error.to_string());
                }
                return;
            }
        }
    }
}

fn resolve_cwd(workspace: &Path, cwd: &str) -> Result<PathBuf> {
    let workspace = fs::canonicalize(workspace)?;
    let requested = Path::new(cwd);
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let requested = fs::canonicalize(requested)?;
    if !requested.starts_with(&workspace) {
        return Err("Terminal.Create cwd must stay inside the workspace".into());
    }
    if !requested.is_dir() {
        return Err("Terminal.Create cwd must be a directory".into());
    }
    Ok(requested)
}

fn pty_size(width: u16, height: u16) -> PtySize {
    PtySize {
        rows: height,
        cols: width,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
pub(crate) fn test_update(content: &str) -> TerminalLineUpdate {
    let rows = content
        .split('\n')
        .enumerate()
        .map(|(row, text)| TerminalRowUpdate {
            row: u64::try_from(row).unwrap(),
            wrapped: false,
            runs: if text.is_empty() {
                Vec::new()
            } else {
                vec![TerminalRowRun {
                    col: 0,
                    width: u16::try_from(UnicodeWidthStr::width(text)).unwrap(),
                    text: text.to_owned(),
                    style: 0,
                }]
            },
        })
        .collect::<Vec<_>>();
    TerminalLineUpdate {
        session_id: "pty-test".into(),
        sequence: 1,
        width: 120,
        height: 40,
        viewport: [0, 39],
        style_count: 1,
        style_defs: Vec::new(),
        rows,
        cursor: TerminalCursor {
            row: u64::try_from(content.matches('\n').count()).unwrap(),
            col: u16::try_from(UnicodeWidthStr::width(
                content.rsplit('\n').next().unwrap_or(""),
            ))
            .unwrap(),
            visible: true,
            underlying: String::new(),
            wide: false,
            wide_continuation: false,
        },
        state: "running".into(),
        exit_code: None,
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::*;

    fn real_pty_test_guard() -> MutexGuard<'static, ()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("me-pty-{name}-{}", std::process::id()))
    }

    fn text(text: &str) -> TerminalInputAction {
        TerminalInputAction::Text { text: text.into() }
    }

    fn key(key: &str) -> TerminalInputAction {
        TerminalInputAction::Key {
            key: key.into(),
            modifiers: Vec::new(),
            repeat: 1,
        }
    }

    fn modified_key(
        key: &str,
        modifiers: &[TerminalKeyModifier],
        repeat: u16,
    ) -> TerminalInputAction {
        TerminalInputAction::Key {
            key: key.into(),
            modifiers: modifiers.to_vec(),
            repeat,
        }
    }

    fn command_input(command: &str) -> Vec<TerminalInputAction> {
        vec![text(command), key("enter")]
    }

    fn interact(session_id: &str, input: Option<&str>) -> InteractRequest {
        let input = input.map(command_input).unwrap_or_default();
        InteractRequest {
            session_id: session_id.into(),
            input,
            wait_ms: 1_000,
            max_wait_ms: 3_000,
            max_output_chars: 20_000,
        }
    }

    #[test]
    fn exposes_all_terminal_actions_and_shell_prompt() {
        let names = tool_definitions()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![API_CREATE, API_INTERACT, API_STATUS, API_LIST, API_KILL]
        );
        let manager = TerminalManager::new();
        let shell = manager.shell_backend();
        #[cfg(unix)]
        {
            assert!(manager.is_available());
            assert_eq!(shell, UNIX_BASH_BACKEND);
        }
        assert!(tool_prompt(shell).contains(&format!("Current PTY shell backend: `{shell}`")));
        assert!(tool_prompt(shell).contains("bundled Python 3.12 is available as `python`"));
        #[cfg(unix)]
        assert!(tool_prompt(shell).contains("--noprofile --norc -i"));
        assert!(tool_prompt(shell).contains("session_not_found"));
        assert!(tool_prompt(shell).contains("no separate session-loss result is synthesized"));
        assert!(tool_prompt(shell).contains(r#""terminal_updates""#));
        assert!(tool_prompt(shell).contains(r#""version": 2"#));
        assert!(tool_prompt(shell).contains(r#""terminal_row": 2, "text": """#));
        assert!(tool_prompt(shell).contains(r#""rows": []"#));
        assert!(tool_prompt(shell).contains("not content or command arguments"));
        assert!(tool_prompt(shell).contains("after a simple `echo hello`"));
        assert!(tool_prompt(shell).contains("autowrap, wide characters"));
        assert!(tool_prompt(shell).contains("truncated changed rows and an exited PTY"));
        assert!(tool_prompt(shell).contains("do not expect a later poll to resend them"));
        assert!(tool_prompt(shell).contains("These are separate lifecycles"));
        assert!(tool_prompt(shell).contains("Old unread output cannot make"));
        assert!(tool_prompt(shell).contains(
            "Conversation rewind, clearing, or context replacement does not restore or reset it"
        ));
        assert!(!tool_prompt(shell).contains("TerminalSession"));
        assert!(!tool_prompt(shell).contains("ModelContext"));
        assert!(tool_prompt(shell).contains(
            "examples progress from simple to complex and only explain how to interpret"
        ));
        assert!(tool_prompt(shell).contains("do not prescribe tool input"));
        assert!(!tool_prompt(shell).contains("Vim"));
        assert!(!tool_prompt(shell).contains(r#""input":["#));
        assert!(!tool_prompt(shell).contains(r#"`stdin`"#));
    }

    #[test]
    fn unavailable_terminal_has_no_callable_backend() {
        let mut manager = TerminalManager {
            shell: UNAVAILABLE_BACKEND.to_owned(),
            shell_kind: None,
            sessions: BTreeMap::new(),
            observer: TerminalObserver::default(),
        };
        assert!(!manager.is_available());
        assert!(tool_prompt(manager.shell_backend()).contains("No Terminal functions"));

        let workspace = temp_workspace("unavailable");
        fs::create_dir_all(&workspace).unwrap();
        let error = manager
            .create(
                &workspace,
                1,
                &CreateRequest {
                    width: DEFAULT_WIDTH,
                    height: DEFAULT_HEIGHT,
                    cwd: ".".into(),
                    wait_ms: DEFAULT_WAIT_MS,
                    max_wait_ms: DEFAULT_MAX_WAIT_MS,
                    max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
                },
            )
            .err()
            .expect("unavailable Terminal unexpectedly created a session");
        assert!(error.to_string().contains("PowerShell"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn semantic_input_actions_encode_text_keys_modifiers_and_repetition() {
        let request = interact("pty-1", Some("command"));
        assert_eq!(request.input().unwrap(), b"command\r");

        let actions = InteractRequest {
            session_id: "pty-1".into(),
            input: vec![
                text("中文\n\t"),
                key("escape"),
                text(":wq"),
                key("enter"),
                modified_key("c", &[TerminalKeyModifier::Ctrl], 1),
                modified_key("x", &[TerminalKeyModifier::Alt], 1),
                modified_key(
                    "left",
                    &[TerminalKeyModifier::Ctrl, TerminalKeyModifier::Shift],
                    3,
                ),
                modified_key("tab", &[TerminalKeyModifier::Shift], 1),
                modified_key("f5", &[TerminalKeyModifier::Alt], 1),
            ],
            wait_ms: 1_000,
            max_wait_ms: 3_000,
            max_output_chars: 20_000,
        };
        assert_eq!(
            actions.input().unwrap(),
            "中文\n\t"
                .as_bytes()
                .iter()
                .copied()
                .chain(
                    b"\x1b:wq\r\x03\x1bx\x1b[1;6D\x1b[1;6D\x1b[1;6D\x1b[Z\x1b[15;3~"
                        .iter()
                        .copied()
                )
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn semantic_named_keys_have_stable_pty_encodings() {
        let cases = [
            (key("enter"), b"\r".as_slice()),
            (key("escape"), b"\x1b".as_slice()),
            (key("backspace"), b"\x7f".as_slice()),
            (key("tab"), b"\t".as_slice()),
            (key("up"), b"\x1b[A".as_slice()),
            (key("home"), b"\x1b[H".as_slice()),
            (key("delete"), b"\x1b[3~".as_slice()),
            (key("page_down"), b"\x1b[6~".as_slice()),
            (key("f1"), b"\x1bOP".as_slice()),
            (key("f12"), b"\x1b[24~".as_slice()),
            (
                modified_key("f1", &[TerminalKeyModifier::Ctrl], 1),
                b"\x1b[1;5P".as_slice(),
            ),
            (
                modified_key("i", &[TerminalKeyModifier::Ctrl], 1),
                b"\t".as_slice(),
            ),
            (
                modified_key("[", &[TerminalKeyModifier::Ctrl], 1),
                b"\x1b".as_slice(),
            ),
        ];
        for (action, expected) in cases {
            let request = InteractRequest {
                session_id: "pty-1".into(),
                input: vec![action],
                wait_ms: 1,
                max_wait_ms: 1,
                max_output_chars: 1,
            };
            assert_eq!(request.input().unwrap(), expected);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_terminal_uses_powershell_rules() {
        let manager = TerminalManager::new();
        if manager.is_available() {
            let backend = manager.shell_backend().to_ascii_lowercase();
            assert!(backend.ends_with("powershell.exe") || backend.ends_with("pwsh.exe"));
            let prompt = tool_prompt(manager.shell_backend());
            assert!(prompt.contains("-NoLogo -NoProfile"));
            assert!(prompt.contains("PowerShell syntax"));
            assert!(!prompt.contains("Commands must use Bash syntax"));
        } else {
            assert_eq!(manager.shell_backend(), UNAVAILABLE_BACKEND);
        }
    }

    #[test]
    fn validates_terminal_arguments() {
        assert!(matches!(
            parse_request(CREATE, "{}").unwrap(),
            TerminalRequest::Create(CreateRequest {
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
                wait_ms: DEFAULT_WAIT_MS,
                max_wait_ms: DEFAULT_MAX_WAIT_MS,
                max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
                ..
            })
        ));
        assert!(parse_request(CREATE, r#"{"wait_ms":11,"max_wait_ms":10}"#).is_err());
        for legacy in [
            r#"{"session_id":"pty-1","stdin":"pwd"}"#,
            r#"{"session_id":"pty-1","enter":true}"#,
            r#"{"session_id":"pty-1","stdin_base64":"eA=="}"#,
        ] {
            assert!(parse_request(INTERACT, legacy).is_err());
        }
        assert!(
            parse_request(
                INTERACT,
                r#"{"session_id":"pty-1","wait_ms":11,"max_wait_ms":10}"#
            )
            .is_err()
        );
        let actions = parse_request(
            INTERACT,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"escape"},{"type":"text","text":":wq"},{"type":"key","key":"enter"},{"type":"key","key":"c","modifiers":["ctrl"]}]}"#,
        )
        .unwrap();
        let TerminalRequest::Interact(actions) = actions else {
            panic!("expected Terminal.Interact request");
        };
        assert_eq!(actions.input().unwrap(), b"\x1b:wq\r\x03");
        for invalid in [
            r#"{"session_id":"pty-1","input":[{"type":"text","text":"\u001b"}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"unknown"}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"c","modifiers":["ctrl","ctrl"]}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"up","repeat":0}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"up","repeat":1001}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"1","modifiers":["shift"]}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"中","modifiers":["ctrl"]}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"tab","modifiers":["ctrl"]}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"text","text":"x","key":"enter"}]}"#,
            r#"{"session_id":"pty-1","input":[{"type":"key","key":"enter","text":"x"}]}"#,
        ] {
            assert!(parse_request(INTERACT, invalid).is_err(), "{invalid}");
        }
        let too_many = serde_json::json!({
            "session_id": "pty-1",
            "input": (0..=MAX_INPUT_ACTIONS)
                .map(|_| serde_json::json!({"type":"text","text":"x"}))
                .collect::<Vec<_>>()
        });
        assert!(parse_request(INTERACT, &too_many.to_string()).is_err());
        let definitions = tool_definitions()
            .iter()
            .map(Value::to_string)
            .collect::<String>();
        assert!(definitions.contains(r#""input""#));
        assert!(definitions.contains("simultaneous key chords"));
        assert!(definitions.contains("separate actions are sequential"));
        assert!(
            !["stdin_base64", r#""stdin""#, r#""enter""#]
                .iter()
                .any(|legacy| definitions.contains(legacy))
        );
        assert!(parse_request("Terminal.Resize", r#"{"session_id":"x"}"#).is_err());
        assert!(
            tool_definitions()
                .iter()
                .all(|tool| tool["function"]["name"] != "Terminal_Resize")
        );
    }

    #[test]
    fn reader_updates_the_shared_renderer_until_eof() {
        let output = SharedTerminalOutput::new(4, 40);
        let discard = Arc::new(AtomicBool::new(false));
        read_pty(
            &mut std::io::Cursor::new(b"hello\r\nworld".to_vec()),
            &output,
            &discard,
        );
        let activity = output.activity().unwrap();
        assert!(activity.closed);
        assert_eq!(activity.output_revision, 1);
        let frame = output
            .frame(&TerminalCreated {
                session_id: "pty-test".into(),
                state: "running".into(),
                shell: "/bin/bash".into(),
                width: 40,
                height: 4,
                cwd: ".".into(),
            })
            .unwrap();
        assert_eq!(frame.rows[0].plain_text(), "hello");
        assert_eq!(frame.rows[1].plain_text(), "world");
        assert_eq!(frame.viewport, [0, 3]);
    }

    #[test]
    fn observer_lists_live_sessions_in_creation_order_and_returns_frames() {
        let observer = TerminalObserver::default();
        for (id, session_id) in [(20, "pty-20"), (10, "pty-10")] {
            let output = Arc::new(SharedTerminalOutput::new(4, 40));
            output.process(session_id.as_bytes());
            observer
                .insert(
                    id,
                    TerminalCreated {
                        session_id: session_id.into(),
                        state: "running".into(),
                        shell: "test-shell".into(),
                        width: 40,
                        height: 4,
                        cwd: ".".into(),
                    },
                    output,
                )
                .unwrap();
        }
        let active = observer.active_sessions().unwrap();
        assert_eq!(observer.active_count().unwrap(), 2);
        assert_eq!(
            active
                .iter()
                .map(|session| (session.creation_order, session.session_id.as_str()))
                .collect::<Vec<_>>(),
            vec![(10, "pty-10"), (20, "pty-20")]
        );
        let frame = observer.frame("pty-10").unwrap().unwrap();
        assert_eq!(frame.width, 40);
        assert_eq!(frame.height, 4);
        assert_eq!(frame.viewport, [0, 3]);
        assert_eq!(frame.rows[0].plain_text(), "pty-10");
        observer.remove("pty-10");
        assert!(observer.frame("pty-10").unwrap().is_none());
        assert_eq!(observer.active_count().unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_preserves_shell_state_and_supports_lifecycle() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("state");
        fs::create_dir_all(workspace.join("child")).unwrap();
        let mut manager = TerminalManager::new();
        let created = manager
            .create(
                &workspace,
                7,
                &CreateRequest {
                    width: 200,
                    height: 24,
                    cwd: ".".into(),
                    wait_ms: 1_000,
                    max_wait_ms: 3_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        assert_eq!(created.created.session_id, "pty-7");
        assert_eq!(created.update.session_id, "pty-7");
        assert_eq!(created.update.sequence, 1);
        assert!(
            !created.update.plain_text().trim().is_empty(),
            "PTY initial output was empty"
        );
        let session_id = created.created.session_id;
        #[cfg(unix)]
        assert_eq!(created.created.shell, UNIX_BASH_BACKEND);
        assert!(manager.contains(&session_id));

        let bash = manager
            .interact(&interact(
                &session_id,
                Some("printf 'BACKEND-%s\\n' BASH-$BASH_VERSION"),
            ))
            .unwrap();
        #[cfg(unix)]
        assert!(
            bash.update.plain_text().contains("BACKEND-BASH-"),
            "PTY backend was not Bash: {:?}",
            bash.update.plain_text()
        );
        let shell_variable = manager
            .interact(&interact(
                &session_id,
                Some("printf 'SHELL-BACKEND-%s\\n' \"$SHELL\""),
            ))
            .unwrap();
        #[cfg(unix)]
        assert!(
            shell_variable
                .update
                .plain_text()
                .contains("SHELL-BACKEND-/bin/bash"),
            "PTY SHELL did not describe the Bash backend: {:?}",
            shell_variable.update.plain_text()
        );

        manager
            .interact(&interact(&session_id, Some("export ME_PTY_TEST=preserved")))
            .unwrap();
        let output = manager
            .interact(&interact(
                &session_id,
                Some("printf 'STATE:%s\\n' \"$ME_PTY_TEST\""),
            ))
            .unwrap();
        assert!(
            output.update.plain_text().contains("STATE:preserved"),
            "PTY output was {:?}",
            output.update.plain_text()
        );
        assert_eq!(output.update.state, "running");

        manager
            .interact(&interact(&session_id, Some("cd child")))
            .unwrap();
        let output = manager
            .interact(&interact(&session_id, Some("pwd")))
            .unwrap();
        assert!(
            output
                .update
                .plain_text()
                .contains(workspace.join("child").to_string_lossy().as_ref()),
            "PTY output was {:?}",
            output.update.plain_text()
        );

        assert_eq!(manager.status(&session_id).unwrap().status.state, "running");

        let killed = manager
            .kill(&KillRequest {
                session_id: session_id.clone(),
                grace_ms: 1_000,
            })
            .unwrap();
        assert_eq!(killed.end.state, TerminalSessionState::Killed);
        assert!(!manager.contains(&session_id));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_semantic_actions_drive_vim_without_escaped_control_text() {
        if std::process::Command::new("vim")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("vim-semantic-input");
        fs::create_dir_all(&workspace).unwrap();
        let mut manager = TerminalManager::new();
        let created = manager
            .create(
                &workspace,
                31,
                &CreateRequest {
                    width: 120,
                    height: 40,
                    cwd: ".".into(),
                    wait_ms: 100,
                    max_wait_ms: 3_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;
        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("vim -N -u NONE -n semantic-input.txt"),
                wait_ms: 100,
                max_wait_ms: 3_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: vec![
                    text("i"),
                    text("first line\n第二行\nprintf(\"hello\\n\");"),
                    key("escape"),
                    text(":wq"),
                    key("enter"),
                ],
                wait_ms: 100,
                max_wait_ms: 3_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("semantic-input.txt")).unwrap(),
            "first line\n第二行\nprintf(\"hello\\n\");\n"
        );
        manager
            .kill(&KillRequest {
                session_id,
                grace_ms: 1_000,
            })
            .unwrap();
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn observer_receives_delayed_output_without_another_tool_read() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("observer-live");
        fs::create_dir_all(&workspace).unwrap();
        let mut manager = TerminalManager::new();
        let observer = manager.observer();
        let created = manager
            .create(
                &workspace,
                37,
                &CreateRequest {
                    width: 120,
                    height: 24,
                    cwd: ".".into(),
                    wait_ms: 100,
                    max_wait_ms: 1_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;
        let mut delayed = interact(
            &session_id,
            Some(
                "(sleep 0.3; printf '\\n\\117\\102\\123\\105\\122\\126\\105\\122\\055\\114\\111\\126\\105\\n') &",
            ),
        );
        delayed.wait_ms = 50;
        delayed.max_wait_ms = 50;
        manager.interact(&delayed).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let frame = observer.frame(&session_id).unwrap().unwrap();
            let text = frame
                .rows
                .iter()
                .map(TerminalRowUpdate::plain_text)
                .collect::<Vec<_>>()
                .join("\n");
            if text.contains("OBSERVER-LIVE") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "observer did not receive delayed PTY output: {text:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        manager
            .kill(&KillRequest {
                session_id,
                grace_ms: 1_000,
            })
            .unwrap();
        assert_eq!(observer.active_count().unwrap(), 0);
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn real_powershell_pty_answers_cursor_handshake_and_preserves_lifecycle() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("powershell-state");
        fs::create_dir_all(workspace.join("child")).unwrap();
        let mut manager = TerminalManager::new();
        assert!(
            manager.is_available(),
            "Windows Terminal tests require powershell.exe or pwsh.exe"
        );
        let created = manager
            .create(
                &workspace,
                7,
                &CreateRequest {
                    width: 200,
                    height: 24,
                    cwd: ".".into(),
                    wait_ms: 1_000,
                    max_wait_ms: 3_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;
        let shell = created.created.shell.to_ascii_lowercase();
        assert!(shell.ends_with("powershell.exe") || shell.ends_with("pwsh.exe"));
        assert!(manager.contains(&session_id));

        manager
            .interact(&interact(
                &session_id,
                Some("$env:ME_PTY_TEST = 'preserved'"),
            ))
            .unwrap();
        let state = manager
            .interact(&interact(
                &session_id,
                Some("Write-Output \"STATE:$env:ME_PTY_TEST\""),
            ))
            .unwrap();
        assert!(
            state.update.plain_text().contains("STATE:preserved"),
            "PowerShell state was not preserved: {:?}",
            state.update.plain_text()
        );

        let waiting_for_enter = manager
            .interact(&interact(
                &session_id,
                Some(
                    "$key = [Console]::ReadKey($true); Start-Sleep -Milliseconds 100; \
                     $extra = [Console]::KeyAvailable; \
                     Write-Output (\"READKEY:{0}:{1}:EXTRA={2}\" -f $key.Key, [int]$key.KeyChar, $extra); \
                     if ($extra) { $null = [Console]::ReadKey($true) }",
                ),
            ))
            .unwrap();
        assert!(
            !waiting_for_enter
                .update
                .plain_text()
                .contains("READKEY:Enter:13:EXTRA="),
            "the command-terminating Enter leaked into ReadKey: {:?}",
            waiting_for_enter.update.plain_text()
        );
        let enter_key = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: vec![key("enter")],
                wait_ms: 250,
                max_wait_ms: 3_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        assert!(
            enter_key
                .update
                .plain_text()
                .matches("READKEY:Enter:13:EXTRA=False")
                .count()
                == 1,
            "logical Enter did not become exactly one VK_RETURN/CR event: {:?}",
            enter_key.update.plain_text()
        );

        manager
            .interact(&interact(&session_id, Some("cmd.exe /Q")))
            .unwrap();
        let cmd = manager
            .interact(&interact(&session_id, Some("@echo CMD-RETURN-WORKED")))
            .unwrap();
        assert!(
            cmd.update.plain_text().contains("CMD-RETURN-WORKED"),
            "logical Enter did not submit a nested cmd.exe command: {:?}",
            cmd.update.plain_text()
        );
        manager
            .interact(&interact(&session_id, Some("exit")))
            .unwrap();
        let after_cmd = manager
            .interact(&interact(
                &session_id,
                Some("Write-Output 'POWERSHELL-AFTER-CMD'"),
            ))
            .unwrap();
        assert!(
            after_cmd
                .update
                .plain_text()
                .contains("POWERSHELL-AFTER-CMD"),
            "PowerShell did not recover after nested cmd.exe exited: {:?}",
            after_cmd.update.plain_text()
        );
        assert!(
            !after_cmd.update.plain_text().contains("\n>> "),
            "PowerShell entered continuation mode after nested cmd.exe: {:?}",
            after_cmd.update.plain_text()
        );

        manager
            .interact(&interact(&session_id, Some("Set-Location child")))
            .unwrap();
        let location = manager
            .interact(&interact(
                &session_id,
                Some("Write-Output (Get-Location).Path"),
            ))
            .unwrap();
        assert!(
            location
                .update
                .plain_text()
                .contains(workspace.join("child").to_string_lossy().as_ref()),
            "PowerShell cwd was not preserved: {:?}",
            location.update.plain_text()
        );

        let kill_started = Instant::now();
        let killed = manager
            .kill(&KillRequest {
                session_id,
                grace_ms: 1_000,
            })
            .unwrap();
        assert_eq!(killed.end.state, TerminalSessionState::Killed);
        assert!(
            kill_started.elapsed() < Duration::from_secs(8),
            "Windows ConPTY shutdown exceeded the kill bound"
        );
        drop(manager);
        fs::remove_dir_all(workspace).unwrap();
    }

    fn complete_update(rendered: RenderedOutput) -> TerminalLineUpdate {
        TerminalLineUpdate {
            session_id: "pty-test".into(),
            sequence: 7,
            width: 20,
            height: 4,
            viewport: rendered.viewport,
            style_count: rendered.style_count,
            style_defs: rendered.style_defs,
            rows: rendered.rows,
            cursor: rendered.cursor,
            state: "running".into(),
            exit_code: None,
            truncated: rendered.truncated,
        }
    }

    #[test]
    fn renderer_reports_only_final_rows_against_the_previous_call() {
        let mut renderer = TerminalRenderer::new(4, 20);
        renderer.process(b"progress 10%");
        let first = renderer.capture(1_000);
        assert_eq!(first.rows.len(), 1);
        assert_eq!(first.rows[0].row, 0);
        assert_eq!(first.rows[0].plain_text(), "progress 10%");

        renderer.process(b"\r\x1b[2Kprogress 20%\r\x1b[2Kprogress 10%");
        let returned_to_baseline = renderer.capture(1_000);
        assert!(returned_to_baseline.rows.is_empty());

        renderer.process(b"\r\x1b[2Kprogress 30%");
        let final_change = renderer.capture(1_000);
        assert_eq!(final_change.rows.len(), 1);
        assert_eq!(final_change.rows[0].plain_text(), "progress 30%");
    }

    #[test]
    fn read_only_frame_does_not_advance_the_model_patch_baseline() {
        let mut renderer = TerminalRenderer::new(4, 40);
        renderer.process(b"\x1b[31mred\x1b[0m");
        let frame = renderer.frame(
            &TerminalCreated {
                session_id: "pty-1".into(),
                state: "running".into(),
                shell: "test-shell".into(),
                width: 40,
                height: 4,
                cwd: ".".into(),
            },
            7,
        );
        assert_eq!(frame.revision, 7);
        assert_eq!(frame.rows[0].plain_text(), "red");
        assert_eq!(frame.style_defs.len(), 1);

        let first_model_read = renderer.capture(20_000);
        assert_eq!(first_model_read.plain_text(), "red");
        assert_eq!(first_model_read.style_defs.len(), 1);
        assert!(renderer.capture(20_000).rows.is_empty());
    }

    #[test]
    fn read_only_frame_contains_primary_scrollback_and_current_viewport() {
        let mut renderer = TerminalRenderer::new(2, 20);
        renderer.process(b"one\r\ntwo\r\nthree\r\nfour");
        let frame = renderer.frame(
            &TerminalCreated {
                session_id: "pty-history".into(),
                state: "running".into(),
                shell: "test-shell".into(),
                width: 20,
                height: 2,
                cwd: ".".into(),
            },
            9,
        );
        assert_eq!(frame.viewport, [2, 3]);
        assert_eq!(
            frame
                .rows
                .iter()
                .map(TerminalRowUpdate::plain_text)
                .collect::<Vec<_>>(),
            vec!["one", "two", "three", "four"]
        );
        assert_eq!(frame.cursor.row, 3);

        let patch = renderer.capture(20_000);
        assert_eq!(patch.viewport, [2, 3]);
        assert_eq!(patch.plain_text(), "one\ntwo\nthree\nfour");
    }

    #[test]
    fn renderer_treats_style_and_empty_clears_as_real_row_changes() {
        let mut renderer = TerminalRenderer::new(4, 20);
        renderer.process(b"\x1b[31mword\x1b[0m");
        let red = renderer.capture(1_000);
        let red_style = red.rows[0].runs[0].style;
        assert_eq!(
            renderer.styles.styles[usize::try_from(red_style).unwrap()].foreground,
            Some(TerminalColor::Indexed(1))
        );

        renderer.process(b"\r\x1b[2K\x1b[31mWORD\x1b[0m");
        let same_style = renderer.capture(1_000);
        assert_eq!(same_style.style_defs.len(), 1);
        assert_eq!(same_style.style_defs[0].id, red_style);
        assert_eq!(same_style.rows[0].runs[0].style, red_style);

        renderer.process(b"\r\x1b[2K\x1b[32mWORD\x1b[0m");
        let green = renderer.capture(1_000);
        assert_eq!(green.rows.len(), 1);
        assert_ne!(green.rows[0].runs[0].style, red_style);
        assert_eq!(green.style_defs.len(), 1);

        renderer.process(b"\r\x1b[2K");
        let cleared = renderer.capture(1_000);
        assert_eq!(cleared.rows.len(), 1);
        assert_eq!(cleared.rows[0].row, 0);
        assert!(cleared.rows[0].runs.is_empty());
    }

    #[test]
    fn renderer_uses_permanent_absolute_rows_while_the_viewport_scrolls() {
        let mut renderer = TerminalRenderer::new(3, 20);
        renderer.process(b"line-0\r\nline-1\r\nline-2\r\nline-3");
        let first = renderer.capture(1_000);
        assert_eq!(first.viewport, [1, 3]);
        assert_eq!(
            first.rows.iter().map(|row| row.row).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(first.rows[0].plain_text(), "line-0");
        assert_eq!(first.rows[3].plain_text(), "line-3");

        renderer.process(b"\r\nline-4");
        let second = renderer.capture(1_000);
        assert_eq!(second.viewport, [2, 4]);
        assert_eq!(
            second.rows.iter().map(|row| row.row).collect::<Vec<_>>(),
            vec![4]
        );
        assert_eq!(second.rows[0].plain_text(), "line-4");
    }

    #[test]
    fn renderer_always_reports_cursor_even_without_changed_rows() {
        let mut renderer = TerminalRenderer::new(4, 20);
        renderer.process(b"abc");
        renderer.capture(1_000);
        renderer.process(b"\x1b[2D");
        let cursor_only = renderer.capture(1_000);
        assert!(cursor_only.rows.is_empty());
        assert_eq!((cursor_only.cursor.row, cursor_only.cursor.col), (0, 1));
        assert_eq!(cursor_only.cursor.underlying, "b");
        assert!(cursor_only.cursor.visible);
    }

    #[test]
    fn alternate_screen_is_internal_and_only_final_active_rows_are_reported() {
        let mut renderer = TerminalRenderer::new(4, 20);
        renderer.process(b"shell");
        renderer.capture(1_000);

        renderer.process(b"\x1b[?1049h\x1b[2J\x1b[Hmenu\r\nitem");
        let menu = renderer.capture(1_000);
        assert_eq!(menu.viewport, [0, 3]);
        assert_eq!(menu.rows[0].plain_text(), "menu");
        assert_eq!(menu.rows[1].plain_text(), "item");

        renderer.process(b"\x1b[?1049l");
        let restored = renderer.capture(1_000);
        assert_eq!(restored.rows[0].plain_text(), "shell");
        assert!(restored.rows[1].runs.is_empty());

        renderer.process(b"\x1b[?1049htransient\x1b[?1049l");
        let transient = renderer.capture(1_000);
        assert!(transient.rows.is_empty());
    }

    #[test]
    fn renderer_preserves_colors_attributes_wide_cells_and_wrapping() {
        let mut renderer = TerminalRenderer::new(4, 5);
        renderer.process(
            "\x1b[38;2;10;20;30;1mA\x1b[0m\
             \x1b[48;5;42;2mB\x1b[0m\
             \x1b[3;4;7m你\x1b[0mZQ"
                .as_bytes(),
        );
        renderer.process(b"\x1b[1;3H");
        let output = renderer.capture(10_000);
        assert_eq!(output.rows.len(), 2);
        assert!(output.rows[0].wrapped);
        assert_eq!(output.rows[0].plain_text(), "AB你Z");
        assert_eq!(output.rows[1].plain_text(), "Q");
        assert_eq!(output.cursor.underlying, "你");
        assert!(output.cursor.wide);

        let row = &output.rows[0];
        let style = |text: &str| {
            let run = row.runs.iter().find(|run| run.text == text).unwrap();
            &renderer.styles.styles[usize::try_from(run.style).unwrap()]
        };
        assert_eq!(
            style("A").foreground,
            Some(TerminalColor::Rgb([10, 20, 30]))
        );
        assert!(style("A").bold);
        assert_eq!(style("B").background, Some(TerminalColor::Indexed(42)));
        assert!(style("B").dim);
        assert!(style("你").italic);
        assert!(style("你").underline);
        assert!(style("你").inverse);
    }

    #[test]
    fn typed_update_round_trips_and_projects_to_structured_line_json() {
        let mut renderer = TerminalRenderer::new(4, 20);
        renderer.process(b"\x1b[31;1mred\x1b[0m");
        let update = complete_update(renderer.capture(10_000));
        update.validate().unwrap();

        let encoded = serde_json::to_string(&update).unwrap();
        let decoded: TerminalLineUpdate = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, update);

        let model = update.model_value();
        assert_eq!(model["type"], "terminal_patch");
        assert_eq!(model["version"], 2);
        assert_eq!(model["terminal_size"], json!({"columns": 20, "rows": 4}));
        assert_eq!(
            model["viewport"],
            json!({"first_terminal_row": 0, "last_terminal_row": 3})
        );
        assert_eq!(
            model["styles"],
            json!([{"id": 1, "foreground": "indexed(1)", "attributes": ["bold"]}])
        );
        assert_eq!(model["rows"][0]["terminal_row"], 0);
        assert_eq!(model["rows"][0]["text"], "red");
        assert_eq!(
            model["rows"][0]["style_spans"],
            json!([{"start_column": 0, "width": 3, "style": 1}])
        );
        assert_eq!(model["cursor"]["terminal_row"], 0);
        assert_eq!(model["cursor"]["column"], 3);
        assert_eq!(model["cursor"]["underlying"], "");
        let serialized = serde_json::to_string(&model).unwrap();
        assert!(!serialized.contains("\"runs\""));
        assert!(!serialized.contains("\"cells\""));
        assert!(!serialized.contains("base_event_id"));
    }

    #[test]
    fn model_json_separates_terminal_coordinates_from_literal_text_and_styles() {
        let mut styled = test_update("");
        styled.width = 20;
        styled.height = 2;
        styled.viewport = [99, 100];
        styled.style_count = 2;
        styled.style_defs = vec![TerminalStyleDefinition {
            id: 1,
            style: TerminalStyle {
                foreground: Some(TerminalColor::Rgb([1, 2, 3])),
                background: Some(TerminalColor::Indexed(4)),
                underline: true,
                inverse: true,
                ..TerminalStyle::default()
            },
        }];
        styled.rows = vec![
            TerminalRowUpdate {
                row: 99,
                wrapped: true,
                runs: vec![
                    TerminalRowRun {
                        col: 2,
                        width: 5,
                        text: "<red>".into(),
                        style: 1,
                    },
                    TerminalRowRun {
                        col: 8,
                        width: 3,
                        text: "<0>".into(),
                        style: 0,
                    },
                ],
            },
            TerminalRowUpdate {
                row: 100,
                wrapped: false,
                runs: Vec::new(),
            },
        ];
        styled.cursor = TerminalCursor {
            row: 100,
            col: 3,
            visible: false,
            underlying: "<".into(),
            wide: true,
            wide_continuation: true,
        };
        styled.validate().unwrap();
        let model = styled.model_value();
        assert_eq!(
            model["styles"],
            json!([{
                "id": 1,
                "foreground": "#010203",
                "background": "indexed(4)",
                "attributes": ["underline", "inverse"]
            }])
        );
        assert_eq!(
            model["rows"][0],
            json!({
                "terminal_row": 99,
                "text": "  <red> <0>",
                "wrapped": true,
                "style_spans": [{"start_column": 2, "width": 5, "style": 1}]
            })
        );
        assert_eq!(model["rows"][1], json!({"terminal_row": 100, "text": ""}));
        assert_eq!(
            model["cursor"],
            json!({
                "terminal_row": 100,
                "column": 3,
                "visible": false,
                "width": 2,
                "wide_continuation": true,
                "underlying": "<"
            })
        );

        let mut cursor_only = styled;
        cursor_only.rows.clear();
        cursor_only.style_defs.clear();
        cursor_only.validate().unwrap();
        let cursor_only = cursor_only.model_value();
        assert_eq!(cursor_only["rows"], json!([]));
        assert_eq!(cursor_only["styles"], json!([]));
    }

    #[test]
    fn model_json_preserves_coordinate_like_terminal_text_without_ambiguity() {
        let update = test_update("25: source line\n{\"terminal_row\":999}");
        let model = update.model_value();
        assert_eq!(model["rows"][0]["terminal_row"], 0);
        assert_eq!(model["rows"][0]["text"], "25: source line");
        assert_eq!(model["rows"][1]["terminal_row"], 1);
        assert_eq!(model["rows"][1]["text"], r#"{"terminal_row":999}"#);
    }

    #[test]
    fn terminal_patch_requires_exactly_the_non_default_styles_it_references() {
        let mut update = test_update("styled");
        update.style_count = 2;
        update.rows[0].runs[0].style = 1;
        assert!(update.validate().is_err());

        update.style_defs.push(TerminalStyleDefinition {
            id: 1,
            style: TerminalStyle {
                bold: true,
                ..TerminalStyle::default()
            },
        });
        update.validate().unwrap();

        update.rows[0].runs[0].style = 0;
        assert!(update.validate().is_err());
    }

    #[test]
    fn renderer_handles_utf8_and_controls_split_across_reads() {
        let mut renderer = TerminalRenderer::new(4, 20);
        renderer.process(&[0xe4, 0xbd]);
        assert!(renderer.capture(1_000).rows.is_empty());
        renderer.process(&[0xa0]);
        assert_eq!(renderer.capture(1_000).plain_text(), "你");

        renderer.process(b"\x1b[3");
        assert!(renderer.capture(1_000).rows.is_empty());
        renderer.process(b"1mred\x1b[0m");
        let styled = renderer.capture(1_000);
        assert_eq!(styled.rows[0].plain_text(), "你red");
        assert!(!styled.plain_text().contains('\x1b'));

        renderer.process(b"\x1b]0;split title");
        assert!(renderer.capture(1_000).rows.is_empty());
        renderer.process(b"\x07tail");
        assert_eq!(renderer.capture(1_000).plain_text(), "你redtail");
    }

    #[test]
    fn renderer_truncates_only_whole_changed_rows_and_advances_its_baseline() {
        let mut renderer = TerminalRenderer::new(4, 20);
        renderer.process(b"one\r\ntwo\r\nthree");
        let truncated = renderer.capture(7);
        assert!(truncated.truncated);
        assert_eq!(truncated.rows.len(), 1);
        assert_eq!(truncated.rows[0].row, 2);
        assert_eq!(truncated.rows[0].plain_text(), "three");

        let unchanged = renderer.capture(1_000);
        assert!(unchanged.rows.is_empty());
        assert!(!unchanged.truncated);

        renderer.process(b"!");
        let next = renderer.capture(1_000);
        assert_eq!(next.rows.len(), 1);
        assert_eq!(next.rows[0].plain_text(), "three!");
    }

    #[test]
    fn renderer_keeps_a_large_scrollback_with_monotonic_row_numbers() {
        let mut renderer = TerminalRenderer::new(4, 40);
        for line in 0..2_000 {
            renderer.process(format!("scrollback-{line}\r\n").as_bytes());
        }
        let output = renderer.capture(100_000);
        assert!(output.rows.len() >= 2_000);
        assert!(output.plain_text().contains("scrollback-0"));
        assert!(output.plain_text().contains("scrollback-1999"));
        assert!(output.viewport[0] >= 1_997);
        assert!(output.rows.windows(2).all(|rows| rows[0].row < rows[1].row));
    }

    #[test]
    fn renderer_survives_fragmented_arbitrary_terminal_bytes() {
        let mut renderer = TerminalRenderer::new(12, 40);
        let mut random = 0x9e37_79b9_u32;
        for index in 0..4_096 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            renderer.process(&[random as u8]);
            if index % 97 == 0 {
                let output = renderer.capture(5_000);
                assert!(!output.plain_text().contains('\x1b'));
                let update = TerminalLineUpdate {
                    session_id: "pty-fuzz".into(),
                    sequence: u64::try_from(index).unwrap(),
                    width: 40,
                    height: 12,
                    viewport: output.viewport,
                    style_count: output.style_count,
                    style_defs: output.style_defs,
                    rows: output.rows,
                    cursor: output.cursor,
                    state: "running".into(),
                    exit_code: None,
                    truncated: output.truncated,
                };
                update.validate().unwrap();
            }
        }
        assert!(!renderer.capture(5_000).plain_text().contains('\x1b'));
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_drains_a_large_burst_before_reporting_exit() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("burst-exit");
        fs::create_dir_all(&workspace).unwrap();
        let mut manager = TerminalManager::new();
        let created = manager
            .create(
                &workspace,
                8,
                &CreateRequest {
                    width: 120,
                    height: 500,
                    cwd: ".".into(),
                    wait_ms: 25,
                    max_wait_ms: 3_000,
                    max_output_chars: 200_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;
        let command = "exec /bin/sh -c 'i=0; while [ $i -lt 12000 ]; do printf \"BURST-%05d\\n\" \"$i\"; i=$((i+1)); done; printf \"FINAL-TAIL-MARKER\\n\"'";
        let outcome = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input(command),
                wait_ms: 100,
                max_wait_ms: 5_000,
                max_output_chars: 200_000,
            })
            .unwrap();

        assert_eq!(outcome.update.state, "exited");
        assert_eq!(
            outcome.end.as_ref().map(|end| end.state),
            Some(TerminalSessionState::Exited)
        );
        assert!(
            outcome.update.plain_text().contains("FINAL-TAIL-MARKER"),
            "final PTY output was lost: {:?}",
            outcome.update.plain_text()
        );
        assert!(!outcome.update.plain_text().contains('\x1b'));
        assert!(!manager.contains(&session_id));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_supports_delayed_polling_and_an_interactive_program() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("poll-interactive");
        fs::create_dir_all(&workspace).unwrap();
        let mut manager = TerminalManager::new();
        let created = manager
            .create(
                &workspace,
                9,
                &CreateRequest {
                    width: 120,
                    height: 40,
                    cwd: ".".into(),
                    wait_ms: 25,
                    max_wait_ms: 3_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;

        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("exec /bin/sh"),
                wait_ms: 25,
                max_wait_ms: 1_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        let early = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("sleep 0.15; printf 'LATE-%s\\n' MARKER"),
                wait_ms: 20,
                max_wait_ms: 50,
                max_output_chars: 20_000,
            })
            .unwrap();
        assert!(!early.update.plain_text().contains("LATE-MARKER"));

        let poll_deadline = Instant::now() + Duration::from_secs(2);
        let mut late_content = String::new();
        while Instant::now() < poll_deadline && !late_content.contains("LATE-MARKER") {
            let late = manager
                .interact(&InteractRequest {
                    session_id: session_id.clone(),
                    input: Vec::new(),
                    wait_ms: 100,
                    max_wait_ms: 500,
                    max_output_chars: 20_000,
                })
                .unwrap();
            late_content.push_str(&late.update.plain_text());
        }
        assert!(
            late_content.contains("LATE-MARKER"),
            "delayed PTY output was not available to polling: {:?}",
            late_content
        );

        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("cat"),
                wait_ms: 25,
                max_wait_ms: 500,
                max_output_chars: 20_000,
            })
            .unwrap();
        let echoed = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("INTERACTIVE-ROUNDTRIP"),
                wait_ms: 50,
                max_wait_ms: 1_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        assert!(echoed.update.plain_text().contains("INTERACTIVE-ROUNDTRIP"));

        let mut end_cat = InteractRequest {
            session_id: session_id.clone(),
            input: vec![modified_key("d", &[TerminalKeyModifier::Ctrl], 1)],
            wait_ms: 50,
            max_wait_ms: 1_000,
            max_output_chars: 20_000,
        };
        manager.interact(&end_cat).unwrap();
        end_cat.input = command_input("printf 'AFTER-CAT\\n'");
        let after_cat = manager.interact(&end_cat).unwrap();
        assert!(after_cat.update.plain_text().contains("AFTER-CAT"));

        end_cat.input =
            command_input("printf 'MERGED-%s\\n' STDOUT; printf 'MERGED-%s\\n' STDERR >&2");
        let merged = manager.interact(&end_cat).unwrap();
        assert!(merged.update.plain_text().contains("MERGED-STDOUT"));
        assert!(merged.update.plain_text().contains("MERGED-STDERR"));

        manager
            .kill(&KillRequest {
                session_id,
                grace_ms: 1_000,
            })
            .unwrap();
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_wait_distinguishes_new_input_from_aged_pending_output() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("aged-output-wait");
        fs::create_dir_all(&workspace).unwrap();
        let mut manager = TerminalManager::new();
        let created = manager
            .create(
                &workspace,
                41,
                &CreateRequest {
                    width: 120,
                    height: 40,
                    cwd: ".".into(),
                    wait_ms: 25,
                    max_wait_ms: 3_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;

        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("stty -echo"),
                wait_ms: 25,
                max_wait_ms: 1_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("(sleep 0.08; printf 'AGED-PENDING\\n') &"),
                wait_ms: 10,
                max_wait_ms: 20,
                max_output_chars: 20_000,
            })
            .unwrap();
        thread::sleep(Duration::from_millis(250));

        let nonempty_started = Instant::now();
        let after_new_input = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("sleep 0.06; printf 'FRESH-AFTER-INPUT\\n'"),
                wait_ms: 100,
                max_wait_ms: 1_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        let nonempty_elapsed = nonempty_started.elapsed();
        let nonempty_text = after_new_input.update.plain_text();
        assert!(
            nonempty_text.contains("AGED-PENDING"),
            "aged pending output was not returned: {nonempty_text:?}"
        );
        assert!(
            nonempty_text.contains("FRESH-AFTER-INPUT"),
            "old output incorrectly satisfied the new input wait after {nonempty_elapsed:?}: {nonempty_text:?}"
        );
        assert!(
            nonempty_elapsed >= Duration::from_millis(120),
            "non-empty input returned before its own output became idle: {nonempty_elapsed:?}"
        );

        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("(sleep 0.08; printf 'AGED-FOR-POLL\\n') &"),
                wait_ms: 10,
                max_wait_ms: 20,
                max_output_chars: 20_000,
            })
            .unwrap();
        thread::sleep(Duration::from_millis(250));

        let poll_started = Instant::now();
        let polled = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: Vec::new(),
                wait_ms: 250,
                max_wait_ms: 1_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        let poll_elapsed = poll_started.elapsed();
        assert!(
            polled.update.plain_text().contains("AGED-FOR-POLL"),
            "empty polling did not return pending output: {:?}",
            polled.update.plain_text()
        );
        assert!(
            poll_elapsed < Duration::from_millis(180),
            "empty polling restarted the idle timer for aged output: {poll_elapsed:?}"
        );

        manager
            .kill(&KillRequest {
                session_id,
                grace_ms: 1_000,
            })
            .unwrap();
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_honors_the_absolute_wait_limit_for_continuous_output() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("continuous");
        fs::create_dir_all(&workspace).unwrap();
        let mut manager = TerminalManager::new();
        let created = manager
            .create(
                &workspace,
                10,
                &CreateRequest {
                    width: 120,
                    height: 40,
                    cwd: ".".into(),
                    wait_ms: 25,
                    max_wait_ms: 3_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;
        manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("exec /bin/sh"),
                wait_ms: 25,
                max_wait_ms: 1_000,
                max_output_chars: 20_000,
            })
            .unwrap();

        let start = Instant::now();
        let partial = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input(
                    "i=0; while [ $i -lt 20 ]; do printf 'tick-%s\\n' \"$i\"; sleep 0.03; i=$((i+1)); done; printf 'CONTINUOUS-%s\\n' DONE"
                ),
                wait_ms: 100,
                max_wait_ms: 150,
                max_output_chars: 20_000,
            })
            .unwrap();
        assert!(start.elapsed() < Duration::from_millis(600));
        assert_eq!(partial.update.state, "running");
        assert!(!partial.update.plain_text().contains("CONTINUOUS-DONE"));

        let poll_deadline = Instant::now() + Duration::from_secs(6);
        let mut completed_content = String::new();
        while Instant::now() < poll_deadline && !completed_content.contains("CONTINUOUS-DONE") {
            let completed = manager
                .interact(&InteractRequest {
                    session_id: session_id.clone(),
                    input: Vec::new(),
                    wait_ms: 100,
                    max_wait_ms: 1_000,
                    max_output_chars: 20_000,
                })
                .unwrap();
            completed_content.push_str(&completed.update.plain_text());
        }
        assert!(
            completed_content.contains("CONTINUOUS-DONE"),
            "continuous output tail was not returned by polling: {:?}",
            completed_content
        );

        manager
            .kill(&KillRequest {
                session_id,
                grace_ms: 1_000,
            })
            .unwrap();
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_does_not_write_after_the_shell_has_already_exited() {
        let _guard = real_pty_test_guard();
        let workspace = temp_workspace("exit-before-write");
        fs::create_dir_all(&workspace).unwrap();
        let mut manager = TerminalManager::new();
        let created = manager
            .create(
                &workspace,
                11,
                &CreateRequest {
                    width: 120,
                    height: 40,
                    cwd: ".".into(),
                    wait_ms: 25,
                    max_wait_ms: 3_000,
                    max_output_chars: 20_000,
                },
            )
            .unwrap();
        let session_id = created.created.session_id;
        let scheduled_exit = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("exec /bin/sh -c 'sleep 0.2'"),
                wait_ms: 0,
                max_wait_ms: 1,
                max_output_chars: 20_000,
            })
            .unwrap();
        assert_eq!(scheduled_exit.update.state, "running");
        thread::sleep(Duration::from_millis(500));

        let observed = manager
            .interact(&InteractRequest {
                session_id: session_id.clone(),
                input: command_input("printf 'MUST-NOT-RUN\\n'"),
                wait_ms: 25,
                max_wait_ms: 1_000,
                max_output_chars: 20_000,
            })
            .unwrap();
        assert_eq!(observed.update.state, "exited");
        assert_eq!(
            observed.end.as_ref().map(|end| end.state),
            Some(TerminalSessionState::Exited)
        );
        assert!(!observed.update.plain_text().contains("MUST-NOT-RUN"));
        assert!(!manager.contains(&session_id));
        fs::remove_dir_all(workspace).unwrap();
    }
}
