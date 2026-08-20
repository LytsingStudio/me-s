use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Result,
    config::ModelConfig,
    event::{
        AgentTurnProjection, AgentTurnState, ApiState, Event, EventId, completed_compact_count,
        effective_conversation_events, latest_agent_turn, latest_context_usage,
    },
    toolbox::{ToolboxExecutionError, ToolboxTool, api_safe_name},
    workspace::{AgentId, WorkspaceHandle},
};

pub const AGENT_TOOLBOX_NAME: &str = "Agent";
pub const AGENT_CREATE: &str = "Agent.Create";
pub const AGENT_WAIT: &str = "Agent.Wait";
pub const AGENT_ASK: &str = "Agent.Ask";
pub const AGENT_STOP: &str = "Agent.Stop";
pub const AGENT_CLEAR_CONTEXT: &str = "Agent.ClearContext";
pub const AGENT_KILL: &str = "Agent.Kill";
pub const WORKER_TOOLBOX_NAME: &str = "Worker";
pub const WORKER_WAIT: &str = "Worker.Wait";
pub const WORKER_ASK: &str = "Worker.Ask";
pub const WORKER_STOP: &str = "Worker.Stop";
pub const WORKER_CLEAR_CONTEXT: &str = "Worker.ClearContext";
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_WAIT_MS: u64 = 3_600_000;
const STOP_START_TIMEOUT: Duration = Duration::from_secs(5);
const CLEAR_CONTEXT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn is_agent_tool(name: &str) -> bool {
    matches!(
        name,
        AGENT_CREATE | AGENT_WAIT | AGENT_ASK | AGENT_STOP | AGENT_CLEAR_CONTEXT | AGENT_KILL
    )
}

pub fn is_worker_tool(name: &str) -> bool {
    matches!(
        name,
        WORKER_WAIT | WORKER_ASK | WORKER_STOP | WORKER_CLEAR_CONTEXT
    )
}

pub fn worker_catalog_parts() -> (Vec<ToolboxTool>, (String, String)) {
    let tools = ["Ask", "Wait", "Stop", "ClearContext"]
        .into_iter()
        .map(|local_name| {
            let full_name = format!("{WORKER_TOOLBOX_NAME}.{local_name}");
            ToolboxTool {
                toolbox: WORKER_TOOLBOX_NAME.into(),
                local_name: local_name.into(),
                api_name: api_safe_name(&full_name),
                full_name,
                input_schema: worker_input_schema(local_name),
                output_schema: worker_output_schema(local_name),
                instructions: worker_instructions(local_name).into(),
                route: worker_route(local_name).into(),
                examples: worker_examples(local_name).into(),
            }
        })
        .collect();
    (
        tools,
        (
            WORKER_TOOLBOX_NAME.into(),
            "Provides your dedicated operational interface to the execution environment. Use it for precise observations, for materializing exact content you have already authored, for non-creative mechanical side effects, and for executing specified review, acceptance, or other checks whose evidence is returned without judgment. The Worker must not supply missing analysis, design, code, prose, review conclusions, acceptance conclusions, or other solution content. It may collect image evidence and return its path and provenance, but it never inspects image content. Its conversation may support related steps within one bounded piece of work, but every Ask must independently restate all applicable rules and requirements and contain only already-determined operations; split at any point where returned evidence could change the next task operation. Every operation targets that Worker without a session identifier.".into(),
        ),
    )
}

fn worker_input_schema(tool: &str) -> Value {
    match tool {
        "Ask" => json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "description": "An explicit, self-contained, independently executable operational request for the dedicated Worker. It must not ask the Worker to choose a task branch from an intermediate result."
                }
            },
            "additionalProperties": false
        }),
        "Wait" => json!({
            "type": "object",
            "required": ["max_wait_ms"],
            "properties": {
                "max_wait_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_WAIT_MS,
                    "description": "Maximum time to wait in milliseconds, up to 3600000 (one hour). Use 0 to check immediately."
                }
            },
            "additionalProperties": false
        }),
        "Stop" | "ClearContext" => json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        _ => unreachable!("fixed native Worker tool"),
    }
}

fn worker_output_schema(tool: &str) -> Value {
    match tool {
        "Ask" => json!({
            "type": "object",
            "required": ["worker", "state"],
            "properties": {
                "worker": {"const": "worker", "description": "The dedicated Worker."},
                "state": {"const": "working", "description": "The request was accepted and is in progress."}
            },
            "additionalProperties": false
        }),
        "Wait" => json!({
            "type": "object",
            "required": ["worker", "state", "turn_id", "final_answer", "progress", "context_usage", "compact_count"],
            "properties": {
                "worker": {"const": "worker", "description": "The dedicated Worker."},
                "state": {
                    "enum": ["pending", "working", "completed", "interrupted", "failed", "wait_interrupted"],
                    "description": "pending: no observable progress during this wait; working: progress occurred but no final answer exists; completed: a final answer is available; interrupted: Worker execution stopped; failed: Worker execution ended because of an error; wait_interrupted: only this Wait ended early because a real user follow-up is attached immediately after the result, while the Worker continues."
                },
                "reason": {"type": ["string", "null"], "enum": ["follow_up", null], "description": "follow_up when state is wait_interrupted; otherwise absent or null."},
                "turn_id": {"type": ["integer", "null"], "description": "Identifier of the observed Worker request, or null if it has not started."},
                "final_answer": {"type": ["string", "null"], "description": "The complete answer when state is completed; otherwise null. Worker.Wait never safety-truncates this text."},
                "error": {"type": ["string", "null"], "description": "Failure or interruption details when available; otherwise null."},
                "progress": {
                    "type": "array",
                    "description": "Incremental activity observed since the preceding Wait, returned for pending, working, completed, interrupted, failed, and wait_interrupted results.",
                    "items": {
                        "type": "object",
                        "required": ["assistant_text", "tool_calls"],
                        "properties": {
                            "assistant_text": {"type": "string", "description": "The complete Worker text emitted with this model step. Worker.Wait never safety-truncates it."},
                            "tool_calls": {"type": "array", "items": {"type": "string"}, "description": "Tool names in call order, without arguments or results."}
                        },
                        "additionalProperties": false
                    }
                },
                "context_usage": {
                    "type": ["object", "null"],
                    "description": "The latest known real context usage for the Worker, or null before usage is available or after a context boundary until a new request reports usage.",
                    "required": ["input_tokens", "output_tokens", "total_tokens"],
                    "properties": {
                        "input_tokens": {"type": "integer", "minimum": 0},
                        "output_tokens": {"type": "integer", "minimum": 0},
                        "total_tokens": {"type": "integer", "minimum": 0}
                    },
                    "additionalProperties": false
                },
                "compact_count": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of successfully completed context compactions in the Worker's current history."
                }
            },
            "additionalProperties": false
        }),
        "Stop" => json!({
            "type": "object",
            "required": ["worker", "state", "turn_id"],
            "properties": {
                "worker": {"const": "worker", "description": "The dedicated Worker."},
                "state": {"const": "stopped", "description": "The active request was interrupted."},
                "turn_id": {"type": "integer", "description": "Identifier of the interrupted Worker request."}
            },
            "additionalProperties": false
        }),
        "ClearContext" => json!({
            "type": "object",
            "required": ["worker", "state"],
            "properties": {
                "worker": {"const": "worker", "description": "The dedicated Worker."},
                "state": {"const": "cleared", "description": "The Worker's conversation context was cleared."}
            },
            "additionalProperties": false
        }),
        _ => unreachable!("fixed native Worker tool"),
    }
}

fn worker_route(tool: &str) -> &'static str {
    match tool {
        "Ask" => {
            "Use for one explicit, independently executable observation or non-creative operation, or for several already-determined operations that require no decision between them. Leave tool choice and protocol details to the Worker. If an intermediate result could change what happens next, request only the evidence before that point, decide the route yourself, and issue a later Ask. Never ask the Worker to inspect image content, interpret evidence, turn requirements into a solution, invent task content, select a conditional branch, or decide correctness or acceptance."
        }
        "Wait" => "Use to monitor the dedicated Worker's progress or obtain its latest answer.",
        "Stop" => {
            "Use when the dedicated Worker's current request must stop while preserving its conversation for a corrected follow-up."
        }
        "ClearContext" => {
            "Clear the dedicated Worker's conversation context, WorkMap, and live external tool sessions while the Worker is idle."
        }
        _ => unreachable!("fixed native Worker tool"),
    }
}

fn worker_instructions(tool: &str) -> &'static str {
    match tool {
        "Ask" => {
            "Ask targets your one persistent Worker. It starts the Worker's first operation when idle, or starts a new operation after any preceding operation has reached a terminal state. A completed, explicitly stopped, externally interrupted, host-restarted, model-API-interrupted, or failed Worker can always accept another Ask; only a Worker whose operation is still active rejects Ask. Ask starts a new operation using the preserved conversation and does not resume a lost process or undo external effects from the preceding operation. Never assume the Worker remembers rules or requirements from any earlier Ask: every prompt must independently restate all applicable rules, prohibitions, scope boundaries, exact requirements, relevant context, supplied content, and evidence to return. Do not use references such as 'as before' or 'under the same rules' in place of the complete rules. The Manager retains sole ownership of analysis, diagnosis, design, semantic choices, substantive authorship, review conclusions, acceptance conclusions, and final delivery. Ask may request specific evidence, materialization of exact code or text already authored by the Manager, an exact mechanical transformation that requires no substantive invention, or execution of specified commands, tests, browser actions, review procedures, or acceptance procedures. A detailed requirement or desired behavior is not an authored implementation: never ask the Worker to implement, write, fix, refactor, design, diagnose, independently review, or complete a feature, project, module, file, function, document, or other deliverable from requirements. The Worker may collect image evidence, including browser screenshots, but must return only each path or URL, its producing step, source, and non-visual provenance; it must never call Image tools or inspect visual content. The Manager inspects those images with Image and decides whether work is correct, complete, compliant, accepted, or ready. Specify concrete targets, scope boundaries, supplied content or mechanical operation, exact review or acceptance steps when applicable, and evidence to return. Several observations or mechanical operations may be batched only when all are already determined and none requires an intermediate Manager decision or authorship; batching does not imply concurrent tool execution. Never ask the Worker to interpret an intermediate result and select or continue a conditional branch. Instead, stop the Ask at that observation, receive the evidence, decide the next route yourself, and issue a new explicit Ask. Normally describe the operation without prescribing low-level tool names, parameters, hashes, or sessions. Supply exact code or text in a clear natural form when it must be materialized; a tool-ready payload is unnecessary. The Worker owns mechanical tool selection and execution, recoverable tool-protocol errors, confirmation that requested operations actually ran, and accurate transmission of specified check results. It must not interpret those results as a review or acceptance verdict, and it must stop rather than invent or repair substantive content. After Ask succeeds, Wait observes only this new request."
        }
        "Wait" => {
            "Wait observes the Worker for at most max_wait_ms. Use 60000 to 600000 milliseconds for normal substantive work and shorter checks only when prompt feedback is useful. pending means no observable progress occurred during this wait; working means progress occurred but no final answer exists; completed returns the answer; interrupted or failed reports the Worker's terminal condition. Every result, including completed, interrupted, failed, and wait_interrupted, also returns progress observed since the preceding Wait. After any terminal result, including interrupted or failed for any reason, Worker.Ask can start the next operation using the preserved conversation. If a real user follow-up arrives while waiting, Wait returns immediately with state=wait_interrupted and reason=follow_up; only the wait ends, the Worker continues, and the actual follow-up is attached immediately after this result in the same model context. Read and address that follow-up before deciding whether to wait again, stop the Worker, or take another action. Progress contains assistant text and ordered tool names without tool arguments or results. Worker.Wait is the explicit exception to the shared tool-result safety limit: every progress step and the final answer are always returned completely, without truncation, summarization, abbreviation, or fragment conversion. Its top-level truncate field is therefore always false."
        }
        "Stop" => {
            "Stop interrupts the active Worker request but preserves the Worker's conversation. Use Ask afterward to correct, redirect, or continue it. Stop does not undo files, commands, network actions, or other external effects already produced. Use Wait, not Stop, merely to inspect progress."
        }
        "ClearContext" => {
            "ClearContext accepts no arguments and is available only while the Worker is idle; if an operation is active, wait for it to finish or call Worker.Stop first. Clearing removes the Worker's conversation, the entire WorkMap, and every live external tool session such as Terminal and WebBrowser; previous session and page identifiers become invalid. System instructions and model settings remain active. The operation returns only after the clear has been recorded."
        }
        _ => unreachable!("fixed native Worker tool"),
    }
}

fn worker_examples(tool: &str) -> &'static str {
    match tool {
        "Ask" => {
            r#"{"prompt":"Purpose: inventory the specified directory. Rules and boundaries: work only inside <path>; keep project content read only; do not create, modify, move, or delete project files; temporary work in your Agent-specific temporary workspace remains permitted; do not interpret the project or recommend changes. Work: return a directory tree. Required evidence: include every listed file's path, type, size, and modification time; report access errors exactly."}
{"prompt":"Purpose: obtain source evidence for Manager analysis. Rules and boundaries: inspect only <module>; keep project content read only; do not modify project files; temporary work in your Agent-specific temporary workspace remains permitted; do not infer missing code, diagnose, or propose changes. Work: list all function and method signatures with exact file paths and line numbers, then return the complete source around <target>, including nearby definitions required to read it. Required evidence: preserve source text exactly and report omissions or read errors."}
{"prompt":"Purpose: materialize an exact Manager-authored change. Rules and boundaries: modify only <file> at <target>; use the replacement below exactly; preserve all unrelated content; do not redesign, repair, or add anything. Exact replacement: <complete replacement content>. Verification: run <exact check>. Required evidence: return the resulting target source, the complete relevant check output, exit status, and any error; do not decide whether the change is correct or accepted."}
{"prompt":"Purpose: collect visual evidence from <page>. Rules and boundaries: perform only the stated navigation; do not inspect, describe, classify, compare, or judge image content; do not call Image tools. Work: capture the requested viewport. Required evidence: return the screenshot path, producing step, current URL, title, and available non-visual metadata; report any browser error exactly."}
{"prompt":"Purpose: collect several independent source observations. Rules and boundaries: keep project content read only within <scopes>; do not modify project files; temporary work in your Agent-specific temporary workspace remains permitted; do not interpret findings, diagnose, propose changes, or choose a follow-up branch. Every listed observation is required regardless of earlier results. Work: return the specified directory trees with file details; list signatures and exact locations in the named modules; return complete source around each named target. Required evidence: preserve source text exactly, identify the source path for every item, and report omissions or errors."}"#
        }
        "Wait" => r#"{"max_wait_ms":300000}"#,
        "Stop" => r#"{}"#,
        "ClearContext" => r#"{}"#,
        _ => unreachable!("fixed native Worker tool"),
    }
}

pub fn catalog_parts() -> (Vec<ToolboxTool>, (String, String)) {
    let tools = ["Create", "Wait", "Ask", "Stop", "ClearContext", "Kill"]
        .into_iter()
        .map(|local_name| {
            let full_name = format!("{AGENT_TOOLBOX_NAME}.{local_name}");
            ToolboxTool {
                toolbox: AGENT_TOOLBOX_NAME.into(),
                local_name: local_name.into(),
                api_name: api_safe_name(&full_name),
                full_name,
                input_schema: input_schema(local_name),
                output_schema: output_schema(local_name),
                instructions: instructions(local_name).into(),
                route: route(local_name).into(),
                examples: examples(local_name).into(),
            }
        })
        .collect();
    (
        tools,
        (
            AGENT_TOOLBOX_NAME.into(),
            "Creates and controls independent sub-Agents. Each sub-Agent works concurrently in its own conversation while sharing the same workspace. Use Wait to observe progress or obtain an answer, Ask to continue a completed, explicitly stopped, or model-API-interrupted session, Stop to interrupt current work, ClearContext to reset an idle session's conversation, and Kill to end a session that is no longer needed.".into(),
        ),
    )
}

fn input_schema(tool: &str) -> Value {
    match tool {
        "Create" => json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "system_prompt": {
                    "type": "string",
                    "description": "Optional high-priority instructions that govern the sub-Agent. Put the concrete task in prompt."
                },
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The complete initial task. Include all context the sub-Agent needs because it does not inherit this conversation."
                }
            },
            "additionalProperties": false
        }),
        "Wait" => json!({
            "type": "object",
            "required": ["session_id", "max_wait_ms"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The session identifier returned by Agent.Create."
                },
                "max_wait_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_WAIT_MS,
                    "description": "Maximum time to wait in milliseconds, up to 3600000 (one hour). Use 0 to check immediately."
                },
            },
            "additionalProperties": false
        }),
        "Ask" => json!({
            "type": "object",
            "required": ["session_id", "prompt"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "A persistent session whose previous request completed successfully, was explicitly stopped with Agent.Stop, or was interrupted by a model API failure."
                },
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The follow-up request. The sub-Agent keeps its previous conversation."
                }
            },
            "additionalProperties": false
        }),
        "Stop" | "ClearContext" | "Kill" => json!({
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": match tool {
                        "Stop" => "The active session to stop while retaining its conversation.",
                        "ClearContext" => "The idle session whose conversation context should be cleared.",
                        "Kill" => "The session to end.",
                        _ => unreachable!("fixed native Agent tool"),
                    }
                }
            },
            "additionalProperties": false
        }),
        _ => unreachable!("fixed native Agent tool"),
    }
}

fn output_schema(tool: &str) -> Value {
    match tool {
        "Create" => json!({
            "type": "object",
            "required": ["session_id", "state"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Use this identifier with Agent.Wait, Agent.Ask, Agent.Stop, Agent.ClearContext, or Agent.Kill."
                },
                "state": {
                    "const": "working",
                    "description": "The initial task was accepted and is in progress."
                }
            },
            "additionalProperties": false
        }),
        "Wait" => json!({
            "type": "object",
            "required": ["session_id", "state", "turn_id", "final_answer", "progress", "context_usage", "compact_count"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The observed session."
                },
                "state": {
                    "enum": ["pending", "working", "completed", "interrupted", "failed", "wait_interrupted"],
                    "description": "pending: no observable progress since the preceding Wait; working: progress occurred but no final answer exists yet; completed: a final answer is available; interrupted: work stopped before a final answer; failed: work ended because of an error; wait_interrupted: only this Wait ended early because a real user follow-up is attached immediately after the result, while the sub-Agent continues."
                },
                "reason": {
                    "type": ["string", "null"],
                    "enum": ["follow_up", null],
                    "description": "follow_up when state is wait_interrupted; otherwise absent or null."
                },
                "turn_id": {
                    "type": ["integer", "null"],
                    "description": "Identifier of the request being observed, or null if no request has started."
                },
                "final_answer": {
                    "type": ["string", "object", "null"],
                    "description": "The answer when state is completed; otherwise null. An oversized answer may use the safe text_fragments representation described by the tool result envelope."
                },
                "error": {
                    "type": ["string", "null"],
                    "description": "Failure or interruption details when available; otherwise null."
                },
                "progress": {
                    "type": "array",
                    "description": "Incremental activity observed since the preceding Wait. Returned for pending, working, completed, interrupted, failed, and wait_interrupted results.",
                    "items": {
                        "type": "object",
                        "required": ["assistant_text", "tool_calls"],
                        "properties": {
                            "assistant_text": {
                                "type": "string",
                                "description": "Assistant text emitted with this model step."
                            },
                            "tool_calls": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Tool names in call order, without arguments or results."
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "context_usage": {
                    "type": ["object", "null"],
                    "description": "The latest known real context usage for the sub-Agent, or null before usage is available or after a context boundary until a new request reports usage.",
                    "required": ["input_tokens", "output_tokens", "total_tokens"],
                    "properties": {
                        "input_tokens": {"type": "integer", "minimum": 0},
                        "output_tokens": {"type": "integer", "minimum": 0},
                        "total_tokens": {"type": "integer", "minimum": 0}
                    },
                    "additionalProperties": false
                },
                "compact_count": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of successfully completed context compactions in the sub-Agent's current history."
                }
            },
            "additionalProperties": false
        }),
        "Ask" => json!({
            "type": "object",
            "required": ["session_id", "state"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The continued session."
                },
                "state": {
                    "const": "working",
                    "description": "The follow-up request was accepted and is in progress."
                }
            },
            "additionalProperties": false
        }),
        "Stop" => json!({
            "type": "object",
            "required": ["session_id", "state", "turn_id"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The persistent session."
                },
                "state": {
                    "const": "stopped",
                    "description": "The active request was interrupted."
                },
                "turn_id": {
                    "type": "integer",
                    "description": "Identifier of the interrupted request."
                }
            },
            "additionalProperties": false
        }),
        "ClearContext" => json!({
            "type": "object",
            "required": ["session_id", "state"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The cleared session."
                },
                "state": {
                    "const": "cleared",
                    "description": "The sub-Agent's conversation context was cleared."
                }
            },
            "additionalProperties": false
        }),
        "Kill" => json!({
            "type": "object",
            "required": ["session_id", "state"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The ended session."
                },
                "state": {
                    "const": "killed",
                    "description": "The session was stopped and can no longer be used."
                }
            },
            "additionalProperties": false
        }),
        _ => unreachable!("fixed native Agent tool"),
    }
}

fn route(tool: &str) -> &'static str {
    match tool {
        "Create" => {
            "Use when an independent Agent can own a bounded, separable task whose result will be useful before the parent proceeds. Do not delegate trivial work, a task tightly coupled to the parent's next action, or work the parent will duplicate itself."
        }
        "Wait" => {
            "Use to check progress or obtain the current request's result from a sub-Agent created by Agent.Create."
        }
        "Ask" => {
            "Use after a sub-Agent completed its previous request, Agent.Stop explicitly stopped it, or its model API failed, and a follow-up request is useful."
        }
        "Stop" => {
            "Use when a sub-Agent's current request must stop but its conversation should remain available for a later Agent.Ask."
        }
        "ClearContext" => {
            "Use only when the current task is completely finished or the task has changed completely and an idle sub-Agent's prior conversation is no longer relevant. Do not use as routine cleanup or between steps of the same task."
        }
        "Kill" => {
            "Use when a sub-Agent is no longer needed or its current work must be stopped and the session ended."
        }
        _ => unreachable!("fixed native Agent tool"),
    }
}

fn instructions(tool: &str) -> &'static str {
    match tool {
        "Create" => {
            "Create starts an independent sub-Agent and returns immediately. The sub-Agent uses the current model and reasoning effort, shares the same workspace, and does not inherit this conversation. Brief it like a capable colleague who just arrived: state the concrete objective and why it matters, expected deliverable, relevant paths or inputs, constraints, material facts already learned or ruled out, and whether it may modify files or should only investigate. Do not delegate the missing understanding itself with vague wording such as 'use the previous findings'; put the needed findings in prompt. Keep the requested response proportional so raw research does not needlessly fill the parent context. system_prompt is optional and governs the sub-Agent with higher priority than prompt."
        }
        "Wait" => {
            "Wait observes the sub-Agent for at most max_wait_ms, which may range from 0 for an immediate check through 3600000 for a one-hour wait. Every result returns progress observed since the preceding Wait as assistant text paired with ordered tool names; it never includes tool arguments or results. pending means no observable progress occurred since the preceding Wait; it does not mean the task failed or stopped. working means progress occurred but no final answer exists yet. completed returns the final answer. interrupted means work stopped before producing a final answer. failed means work ended because of an error. If a real user follow-up arrives while waiting, Wait returns immediately with state=wait_interrupted and reason=follow_up; only the wait ends, the sub-Agent continues, and the actual follow-up is attached immediately after this result in the same model context. Read and address that follow-up before deciding what to do next. If a Wait result exceeds the shared tool-result safety limit, the oldest complete progress steps are omitted first and oversized text preserves exact beginning and ending fragments; inspect the top-level truncate and truncate_info fields. Each result also reports the latest known context usage and successful context-compaction count."
        }
        "Ask" => {
            "Ask continues a sub-Agent using its previous conversation. It is accepted after that session completed its preceding request, Agent.Stop explicitly interrupted it, or its model API failed. A busy session, an unrelated interruption, a host failure, or an unavailable or unknown session is rejected. After Ask succeeds, the next Agent.Wait observes only the new request and cannot return the preceding answer."
        }
        "Stop" => {
            "Stop interrupts the sub-Agent's active request and waits for it to close as interrupted. Its conversation remains available, and Agent.Ask can continue from it. Stop does not undo files, commands, network actions, or other external effects already produced. Do not use Stop merely to check progress; use Agent.Wait instead."
        }
        "ClearContext" => {
            "ClearContext is a low-frequency reset for an idle sub-Agent. Use it only after the current task is completely finished or when the task has changed completely and the prior conversation is no longer relevant. It removes the prior conversation, the entire WorkMap, and every live external tool session such as Terminal and WebBrowser; previous session and page identifiers become invalid. Do not clear routinely or between related operations. If the sub-Agent is active, wait for it to finish or call Agent.Stop first. System instructions and model settings remain active. The operation returns only after the clear has been recorded."
        }
        "Kill" => {
            "Kill stops any work still in progress and ends the sub-Agent session. The session_id cannot be used afterward. Kill does not undo files, commands, network actions, or other external effects already produced."
        }
        _ => unreachable!("fixed native Agent tool"),
    }
}

fn examples(tool: &str) -> &'static str {
    match tool {
        "Create" => {
            r#"{"prompt":"Inspect the supplied material and return a concise result."} or {"system_prompt":"Return only supported claims.","prompt":"Analyze the supplied material."}"#
        }
        "Wait" => {
            r#"Check immediately: {"session_id":"agent-a13f9c20","max_wait_ms":0}. Wait up to 30 seconds: {"session_id":"agent-a13f9c20","max_wait_ms":30000}."#
        }
        "Ask" => {
            r#"{"session_id":"agent-a13f9c20","prompt":"Explain the previous conclusion more concisely."}"#
        }
        "Stop" => r#"{"session_id":"agent-a13f9c20"}"#,
        "ClearContext" => r#"{"session_id":"agent-a13f9c20"}"#,
        "Kill" => r#"{"session_id":"agent-a13f9c20"}"#,
        _ => unreachable!("fixed native Agent tool"),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    #[serde(default)]
    system_prompt: Option<String>,
    prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitInput {
    session_id: String,
    max_wait_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskInput {
    session_id: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerAskInput {
    prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerWaitInput {
    max_wait_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopInput {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KillInput {
    session_id: String,
}

#[derive(Clone, Copy)]
enum ToolSurface {
    Agent,
    Worker,
}

impl ToolSurface {
    fn label(self) -> &'static str {
        match self {
            Self::Agent => "sub-Agent",
            Self::Worker => "Worker",
        }
    }

    fn tool(self, local_name: &str) -> String {
        match self {
            Self::Agent => format!("Agent.{local_name}"),
            Self::Worker => format!("Worker.{local_name}"),
        }
    }

    fn busy_code(self) -> &'static str {
        match self {
            Self::Agent => "agent_busy",
            Self::Worker => "worker_busy",
        }
    }
}

#[derive(Clone, Debug)]
struct AgentSession {
    agent_id: AgentId,
    target_after_event_id: Option<EventId>,
    target_turn_id: Option<EventId>,
    last_wait_event_id: Option<EventId>,
}

#[derive(Default)]
pub struct NativeAgentToolbox {
    workspace: Option<WorkspaceHandle>,
    parent_agent_id: Option<AgentId>,
    sessions: HashMap<String, AgentSession>,
}

impl NativeAgentToolbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn configure(&mut self, workspace: WorkspaceHandle, parent_agent_id: AgentId) {
        self.workspace = Some(workspace);
        self.parent_agent_id = Some(parent_agent_id);
    }

    pub fn execute(
        &mut self,
        full_name: &str,
        arguments: &str,
        model: &ModelConfig,
        effort: &str,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.execute_cancellable(full_name, arguments, model, effort, &mut || false)
    }

    pub fn execute_cancellable(
        &mut self,
        full_name: &str,
        arguments: &str,
        model: &ModelConfig,
        effort: &str,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.execute_cancellable_with_follow_up(
            full_name,
            arguments,
            model,
            effort,
            should_cancel,
            &mut || Ok(false),
        )
    }

    pub(crate) fn execute_cancellable_with_follow_up(
        &mut self,
        full_name: &str,
        arguments: &str,
        model: &ModelConfig,
        effort: &str,
        should_cancel: &mut dyn FnMut() -> bool,
        should_interrupt_wait: &mut dyn FnMut() -> Result<bool>,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.sync_sessions()?;
        if should_cancel() {
            return Err(ToolboxExecutionError::Interrupted(
                "Agent tool request cancelled".into(),
            ));
        }
        match full_name {
            AGENT_CREATE => self.create(arguments, model, effort),
            AGENT_WAIT => {
                let mut output = self.wait(
                    arguments,
                    ToolSurface::Agent,
                    should_cancel,
                    should_interrupt_wait,
                )?;
                let session_id = output
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| execution_error("Agent.Wait omitted session_id"))?
                    .to_owned();
                self.append_wait_stats(&session_id, ToolSurface::Agent, &mut output)?;
                Ok(output)
            }
            AGENT_ASK => self.ask(arguments, ToolSurface::Agent),
            AGENT_STOP => self.stop(arguments, ToolSurface::Agent, should_cancel),
            AGENT_CLEAR_CONTEXT => {
                let input: StopInput = parse_arguments(arguments)?;
                self.clear_context(&input.session_id, ToolSurface::Agent, should_cancel)
            }
            AGENT_KILL => self.kill(arguments),
            _ => Err(tool_error(
                "unknown_tool",
                format!("native Agent tool {full_name} does not exist"),
                false,
            )),
        }
    }

    pub fn execute_worker_cancellable(
        &mut self,
        full_name: &str,
        arguments: &str,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.execute_worker_cancellable_with_follow_up(
            full_name,
            arguments,
            should_cancel,
            &mut || Ok(false),
        )
    }

    pub(crate) fn execute_worker_cancellable_with_follow_up(
        &mut self,
        full_name: &str,
        arguments: &str,
        should_cancel: &mut dyn FnMut() -> bool,
        should_interrupt_wait: &mut dyn FnMut() -> Result<bool>,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        self.sync_sessions()?;
        if should_cancel() {
            return Err(ToolboxExecutionError::Interrupted(
                "Worker tool request cancelled".into(),
            ));
        }
        let session_id = match self.sessions.keys().next() {
            Some(session_id) if self.sessions.len() == 1 => session_id.clone(),
            Some(_) => {
                return Err(tool_error(
                    "worker_binding_invalid",
                    "the Manager has more than one dedicated Worker",
                    false,
                ));
            }
            None => {
                return Err(tool_error(
                    "worker_unavailable",
                    "the dedicated Worker is unavailable",
                    true,
                ));
            }
        };
        if full_name == WORKER_CLEAR_CONTEXT {
            let _: EmptyInput = parse_arguments(arguments)?;
            let mut output = self.clear_context(&session_id, ToolSurface::Worker, should_cancel)?;
            if let Some(output) = output.as_object_mut() {
                output.remove("session_id");
                output.insert("worker".into(), Value::String("worker".into()));
            }
            return Ok(output);
        }
        let delegated = match full_name {
            WORKER_ASK => {
                let input: WorkerAskInput = parse_arguments(arguments)?;
                json!({"session_id": session_id, "prompt": input.prompt})
            }
            WORKER_WAIT => {
                let input: WorkerWaitInput = parse_arguments(arguments)?;
                if input.max_wait_ms > MAX_WAIT_MS {
                    return Err(max_wait_error());
                }
                json!({"session_id": session_id, "max_wait_ms": input.max_wait_ms})
            }
            WORKER_STOP => {
                let _: EmptyInput = parse_arguments(arguments)?;
                json!({"session_id": session_id})
            }
            _ => {
                return Err(tool_error(
                    "unknown_tool",
                    format!("native Worker tool {full_name} does not exist"),
                    false,
                ));
            }
        };
        let delegated = serde_json::to_string(&delegated).map_err(execution_error)?;
        let mut output = match full_name {
            WORKER_ASK => self.ask(&delegated, ToolSurface::Worker),
            WORKER_WAIT => self.wait(
                &delegated,
                ToolSurface::Worker,
                should_cancel,
                should_interrupt_wait,
            ),
            WORKER_STOP => self.stop(&delegated, ToolSurface::Worker, should_cancel),
            _ => unreachable!("validated Worker tool"),
        }?;
        if full_name == WORKER_WAIT {
            self.append_wait_stats(&session_id, ToolSurface::Worker, &mut output)?;
        }
        if let Some(output) = output.as_object_mut() {
            output.remove("session_id");
            output.insert("worker".into(), Value::String("worker".into()));
        }
        Ok(output)
    }

    fn append_wait_stats(
        &self,
        session_id: &str,
        surface: ToolSurface,
        output: &mut Value,
    ) -> std::result::Result<(), ToolboxExecutionError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| session_not_found(session_id, surface))?;
        let events = self
            .workspace()?
            .events(&session.agent_id)
            .map_err(execution_error)?;
        let object = output
            .as_object_mut()
            .ok_or_else(|| execution_error("Wait produced a non-object result"))?;
        object.insert(
            "context_usage".into(),
            latest_context_usage(&events).map_or(Value::Null, |usage| json!(usage)),
        );
        object.insert(
            "compact_count".into(),
            Value::from(completed_compact_count(&events)),
        );
        Ok(())
    }

    fn clear_context(
        &mut self,
        session_id: &str,
        surface: ToolSurface,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        let workspace = self.workspace()?.clone();
        let agent_id = self
            .sessions
            .get(session_id)
            .ok_or_else(|| session_not_found(session_id, surface))?
            .agent_id
            .clone();
        let events = workspace.events(&agent_id).map_err(execution_error)?;
        let advancing = workspace.is_advancing(&agent_id).map_err(execution_error)?;
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| session_not_found(session_id, surface))?;
        let turn = resolve_target_turn(session, &events).map_err(execution_error)?;
        let request_pending = session.target_after_event_id.is_some() && turn.is_none();
        if advancing || request_pending || turn.is_some_and(|turn| !turn.state.is_terminal()) {
            return Err(tool_error(
                surface.busy_code(),
                format!(
                    "the {} is active; wait for it to finish or call {} before clearing its context",
                    surface.label(),
                    surface.tool("Stop")
                ),
                false,
            ));
        }

        let baseline = events.last().map(Event::id);
        workspace
            .submit_context_clear(&agent_id)
            .map_err(execution_error)?;
        let started = Instant::now();
        loop {
            if should_cancel() {
                return Err(ToolboxExecutionError::Interrupted(format!(
                    "{} cancelled after submission",
                    surface.tool("ClearContext")
                )));
            }
            let events = workspace.events(&agent_id).map_err(execution_error)?;
            if events.iter().any(|event| {
                matches!(event, Event::ContextCleared(clear) if baseline.is_none_or(|baseline| clear.id > baseline))
            }) {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.last_wait_event_id = events.last().map(Event::id);
                }
                return Ok(json!({"session_id": session_id, "state": "cleared"}));
            }
            if started.elapsed() >= CLEAR_CONTEXT_TIMEOUT {
                return Err(tool_error(
                    "context_clear_timeout",
                    format!(
                        "the {} did not record the context clear before the timeout",
                        surface.label()
                    ),
                    true,
                ));
            }
            thread::sleep(WAIT_POLL_INTERVAL);
        }
    }

    fn workspace(&self) -> std::result::Result<&WorkspaceHandle, ToolboxExecutionError> {
        self.workspace.as_ref().ok_or_else(|| {
            tool_error(
                "toolbox_unavailable",
                "Agent toolbox is not attached to a Workspace",
                true,
            )
        })
    }

    fn parent_id(&self) -> std::result::Result<&AgentId, ToolboxExecutionError> {
        self.parent_agent_id.as_ref().ok_or_else(|| {
            tool_error(
                "toolbox_unavailable",
                "Agent toolbox has no parent Agent identity",
                true,
            )
        })
    }

    fn sync_sessions(&mut self) -> std::result::Result<(), ToolboxExecutionError> {
        let Some(workspace) = self.workspace.clone() else {
            return Ok(());
        };
        let Some(parent) = self.parent_agent_id.clone() else {
            return Ok(());
        };
        let children = workspace
            .child_agent_ids(&parent)
            .map_err(execution_error)?;
        for child in &children {
            let session_id = child.to_string();
            if self.sessions.contains_key(&session_id) {
                continue;
            }
            let events = workspace.events(child).map_err(execution_error)?;
            let latest_turn = latest_agent_turn(&events).map_err(execution_error)?;
            self.sessions.insert(
                session_id,
                AgentSession {
                    agent_id: child.clone(),
                    target_after_event_id: None,
                    target_turn_id: latest_turn.as_ref().map(|turn| turn.turn_id),
                    last_wait_event_id: events.last().map(Event::id),
                },
            );
        }
        self.sessions
            .retain(|_, session| children.contains(&session.agent_id));
        Ok(())
    }

    fn create(
        &mut self,
        arguments: &str,
        model: &ModelConfig,
        effort: &str,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        let mut input: CreateInput = parse_arguments(arguments)?;
        if input.prompt.is_empty() {
            return Err(tool_error(
                "invalid_arguments",
                "prompt cannot be empty",
                false,
            ));
        }
        if input.system_prompt.as_ref().is_some_and(String::is_empty) {
            input.system_prompt = None;
        }
        let workspace = self.workspace()?.clone();
        let parent = self.parent_id()?.clone();
        let agent_id = workspace
            .create_sub_agent(
                &parent,
                input.system_prompt,
                input.prompt,
                model.name.clone(),
                effort.to_owned(),
            )
            .map_err(execution_error)?;
        let events = workspace.events(&agent_id).map_err(execution_error)?;
        let latest_turn = latest_agent_turn(&events).map_err(execution_error)?;
        let session_id = agent_id.to_string();
        self.sessions.insert(
            session_id.clone(),
            AgentSession {
                agent_id,
                target_after_event_id: None,
                target_turn_id: latest_turn.as_ref().map(|turn| turn.turn_id),
                last_wait_event_id: None,
            },
        );
        Ok(json!({"session_id": session_id, "state": "working"}))
    }

    fn wait(
        &mut self,
        arguments: &str,
        surface: ToolSurface,
        should_cancel: &mut dyn FnMut() -> bool,
        should_interrupt_wait: &mut dyn FnMut() -> Result<bool>,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        let input = parse_wait_input(arguments)?;
        let started = Instant::now();
        let timeout = Duration::from_millis(input.max_wait_ms);
        let mut activity = false;
        let observation_baseline = self
            .sessions
            .get(&input.session_id)
            .ok_or_else(|| session_not_found(&input.session_id, surface))?
            .last_wait_event_id;
        loop {
            if should_cancel() {
                return Err(ToolboxExecutionError::Interrupted(format!(
                    "{} cancelled",
                    surface.tool("Wait")
                )));
            }
            let workspace = self.workspace()?.clone();
            let session = self
                .sessions
                .get_mut(&input.session_id)
                .ok_or_else(|| session_not_found(&input.session_id, surface))?;
            let events = workspace
                .events(&session.agent_id)
                .map_err(execution_error)?;
            let latest_event_id = events.last().map(Event::id);
            activity |= event_id_after(latest_event_id, session.last_wait_event_id);
            let turn = resolve_target_turn(session, &events).map_err(execution_error)?;
            let advancing = workspace
                .is_advancing(&session.agent_id)
                .map_err(execution_error)?;
            if let Some(turn) = turn.filter(|turn| turn.state.is_terminal() && !advancing) {
                let progress =
                    wait_progress(&events, observation_baseline).map_err(execution_error)?;
                let output = terminal_output(&events, &turn, &input.session_id, progress, surface);
                session.last_wait_event_id = latest_event_id;
                return Ok(output);
            }
            if should_interrupt_wait().map_err(execution_error)? {
                let progress =
                    wait_progress(&events, observation_baseline).map_err(execution_error)?;
                session.last_wait_event_id = latest_event_id;
                return Ok(json!({
                    "session_id": input.session_id,
                    "state": "wait_interrupted",
                    "reason": "follow_up",
                    "turn_id": session.target_turn_id,
                    "final_answer": null,
                    "error": null,
                    "progress": progress,
                }));
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                let progress =
                    wait_progress(&events, observation_baseline).map_err(execution_error)?;
                session.last_wait_event_id = latest_event_id;
                return Ok(json!({
                    "session_id": input.session_id,
                    "state": if activity { "working" } else { "pending" },
                    "turn_id": session.target_turn_id,
                    "final_answer": null,
                    "error": null,
                    "progress": progress,
                }));
            }
            thread::sleep(WAIT_POLL_INTERVAL.min(timeout.saturating_sub(elapsed)));
        }
    }

    fn ask(
        &mut self,
        arguments: &str,
        surface: ToolSurface,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        let input: AskInput = parse_arguments(arguments)?;
        if input.prompt.is_empty() {
            return Err(tool_error(
                "invalid_arguments",
                "prompt cannot be empty",
                false,
            ));
        }
        let workspace = self.workspace()?.clone();
        let session = self
            .sessions
            .get_mut(&input.session_id)
            .ok_or_else(|| session_not_found(&input.session_id, surface))?;
        let events = workspace
            .events(&session.agent_id)
            .map_err(execution_error)?;
        if workspace
            .is_advancing(&session.agent_id)
            .map_err(execution_error)?
        {
            return Err(tool_error(
                surface.busy_code(),
                format!("{} is still active and cannot accept Ask", surface.label()),
                false,
            ));
        }
        if let Some(turn) = resolve_target_turn(session, &events).map_err(execution_error)? {
            if !askable_turn(&events, &turn, surface) {
                let accepted = match surface {
                    ToolSurface::Agent => format!(
                        "a completed turn, a turn explicitly stopped with {}, or a turn interrupted by a model API failure",
                        surface.tool("Stop")
                    ),
                    ToolSurface::Worker => "any terminal Worker turn".into(),
                };
                return Err(tool_error(
                    match surface {
                        ToolSurface::Agent => "agent_not_askable",
                        ToolSurface::Worker => "worker_not_askable",
                    },
                    format!(
                        "{} cannot accept Ask after a {} turn; only {accepted} can continue",
                        surface.label(),
                        turn.state,
                    ),
                    false,
                ));
            }
        } else if events.iter().any(Event::is_root_prompt) {
            return Err(tool_error(
                surface.busy_code(),
                format!("{} has not completed its current turn", surface.label()),
                false,
            ));
        }
        let baseline = events.last().map(Event::id);
        match surface {
            ToolSurface::Agent => {
                workspace.submit_parent_agent_prompt(&session.agent_id, input.prompt)
            }
            ToolSurface::Worker => workspace.submit_manager_prompt(&session.agent_id, input.prompt),
        }
        .map_err(execution_error)?;
        session.target_after_event_id = baseline;
        session.target_turn_id = None;
        session.last_wait_event_id = baseline;
        Ok(json!({"session_id": input.session_id, "state": "working"}))
    }

    fn stop(
        &mut self,
        arguments: &str,
        surface: ToolSurface,
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, ToolboxExecutionError> {
        let input: StopInput = parse_arguments(arguments)?;
        let workspace = self.workspace()?.clone();
        let started = Instant::now();
        let mut abort_submitted = false;
        loop {
            if should_cancel() {
                return Err(ToolboxExecutionError::Interrupted(format!(
                    "{} cancelled",
                    surface.tool("Stop")
                )));
            }
            let session = self
                .sessions
                .get_mut(&input.session_id)
                .ok_or_else(|| session_not_found(&input.session_id, surface))?;
            let events = workspace
                .events(&session.agent_id)
                .map_err(execution_error)?;
            let turn = resolve_target_turn(session, &events).map_err(execution_error)?;
            let advancing = workspace
                .is_advancing(&session.agent_id)
                .map_err(execution_error)?;

            match turn {
                Some(turn) if turn.state == AgentTurnState::Started => {
                    if !abort_submitted {
                        abort_submitted = workspace
                            .with_runtime_for_agent_tool(&session.agent_id, |runtime| {
                                runtime.submit_turn_abort()
                            })
                            .map_err(execution_error)?;
                    }
                }
                Some(turn) if abort_submitted && !advancing => {
                    if turn.state == AgentTurnState::Interrupted
                        && turn_was_explicitly_stopped(&events, turn.prompt_id)
                    {
                        session.last_wait_event_id = events.last().map(Event::id);
                        return Ok(json!({
                            "session_id": input.session_id,
                            "state": "stopped",
                            "turn_id": turn.turn_id,
                        }));
                    }
                    return Err(tool_error(
                        match surface {
                            ToolSurface::Agent => "agent_not_running",
                            ToolSurface::Worker => "worker_not_running",
                        },
                        format!(
                            "{} reached {} before {} could interrupt it",
                            surface.label(),
                            turn.state,
                            surface.tool("Stop")
                        ),
                        false,
                    ));
                }
                Some(turn) if turn.state.is_terminal() => {
                    return Err(tool_error(
                        match surface {
                            ToolSurface::Agent => "agent_not_running",
                            ToolSurface::Worker => "worker_not_running",
                        },
                        format!("{} turn is already {}", surface.label(), turn.state),
                        false,
                    ));
                }
                None if !advancing && started.elapsed() >= STOP_START_TIMEOUT => {
                    return Err(tool_error(
                        match surface {
                            ToolSurface::Agent => "agent_not_running",
                            ToolSurface::Worker => "worker_not_running",
                        },
                        format!("{} did not start an active request", surface.label()),
                        false,
                    ));
                }
                _ => {}
            }
            thread::sleep(WAIT_POLL_INTERVAL);
        }
    }

    fn kill(&mut self, arguments: &str) -> std::result::Result<Value, ToolboxExecutionError> {
        let input: KillInput = parse_arguments(arguments)?;
        let workspace = self.workspace()?.clone();
        let session = self
            .sessions
            .remove(&input.session_id)
            .ok_or_else(|| session_not_found(&input.session_id, ToolSurface::Agent))?;
        if workspace
            .is_advancing(&session.agent_id)
            .map_err(execution_error)?
        {
            let _ = workspace.with_runtime_for_agent_tool(&session.agent_id, |runtime| {
                runtime.submit_turn_abort()
            });
        }
        workspace
            .delete_agent(&session.agent_id, true)
            .map_err(execution_error)?;
        Ok(json!({"session_id": input.session_id, "state": "killed"}))
    }
}

fn event_id_after(candidate: Option<EventId>, baseline: Option<EventId>) -> bool {
    match (candidate, baseline) {
        (Some(candidate), Some(baseline)) => candidate > baseline,
        (Some(_), None) => true,
        _ => false,
    }
}

fn resolve_target_turn(
    session: &mut AgentSession,
    events: &[Event],
) -> Result<Option<AgentTurnProjection>> {
    if session.target_turn_id.is_none() {
        session.target_turn_id = events.iter().find_map(|event| match event {
            Event::AgentTurn(turn)
                if turn.state == AgentTurnState::Started
                    && session
                        .target_after_event_id
                        .is_none_or(|baseline| turn.id > baseline) =>
            {
                Some(turn.turn_id)
            }
            _ => None,
        });
    }
    let Some(target) = session.target_turn_id else {
        return Ok(None);
    };
    Ok(latest_agent_turn(events)?.filter(|turn| turn.turn_id == target))
}

fn terminal_output(
    events: &[Event],
    turn: &AgentTurnProjection,
    session_id: &str,
    progress: Vec<Value>,
    surface: ToolSurface,
) -> Value {
    let (state, final_answer, error) = match turn.state {
        AgentTurnState::Completed => (
            "completed",
            Some(final_answer(events, turn.prompt_id)),
            None,
        ),
        AgentTurnState::Interrupted => (
            "interrupted",
            None,
            terminal_api_failure(events, turn.prompt_id)
                .map(ToOwned::to_owned)
                .or_else(|| (!turn.detail.is_empty()).then(|| turn.detail.clone())),
        ),
        AgentTurnState::Failed => (
            "failed",
            None,
            Some(if turn.detail.is_empty() {
                format!("{} turn failed", surface.label())
            } else {
                turn.detail.clone()
            }),
        ),
        AgentTurnState::Started => unreachable!("terminal output requires terminal turn"),
    };
    json!({
        "session_id": session_id,
        "state": state,
        "turn_id": turn.turn_id,
        "final_answer": final_answer,
        "error": error,
        "progress": progress,
    })
}

fn askable_turn(events: &[Event], turn: &AgentTurnProjection, surface: ToolSurface) -> bool {
    match surface {
        ToolSurface::Worker => turn.state.is_terminal(),
        ToolSurface::Agent => {
            turn.state == AgentTurnState::Completed
                || (turn.state == AgentTurnState::Interrupted
                    && (turn_was_explicitly_stopped(events, turn.prompt_id)
                        || terminal_api_failure(events, turn.prompt_id).is_some()))
        }
    }
}

fn terminal_api_failure(events: &[Event], prompt_id: EventId) -> Option<&str> {
    let interrupted = events.iter().rev().find_map(|event| match event {
        Event::ApiStateUpdate(update)
            if update.prompt_id == prompt_id
                && matches!(update.state, ApiState::Completed | ApiState::Interrupted) =>
        {
            Some(update)
        }
        _ => None,
    })?;
    if interrupted.state != ApiState::Interrupted
        || !events.iter().any(|event| {
            matches!(
                event,
                Event::ApiStateUpdate(update)
                    if update.prompt_id == prompt_id
                        && update.api_call_id == interrupted.api_call_id
                        && update.state == ApiState::Error
            )
        })
    {
        return None;
    }
    Some(interrupted.detail.as_str())
}

fn turn_was_explicitly_stopped(events: &[Event], prompt_id: EventId) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            Event::UserTurnAborted(aborted) if aborted.prompt_id == prompt_id
        )
    })
}

fn wait_progress(events: &[Event], baseline: Option<EventId>) -> Result<Vec<Value>> {
    #[derive(Default)]
    struct ModelStep {
        assistant_text: String,
        tool_calls: Vec<String>,
    }

    let effective = effective_conversation_events(events)?;
    let mut active_calls = BTreeMap::<EventId, EventId>::new();
    let mut steps = BTreeMap::<EventId, ModelStep>::new();
    let mut tool_api_calls = BTreeMap::<EventId, EventId>::new();
    let mut observed = BTreeSet::new();

    for event in effective {
        match event {
            Event::ApiStateUpdate(update) => match update.state {
                ApiState::Requesting => {
                    active_calls.insert(update.prompt_id, update.api_call_id);
                }
                ApiState::Completed | ApiState::Error | ApiState::Interrupted => {
                    if active_calls.get(&update.prompt_id) == Some(&update.api_call_id) {
                        active_calls.remove(&update.prompt_id);
                    }
                }
                ApiState::Streaming | ApiState::Retrying => {}
            },
            Event::AssistResponse(response) => {
                let Some(&api_call_id) = active_calls.get(&response.prompt_id) else {
                    continue;
                };
                steps
                    .entry(api_call_id)
                    .or_default()
                    .assistant_text
                    .push_str(&response.content);
                if event_id_after(Some(response.id), baseline) {
                    observed.insert(api_call_id);
                }
            }
            Event::ToolCall(call) => {
                tool_api_calls.insert(call.id, call.api_call_id);
                steps
                    .entry(call.api_call_id)
                    .or_default()
                    .tool_calls
                    .push(call.name.clone());
                if event_id_after(Some(call.id), baseline) {
                    observed.insert(call.api_call_id);
                }
            }
            Event::ToolInfoUpdate(update) => {
                if event_id_after(Some(update.id), baseline)
                    && let Some(api_call_id) = tool_api_calls.get(&update.tool_call_id)
                {
                    observed.insert(*api_call_id);
                }
            }
            Event::ToolCallResult(result) => {
                if event_id_after(Some(result.id), baseline)
                    && let Some(api_call_id) = tool_api_calls.get(&result.tool_call_id)
                {
                    observed.insert(*api_call_id);
                }
            }
            _ => {}
        }
    }

    Ok(observed
        .into_iter()
        .filter_map(|api_call_id| steps.remove(&api_call_id))
        .filter(|step| !step.assistant_text.is_empty() || !step.tool_calls.is_empty())
        .map(|step| {
            json!({
                "assistant_text": step.assistant_text,
                "tool_calls": step.tool_calls,
            })
        })
        .collect())
}

fn final_answer(events: &[Event], prompt_id: EventId) -> String {
    let last_api_call = events.iter().rev().find_map(|event| match event {
        Event::ApiStateUpdate(update)
            if update.prompt_id == prompt_id
                && update.state == crate::event::ApiState::Requesting =>
        {
            Some(update.api_call_id)
        }
        _ => None,
    });
    events
        .iter()
        .filter_map(|event| match event {
            Event::AssistResponse(response)
                if response.prompt_id == prompt_id
                    && last_api_call.is_none_or(|api_call_id| response.id > api_call_id) =>
            {
                Some(response.content.as_str())
            }
            _ => None,
        })
        .collect()
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(
    arguments: &str,
) -> std::result::Result<T, ToolboxExecutionError> {
    serde_json::from_str(arguments)
        .map_err(|error| tool_error("invalid_arguments", error.to_string(), false))
}

fn parse_wait_input(arguments: &str) -> std::result::Result<WaitInput, ToolboxExecutionError> {
    let input: WaitInput = parse_arguments(arguments)?;
    if input.max_wait_ms > MAX_WAIT_MS {
        return Err(max_wait_error());
    }
    Ok(input)
}

fn max_wait_error() -> ToolboxExecutionError {
    tool_error(
        "invalid_arguments",
        format!("max_wait_ms must not exceed {MAX_WAIT_MS}"),
        false,
    )
}

fn session_not_found(session_id: &str, surface: ToolSurface) -> ToolboxExecutionError {
    tool_error(
        "session_not_found",
        match surface {
            ToolSurface::Agent => {
                format!(
                    "Agent session {session_id} does not exist; it may have been killed or deleted"
                )
            }
            ToolSurface::Worker => "the dedicated Worker is unavailable".into(),
        },
        false,
    )
}

fn execution_error(error: impl std::fmt::Display) -> ToolboxExecutionError {
    tool_error("execution_error", error.to_string(), true)
}

fn tool_error(
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
) -> ToolboxExecutionError {
    ToolboxExecutionError::Tool {
        code: code.into(),
        message: message.into(),
        retryable,
        tip: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        fs,
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        config::{ModelCapabilities, ProviderType, WorkspaceConfig},
        event::{AgentKind, EventDataBase, agent_kind_definition},
        workspace::{AgentId, Workspace},
    };

    fn read_complete_http_request(stream: &mut std::net::TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 8192];
        let (body_start, content_length) = loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "HTTP request ended before its headers");
            bytes.extend_from_slice(&chunk[..read]);
            let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .expect("model request must have Content-Length");
            break (header_end + 4, content_length);
        };
        while bytes.len() < body_start + content_length {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "HTTP request ended before its body");
            bytes.extend_from_slice(&chunk[..read]);
        }
    }

    #[test]
    fn catalog_exposes_agent_control_tools_without_automatic_lifecycle_language() {
        let (tools, (_, brief)) = catalog_parts();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.full_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                AGENT_CREATE,
                AGENT_WAIT,
                AGENT_ASK,
                AGENT_STOP,
                AGENT_CLEAR_CONTEXT,
                AGENT_KILL,
            ]
        );
        assert!(input_schema("Wait")["properties"].get("retain").is_none());
        assert!(
            output_schema("Wait")["required"]
                .as_array()
                .unwrap()
                .contains(&json!("context_usage"))
        );
        assert!(
            output_schema("Wait")["required"]
                .as_array()
                .unwrap()
                .contains(&json!("compact_count"))
        );
        assert!(
            output_schema("Wait")["properties"]["state"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("wait_interrupted"))
        );
        assert_eq!(
            input_schema("Wait")["properties"]["max_wait_ms"]["maximum"],
            MAX_WAIT_MS
        );
        let model_facing_text = tools.iter().fold(brief, |mut text, tool| {
            text.push_str(&tool.input_schema.to_string());
            text.push_str(&tool.output_schema.to_string());
            text.push_str(&tool.route);
            text.push_str(&tool.instructions);
            text.push_str(&tool.examples);
            text
        });
        for internal_term in [
            "EDB",
            "Workspace",
            "AgentRuntime",
            "ToolboxRuntime",
            "Orchestrator",
            "EventId",
        ] {
            assert!(
                !model_facing_text.contains(internal_term),
                "model-facing Agent prompt leaked internal term {internal_term}"
            );
        }
        for state in [
            "pending means",
            "working means",
            "completed returns",
            "interrupted means",
            "failed means",
            "wait_interrupted",
        ] {
            assert!(model_facing_text.contains(state));
        }
        for delegation_rule in [
            "bounded, separable task",
            "Do not delegate trivial work",
            "concrete objective and why it matters",
            "material facts already learned or ruled out",
            "whether it may modify files or should only investigate",
            "Do not delegate the missing understanding itself",
        ] {
            assert!(
                model_facing_text.contains(delegation_rule),
                "Agent prompt omitted {delegation_rule:?}"
            );
        }
        assert!(is_agent_tool(AGENT_STOP));
        assert!(is_agent_tool(AGENT_CLEAR_CONTEXT));
        assert!(
            !input_schema("Wait")["properties"]
                .as_object()
                .unwrap()
                .contains_key("retain")
        );
        assert!(
            !output_schema("Wait")["properties"]
                .as_object()
                .unwrap()
                .contains_key("retained")
        );
        assert!(model_facing_text.contains("model API failed"));
        assert!(model_facing_text.contains("ClearContext"));

        let wait = tools
            .iter()
            .find(|tool| tool.full_name == AGENT_WAIT)
            .unwrap();
        let wait_prompt = format!(
            "{}\n{}\n{}\n{}",
            wait.input_schema, wait.output_schema, wait.instructions, wait.route
        )
        .to_ascii_lowercase();
        for obsolete_lifecycle_term in ["retain", "destroy", "discard", "consume", "delete", "kill"]
        {
            assert!(
                !wait_prompt.contains(obsolete_lifecycle_term),
                "Agent.Wait prompt contains obsolete lifecycle term {obsolete_lifecycle_term:?}"
            );
        }
    }

    #[test]
    fn worker_catalog_targets_one_implicit_persistent_worker() {
        let (tools, (_, brief)) = worker_catalog_parts();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.full_name.as_str())
                .collect::<Vec<_>>(),
            vec![WORKER_ASK, WORKER_WAIT, WORKER_STOP, WORKER_CLEAR_CONTEXT,]
        );
        let text = tools.iter().fold(brief, |mut text, tool| {
            text.push_str(&tool.input_schema.to_string());
            text.push_str(&tool.output_schema.to_string());
            text.push_str(&tool.instructions);
            text.push_str(&tool.route);
            text.push_str(&tool.examples);
            text
        });
        assert!(!text.contains("session_id"));
        assert!(text.contains("persistent Worker"));
        assert!(text.contains("operational interface to the execution environment"));
        assert!(text.contains("Manager retains sole ownership"));
        assert!(text.contains(
            "A detailed requirement or desired behavior is not an authored implementation"
        ));
        assert!(text.contains("never ask the Worker to implement"));
        assert!(text.contains("specified review, acceptance, or other checks"));
        assert!(text.contains("may collect image evidence"));
        assert!(text.contains("never inspects image content"));
        assert!(text.contains("must never call Image tools"));
        assert!(text.contains("review or acceptance verdict"));
        assert!(text.contains("screenshot path"));
        assert!(text.contains("exact code or text already authored by the Manager"));
        assert!(text.contains("exact mechanical transformation"));
        assert!(text.contains("directory tree"));
        assert!(text.contains("function and method signatures"));
        assert!(text.contains("recoverable tool-protocol errors"));
        assert!(text.contains("externally interrupted"));
        assert!(text.contains("failed Worker can always accept another Ask"));
        assert!(text.contains("only a Worker whose operation is still active rejects Ask"));
        assert!(!text.contains("editing target code according to requirements"));
        assert!(!text.contains("implements the stated behavior"));
        assert!(!text.contains("bounded assignment"));
        assert!(!text.contains("make the bounded correction"));
        assert!(text.contains("latest known real context usage"));
        assert!(text.contains("successfully completed context compactions"));
        assert!(text.contains("available only while the Worker is idle"));
        assert!(text.contains("the entire WorkMap"));
        assert!(text.contains("previous session and page identifiers become invalid"));
        assert!(text.contains("System instructions and model settings remain active"));
        assert!(!text.contains("reply_id"));
        assert!(!text.contains("Recall"));
        assert!(!text.contains("bounded, independently closable piece of work is finished"));
        assert!(!text.contains("larger user task or project may still be in progress"));
        assert!(!text.contains("Keep context only throughout related steps"));
        assert!(text.contains("Never assume the Worker remembers"));
        assert!(text.contains("all applicable rules, prohibitions, scope boundaries"));
        assert!(text.contains("'as before'"));
        assert!(text.contains("every Ask must independently restate"));
        assert!(text.contains("one explicit, independently executable observation"));
        assert!(text.contains("all are already determined"));
        assert!(text.contains("select or continue a conditional branch"));
        assert!(text.contains("decide the next route yourself"));
        assert!(
            text.contains("Every listed observation is required regardless of earlier results")
        );
        assert!(!text.contains("current task is completely finished"));
        assert!(!text.contains("task has changed completely"));
        assert!(!text.contains("deliberately a low-frequency reset"));
        assert!(!text.contains("do not clear routinely"));
        assert!(text.contains("state=wait_interrupted"));
        assert!(text.contains("reason=follow_up"));
        assert!(text.contains("the Worker continues"));
        assert!(text.contains("explicit exception to the shared tool-result safety limit"));
        assert!(text.contains("always returned completely"));
        assert!(text.contains("top-level truncate field is therefore always false"));
        assert_eq!(
            worker_output_schema("Wait")["properties"]["final_answer"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            worker_input_schema("Wait")["properties"]["max_wait_ms"]["maximum"],
            MAX_WAIT_MS
        );
    }

    #[test]
    fn agent_and_worker_wait_and_clear_contracts_stay_aligned() {
        let agent_wait = output_schema("Wait");
        let worker_wait = worker_output_schema("Wait");
        assert_eq!(
            agent_wait["properties"]["state"]["enum"],
            worker_wait["properties"]["state"]["enum"]
        );
        for field in [
            "turn_id",
            "final_answer",
            "error",
            "progress",
            "context_usage",
            "compact_count",
        ] {
            assert!(agent_wait["properties"].get(field).is_some());
            assert!(worker_wait["properties"].get(field).is_some());
        }
        assert!(input_schema("Wait")["properties"].get("retain").is_none());
        assert!(
            worker_input_schema("Wait")["properties"]
                .get("retain")
                .is_none()
        );
        assert!(agent_wait["properties"].get("retained").is_none());
        assert!(worker_wait["properties"].get("retained").is_none());
        assert_eq!(
            output_schema("ClearContext")["properties"]["state"]["const"],
            worker_output_schema("ClearContext")["properties"]["state"]["const"]
        );
    }

    #[test]
    fn worker_tools_start_and_wait_for_the_implicit_persistent_worker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let release_response = Arc::new(AtomicBool::new(false));
        let server = {
            let release_response = Arc::clone(&release_response);
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_complete_http_request(&mut stream);
                while !release_response.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"worker result\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15}}\n\ndata: [DONE]\n\n")
                .unwrap();
            })
        };
        let directory = std::env::temp_dir().join(format!(
            "me-worker-toolbox-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
        let model = ModelConfig {
            name: "local".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: format!("http://{address}"),
            endpoint: "/chat/completions".into(),
            api_key: Some("test".into()),
            api_key_env: None,
            credential_file: None,
            model: "local".into(),
            source_url: None,
            timeout_seconds: 2,
            capabilities: ModelCapabilities {
                context_window: 4096,
                reasoning_efforts: vec!["unset".into()],
                streaming: true,
                ..Default::default()
            },
            parameters: toml::Table::new(),
            effort_parameters: BTreeMap::new(),
        };
        let config = WorkspaceConfig {
            version: 2,
            model: model.name.clone(),
            effort: "unset".into(),
            orchestrator: "manager-agent".into(),
        };
        let workspace = Workspace::open(&directory, config, vec![model.clone()]).unwrap();
        let parent = AgentId::new("main").unwrap();
        let worker = workspace.handle().child_agent_ids(&parent).unwrap()[0].clone();
        let mut toolbox = NativeAgentToolbox::new();
        toolbox.configure(workspace.handle(), parent);

        for (tool, arguments) in [
            (WORKER_ASK, r#"{"prompt":"work","unexpected":true}"#),
            (WORKER_WAIT, r#"{"max_wait_ms":0,"unexpected":true}"#),
            (WORKER_STOP, r#"{"unexpected":true}"#),
        ] {
            assert!(matches!(
                toolbox.execute_worker_cancellable(tool, arguments, &mut || false),
                Err(ToolboxExecutionError::Tool { code, retryable: false, .. })
                    if code == "invalid_arguments"
            ));
        }

        let asked = toolbox
            .execute_worker_cancellable(
                WORKER_ASK,
                r#"{"prompt":"perform the bounded check"}"#,
                &mut || false,
            )
            .unwrap();
        assert_eq!(asked, json!({"worker": "worker", "state": "working"}));
        let interrupted_wait = toolbox
            .execute_worker_cancellable_with_follow_up(
                WORKER_WAIT,
                r#"{"max_wait_ms":2000}"#,
                &mut || false,
                &mut || Ok(true),
            )
            .unwrap();
        assert_eq!(interrupted_wait["worker"], "worker");
        assert_eq!(interrupted_wait["state"], "wait_interrupted");
        assert_eq!(interrupted_wait["reason"], "follow_up");
        assert_eq!(interrupted_wait["final_answer"], Value::Null);
        assert!(matches!(
            toolbox.execute_worker_cancellable(
                WORKER_CLEAR_CONTEXT,
                r#"{}"#,
                &mut || false
            ),
            Err(ToolboxExecutionError::Tool { code, retryable: false, .. })
                if code == "worker_busy"
        ));
        release_response.store(true, Ordering::Release);
        let waited = toolbox
            .execute_worker_cancellable(WORKER_WAIT, r#"{"max_wait_ms":2000}"#, &mut || false)
            .unwrap();
        assert_eq!(waited["worker"], "worker");
        assert_eq!(waited["state"], "completed", "{waited}");
        assert_eq!(waited["final_answer"], "worker result");
        assert_eq!(
            waited["progress"],
            json!([{"assistant_text": "worker result", "tool_calls": []}])
        );
        assert_eq!(
            waited["context_usage"],
            json!({"input_tokens": 12, "output_tokens": 3, "total_tokens": 15})
        );
        assert_eq!(waited["compact_count"], 0);
        assert!(waited.get("retained").is_none());
        assert!(waited.get("session_id").is_none());
        assert!(workspace.contains(&worker));
        assert!(
            workspace
                .handle()
                .events(&worker)
                .unwrap()
                .iter()
                .any(|event| {
                    matches!(event, Event::ManagerPrompt(prompt)
                if prompt.content == "perform the bounded check")
                })
        );

        assert!(matches!(
            toolbox.execute_worker_cancellable(
                WORKER_CLEAR_CONTEXT,
                r#"{"unexpected":true}"#,
                &mut || false
            ),
            Err(ToolboxExecutionError::Tool { code, retryable: false, .. })
                if code == "invalid_arguments"
        ));
        let cleared = toolbox
            .execute_worker_cancellable(WORKER_CLEAR_CONTEXT, r#"{}"#, &mut || false)
            .unwrap();
        assert_eq!(cleared, json!({"worker": "worker", "state": "cleared"}));
        assert!(matches!(
            workspace.handle().events(&worker).unwrap().last(),
            Some(Event::ContextCleared(_))
        ));
        let after_clear = toolbox
            .execute_worker_cancellable(WORKER_WAIT, r#"{"max_wait_ms":0}"#, &mut || false)
            .unwrap();
        assert_eq!(after_clear["context_usage"], Value::Null);
        assert_eq!(after_clear["compact_count"], 0);

        server.join().unwrap();
        drop(toolbox);
        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn worker_can_continue_after_a_terminal_model_api_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            read_complete_http_request(&mut first);
            first
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"error\":\"offline\"}",
                )
                .unwrap();

            let (mut second, _) = listener.accept().unwrap();
            read_complete_http_request(&mut second);
            second
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: [DONE]\n\n")
                .unwrap();
        });
        let directory = std::env::temp_dir().join(format!(
            "me-worker-api-recovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
        let model = ModelConfig {
            name: "local".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: format!("http://{address}"),
            endpoint: "/chat/completions".into(),
            api_key: Some("test".into()),
            api_key_env: None,
            credential_file: None,
            model: "local".into(),
            source_url: None,
            timeout_seconds: 2,
            capabilities: ModelCapabilities {
                context_window: 4096,
                reasoning_efforts: vec!["unset".into()],
                streaming: true,
                ..Default::default()
            },
            parameters: toml::Table::new(),
            effort_parameters: BTreeMap::new(),
        };
        let config = WorkspaceConfig {
            version: 2,
            model: model.name.clone(),
            effort: "unset".into(),
            orchestrator: "manager-agent".into(),
        };
        let workspace = Workspace::open(&directory, config, vec![model]).unwrap();
        let parent = AgentId::new("main").unwrap();
        let mut toolbox = NativeAgentToolbox::new();
        toolbox.configure(workspace.handle(), parent);

        toolbox
            .execute_worker_cancellable(
                WORKER_ASK,
                r#"{"prompt":"perform the operation"}"#,
                &mut || false,
            )
            .unwrap();
        let failed = toolbox
            .execute_worker_cancellable(WORKER_WAIT, r#"{"max_wait_ms":2000}"#, &mut || false)
            .unwrap();
        assert_eq!(failed["state"], "interrupted");
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("400 Bad Request")
        );

        let retried = toolbox
            .execute_worker_cancellable(
                WORKER_ASK,
                r#"{"prompt":"retry the interrupted operation"}"#,
                &mut || false,
            )
            .unwrap();
        assert_eq!(retried, json!({"worker": "worker", "state": "working"}));
        let recovered = toolbox
            .execute_worker_cancellable(WORKER_WAIT, r#"{"max_wait_ms":2000}"#, &mut || false)
            .unwrap();
        assert_eq!(recovered["state"], "completed", "{recovered}");
        assert_eq!(recovered["final_answer"], "recovered");
        assert_eq!(
            recovered["progress"],
            json!([{"assistant_text": "recovered", "tool_calls": []}])
        );

        server.join().unwrap();
        drop(toolbox);
        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn worker_wait_protocol_has_no_lifecycle_controls() {
        let (tools, (_, brief)) = worker_catalog_parts();
        let wait = tools
            .iter()
            .find(|tool| tool.full_name == WORKER_WAIT)
            .unwrap();
        let visible_protocol = format!(
            "{brief}\n{}\n{}\n{}\n{}",
            wait.input_schema, wait.output_schema, wait.instructions, wait.route
        )
        .to_ascii_lowercase();

        assert!(!visible_protocol.contains("session_id"));
        assert!(!visible_protocol.contains("retain"));
        assert!(!visible_protocol.contains("destroy"));
        assert!(!visible_protocol.contains("discard"));
        assert_eq!(wait.input_schema["required"], json!(["max_wait_ms"]));
        assert!(
            wait.output_schema["properties"]["state"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("wait_interrupted"))
        );
        assert_eq!(
            wait.output_schema["properties"]["reason"]["enum"],
            json!(["follow_up", null])
        );
        assert!(
            wait.output_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("context_usage"))
        );
        assert!(
            wait.output_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("compact_count"))
        );
    }

    #[test]
    fn wait_accepts_up_to_one_hour_and_rejects_larger_limits() {
        assert_eq!(
            parse_wait_input(r#"{"session_id":"agent-test","max_wait_ms":3600000}"#)
                .unwrap()
                .max_wait_ms,
            MAX_WAIT_MS
        );
        assert!(matches!(
            parse_wait_input(r#"{"session_id":"agent-test","max_wait_ms":3600001}"#),
            Err(ToolboxExecutionError::Tool { code, retryable: false, .. })
                if code == "invalid_arguments"
        ));
        assert!(matches!(
            parse_wait_input(
                r#"{"session_id":"agent-test","max_wait_ms":0,"retain":true}"#
            ),
            Err(ToolboxExecutionError::Tool { code, retryable: false, .. })
                if code == "invalid_arguments"
        ));
    }

    #[test]
    fn wait_progress_returns_complete_assistant_text_and_only_tool_names() {
        use crate::event::{AgentTurnState, ApiState, ToolOutputStream};

        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("inspect").unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(api, prompt, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt, "我要查看一下", false)
            .unwrap();
        edb.append_assist_response(prompt, "工作区", true).unwrap();
        let tool = edb
            .append_tool_call(
                api,
                prompt,
                "provider-call",
                "File.List",
                r#"{"secret_argument":"must-not-leak"}"#,
            )
            .unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        let baseline = edb.events().last().map(Event::id);
        edb.append_tool_info(tool, ToolOutputStream::Stdout, "must-not-leak-output")
            .unwrap();

        let progress = wait_progress(edb.events(), baseline).unwrap();
        assert_eq!(
            progress,
            vec![json!({
                "assistant_text": "我要查看一下工作区",
                "tool_calls": ["File.List"],
            })]
        );
        let encoded = serde_json::to_string(&progress).unwrap();
        assert!(!encoded.contains("secret_argument"));
        assert!(!encoded.contains("must-not-leak-output"));
    }

    #[test]
    fn worker_wait_preserves_arbitrarily_long_messages_without_truncation() {
        use crate::event::{AgentTurnState, ApiState};

        let mut edb = EventDataBase::new();
        let prompt = edb
            .append_user_prompt("return the complete material")
            .unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(api, prompt, ApiState::Streaming, "")
            .unwrap();
        let baseline = edb.events().last().map(Event::id);
        let message = format!("BEGIN:{}:END", "0123456789abcdef".repeat(65_536));
        let split = message.len() / 2;
        edb.append_assist_response(prompt, &message[..split], false)
            .unwrap();
        edb.append_assist_response(prompt, &message[split..], true)
            .unwrap();

        let progress = wait_progress(edb.events(), baseline).unwrap();
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0]["assistant_text"], message);

        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Completed, "")
            .unwrap();
        assert_eq!(final_answer(edb.events(), prompt), message);
    }

    #[test]
    fn worker_ask_accepts_every_terminal_state_while_agent_ask_stays_restricted() {
        use crate::event::{AgentTurnState, ApiState};

        let mut stopped = EventDataBase::new();
        let prompt = stopped.append_user_prompt("stop").unwrap();
        stopped
            .append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        stopped.append_user_turn_aborted(prompt).unwrap();
        stopped
            .append_agent_turn(
                prompt,
                prompt,
                AgentTurnState::Interrupted,
                "user requested turn abort",
            )
            .unwrap();
        let turn = latest_agent_turn(stopped.events()).unwrap().unwrap();
        assert!(askable_turn(stopped.events(), &turn, ToolSurface::Agent));
        assert!(askable_turn(stopped.events(), &turn, ToolSurface::Worker));

        let mut unexpected = EventDataBase::new();
        let prompt = unexpected.append_user_prompt("interrupt").unwrap();
        unexpected
            .append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        unexpected
            .append_agent_turn(
                prompt,
                prompt,
                AgentTurnState::Interrupted,
                "connection lost",
            )
            .unwrap();
        let turn = latest_agent_turn(unexpected.events()).unwrap().unwrap();
        assert!(!askable_turn(
            unexpected.events(),
            &turn,
            ToolSurface::Agent
        ));
        assert!(askable_turn(
            unexpected.events(),
            &turn,
            ToolSurface::Worker
        ));

        let mut api_failed = EventDataBase::new();
        let prompt = api_failed.append_user_prompt("request").unwrap();
        api_failed
            .append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        let api_call = api_failed.append_api_requesting(prompt).unwrap();
        api_failed
            .append_api_state(api_call, prompt, ApiState::Error, "network unavailable")
            .unwrap();
        api_failed
            .append_api_state(
                api_call,
                prompt,
                ApiState::Interrupted,
                "API request interrupted after 6 attempts: network unavailable",
            )
            .unwrap();
        api_failed
            .append_agent_turn(
                prompt,
                prompt,
                AgentTurnState::Interrupted,
                "Agent turn did not complete normally",
            )
            .unwrap();
        let turn = latest_agent_turn(api_failed.events()).unwrap().unwrap();
        assert!(askable_turn(api_failed.events(), &turn, ToolSurface::Agent));
        assert!(askable_turn(
            api_failed.events(),
            &turn,
            ToolSurface::Worker
        ));
        assert_eq!(
            terminal_output(
                api_failed.events(),
                &turn,
                "worker",
                Vec::new(),
                ToolSurface::Worker,
            )["error"],
            "API request interrupted after 6 attempts: network unavailable"
        );

        let mut completed = EventDataBase::new();
        let prompt = completed.append_user_prompt("complete").unwrap();
        completed
            .append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        completed
            .append_agent_turn(prompt, prompt, AgentTurnState::Completed, "")
            .unwrap();
        let turn = latest_agent_turn(completed.events()).unwrap().unwrap();
        assert!(askable_turn(completed.events(), &turn, ToolSurface::Agent));
        assert!(askable_turn(completed.events(), &turn, ToolSurface::Worker));

        let mut failed = EventDataBase::new();
        let prompt = failed.append_user_prompt("failed").unwrap();
        failed
            .append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        failed
            .append_agent_turn(prompt, prompt, AgentTurnState::Failed, "runtime failed")
            .unwrap();
        let turn = latest_agent_turn(failed.events()).unwrap().unwrap();
        assert!(!askable_turn(failed.events(), &turn, ToolSurface::Agent));
        assert!(askable_turn(failed.events(), &turn, ToolSurface::Worker));
    }

    #[test]
    fn every_terminal_wait_result_keeps_its_incremental_progress() {
        for state in [
            AgentTurnState::Completed,
            AgentTurnState::Interrupted,
            AgentTurnState::Failed,
        ] {
            let mut edb = EventDataBase::new();
            let prompt = edb.append_user_prompt("work").unwrap();
            edb.append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
                .unwrap();
            edb.append_agent_turn(prompt, prompt, state, "terminal detail")
                .unwrap();
            let turn = latest_agent_turn(edb.events()).unwrap().unwrap();
            let progress = vec![json!({
                "assistant_text": "observed work",
                "tool_calls": ["File.Read", "Terminal.Interact"],
            })];
            let output = terminal_output(
                edb.events(),
                &turn,
                "worker",
                progress.clone(),
                ToolSurface::Worker,
            );
            assert_eq!(output["progress"], Value::Array(progress), "state={state}");
        }
    }

    #[test]
    fn final_answer_uses_only_the_last_model_request() {
        use crate::event::{ApiState, EventDataBase};
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("work").unwrap();
        let first = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(first, prompt, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt, "before tool", true)
            .unwrap();
        edb.append_api_state(first, prompt, ApiState::Completed, "")
            .unwrap();
        let second = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(second, prompt, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt, "final", true).unwrap();
        assert_eq!(final_answer(edb.events(), prompt), "final");
    }

    #[test]
    fn workspace_child_wait_ask_clear_stop_and_kill_share_the_normal_agent_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let first_request_received = Arc::new(AtomicBool::new(false));
        let release_first_response = Arc::new(AtomicBool::new(false));
        let stopped_request_received = Arc::new(AtomicBool::new(false));
        let release_stopped_response = Arc::new(AtomicBool::new(false));
        let server = {
            let first_request_received = Arc::clone(&first_request_received);
            let release_first_response = Arc::clone(&release_first_response);
            let stopped_request_received = Arc::clone(&stopped_request_received);
            let release_stopped_response = Arc::clone(&release_stopped_response);
            thread::spawn(move || {
                for attempt in 0..7 {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    read_complete_http_request(&mut stream);
                    if attempt == 0 {
                        first_request_received.store(true, Ordering::Release);
                        while !release_first_response.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                    if attempt == 3 {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"still working\"}}]}\n\n")
                            .unwrap();
                        stream.flush().unwrap();
                        stopped_request_received.store(true, Ordering::Release);
                        while !release_stopped_response.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(1));
                        }
                        stream
                            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"too late\"}}]}\n\ndata: [DONE]\n\n")
                            .unwrap();
                        stream.flush().unwrap();
                        continue;
                    }
                    if attempt == 5 {
                        stream
                            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"error\":\"offline\"}")
                            .unwrap();
                        stream.flush().unwrap();
                        continue;
                    }
                    let response: &[u8] = match attempt {
                        0 => b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"first answer\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":21,\"completion_tokens\":4,\"total_tokens\":25}}\n\ndata: [DONE]\n\n",
                        1 => b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"second answer\"}}]}\n\ndata: [DONE]\n\n",
                        2 => b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"third answer\"}}]}\n\ndata: [DONE]\n\n",
                        4 => b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"answer after stop\"}}]}\n\ndata: [DONE]\n\n",
                        _ => b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"answer after API recovery\"}}]}\n\ndata: [DONE]\n\n",
                    };
                    stream.write_all(response).unwrap();
                    stream.flush().unwrap();
                }
            })
        };

        let directory = std::env::temp_dir().join(format!(
            "me-agent-toolbox-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
        let model = ModelConfig {
            name: "local".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: format!("http://{address}"),
            endpoint: "/chat/completions".into(),
            api_key: Some("test".into()),
            api_key_env: None,
            credential_file: None,
            model: "local".into(),
            source_url: None,
            timeout_seconds: 2,
            capabilities: ModelCapabilities {
                context_window: 4096,
                reasoning_efforts: vec!["unset".into()],
                streaming: true,
                ..Default::default()
            },
            parameters: toml::Table::new(),
            effort_parameters: BTreeMap::new(),
        };
        let config = WorkspaceConfig {
            version: 2,
            model: model.name.clone(),
            effort: "unset".into(),
            orchestrator: "chatbot".into(),
        };
        let workspace = Workspace::open(&directory, config.clone(), vec![model.clone()]).unwrap();
        let parent = AgentId::new("main").unwrap();
        let mut toolbox = NativeAgentToolbox::new();
        toolbox.configure(workspace.handle(), parent.clone());

        let created = toolbox
            .execute(
                AGENT_CREATE,
                r#"{"system_prompt":"Be concise.","prompt":"first"}"#,
                &model,
                "unset",
            )
            .unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_owned();
        let child = AgentId::new(&session_id).unwrap();
        let child_path = workspace.edb_path(&child);
        let child_events = workspace.handle().events(&child).unwrap();
        let definition = agent_kind_definition(&child_events).unwrap();
        assert_eq!(definition.kind, AgentKind::SubAgent);
        assert_eq!(definition.parent_agent_id.as_deref(), Some("main"));
        assert_eq!(definition.system_prompt.as_deref(), Some("Be concise."));
        assert!(workspace.agent_ids().contains(&child));
        assert!(child_path.exists());

        let deadline = Instant::now() + Duration::from_secs(2);
        while !first_request_received.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "child request did not start");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            workspace
                .handle()
                .events(&child)
                .unwrap()
                .iter()
                .any(|event| {
                    matches!(event, Event::ParentAgentPrompt(prompt) if prompt.content == "first")
                })
        );
        let first_poll = toolbox
            .execute(
                AGENT_WAIT,
                &json!({"session_id": session_id, "max_wait_ms": 0}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(first_poll["state"], "working");
        let idle_poll = toolbox
            .execute(
                AGENT_WAIT,
                &json!({"session_id": session_id, "max_wait_ms": 0}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(idle_poll["state"], "pending");
        let follow_up_poll = toolbox
            .execute_cancellable_with_follow_up(
                AGENT_WAIT,
                &json!({"session_id": session_id, "max_wait_ms": 2_000}).to_string(),
                &model,
                "unset",
                &mut || false,
                &mut || Ok(true),
            )
            .unwrap();
        assert_eq!(follow_up_poll["state"], "wait_interrupted");
        assert_eq!(follow_up_poll["reason"], "follow_up");
        assert_eq!(follow_up_poll["context_usage"], Value::Null);
        assert_eq!(follow_up_poll["compact_count"], 0);
        assert!(workspace.contains(&child));
        assert!(matches!(
            toolbox.execute(
                AGENT_CLEAR_CONTEXT,
                &json!({"session_id": session_id}).to_string(),
                &model,
                "unset",
            ),
            Err(ToolboxExecutionError::Tool { code, retryable: false, .. })
                if code == "agent_busy"
        ));
        assert!(matches!(
            toolbox.execute(
                AGENT_ASK,
                &json!({"session_id": session_id, "prompt": "too early"}).to_string(),
                &model,
                "unset",
            ),
            Err(ToolboxExecutionError::Tool { code, .. }) if code == "agent_busy"
        ));

        release_first_response.store(true, Ordering::Release);
        let first = toolbox
            .execute(
                AGENT_WAIT,
                &json!({
                    "session_id": session_id,
                    "max_wait_ms": 2_000
                })
                .to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(first["state"], "completed");
        assert_eq!(
            first["final_answer"],
            "first answer",
            "events: {:#?}",
            workspace.handle().events(&child).unwrap()
        );
        assert!(first.get("retained").is_none());
        assert_eq!(
            first["context_usage"],
            json!({"input_tokens": 21, "output_tokens": 4, "total_tokens": 25})
        );
        assert_eq!(first["compact_count"], 0);
        assert!(
            workspace
                .deletion_blocker(&parent)
                .unwrap()
                .is_some_and(|reason| reason.contains("子 Agent"))
        );

        drop(toolbox);
        drop(workspace);
        let workspace = Workspace::open(&directory, config, vec![model.clone()]).unwrap();
        assert!(workspace.contains(&child));
        let mut toolbox = NativeAgentToolbox::new();
        toolbox.configure(workspace.handle(), parent);

        toolbox
            .execute(
                AGENT_ASK,
                &json!({"session_id": session_id, "prompt": "second"}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        let second = toolbox
            .execute(
                AGENT_WAIT,
                &json!({"session_id": session_id, "max_wait_ms": 2_000}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(second["state"], "completed");
        assert_eq!(second["final_answer"], "second answer");
        assert_eq!(
            workspace
                .handle()
                .events(&child)
                .unwrap()
                .iter()
                .filter(|event| matches!(event, Event::ParentAgentPrompt(_)))
                .count(),
            2
        );
        assert!(second.get("retained").is_none());
        assert!(workspace.contains(&child));
        assert!(child_path.exists());

        let cleared = toolbox
            .execute(
                AGENT_CLEAR_CONTEXT,
                &json!({"session_id": session_id}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(cleared["state"], "cleared");
        assert!(matches!(
            workspace.handle().events(&child).unwrap().last(),
            Some(Event::ContextCleared(_))
        ));
        assert!(matches!(
            toolbox.execute(
                AGENT_CLEAR_CONTEXT,
                &json!({"session_id": session_id, "unexpected": true}).to_string(),
                &model,
                "unset",
            ),
            Err(ToolboxExecutionError::Tool { code, retryable: false, .. })
                if code == "invalid_arguments"
        ));
        let after_clear = toolbox
            .execute(
                AGENT_WAIT,
                &json!({"session_id": session_id, "max_wait_ms": 0}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(after_clear["context_usage"], Value::Null);
        let killed_second = toolbox
            .execute(
                AGENT_KILL,
                &json!({"session_id": session_id}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(killed_second["state"], "killed");
        assert!(!workspace.contains(&child));
        assert!(!child_path.exists());

        let third_created = toolbox
            .execute(AGENT_CREATE, r#"{"prompt":"third"}"#, &model, "unset")
            .unwrap();
        let third_id = third_created["session_id"].as_str().unwrap().to_owned();
        let third = toolbox
            .execute(
                AGENT_WAIT,
                &json!({
                    "session_id": third_id,
                    "max_wait_ms": 2_000
                })
                .to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(third["final_answer"], "third answer");
        let third_agent = AgentId::new(&third_id).unwrap();
        let third_path = workspace.edb_path(&third_agent);
        assert!(third_path.exists());
        let killed = toolbox
            .execute(
                AGENT_KILL,
                &json!({"session_id": third_id}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(killed["state"], "killed");
        assert!(!workspace.contains(&third_agent));
        assert!(!third_path.exists());

        let stopped_created = toolbox
            .execute(
                AGENT_CREATE,
                r#"{"prompt":"stop this request"}"#,
                &model,
                "unset",
            )
            .unwrap();
        let stopped_id = stopped_created["session_id"].as_str().unwrap().to_owned();
        let stopped_agent = AgentId::new(&stopped_id).unwrap();
        let stopped_path = workspace.edb_path(&stopped_agent);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !stopped_request_received.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "stoppable request did not start");
            thread::sleep(Duration::from_millis(1));
        }
        let release = Arc::clone(&release_stopped_response);
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            release.store(true, Ordering::Release);
        });
        let stopped = toolbox
            .execute(
                AGENT_STOP,
                &json!({"session_id": stopped_id}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        releaser.join().unwrap();
        assert_eq!(stopped["state"], "stopped");
        assert!(stopped.get("retained").is_none());
        assert!(workspace.contains(&stopped_agent));
        assert!(stopped_path.exists());
        let stopped_events = workspace.handle().events(&stopped_agent).unwrap();
        let stopped_turn = latest_agent_turn(&stopped_events).unwrap().unwrap();
        assert_eq!(stopped_turn.state, AgentTurnState::Interrupted);
        assert!(turn_was_explicitly_stopped(
            &stopped_events,
            stopped_turn.prompt_id
        ));

        toolbox
            .execute(
                AGENT_ASK,
                &json!({"session_id": stopped_id, "prompt": "continue after stop"}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        let after_stop = toolbox
            .execute(
                AGENT_WAIT,
                &json!({"session_id": stopped_id, "max_wait_ms": 2_000}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(after_stop["state"], "completed");
        assert_eq!(after_stop["final_answer"], "answer after stop");
        assert!(workspace.contains(&stopped_agent));
        assert!(stopped_path.exists());
        toolbox
            .execute(
                AGENT_KILL,
                &json!({"session_id": stopped_id}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert!(!workspace.contains(&stopped_agent));
        assert!(!stopped_path.exists());

        let failed_created = toolbox
            .execute(
                AGENT_CREATE,
                r#"{"prompt":"fail through the model API"}"#,
                &model,
                "unset",
            )
            .unwrap();
        let failed_id = failed_created["session_id"].as_str().unwrap().to_owned();
        let failed = toolbox
            .execute(
                AGENT_WAIT,
                &json!({"session_id": failed_id, "max_wait_ms": 2_000}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(failed["state"], "interrupted");
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("400 Bad Request")
        );
        toolbox
            .execute(
                AGENT_ASK,
                &json!({"session_id": failed_id, "prompt": "retry after the API failure"})
                    .to_string(),
                &model,
                "unset",
            )
            .unwrap();
        let recovered = toolbox
            .execute(
                AGENT_WAIT,
                &json!({"session_id": failed_id, "max_wait_ms": 2_000}).to_string(),
                &model,
                "unset",
            )
            .unwrap();
        assert_eq!(recovered["state"], "completed");
        assert_eq!(recovered["final_answer"], "answer after API recovery");
        toolbox
            .execute(
                AGENT_KILL,
                &json!({"session_id": failed_id}).to_string(),
                &model,
                "unset",
            )
            .unwrap();

        server.join().unwrap();
        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }
}
