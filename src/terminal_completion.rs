use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

const PREFIX: &[u8] = b"\x1b]633;D;";
const MAX_NOTIFICATION_BYTES: usize = 160;

#[derive(Clone, Debug)]
pub(super) struct Completion {
    pub revision: u64,
    pub received_at: Instant,
    pub exit_code: Option<i32>,
    pub succeeded: bool,
    pub ready: bool,
}

pub(super) struct CompletionDecoder {
    nonce: String,
    pending: Vec<u8>,
    revision: u64,
}

impl CompletionDecoder {
    pub fn new(nonce: String) -> Self {
        Self {
            nonce,
            pending: Vec::new(),
            revision: 0,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) -> (Vec<u8>, Option<Completion>) {
        self.pending.extend_from_slice(bytes);
        let mut visible = Vec::with_capacity(self.pending.len());
        let mut completed = None;
        loop {
            let Some(start) = self
                .pending
                .windows(PREFIX.len())
                .position(|part| part == PREFIX)
            else {
                let keep = (1..PREFIX.len())
                    .rev()
                    .find(|&length| self.pending.ends_with(&PREFIX[..length]))
                    .unwrap_or(0);
                visible.extend(self.pending.drain(..self.pending.len() - keep));
                break;
            };
            visible.extend(self.pending.drain(..start));
            let bell = self
                .pending
                .iter()
                .position(|&byte| byte == 7)
                .map(|index| (index, 1));
            let st = self
                .pending
                .windows(2)
                .position(|part| part == b"\x1b\\")
                .map(|index| (index, 2));
            let end = bell.into_iter().chain(st).min_by_key(|&(index, _)| index);
            let Some((end, terminator)) = end else {
                if self.pending.len() > MAX_NOTIFICATION_BYTES {
                    visible.push(self.pending.remove(0));
                    continue;
                }
                break;
            };
            let message = self.pending[..end + terminator].to_vec();
            if let Some(notification) = self.decode(&message[PREFIX.len()..end]) {
                completed = Some(notification);
            } else {
                visible.extend_from_slice(&message);
            }
            self.pending.drain(..end + terminator);
        }
        (visible, completed)
    }

    fn decode(&mut self, payload: &[u8]) -> Option<Completion> {
        let text = std::str::from_utf8(payload).ok()?;
        let fields = text.split(';').collect::<Vec<_>>();
        if fields.len() != 4 || fields[1] != self.nonce {
            return None;
        }
        let exit_code = if fields[0] == "_" {
            None
        } else {
            Some(fields[0].parse::<i32>().ok()?)
        };
        let ready = match fields[2] {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        let succeeded = match fields[3] {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        if exit_code.is_some_and(|code| (code == 0) != succeeded) {
            return None;
        }
        self.revision += 1;
        Some(Completion {
            revision: self.revision,
            received_at: Instant::now(),
            exit_code,
            succeeded,
            ready,
        })
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WaitReason {
    ShellReady,
    CommandCompleted,
    OutputIdle,
    MaxWait,
    FollowUp,
    SessionEnded,
}

#[derive(Debug)]
pub(super) struct WaitOutcome {
    pub reason: WaitReason,
    pub command_exit_code: Option<i32>,
    pub command_succeeded: Option<bool>,
}

impl WaitOutcome {
    pub fn new(reason: WaitReason) -> Self {
        Self {
            reason,
            command_exit_code: None,
            command_succeeded: None,
        }
    }

    pub fn completed(completion: &Completion, initial: bool) -> Self {
        if initial {
            Self::new(WaitReason::ShellReady)
        } else {
            Self {
                reason: WaitReason::CommandCompleted,
                command_exit_code: completion.exit_code,
                command_succeeded: Some(completion.succeeded),
            }
        }
    }

    pub fn metadata(&self, mut value: Value) -> Value {
        value["wait_reason"] =
            serde_json::to_value(self.reason).expect("wait reason is serializable");
        value["command_exit_code"] = self.command_exit_code.into();
        value["command_succeeded"] = self.command_succeeded.into();
        if self.reason == WaitReason::FollowUp {
            value["tip"] = "Waiting ended early because a user follow-up arrived; the terminal command was not stopped.".into();
        }
        value
    }
}

#[cfg(any(unix, test))]
pub(super) fn bash_prompt_command(nonce: &str) -> String {
    // Keep the hook in this shell only; a nested Bash must not report its prompt as our completion.
    format!(
        r#"__me_status=$?; export -n PROMPT_COMMAND; __me_ready=1; if builtin read -t 0; then __me_ready=0; fi; if [[ -n "$(builtin jobs -p)" ]]; then __me_ready=0; fi; builtin printf '\033]633;D;%s;{nonce};%s;%s\007' "$__me_status" "$__me_ready" "$((__me_status == 0))"; (exit "$__me_status")"#
    )
}

// Use inline initialization, not a profile or a change to the user's execution policy.
#[cfg(any(windows, test))]
pub(super) fn powershell_initialization(nonce: &str) -> String {
    format!(
        r#"$global:__me_original_prompt = $function:prompt
$global:__me_history_id = -1
function global:prompt {{
    $__me_ok = $?
    $__me_native = $global:LASTEXITCODE
    try {{
        $__me_history = Get-History -Count 1 -ErrorAction Stop
        $__me_id = if ($null -eq $__me_history) {{ 0 }} else {{ $__me_history.Id }}
        if ($NestedPromptLevel -eq 0 -and $__me_id -ne $global:__me_history_id) {{
            $__me_ready = 1
            if ([Console]::KeyAvailable) {{ $__me_ready = 0 }}
            if (@(Get-Job -ErrorAction SilentlyContinue | Where-Object {{ $_.State -notin @('Completed', 'Failed', 'Stopped') }}).Count -gt 0) {{ $__me_ready = 0 }}
            [Console]::Write(([char]27) + ']633;D;_;{nonce};' + $__me_ready + ';' + [int]$__me_ok + [char]7)
            $global:__me_history_id = $__me_id
        }}
    }} catch {{
    }} finally {{
        $global:LASTEXITCODE = $__me_native
    }}
    & $global:__me_original_prompt
}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_is_removed_at_every_possible_chunk_boundary() {
        let marker = b"\x1b]633;D;7;abc;1;0\x07";
        for split in 0..=marker.len() {
            let mut decoder = CompletionDecoder::new("abc".into());
            let (mut visible, first) =
                decoder.process(&[b"before".as_slice(), &marker[..split]].concat());
            let (tail, second) = decoder.process(&[&marker[split..], b"after".as_slice()].concat());
            visible.extend(tail);
            let completion = second.or(first).unwrap();
            assert_eq!(visible, b"beforeafter");
            assert_eq!(completion.exit_code, Some(7));
            assert!(!completion.succeeded);
            assert!(completion.ready);
        }
    }

    #[test]
    fn foreign_and_invalid_sequences_are_preserved_and_buffer_is_bounded() {
        let mut decoder = CompletionDecoder::new("abc".into());
        for message in [
            b"\x1b]633;D;0;foreign;1;1\x07".as_slice(),
            b"\x1b]633;D;0;abc;1;0\x07",
            b"\x1b]633;D;0;abc;2;1\x07",
        ] {
            let (visible, completion) = decoder.process(message);
            assert_eq!(visible, message);
            assert!(completion.is_none());
        }
        let message = [PREFIX, &vec![b'x'; 10_000]].concat();
        let (mut visible, completion) = decoder.process(&message);
        assert!(completion.is_none());
        assert!(decoder.pending.len() < MAX_NOTIFICATION_BYTES);
        visible.extend(decoder.finish());
        assert_eq!(visible, message);
    }

    #[test]
    fn notifications_count_but_only_latest_readiness_is_used() {
        let mut decoder = CompletionDecoder::new("abc".into());
        let (_, completion) =
            decoder.process(b"\x1b]633;D;0;abc;1;1\x07\x1b]633;D;_;abc;0;0\x1b\\");
        let completion = completion.unwrap();
        assert_eq!(completion.revision, 2);
        assert!(!completion.ready);
        assert_eq!(completion.exit_code, None);
    }

    #[test]
    fn powershell_uses_inline_prompt_and_does_not_invent_a_native_exit_code() {
        let script = powershell_initialization("abc");
        assert!(script.contains("function global:prompt"));
        assert!(script.contains("]633;D;_;abc;"));
        assert!(script.contains("[Console]::KeyAvailable"));
        assert!(script.contains("Get-Job"));
        assert!(script.contains("$global:LASTEXITCODE = $__me_native"));
        assert!(!script.contains("ExecutionPolicy"));
        assert!(!script.contains("$Profile"));
    }
}
