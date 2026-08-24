use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fmt::Write as _,
    io::{self, Stdout, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    Result,
    agent_markdown_renderer::{
        self, ColorRole as MarkdownColorRole, StyledSpan as MarkdownSpan,
        TextStyle as MarkdownTextStyle,
    },
    event::{
        AgentKind, AgentTurnState, ApiState, ApiUsage, CompactState, EdbMutation, Event,
        EventDataBase, EventId, ModelChangeCause, ReasoningEffortChangeCause, ToolInfoContent,
        ToolOutputStream, ToolResultState, effective_conversation_events, effective_ui_events,
        latest_agent_turn, latest_context_usage,
    },
    orchestrator::{UserTurnState, current_user_turn_state},
    terminal::{TerminalColor, TerminalFrame, TerminalSessionPreview, TerminalStyle},
    turn_history,
    ui_backend::{
        ChatToolPresentation, UiAgentSnapshot, UiApiActivity, UiBackend, UiCommand,
        UiCommandGateway, UiCommandReceipt, UiModelOption, UiSnapshot, tool_chat_presentation,
    },
    workmap::{
        MemoryBasis, MemoryKind, MemoryState, NoteKind, ObjectiveState, PlanState, WorkMapMemory,
        WorkMapObjectiveSnapshot, WorkMapPlanSnapshot, WorkMapProjection, WorkMapSnapshot,
    },
    workspace::AgentId,
};

#[cfg(test)]
use crate::event::CompactKind;
use chrono::{DateTime, Local, Utc};
use crossterm::{
    cursor::{Hide, MoveDown, MoveTo, MoveToColumn, MoveUp, RestorePosition, SavePosition, Show},
    event::{self, Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatBlockKind {
    User,
    Assistant,
    TurnToolbar,
    ToolCall,
    WorkerActivity,
    SessionState,
    StateNotice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub kind: ChatBlockKind,
    pub content: String,
    pub timestamp_ms: u64,
    pub tool: Option<ToolCard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCard {
    pub id: EventId,
    pub api_call_id: EventId,
    pub name: String,
    pub arguments: String,
    pub started_at_ms: u64,
    pub queued: bool,
    pub session_id: Option<String>,
    pub output: String,
    pub result: Option<ToolCardResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCardResult {
    pub state: ToolResultState,
    pub exit_code: Option<i32>,
    pub detail: String,
    pub finished_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerActivityState {
    Running,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkerActivity {
    state: WorkerActivityState,
    tools: Vec<ToolCard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompactUiActivity {
    compact_id: EventId,
    total_stages: usize,
    stage: usize,
    message_index: usize,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ChatProjection {
    next_order: usize,
    last_event_id: Option<EventId>,
    active_assistant: Option<(EventId, usize)>,
    active_tools: BTreeMap<EventId, usize>,
    turn_started_at: BTreeMap<EventId, u64>,
    turn_context_baseline: BTreeMap<EventId, Option<u64>>,
    last_assistant_by_prompt: BTreeMap<EventId, usize>,
    completed_api_usage: BTreeMap<EventId, (EventId, Option<ApiUsage>)>,
    errored_api_calls: BTreeSet<EventId>,
    completed_compact_tools: BTreeSet<EventId>,
    hidden_tools: BTreeSet<EventId>,
    worker_activities: BTreeMap<EventId, WorkerActivity>,
    compact_activity: Option<CompactUiActivity>,
    pub messages: Vec<ChatMessage>,
    pub api_state: Option<ApiState>,
    pub api_usage: Option<ApiUsage>,
    pub model_name: Option<String>,
    pub effort: Option<String>,
}

impl ChatProjection {
    fn replay_events(events: &[Event]) -> Result<Self> {
        let mut projection = Self::default();
        let effective = effective_ui_events(events)?;
        projection.completed_compact_tools = effective
            .iter()
            .filter_map(|event| match event {
                Event::CompactStateUpdate(update) if update.state == CompactState::Completed => {
                    Some(update.tool_call_id)
                }
                _ => None,
            })
            .collect();
        projection.hidden_tools = effective
            .iter()
            .filter_map(|event| match event {
                Event::ToolCall(call)
                    if tool_chat_presentation(&call.name) == ChatToolPresentation::Hidden =>
                {
                    Some(call.id)
                }
                _ => None,
            })
            .collect();
        for event in &effective {
            projection.project_event(event)?;
        }
        let latest_model = events.iter().rev().find_map(|event| match event {
            Event::ModelChanged(changed) => Some(changed),
            _ => None,
        });
        projection.model_name = latest_model.map(|changed| changed.model.clone());
        projection.api_usage =
            latest_model.and_then(|changed| effective_api_usage(&effective, changed.id));
        projection.effort = events.iter().rev().find_map(|event| match event {
            Event::ReasoningEffortChanged(changed) => Some(changed.effort.clone()),
            _ => None,
        });
        projection.next_order = events.len();
        projection.last_event_id = events.last().map(Event::id);
        Ok(projection)
    }

    fn update_worker_activities(&mut self, worker_events: &[Event]) {
        self.worker_activities.clear();
        for message in &self.messages {
            if message.kind != ChatBlockKind::WorkerActivity {
                continue;
            }
            let Some(wait) = message.tool.as_ref() else {
                continue;
            };
            if let Some(activity) = project_worker_activity(worker_events, wait) {
                self.worker_activities.insert(wait.id, activity);
            }
        }
    }

    pub fn consume(&mut self, edb: &EventDataBase) -> Result<()> {
        self.consume_events(edb.events())
    }

    pub fn consume_events(&mut self, events: &[Event]) -> Result<()> {
        let start = self.next_order;
        let prefix_changed = start > events.len()
            || (start > 0 && events.get(start - 1).map(Event::id) != self.last_event_id);
        if prefix_changed {
            *self = Self::replay_events(events)?;
            return Ok(());
        }
        if events.get(start..).is_some_and(|events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    Event::ContextCleared(_)
                        | Event::CompactStateUpdate(crate::event::CompactStateUpdateEvent {
                            state: CompactState::Completed,
                            ..
                        })
                        | Event::ApiStateUpdate(crate::event::ApiStateUpdateEvent {
                            state: ApiState::Error,
                            ..
                        })
                )
            })
        }) {
            *self = Self::replay_events(events)?;
            return Ok(());
        }
        while let Some(event) = events.get(self.next_order) {
            self.project_event(event)?;
            self.next_order += 1;
            self.last_event_id = Some(event.id());
        }
        Ok(())
    }

    fn project_event(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::AgentKindDef(_)
            | Event::SystemPrompt(_)
            | Event::ModelContextItem(_)
            | Event::UserTurnAborted(_)
            | Event::WorkMapMutation(_)
            | Event::WorkMapPendingReminder(_)
            | Event::AgentTitleChanged(_)
            | Event::ImageContent(_) => {}
            Event::AgentTurn(turn) => {
                if turn.state == AgentTurnState::Completed
                    && let Some(started_at_ms) = self.turn_started_at.get(&turn.prompt_id)
                    && let Some(&assistant_index) =
                        self.last_assistant_by_prompt.get(&turn.prompt_id)
                    && assistant_index + 1 == self.messages.len()
                    && message_is_visible(&self.messages[assistant_index])
                {
                    let baseline = self
                        .turn_context_baseline
                        .get(&turn.prompt_id)
                        .copied()
                        .flatten();
                    let tokens = completed_turn_context_growth(
                        &self.completed_api_usage,
                        turn.prompt_id,
                        baseline,
                    );
                    self.messages.push(ChatMessage {
                        kind: ChatBlockKind::TurnToolbar,
                        content: format!(
                            "{} · {}",
                            format_turn_elapsed(turn.timestamp_ms.saturating_sub(*started_at_ms),),
                            format_turn_tokens(tokens),
                        ),
                        timestamp_ms: turn.timestamp_ms,
                        tool: None,
                    });
                }
            }
            Event::ModelChanged(changed) => {
                self.model_name = Some(changed.model.clone());
                self.api_usage = None;
                if changed.cause != ModelChangeCause::Initial {
                    self.messages.push(ChatMessage {
                        kind: ChatBlockKind::StateNotice,
                        content: format!("模型已变更为 {}", changed.model),
                        timestamp_ms: changed.timestamp_ms,
                        tool: None,
                    });
                }
            }
            Event::UserPrompt(prompt) => {
                self.turn_started_at.insert(prompt.id, prompt.timestamp_ms);
                self.turn_context_baseline
                    .insert(prompt.id, self.api_usage.map(|usage| usage.total_tokens));
                self.messages.push(ChatMessage {
                    kind: ChatBlockKind::User,
                    content: prompt.content.clone(),
                    timestamp_ms: prompt.timestamp_ms,
                    tool: None,
                });
                self.active_assistant = None;
            }
            Event::ManagerPrompt(prompt) => {
                self.turn_started_at.insert(prompt.id, prompt.timestamp_ms);
                self.turn_context_baseline
                    .insert(prompt.id, self.api_usage.map(|usage| usage.total_tokens));
                self.messages.push(ChatMessage {
                    kind: ChatBlockKind::User,
                    content: prompt.content.clone(),
                    timestamp_ms: prompt.timestamp_ms,
                    tool: None,
                });
                self.active_assistant = None;
            }
            Event::ParentAgentPrompt(prompt) => {
                self.turn_started_at.insert(prompt.id, prompt.timestamp_ms);
                self.turn_context_baseline
                    .insert(prompt.id, self.api_usage.map(|usage| usage.total_tokens));
                self.messages.push(ChatMessage {
                    kind: ChatBlockKind::User,
                    content: prompt.content.clone(),
                    timestamp_ms: prompt.timestamp_ms,
                    tool: None,
                });
                self.active_assistant = None;
            }
            Event::FollowUpPrompt(prompt) => {
                self.messages.push(ChatMessage {
                    kind: ChatBlockKind::User,
                    content: prompt.content.clone(),
                    timestamp_ms: prompt.timestamp_ms,
                    tool: None,
                });
                self.active_assistant = None;
            }
            Event::AssistResponse(response) => {
                if !response.content.is_empty() {
                    let index = match self.active_assistant {
                        Some((prompt_id, index)) if prompt_id == response.prompt_id => index,
                        _ => {
                            self.messages.push(ChatMessage {
                                kind: ChatBlockKind::Assistant,
                                content: String::new(),
                                timestamp_ms: response.timestamp_ms,
                                tool: None,
                            });
                            let index = self.messages.len() - 1;
                            self.active_assistant = Some((response.prompt_id, index));
                            self.last_assistant_by_prompt
                                .insert(response.prompt_id, index);
                            index
                        }
                    };
                    self.messages[index].content.push_str(&response.content);
                }
                if response.finished {
                    self.active_assistant = None;
                }
            }
            Event::ApiStateUpdate(update) => {
                self.api_state = Some(update.state);
                match update.state {
                    ApiState::Requesting => {}
                    ApiState::Completed => {
                        self.api_usage = update.usage;
                        self.completed_api_usage
                            .insert(update.api_call_id, (update.prompt_id, update.usage));
                    }
                    ApiState::Error => {
                        self.errored_api_calls.insert(update.api_call_id);
                        self.messages.push(ChatMessage {
                            kind: ChatBlockKind::StateNotice,
                            content: format!("API 错误：{}", update.detail),
                            timestamp_ms: update.timestamp_ms,
                            tool: None,
                        });
                    }
                    ApiState::Retrying => {
                        self.messages.push(ChatMessage {
                            kind: ChatBlockKind::StateNotice,
                            content: format!(
                                "API 正在重试 {}/{}",
                                update.retry_count, update.retry_limit
                            ),
                            timestamp_ms: update.timestamp_ms,
                            tool: None,
                        });
                    }
                    ApiState::Interrupted => {
                        let closes_failed_attempt =
                            self.errored_api_calls.contains(&update.api_call_id);
                        if !closes_failed_attempt {
                            self.api_usage = update.usage;
                        }
                        if closes_failed_attempt {
                            self.messages.push(ChatMessage {
                                kind: ChatBlockKind::StateNotice,
                                content: format!("API 已中断：{}", update.detail),
                                timestamp_ms: update.timestamp_ms,
                                tool: None,
                            });
                        }
                    }
                    ApiState::Streaming => {}
                }
            }
            Event::ContextUsageEstimate(_) => {}
            Event::ToolCall(call) => {
                let presentation = tool_chat_presentation(&call.name);
                if presentation == ChatToolPresentation::Hidden {
                    self.hidden_tools.insert(call.id);
                    return Ok(());
                }
                if self.completed_compact_tools.contains(&call.id) {
                    return Ok(());
                }
                let queued = self.active_tools.values().any(|&index| {
                    self.messages[index]
                        .tool
                        .as_ref()
                        .is_some_and(|tool| tool.api_call_id == call.api_call_id)
                });
                self.messages.push(ChatMessage {
                    kind: if presentation == ChatToolPresentation::WorkerActivity {
                        ChatBlockKind::WorkerActivity
                    } else {
                        ChatBlockKind::ToolCall
                    },
                    content: String::new(),
                    timestamp_ms: call.timestamp_ms,
                    tool: Some(ToolCard {
                        id: call.id,
                        api_call_id: call.api_call_id,
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        started_at_ms: call.timestamp_ms,
                        queued,
                        session_id: terminal_argument(&call.arguments, "session_id"),
                        output: String::new(),
                        result: None,
                    }),
                });
                self.active_tools.insert(call.id, self.messages.len() - 1);
            }
            Event::ToolInfoUpdate(info) => {
                if self.hidden_tools.contains(&info.tool_call_id) {
                    return Ok(());
                }
                if self.completed_compact_tools.contains(&info.tool_call_id) {
                    return Ok(());
                }
                let Some(index) = self.active_tools.get(&info.tool_call_id).copied() else {
                    return Err(format!(
                        "tool info {} references unknown call {}",
                        info.id, info.tool_call_id
                    )
                    .into());
                };
                let tool = self.messages[index]
                    .tool
                    .as_mut()
                    .ok_or_else(|| format!("tool message {index} has no tool card"))?;
                let (replace, content) = projected_tool_output(info.stream, &info.content);
                if replace {
                    tool.output.clear();
                }
                tool.output.push_str(&content);
            }
            Event::ToolCallResult(result) => {
                if self.hidden_tools.contains(&result.tool_call_id) {
                    return Ok(());
                }
                if self.completed_compact_tools.contains(&result.tool_call_id) {
                    return Ok(());
                }
                let Some(index) = self.active_tools.remove(&result.tool_call_id) else {
                    return Err(format!(
                        "tool result {} references unknown call {}",
                        result.id, result.tool_call_id
                    )
                    .into());
                };
                let tool = self.messages[index]
                    .tool
                    .as_mut()
                    .ok_or_else(|| format!("tool message {index} has no tool card"))?;
                if tool.session_id.is_none() && tool.name == "Terminal.Create" {
                    tool.session_id = serde_json::from_str::<serde_json::Value>(&result.detail)
                        .ok()
                        .and_then(|detail| {
                            detail
                                .get("session_id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        });
                }
                tool.result = Some(ToolCardResult {
                    state: result.state,
                    exit_code: result.exit_code,
                    detail: result.detail.clone(),
                    finished_at_ms: result.timestamp_ms,
                });
                let api_call_id = tool.api_call_id;
                if let Some(next_index) = self.active_tools.values().copied().find(|&index| {
                    self.messages[index]
                        .tool
                        .as_ref()
                        .is_some_and(|tool| tool.api_call_id == api_call_id && tool.queued)
                }) && let Some(next) = self.messages[next_index].tool.as_mut()
                {
                    next.queued = false;
                    next.started_at_ms = result.timestamp_ms;
                }
            }
            Event::TerminalSessionCreated(created) => {
                let Some(index) = self.active_tools.get(&created.tool_call_id).copied() else {
                    return Err(format!(
                        "terminal session {} references unknown call {}",
                        created.id, created.tool_call_id
                    )
                    .into());
                };
                let tool = self.messages[index]
                    .tool
                    .as_mut()
                    .ok_or_else(|| format!("tool message {index} has no tool card"))?;
                tool.session_id = Some(created.session_id.clone());
            }
            Event::TerminalSessionState(update) => {
                self.messages.push(ChatMessage {
                    kind: ChatBlockKind::SessionState,
                    content: format!(
                        "Session {} {} · exit_code={:?} · {}",
                        update.session_id, update.state, update.exit_code, update.detail
                    ),
                    timestamp_ms: update.timestamp_ms,
                    tool: None,
                });
            }
            Event::ReasoningEffortChanged(changed) => {
                self.effort = Some(changed.effort.clone());
                if changed.cause != ReasoningEffortChangeCause::Initial {
                    self.messages.push(ChatMessage {
                        kind: ChatBlockKind::StateNotice,
                        content: if changed.cause == ReasoningEffortChangeCause::ModelUnsupported {
                            "思考强度不支持，已退回 unset".to_owned()
                        } else {
                            format!("effort 已变更为 {}", changed.effort)
                        },
                        timestamp_ms: changed.timestamp_ms,
                        tool: None,
                    });
                }
            }
            Event::ContextCleared(cleared) => {
                self.messages.push(ChatMessage {
                    kind: ChatBlockKind::StateNotice,
                    content: "上下文已清空".to_owned(),
                    timestamp_ms: cleared.timestamp_ms,
                    tool: None,
                });
            }
            Event::CompactStateUpdate(update) => match update.state {
                CompactState::Started => self.begin_compact_activity(update),
                CompactState::StageCompleted => self.advance_compact_activity(update),
                CompactState::Completed => {
                    self.api_usage = None;
                    self.turn_context_baseline.insert(update.prompt_id, None);
                    self.completed_api_usage
                        .retain(|_, (prompt_id, _)| *prompt_id != update.prompt_id);
                    self.finish_compact_activity(update, "上下文已压缩");
                }
                CompactState::Failed => self.finish_compact_activity(update, "压缩失败"),
                CompactState::Interrupted => self.finish_compact_activity(update, "压缩中断"),
            },
            Event::CloneCompleted(completed) => {
                self.messages.push(ChatMessage {
                    kind: ChatBlockKind::StateNotice,
                    content: format!("克隆完成。新会话：{}", completed.title),
                    timestamp_ms: completed.timestamp_ms,
                    tool: None,
                });
            }
        }
        Ok(())
    }

    fn begin_compact_activity(&mut self, update: &crate::event::CompactStateUpdateEvent) {
        let message_index = self.messages.len();
        self.messages.push(ChatMessage {
            kind: ChatBlockKind::StateNotice,
            content: compact_progress_text(usize::from(update.total_stages), 1, None),
            timestamp_ms: update.timestamp_ms,
            tool: None,
        });
        self.compact_activity = Some(CompactUiActivity {
            compact_id: update.compact_id,
            total_stages: usize::from(update.total_stages),
            stage: 1,
            message_index,
        });
    }

    fn advance_compact_activity(&mut self, update: &crate::event::CompactStateUpdateEvent) {
        let Some(activity) = self
            .compact_activity
            .as_mut()
            .filter(|activity| activity.compact_id == update.compact_id)
        else {
            return;
        };
        activity.stage = (activity.stage + 1).min(activity.total_stages);
        self.refresh_compact_activity();
    }

    fn refresh_compact_activity(&mut self) {
        let Some(activity) = self.compact_activity.as_ref() else {
            return;
        };
        if let Some(message) = self.messages.get_mut(activity.message_index) {
            message.content = compact_progress_text(activity.total_stages, activity.stage, None);
        }
    }

    fn apply_api_activity(&mut self, api_activity: UiApiActivity) -> bool {
        let Some(activity) = self.compact_activity.as_ref() else {
            return false;
        };
        let received_sse_events = api_activity
            .active
            .then_some(api_activity.received_sse_events);
        let content =
            compact_progress_text(activity.total_stages, activity.stage, received_sse_events);
        let Some(message) = self.messages.get_mut(activity.message_index) else {
            return false;
        };
        if message.content == content {
            return false;
        }
        message.content = content;
        true
    }

    fn finish_compact_activity(
        &mut self,
        update: &crate::event::CompactStateUpdateEvent,
        content: &str,
    ) {
        if let Some(activity) = self
            .compact_activity
            .take()
            .filter(|activity| activity.compact_id == update.compact_id)
        {
            if let Some(message) = self.messages.get_mut(activity.message_index) {
                message.content = content.to_owned();
                message.timestamp_ms = update.timestamp_ms;
            }
        } else {
            self.messages.push(ChatMessage {
                kind: ChatBlockKind::StateNotice,
                content: content.to_owned(),
                timestamp_ms: update.timestamp_ms,
                tool: None,
            });
        }
    }
}

fn compact_progress_text(
    total_stages: usize,
    stage: usize,
    received_sse_events: Option<u64>,
) -> String {
    let progress = format!(
        "正在压缩 ({}/{}) ...",
        stage.min(total_stages),
        total_stages,
    );
    received_sse_events.map_or(progress.clone(), |events| format!("{progress} ↓ {events}"))
}

fn project_worker_activity(events: &[Event], wait: &ToolCard) -> Option<WorkerActivity> {
    let cutoff_ms = wait
        .result
        .as_ref()
        .map(|result| result.finished_at_ms)
        .unwrap_or(u64::MAX);
    let target_turn_id = worker_wait_turn_id(wait);
    let prompt_index = target_turn_id
        .and_then(|turn_id| {
            events.iter().position(
                |event| matches!(event, Event::ManagerPrompt(prompt) if prompt.id == turn_id),
            )
        })
        .or_else(|| {
            events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| match event {
                    Event::ManagerPrompt(prompt) if prompt.timestamp_ms <= cutoff_ms => Some(index),
                    _ => None,
                })
                .next_back()
        })?;
    let end = events[prompt_index + 1..]
        .iter()
        .position(|event| matches!(event, Event::ManagerPrompt(_)))
        .map(|offset| prompt_index + 1 + offset)
        .unwrap_or(events.len());
    let tools = project_worker_turn_tools(&events[prompt_index..end]).ok()?;
    Some(WorkerActivity {
        state: worker_wait_state(wait),
        tools,
    })
}

fn project_worker_turn_tools(events: &[Event]) -> Result<Vec<ToolCard>> {
    let errored_api_calls = events
        .iter()
        .filter_map(|event| match event {
            Event::ApiStateUpdate(update) if update.state == ApiState::Error => {
                Some(update.api_call_id)
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut skipped_tool_calls = BTreeSet::new();
    let mut projection = ChatProjection {
        completed_compact_tools: events
            .iter()
            .filter_map(|event| match event {
                Event::CompactStateUpdate(update) if update.state == CompactState::Completed => {
                    Some(update.tool_call_id)
                }
                _ => None,
            })
            .collect(),
        ..Default::default()
    };
    for event in events {
        match event {
            Event::ToolCall(call) if errored_api_calls.contains(&call.api_call_id) => {
                skipped_tool_calls.insert(call.id);
            }
            Event::ToolCall(_) => projection.project_event(event)?,
            Event::ToolInfoUpdate(info) if !skipped_tool_calls.contains(&info.tool_call_id) => {
                projection.project_event(event)?;
            }
            Event::ToolCallResult(result) if !skipped_tool_calls.contains(&result.tool_call_id) => {
                projection.project_event(event)?;
            }
            Event::TerminalSessionCreated(created)
                if !skipped_tool_calls.contains(&created.tool_call_id) =>
            {
                projection.project_event(event)?;
            }
            _ => {}
        }
    }
    Ok(projection
        .messages
        .into_iter()
        .filter_map(|message| {
            (message.kind == ChatBlockKind::ToolCall)
                .then_some(message.tool)
                .flatten()
        })
        .collect())
}

fn effective_api_usage(events: &[&Event], model_event_id: EventId) -> Option<ApiUsage> {
    let mut errored = BTreeSet::new();
    let mut usage = None;
    for event in events.iter().copied() {
        if matches!(event, Event::CompactStateUpdate(update) if update.state == CompactState::Completed)
        {
            usage = None;
            continue;
        }
        let Event::ApiStateUpdate(update) = event else {
            continue;
        };
        if update.id <= model_event_id {
            continue;
        }
        match update.state {
            ApiState::Completed => usage = update.usage,
            ApiState::Error => {
                errored.insert(update.api_call_id);
            }
            ApiState::Interrupted if !errored.contains(&update.api_call_id) => {
                usage = update.usage;
            }
            ApiState::Requesting
            | ApiState::Streaming
            | ApiState::Retrying
            | ApiState::Interrupted => {}
        }
    }
    usage
}

fn completed_turn_context_growth(
    completed_api_usage: &BTreeMap<EventId, (EventId, Option<ApiUsage>)>,
    prompt_id: EventId,
    context_baseline: Option<u64>,
) -> Option<u64> {
    let mut matching = completed_api_usage
        .values()
        .filter(|(usage_prompt_id, _)| *usage_prompt_id == prompt_id);
    let first_usage = matching.next()?.1?;
    let mut final_usage = first_usage;
    for (_, usage) in matching {
        final_usage = (*usage)?;
    }
    let baseline = context_baseline.unwrap_or(first_usage.input_tokens);
    Some(final_usage.total_tokens.saturating_sub(baseline))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlashCommand {
    AgentAdd,
    AgentDelete,
    Model,
    Effort,
    Context,
    Stop,
    Clear,
    Rewind,
    Exit,
}

impl SlashCommand {
    const INTERACTIVE: [Self; 8] = [
        Self::AgentAdd,
        Self::AgentDelete,
        Self::Model,
        Self::Effort,
        Self::Context,
        Self::Clear,
        Self::Rewind,
        Self::Exit,
    ];
    const WORKER: [Self; 5] = [
        Self::Model,
        Self::Effort,
        Self::Context,
        Self::Stop,
        Self::Clear,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::AgentAdd => "/new_session",
            Self::AgentDelete => "/delete_session",
            Self::Model => "/model",
            Self::Effort => "/effort",
            Self::Context => "/context",
            Self::Stop => "/stop",
            Self::Clear => "/clear",
            Self::Rewind => "/rewind",
            Self::Exit => "/exit",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::AgentAdd => "新建会话",
            Self::AgentDelete => "永久删除当前空闲会话",
            Self::Model => "切换当前模型",
            Self::Effort => "选择推理强度",
            Self::Context => "查看上下文用量",
            Self::Stop => "停止 Worker 当前任务",
            Self::Clear => "清空当前上下文",
            Self::Rewind => "回溯到指定消息、上下文清理或上下文压缩之前",
            Self::Exit => "退出 me",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandScope {
    Interactive,
    Worker,
}

impl CommandScope {
    fn commands(self) -> &'static [SlashCommand] {
        match self {
            Self::Interactive => &SlashCommand::INTERACTIVE,
            Self::Worker => &SlashCommand::WORKER,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RewindChoice {
    event_id: EventId,
    kind: RewindChoiceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextCategory {
    System,
    Compact,
    Memory,
    User,
    Model,
    Tool,
    Reserve,
    Remaining,
}

impl ContextCategory {
    fn label(self) -> &'static str {
        match self {
            Self::System => "系统提示词",
            Self::Compact => "上下文压缩",
            Self::Memory => "记忆",
            Self::User => "用户消息",
            Self::Model => "模型输出",
            Self::Tool => "工具调用",
            Self::Reserve => "输出预留",
            Self::Remaining => "剩余空间",
        }
    }

    fn color(self) -> MarkdownColorRole {
        match self {
            Self::System => MarkdownColorRole::Accent,
            Self::Compact => MarkdownColorRole::SyntaxFunction,
            Self::Memory => MarkdownColorRole::SyntaxType,
            Self::User => MarkdownColorRole::SyntaxString,
            Self::Model => MarkdownColorRole::SyntaxVariable,
            Self::Tool => MarkdownColorRole::Warning,
            Self::Reserve => MarkdownColorRole::SyntaxKeyword,
            Self::Remaining => MarkdownColorRole::Muted,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ContextTokenValues {
    system: u64,
    compact: u64,
    memory: u64,
    user: u64,
    model: u64,
    tool: u64,
}

impl ContextTokenValues {
    fn value(&self, category: ContextCategory, reserve: u64) -> u64 {
        match category {
            ContextCategory::System => self.system,
            ContextCategory::Compact => self.compact,
            ContextCategory::Memory => self.memory,
            ContextCategory::User => self.user,
            ContextCategory::Model => self.model,
            ContextCategory::Tool => self.tool,
            ContextCategory::Reserve => reserve,
            ContextCategory::Remaining => 0,
        }
    }

    #[cfg(test)]
    fn sum(&self) -> u64 {
        self.system
            .saturating_add(self.compact)
            .saturating_add(self.memory)
            .saturating_add(self.user)
            .saturating_add(self.model)
            .saturating_add(self.tool)
    }
}

impl From<crate::event::ContextTokenUsage> for ContextTokenValues {
    fn from(values: crate::event::ContextTokenUsage) -> Self {
        Self {
            system: values.system,
            compact: values.compact,
            memory: values.memory,
            user: values.user,
            model: values.model,
            tool: values.tool,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContextUsageBreakdown {
    total: Option<u64>,
    limit: Option<u64>,
    reserve: u64,
    values: ContextTokenValues,
    compact_content: Option<String>,
    compact_analysis: Option<String>,
    memory_content: Option<String>,
    can_clear: bool,
}

impl ContextUsageBreakdown {
    fn categories(&self) -> Vec<ContextCategory> {
        let mut categories = vec![ContextCategory::System];
        if self.compact_content.is_some() {
            categories.push(ContextCategory::Compact);
        }
        if self.memory_content.is_some() {
            categories.push(ContextCategory::Memory);
        }
        categories.extend([
            ContextCategory::User,
            ContextCategory::Model,
            ContextCategory::Tool,
            ContextCategory::Reserve,
            ContextCategory::Remaining,
        ]);
        categories
    }

    fn value(&self, category: ContextCategory) -> u64 {
        if category == ContextCategory::Remaining {
            return self
                .limit
                .unwrap_or(0)
                .saturating_sub(self.total.unwrap_or(0).saturating_add(self.reserve));
        }
        self.values.value(category, self.reserve)
    }

    fn actions(&self) -> Vec<ContextAction> {
        let mut actions = Vec::new();
        if self.compact_content.is_some() {
            actions.push(ContextAction::CompactDetail);
        }
        if self.memory_content.is_some() {
            actions.push(ContextAction::MemoryDetail);
        }
        if self.can_clear {
            actions.push(ContextAction::Clear);
        }
        actions
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextAction {
    CompactDetail,
    MemoryDetail,
    Clear,
}

impl ContextAction {
    fn label(self) -> &'static str {
        match self {
            Self::CompactDetail => "查看上下文压缩",
            Self::MemoryDetail => "查看记忆",
            Self::Clear => "清空上下文",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RewindChoiceKind {
    UserPrompt(String),
    ContextCleared,
    ContextCompacted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OverlayState {
    Commands {
        scope: CommandScope,
        selected: usize,
    },
    Effort {
        choices: Vec<String>,
        selected: usize,
    },
    Model {
        choices: Vec<String>,
        selected: usize,
    },
    Clear {
        selected: usize,
    },
    Context {
        breakdown: Box<ContextUsageBreakdown>,
        selected: usize,
    },
    ContextDetail {
        breakdown: Box<ContextUsageBreakdown>,
        selected: usize,
        title: String,
        content: String,
        markdown: bool,
        scroll: usize,
    },
    Rewind {
        choices: Vec<RewindChoice>,
        selected: usize,
    },
    AgentAdd {
        choices: Vec<String>,
        selected: usize,
    },
    AgentDelete {
        id: AgentId,
        path: String,
        blocker: Option<String>,
        selected: usize,
    },
}

struct TuiSession {
    projection: ChatProjection,
    input: String,
    overlay: Option<OverlayState>,
    observed_events: usize,
    detail_scroll: usize,
    renderer: TranscriptRenderer,
    agent_id: String,
    orchestrator_name: String,
    model_options: BTreeMap<String, UiModelOption>,
    pending_escape_rewind: Option<EventId>,
    input_draft_revision: u64,
    api_activity: UiApiActivity,
    read_only_state: Option<ReadOnlyAgentState>,
    worker_events: Arc<[Event]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadOnlyAgentState {
    Working,
    Completed,
    Interrupted,
    Failed,
}

impl std::fmt::Display for ReadOnlyAgentState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        })
    }
}

impl TuiSession {
    fn new(
        agent_id: &str,
        orchestrator_name: &str,
        events: &[Event],
        edb_size_bytes: u64,
        terminal_backend: &str,
        models: &[UiModelOption],
    ) -> Result<Self> {
        let model_name = events.iter().rev().find_map(|event| match event {
            Event::ModelChanged(changed) => Some(changed.model.as_str()),
            _ => None,
        });
        Ok(Self {
            projection: ChatProjection::replay_events(events)?,
            input: String::new(),
            overlay: None,
            observed_events: events.len(),
            detail_scroll: 0,
            renderer: TranscriptRenderer::new(StartupBanner::current(
                agent_id,
                orchestrator_name,
                model_name.unwrap_or("<unset>"),
                events.len(),
                edb_size_bytes,
                terminal_backend,
            )),
            agent_id: agent_id.to_owned(),
            orchestrator_name: orchestrator_name.to_owned(),
            model_options: models
                .iter()
                .map(|model| (model.name.clone(), model.clone()))
                .collect(),
            pending_escape_rewind: None,
            input_draft_revision: 0,
            api_activity: UiApiActivity::default(),
            read_only_state: None,
            worker_events: Arc::from([]),
        })
    }

    fn empty(models: &[UiModelOption]) -> Result<Self> {
        Self::new("<none>", "workspace", &[], 0, "unavailable", models)
    }

    fn current_effort(&self) -> Option<&str> {
        self.projection.effort.as_deref()
    }

    fn current_model(&self) -> Option<&str> {
        self.projection.model_name.as_deref()
    }

    fn current_model_option(&self) -> Option<&UiModelOption> {
        self.current_model()
            .and_then(|model| self.model_options.get(model))
    }

    fn context_usage_breakdown(&self, events: &[Event]) -> Result<ContextUsageBreakdown> {
        let model = self.current_model_option();
        let reserve = model
            .and_then(|model| {
                self.current_effort()
                    .and_then(|effort| model.output_token_reservations.get(effort))
            })
            .copied()
            .unwrap_or(0);
        let memory_content = if self.orchestrator_name == "worker-agent" {
            None
        } else {
            turn_history::latest_snapshot(events)?
        };
        estimate_context_breakdown(
            events,
            latest_context_usage(events),
            model.map(|model| model.context_window),
            reserve,
            memory_content,
            self.read_only_state.is_none(),
        )
    }

    fn command_scope(&self) -> Option<CommandScope> {
        match (self.read_only_state, self.orchestrator_name.as_str()) {
            (None, _) => Some(CommandScope::Interactive),
            (Some(_), "worker-agent") => Some(CommandScope::Worker),
            (Some(_), _) => None,
        }
    }

    fn open_command_palette(&mut self) -> bool {
        let Some(scope) = self.command_scope() else {
            return false;
        };
        self.input.clear();
        self.input.push('/');
        self.overlay = Some(OverlayState::Commands { scope, selected: 0 });
        true
    }

    fn redraw(
        &mut self,
        stdout: &mut Stdout,
        events: &[Event],
        cause: RedrawCause,
        view: TuiView,
    ) -> Result<()> {
        let context_window = self
            .current_model_option()
            .map(|model| model.context_window);
        self.renderer.redraw(
            stdout,
            &mut self.projection,
            &mut self.detail_scroll,
            RedrawRequest {
                cause,
                view,
                events,
                input: &self.input,
                overlay: self.overlay.as_ref(),
                agent_id: &self.agent_id,
                orchestrator_name: &self.orchestrator_name,
                context_window,
                api_activity: self.api_activity,
                read_only_state: self.read_only_state,
                worker_events: &self.worker_events,
            },
        )
    }

    fn redraw_if_terminal_size_changed(
        &mut self,
        stdout: &mut Stdout,
        events: &[Event],
        view: TuiView,
    ) -> Result<()> {
        let current_size = terminal::size()?;
        if terminal_size_changed(self.renderer.terminal_size, current_size) {
            self.redraw(stdout, events, RedrawCause::TerminalResized, view)?;
        }
        Ok(())
    }

    fn redraw_api_activity(&mut self, stdout: &mut Stdout, events: &[Event]) -> Result<()> {
        if self.projection.apply_api_activity(self.api_activity) {
            return self.redraw(stdout, events, RedrawCause::EdbUpdated, TuiView::Transcript);
        }
        let context_window = self
            .current_model_option()
            .map(|model| model.context_window);
        self.renderer.redraw_status(
            stdout,
            &self.projection,
            &self.agent_id,
            &self.orchestrator_name,
            context_window,
            self.api_activity,
            self.read_only_state,
        )
    }

    fn apply_new_event_effects(
        &mut self,
        events: &[Event],
        edb_size_bytes: u64,
        mutation: Option<&EdbMutation>,
    ) -> Result<()> {
        if let Some(EdbMutation::Rewind {
            target_event_id,
            restored_prompt_content: _,
        }) = mutation
        {
            if self.pending_escape_rewind == Some(*target_event_id) {
                self.pending_escape_rewind = None;
            }
        }
        self.observed_events = events.len();
        self.renderer.banner.edb_size_bytes = edb_size_bytes;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalPreviewSelection {
    session_id: String,
    creation_order: EventId,
}

impl From<&TerminalSessionPreview> for TerminalPreviewSelection {
    fn from(session: &TerminalSessionPreview) -> Self {
        Self {
            session_id: session.session_id.clone(),
            creation_order: session.creation_order,
        }
    }
}

struct TerminalPreviewRenderer {
    last_session_id: Option<String>,
    last_revision: Option<u64>,
    last_host_size: Option<(u16, u16)>,
    scroll_top: u64,
    total_rows: u64,
    visible_rows: u16,
    follow_bottom: bool,
    needs_redraw: bool,
}

impl Default for TerminalPreviewRenderer {
    fn default() -> Self {
        Self {
            last_session_id: None,
            last_revision: None,
            last_host_size: None,
            scroll_top: 0,
            total_rows: 0,
            visible_rows: 0,
            follow_bottom: true,
            needs_redraw: true,
        }
    }
}

impl TerminalPreviewRenderer {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn is_current(&self, session: &TerminalSessionPreview, host_size: (u16, u16)) -> bool {
        !self.needs_redraw
            && self.last_session_id.as_deref() == Some(session.session_id.as_str())
            && self.last_revision == Some(session.revision)
            && self.last_host_size == Some(host_size)
    }

    fn maximum_scroll_top(&self) -> u64 {
        self.total_rows.saturating_sub(u64::from(self.visible_rows))
    }

    fn scroll_up(&mut self, rows: u64) {
        let next = self.scroll_top.saturating_sub(rows);
        if next != self.scroll_top {
            self.scroll_top = next;
            self.follow_bottom = false;
            self.needs_redraw = true;
        }
    }

    fn scroll_down(&mut self, rows: u64) {
        let maximum = self.maximum_scroll_top();
        let next = self.scroll_top.saturating_add(rows).min(maximum);
        if next != self.scroll_top || !self.follow_bottom && next == maximum {
            self.scroll_top = next;
            self.follow_bottom = next == maximum;
            self.needs_redraw = true;
        }
    }

    fn page_rows(&self) -> u64 {
        u64::from(self.visible_rows.max(1))
    }

    fn scroll_home(&mut self) {
        if self.scroll_top != 0 || self.follow_bottom {
            self.scroll_top = 0;
            self.follow_bottom = false;
            self.needs_redraw = true;
        }
    }

    fn scroll_end(&mut self) {
        let maximum = self.maximum_scroll_top();
        if self.scroll_top != maximum || !self.follow_bottom {
            self.scroll_top = maximum;
            self.follow_bottom = true;
            self.needs_redraw = true;
        }
    }

    fn redraw<W: Write>(
        &mut self,
        stdout: &mut W,
        frame: &TerminalFrame,
        host_size: (u16, u16),
    ) -> Result<()> {
        if self.last_session_id.as_deref() == Some(frame.session_id.as_str())
            && self.last_revision == Some(frame.revision)
            && self.last_host_size == Some(host_size)
        {
            return Ok(());
        }
        let clear_screen = self.last_session_id.as_deref() != Some(frame.session_id.as_str())
            || self.last_host_size != Some(host_size);
        self.total_rows = u64::try_from(frame.rows.len()).unwrap_or(u64::MAX);
        self.visible_rows = host_size.1.saturating_sub(1);
        let maximum = self.maximum_scroll_top();
        if self.follow_bottom {
            self.scroll_top = maximum;
        } else {
            self.scroll_top = self.scroll_top.min(maximum);
        }
        render_terminal_preview_frame(stdout, frame, host_size, self.scroll_top, clear_screen)?;
        stdout.flush()?;
        self.last_session_id = Some(frame.session_id.clone());
        self.last_revision = Some(frame.revision);
        self.last_host_size = Some(host_size);
        self.needs_redraw = false;
        Ok(())
    }
}

fn next_terminal_preview(
    current: Option<&TerminalPreviewSelection>,
    sessions: &[TerminalSessionPreview],
) -> Option<TerminalSessionPreview> {
    let Some(current) = current else {
        return sessions.first().cloned();
    };
    if let Some(index) = sessions
        .iter()
        .position(|session| session.session_id == current.session_id)
    {
        return sessions.get(index + 1).cloned();
    }
    sessions
        .iter()
        .find(|session| session.creation_order > current.creation_order)
        .cloned()
}

fn refresh_terminal_preview(
    backend: &dyn UiBackend,
    agent_id: &AgentId,
    stdout: &mut Stdout,
    selection: &mut Option<TerminalPreviewSelection>,
    known_sessions: &mut Vec<TerminalSessionPreview>,
    renderer: &mut TerminalPreviewRenderer,
) -> Result<bool> {
    let sessions = match backend.terminal_sessions(agent_id) {
        Ok(sessions) => sessions,
        Err(_) => {
            known_sessions.clear();
            return Ok(false);
        }
    };
    *known_sessions = sessions.clone();
    let selected = selection.as_ref().and_then(|selected| {
        sessions
            .iter()
            .find(|session| session.session_id == selected.session_id)
            .cloned()
            .or_else(|| {
                sessions
                    .iter()
                    .find(|session| session.creation_order > selected.creation_order)
                    .cloned()
            })
    });
    let selected = match (selection.as_ref(), selected) {
        (_, Some(selected)) => selected,
        (None, None) => match sessions.first() {
            Some(selected) => selected.clone(),
            None => return Ok(false),
        },
        (Some(_), None) => return Ok(false),
    };
    let mut selected = selected;
    loop {
        if selection
            .as_ref()
            .map(|current| current.session_id.as_str())
            != Some(selected.session_id.as_str())
        {
            *selection = Some(TerminalPreviewSelection::from(&selected));
            renderer.reset();
        }
        let host_size = terminal::size()?;
        if renderer.is_current(&selected, host_size) {
            return Ok(true);
        }
        if let Ok(Some(frame)) = backend.terminal_frame(agent_id, &selected.session_id) {
            renderer.redraw(stdout, &frame, host_size)?;
            return Ok(true);
        }
        let sessions = match backend.terminal_sessions(agent_id) {
            Ok(sessions) => sessions,
            Err(_) => {
                known_sessions.clear();
                return Ok(false);
            }
        };
        *known_sessions = sessions.clone();
        let Some(next) = sessions
            .into_iter()
            .find(|session| session.creation_order > selected.creation_order)
        else {
            return Ok(false);
        };
        selected = next;
    }
}

fn require_agent(agent_id: Option<&AgentId>) -> Result<&AgentId> {
    agent_id.ok_or_else(|| "Workspace has no Agent; use /new_session first".into())
}

fn sync_input_draft(
    commands: &dyn UiCommandGateway,
    agent_id: &AgentId,
    ui: &mut TuiSession,
) -> Result<()> {
    let UiCommandReceipt::InputDraftUpdated { accepted, revision } =
        commands.submit(UiCommand::UpdateInputDraft {
            agent_id: agent_id.clone(),
            expected_revision: ui.input_draft_revision,
            content: ui.input.clone(),
        })?
    else {
        return Err("UpdateInputDraft did not return its revision".into());
    };
    if accepted {
        ui.input_draft_revision = revision;
    }
    Ok(())
}

fn current_events<'a>(snapshot: &'a UiSnapshot, agent_id: Option<&AgentId>) -> Result<&'a [Event]> {
    match agent_id {
        Some(agent_id) => Ok(&snapshot
            .agent(agent_id)
            .ok_or_else(|| format!("Agent {agent_id} does not exist"))?
            .events),
        None => Ok(&[]),
    }
}

fn session_for_agent(
    backend: &dyn UiBackend,
    snapshot: &UiSnapshot,
    agent_id: Option<&AgentId>,
) -> Result<TuiSession> {
    let Some(agent_id) = agent_id else {
        return TuiSession::empty(&snapshot.models);
    };
    let agent = snapshot
        .agent(agent_id)
        .ok_or_else(|| format!("Agent {agent_id} does not exist"))?;
    let events = &agent.events;
    let terminal_backend = backend.terminal_backend(agent_id).ok().flatten();
    let mut session = TuiSession::new(
        agent_id.as_str(),
        &agent.orchestrator_name,
        events,
        agent.edb_size_bytes,
        terminal_backend.as_deref().unwrap_or("unavailable"),
        &snapshot.models,
    )?;
    if agent.kind == AgentKind::SubAgent {
        session.read_only_state = Some(projected_child_state(events)?);
    }
    session.worker_events = worker_events_for_agent(snapshot, agent_id);
    session.input = agent.input_draft.clone();
    session.input_draft_revision = agent.input_draft_revision;
    session.api_activity = backend.api_activity(agent_id)?;
    Ok(session)
}

fn worker_events_for_agent(snapshot: &UiSnapshot, agent_id: &AgentId) -> Arc<[Event]> {
    snapshot
        .agents
        .iter()
        .find(|agent| {
            agent.kind == AgentKind::SubAgent
                && agent.orchestrator_name == "worker-agent"
                && agent.parent_agent_id.as_ref() == Some(agent_id)
        })
        .map(|agent| Arc::clone(&agent.events))
        .unwrap_or_else(|| Arc::from([]))
}

fn session_for_available_agent(
    backend: &dyn UiBackend,
    snapshot: &UiSnapshot,
    preferred: Option<AgentId>,
) -> Result<(Option<AgentId>, TuiSession)> {
    let mut candidates = Vec::new();
    if let Some(preferred) = preferred {
        candidates.push(preferred);
    }
    for id in snapshot.agent_ids() {
        if !candidates.contains(&id) {
            candidates.push(id);
        }
    }
    for id in candidates {
        match session_for_agent(backend, snapshot, Some(&id)) {
            Ok(session) => return Ok((Some(id), session)),
            Err(_) if !snapshot.contains(&id) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok((None, TuiSession::empty(&snapshot.models)?))
}

fn projected_child_state(events: &[Event]) -> Result<ReadOnlyAgentState> {
    Ok(match latest_agent_turn(events)?.map(|turn| turn.state) {
        Some(AgentTurnState::Completed) => ReadOnlyAgentState::Completed,
        Some(AgentTurnState::Interrupted) => ReadOnlyAgentState::Interrupted,
        Some(AgentTurnState::Failed) => ReadOnlyAgentState::Failed,
        Some(AgentTurnState::Started) | None => ReadOnlyAgentState::Working,
    })
}

fn next_agent_id(agent_ids: &[AgentId], current: Option<&AgentId>) -> Option<AgentId> {
    if agent_ids.is_empty() {
        return None;
    }
    let Some(current) = current else {
        return agent_ids.first().cloned();
    };
    if agent_ids.len() == 1 {
        return None;
    }
    let index = agent_ids
        .iter()
        .position(|candidate| candidate == current)?;
    agent_ids.get((index + 1) % agent_ids.len()).cloned()
}

fn reconcile_agent_selection(
    previous_ids: &[AgentId],
    current_ids: &[AgentId],
    current: Option<&AgentId>,
) -> (Option<AgentId>, bool) {
    let Some(current) = current else {
        return (None, false);
    };
    if current_ids.contains(current) {
        return (Some(current.clone()), false);
    }
    let previous_index = previous_ids
        .iter()
        .position(|candidate| candidate == current)
        .unwrap_or(0);
    (
        current_ids
            .get(previous_index)
            .or_else(|| current_ids.last())
            .cloned(),
        true,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TabDestination {
    Terminal(TerminalSessionPreview),
    WorkMap,
    Agent(AgentId),
    Transcript,
}

fn terminal_sessions_for_tab(
    view: TuiView,
    cached: &[TerminalSessionPreview],
    discover: impl FnOnce() -> Vec<TerminalSessionPreview>,
) -> Vec<TerminalSessionPreview> {
    match view {
        TuiView::TerminalPreview => cached.to_vec(),
        TuiView::Transcript => discover(),
        TuiView::WorkMap | TuiView::ToolDetails | TuiView::ToolDetailsInline => Vec::new(),
    }
}

fn tab_destination(
    view: TuiView,
    current_terminal: Option<&TerminalPreviewSelection>,
    sessions: &[TerminalSessionPreview],
    agent_ids: &[AgentId],
    current_agent: Option<&AgentId>,
) -> TabDestination {
    if matches!(view, TuiView::Transcript | TuiView::TerminalPreview)
        && let Some(terminal) = next_terminal_preview(current_terminal, sessions)
    {
        return TabDestination::Terminal(terminal);
    }
    if matches!(view, TuiView::Transcript | TuiView::TerminalPreview) && current_agent.is_some() {
        return TabDestination::WorkMap;
    }
    next_agent_id(agent_ids, current_agent)
        .map(TabDestination::Agent)
        .unwrap_or(TabDestination::Transcript)
}

pub fn run(
    backend: &dyn UiBackend,
    commands: &dyn UiCommandGateway,
    shutdown: &AtomicBool,
) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut stdout = io::stdout();
    let mut view = TuiView::Transcript;
    let mut preview_selection: Option<TerminalPreviewSelection> = None;
    let mut preview_sessions = Vec::new();
    let mut preview_renderer = TerminalPreviewRenderer::default();
    let mut snapshot = backend.snapshot()?;
    let mut observed_agent_ids = snapshot.agent_ids();
    let (mut current_agent, mut ui) =
        session_for_available_agent(backend, &snapshot, observed_agent_ids.first().cloned())?;
    ui.redraw(
        &mut stdout,
        current_events(&snapshot, current_agent.as_ref())?,
        RedrawCause::Startup,
        view,
    )?;
    let mut observed_edb_mutation_revision = current_agent
        .as_ref()
        .and_then(|id| snapshot.agent(id))
        .map(|agent| agent.mutation_revision)
        .unwrap_or(0);
    let mut observed_prompt_submission_revision = current_agent
        .as_ref()
        .and_then(|id| snapshot.agent(id))
        .map(|agent| agent.prompt_submission_revision)
        .unwrap_or(0);
    let mut observed_workspace_revision = snapshot.revision;

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let next_snapshot = backend.snapshot()?;
        let agent_changed = current_agent.as_ref().is_some_and(|id| {
            snapshot.agent(id).map(UiAgentSnapshot::revision)
                != next_snapshot.agent(id).map(UiAgentSnapshot::revision)
        });
        let workspace_changed = observed_workspace_revision != next_snapshot.revision;
        observed_workspace_revision = next_snapshot.revision;
        let current_agent_ids = next_snapshot.agent_ids();
        let (next_agent, selected_agent_was_removed) = reconcile_agent_selection(
            &observed_agent_ids,
            &current_agent_ids,
            current_agent.as_ref(),
        );
        observed_agent_ids = current_agent_ids;
        snapshot = next_snapshot;
        if selected_agent_was_removed {
            if view.is_tool_details() {
                terminal.leave_detail_screen(&mut stdout)?;
            }
            (current_agent, ui) = session_for_available_agent(backend, &snapshot, next_agent)?;
            observed_edb_mutation_revision = current_agent
                .as_ref()
                .and_then(|id| snapshot.agent(id))
                .map(|agent| agent.mutation_revision)
                .unwrap_or(0);
            observed_prompt_submission_revision = current_agent
                .as_ref()
                .and_then(|id| snapshot.agent(id))
                .map(|agent| agent.prompt_submission_revision)
                .unwrap_or(0);
            view = TuiView::Transcript;
            preview_selection = None;
            preview_sessions.clear();
            preview_renderer.reset();
            clear_for_full_redraw(&mut stdout, true)?;
            ui.redraw(
                &mut stdout,
                current_events(&snapshot, current_agent.as_ref())?,
                RedrawCause::ViewChanged,
                view,
            )?;
            continue;
        }
        let next_api_activity = current_agent
            .as_ref()
            .map(|id| backend.api_activity(id))
            .transpose()?
            .unwrap_or_default();
        let api_activity_changed = ui.api_activity != next_api_activity;
        ui.api_activity = next_api_activity;
        if agent_changed || workspace_changed {
            ui.worker_events = current_agent
                .as_ref()
                .map(|id| worker_events_for_agent(&snapshot, id))
                .unwrap_or_else(|| Arc::from([]));
            let current_prompt_submission_revision = current_agent
                .as_ref()
                .and_then(|id| snapshot.agent(id))
                .map(|agent| agent.prompt_submission_revision)
                .unwrap_or(0);
            let prompt_was_submitted = observe_prompt_submission(
                &mut observed_prompt_submission_revision,
                current_prompt_submission_revision,
            );
            let input_draft_changed = current_agent
                .as_ref()
                .and_then(|id| snapshot.agent(id))
                .is_some_and(|agent| agent.input_draft_revision != ui.input_draft_revision);
            if input_draft_changed
                && let Some(agent) = current_agent.as_ref().and_then(|id| snapshot.agent(id))
            {
                ui.input = agent.input_draft.clone();
                ui.input_draft_revision = agent.input_draft_revision;
            }
            let current_revision = current_agent
                .as_ref()
                .and_then(|id| snapshot.agent(id))
                .map(|agent| agent.mutation_revision)
                .unwrap_or(0);
            let edb_was_modified =
                observe_edb_mutation(&mut observed_edb_mutation_revision, current_revision);
            let mutation = if edb_was_modified {
                current_agent
                    .as_ref()
                    .and_then(|id| snapshot.agent(id))
                    .and_then(|agent| agent.last_mutation.clone())
            } else {
                None
            };
            let events = current_events(&snapshot, current_agent.as_ref())?;
            let edb_events_changed = ui.observed_events != events.len();
            let workmap_changed =
                workmap_view_needs_redraw(ui.observed_events, events, edb_was_modified);
            let edb_size = current_agent
                .as_ref()
                .and_then(|id| snapshot.agent(id))
                .map(|agent| agent.edb_size_bytes)
                .unwrap_or(0);
            if current_agent
                .as_ref()
                .and_then(|id| snapshot.agent(id))
                .is_some_and(|agent| agent.kind == AgentKind::SubAgent)
            {
                ui.read_only_state = Some(projected_child_state(events)?);
            }
            ui.apply_new_event_effects(events, edb_size, mutation.as_ref())?;
            if view != TuiView::TerminalPreview && (view != TuiView::WorkMap || workmap_changed) {
                ui.redraw(
                    &mut stdout,
                    events,
                    if edb_was_modified {
                        RedrawCause::ContextChanged
                    } else if (prompt_was_submitted || input_draft_changed) && !edb_events_changed {
                        RedrawCause::InputChanged
                    } else {
                        RedrawCause::EdbUpdated
                    },
                    view,
                )?;
            }
        } else if api_activity_changed && view == TuiView::Transcript && ui.overlay.is_none() {
            let events = current_events(&snapshot, current_agent.as_ref())?;
            ui.redraw_api_activity(&mut stdout, events)?;
        }
        if view == TuiView::TerminalPreview {
            let Some(agent_id) = current_agent.as_ref() else {
                view = TuiView::Transcript;
                continue;
            };
            if !refresh_terminal_preview(
                backend,
                agent_id,
                &mut stdout,
                &mut preview_selection,
                &mut preview_sessions,
                &mut preview_renderer,
            )? {
                view = TuiView::WorkMap;
                preview_selection = None;
                preview_sessions.clear();
                preview_renderer.reset();
                clear_for_full_redraw(&mut stdout, true)?;
                ui.redraw(
                    &mut stdout,
                    current_events(&snapshot, current_agent.as_ref())?,
                    RedrawCause::ViewChanged,
                    view,
                )?;
            }
        } else {
            ui.redraw_if_terminal_size_changed(
                &mut stdout,
                current_events(&snapshot, current_agent.as_ref())?,
                view,
            )?;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if key.code == KeyCode::Char('c') && control {
                    if ui.overlay.take().is_some() {
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::ViewChanged,
                            view,
                        )?;
                    }
                    break;
                }
                if key.code == KeyCode::Tab && ui.overlay.is_none() && !view.is_tool_details() {
                    let sessions = terminal_sessions_for_tab(view, &preview_sessions, || {
                        current_agent
                            .as_ref()
                            .and_then(|id| backend.terminal_sessions(id).ok())
                            .unwrap_or_default()
                    });
                    let agents = snapshot.agent_ids();
                    match tab_destination(
                        view,
                        preview_selection.as_ref(),
                        &sessions,
                        &agents,
                        current_agent.as_ref(),
                    ) {
                        TabDestination::Terminal(next) => {
                            let agent_id = require_agent(current_agent.as_ref())?;
                            if view != TuiView::TerminalPreview {
                                ui.renderer.suspend_for_external_view();
                                view = TuiView::TerminalPreview;
                                preview_sessions = sessions.clone();
                            }
                            preview_selection = Some(TerminalPreviewSelection::from(&next));
                            preview_renderer.reset();
                            if !refresh_terminal_preview(
                                backend,
                                agent_id,
                                &mut stdout,
                                &mut preview_selection,
                                &mut preview_sessions,
                                &mut preview_renderer,
                            )? {
                                view = TuiView::WorkMap;
                                preview_selection = None;
                                preview_sessions.clear();
                                preview_renderer.reset();
                                clear_for_full_redraw(&mut stdout, true)?;
                                ui.redraw(
                                    &mut stdout,
                                    current_events(&snapshot, current_agent.as_ref())?,
                                    RedrawCause::ViewChanged,
                                    view,
                                )?;
                            }
                        }
                        TabDestination::WorkMap => {
                            if view == TuiView::TerminalPreview {
                                preview_renderer.reset();
                            }
                            view = TuiView::WorkMap;
                            preview_selection = None;
                            preview_sessions.clear();
                            ui.redraw(
                                &mut stdout,
                                current_events(&snapshot, current_agent.as_ref())?,
                                RedrawCause::ViewChanged,
                                view,
                            )?;
                        }
                        TabDestination::Agent(next_agent) => {
                            if view == TuiView::TerminalPreview {
                                preview_renderer.reset();
                            }
                            (current_agent, ui) =
                                session_for_available_agent(backend, &snapshot, Some(next_agent))?;
                            observed_edb_mutation_revision = snapshot
                                .agent(require_agent(current_agent.as_ref())?)
                                .ok_or("selected Agent disappeared")?
                                .mutation_revision;
                            observed_prompt_submission_revision = snapshot
                                .agent(require_agent(current_agent.as_ref())?)
                                .ok_or("selected Agent disappeared")?
                                .prompt_submission_revision;
                            view = TuiView::Transcript;
                            preview_selection = None;
                            preview_sessions.clear();
                            clear_for_full_redraw(&mut stdout, true)?;
                            ui.redraw(
                                &mut stdout,
                                current_events(&snapshot, current_agent.as_ref())?,
                                RedrawCause::ViewChanged,
                                view,
                            )?;
                        }
                        TabDestination::Transcript if view != TuiView::Transcript => {
                            if view == TuiView::TerminalPreview {
                                preview_renderer.reset();
                                ui = session_for_agent(backend, &snapshot, current_agent.as_ref())?;
                            }
                            view = TuiView::Transcript;
                            preview_selection = None;
                            preview_sessions.clear();
                            clear_for_full_redraw(&mut stdout, true)?;
                            ui.redraw(
                                &mut stdout,
                                current_events(&snapshot, current_agent.as_ref())?,
                                RedrawCause::ViewChanged,
                                view,
                            )?;
                        }
                        TabDestination::Transcript => {}
                    }
                    continue;
                }
                if view == TuiView::TerminalPreview {
                    let changed = match key.code {
                        KeyCode::Up => {
                            preview_renderer.scroll_up(1);
                            true
                        }
                        KeyCode::Down => {
                            preview_renderer.scroll_down(1);
                            true
                        }
                        KeyCode::PageUp => {
                            preview_renderer.scroll_up(preview_renderer.page_rows());
                            true
                        }
                        KeyCode::PageDown => {
                            preview_renderer.scroll_down(preview_renderer.page_rows());
                            true
                        }
                        KeyCode::Home => {
                            preview_renderer.scroll_home();
                            true
                        }
                        KeyCode::End => {
                            preview_renderer.scroll_end();
                            true
                        }
                        _ => false,
                    };
                    if changed && let Some(agent_id) = current_agent.as_ref() {
                        refresh_terminal_preview(
                            backend,
                            agent_id,
                            &mut stdout,
                            &mut preview_selection,
                            &mut preview_sessions,
                            &mut preview_renderer,
                        )?;
                    }
                    continue;
                }
                if view == TuiView::WorkMap {
                    if toggles_workmap_history(key.code, control) {
                        ui.renderer.toggle_workmap_history();
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::ViewChanged,
                            view,
                        )?;
                    }
                    continue;
                }
                if ui.read_only_state.is_some() && !view.is_tool_details() && ui.overlay.is_none() {
                    if key.code == KeyCode::Char('o') && control {
                        terminal.enter_detail_screen(&mut stdout)?;
                        view = terminal.detail_view();
                        ui.detail_scroll = 0;
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::ViewChanged,
                            view,
                        )?;
                    } else if key.code == KeyCode::Char('/')
                        && !control
                        && ui.open_command_palette()
                    {
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::InputChanged,
                            view,
                        )?;
                    }
                    continue;
                }
                if ui.overlay.is_some() {
                    let draft_agent_before_action = current_agent.clone();
                    let action = handle_overlay_key(
                        ui.overlay.as_mut().expect("overlay checked above"),
                        &mut ui.input,
                        key.code,
                    );
                    let mut exit = false;
                    match action {
                        OverlayAction::Redraw => {}
                        OverlayAction::Close => {
                            ui.overlay = None;
                            ui.input.clear();
                        }
                        OverlayAction::Open(command) => {
                            ui.input.clear();
                            match command {
                                SlashCommand::AgentAdd => {
                                    let choices = snapshot.orchestrators.to_vec();
                                    let selected = choices
                                        .iter()
                                        .position(|name| name == &snapshot.default_orchestrator)
                                        .unwrap_or(0);
                                    ui.overlay = Some(OverlayState::AgentAdd { choices, selected });
                                }
                                SlashCommand::AgentDelete => {
                                    if let Some(id) = current_agent.clone() {
                                        let path = crate::host_path::public_host_path(
                                            &snapshot
                                                .agent(&id)
                                                .ok_or_else(|| {
                                                    format!("Agent {id} does not exist")
                                                })?
                                                .edb_path,
                                        );
                                        let blocker = backend.deletion_blocker(&id)?;
                                        ui.overlay = Some(OverlayState::AgentDelete {
                                            id,
                                            path,
                                            blocker,
                                            selected: 0,
                                        });
                                    } else {
                                        ui.overlay = None;
                                    }
                                }
                                SlashCommand::Model => {
                                    if current_agent.is_some() {
                                        let choices = snapshot
                                            .models
                                            .iter()
                                            .map(|model| model.name.clone())
                                            .collect::<Vec<_>>();
                                        let selected = ui
                                            .current_model()
                                            .and_then(|current| {
                                                choices.iter().position(|choice| choice == current)
                                            })
                                            .unwrap_or(0);
                                        ui.overlay =
                                            Some(OverlayState::Model { choices, selected });
                                    } else {
                                        ui.overlay = None;
                                    }
                                }
                                SlashCommand::Effort => {
                                    if current_agent.is_some() {
                                        let current_model =
                                            ui.current_model().ok_or("EDB has no model state")?;
                                        let model = snapshot
                                            .models
                                            .iter()
                                            .find(|model| model.name == current_model)
                                            .ok_or_else(|| {
                                                format!("model {current_model} does not exist")
                                            })?;
                                        let choices = model.reasoning_efforts.clone();
                                        let current = ui.current_effort();
                                        let selected = current
                                            .and_then(|effort| {
                                                choices.iter().position(|choice| choice == effort)
                                            })
                                            .unwrap_or(0);
                                        ui.overlay =
                                            Some(OverlayState::Effort { choices, selected });
                                    } else {
                                        ui.overlay = None;
                                    }
                                }
                                SlashCommand::Context => {
                                    ui.overlay = if current_agent.is_some() {
                                        let events =
                                            current_events(&snapshot, current_agent.as_ref())?;
                                        Some(OverlayState::Context {
                                            breakdown: Box::new(
                                                ui.context_usage_breakdown(events)?,
                                            ),
                                            selected: 0,
                                        })
                                    } else {
                                        None
                                    };
                                }
                                SlashCommand::Stop => {
                                    if let Some(agent_id) = current_agent.as_ref() {
                                        commands.submit(UiCommand::AbortTurn {
                                            agent_id: agent_id.clone(),
                                        })?;
                                    }
                                    ui.overlay = None;
                                }
                                SlashCommand::Clear => {
                                    ui.overlay = current_agent
                                        .as_ref()
                                        .map(|_| OverlayState::Clear { selected: 0 });
                                }
                                SlashCommand::Rewind => {
                                    ui.overlay = if current_agent.is_some() {
                                        Some(OverlayState::Rewind {
                                            choices: rewind_choices(current_events(
                                                &snapshot,
                                                current_agent.as_ref(),
                                            )?)?,
                                            selected: 0,
                                        })
                                    } else {
                                        None
                                    };
                                }
                                SlashCommand::Exit => exit = true,
                            }
                        }
                        OverlayAction::SubmitEffort(effort) => {
                            ui.overlay = None;
                            ui.input.clear();
                            if ui.current_effort() != Some(effort.as_str()) {
                                commands.submit(UiCommand::ChangeEffort {
                                    agent_id: require_agent(current_agent.as_ref())?.clone(),
                                    effort,
                                })?;
                            }
                        }
                        OverlayAction::SubmitModel(model) => {
                            ui.overlay = None;
                            ui.input.clear();
                            if ui.current_model() != Some(model.as_str()) {
                                commands.submit(UiCommand::ChangeModel {
                                    agent_id: require_agent(current_agent.as_ref())?.clone(),
                                    model,
                                })?;
                            }
                        }
                        OverlayAction::SubmitClear => {
                            ui.overlay = None;
                            ui.input.clear();
                            commands.submit(UiCommand::ClearContext {
                                agent_id: require_agent(current_agent.as_ref())?.clone(),
                            })?;
                        }
                        OverlayAction::OpenContextDetail(action) => {
                            let Some(OverlayState::Context {
                                breakdown,
                                selected,
                            }) = ui.overlay.take()
                            else {
                                return Err("context action requires its context panel".into());
                            };
                            let content = match action {
                                ContextAction::CompactDetail => compact_detail_content(&breakdown),
                                ContextAction::MemoryDetail => breakdown.memory_content.clone(),
                                ContextAction::Clear => None,
                            }
                            .ok_or("selected context detail is not available")?;
                            let title = action.label().trim_start_matches("查看").to_owned();
                            ui.overlay = Some(OverlayState::ContextDetail {
                                breakdown,
                                selected,
                                title,
                                content,
                                markdown: action == ContextAction::CompactDetail,
                                scroll: 0,
                            });
                        }
                        OverlayAction::ConfirmContextClear => {
                            ui.overlay = Some(OverlayState::Clear { selected: 0 });
                        }
                        OverlayAction::BackToContext {
                            breakdown,
                            selected,
                        } => {
                            ui.overlay = Some(OverlayState::Context {
                                breakdown,
                                selected,
                            });
                        }
                        OverlayAction::SubmitRewind(event_id) => {
                            ui.overlay = None;
                            ui.input.clear();
                            commands.submit(UiCommand::RewindContext {
                                agent_id: require_agent(current_agent.as_ref())?.clone(),
                                event_id,
                            })?;
                        }
                        OverlayAction::SubmitAgentAdd(orchestrator) => {
                            ui.overlay = None;
                            ui.input.clear();
                            let crate::ui_backend::UiCommandReceipt::AgentCreated(draft) =
                                commands.submit(UiCommand::AddAgent { orchestrator })?
                            else {
                                return Err("AddAgent did not return its creation result".into());
                            };
                            let id = draft.id;
                            snapshot = backend.snapshot()?;
                            observed_workspace_revision = snapshot.revision;
                            observed_agent_ids = snapshot.agent_ids();
                            (current_agent, ui) =
                                session_for_available_agent(backend, &snapshot, Some(id))?;
                            observed_edb_mutation_revision = snapshot
                                .agent(require_agent(current_agent.as_ref())?)
                                .ok_or("created Agent is missing from UI snapshot")?
                                .mutation_revision;
                            observed_prompt_submission_revision = snapshot
                                .agent(require_agent(current_agent.as_ref())?)
                                .ok_or("created Agent is missing from UI snapshot")?
                                .prompt_submission_revision;
                            view = TuiView::Transcript;
                            preview_selection = None;
                            preview_sessions.clear();
                            preview_renderer.reset();
                        }
                        OverlayAction::SubmitAgentDelete(id) => {
                            let ids = snapshot.agent_ids();
                            let index = ids
                                .iter()
                                .position(|candidate| candidate == &id)
                                .ok_or_else(|| format!("Agent {id} does not exist"))?;
                            let adjacent = ids
                                .get(index + 1)
                                .or_else(|| index.checked_sub(1).and_then(|index| ids.get(index)))
                                .cloned();
                            commands.submit(UiCommand::DeleteAgent { agent_id: id })?;
                            snapshot = backend.snapshot()?;
                            observed_workspace_revision = snapshot.revision;
                            observed_agent_ids = snapshot.agent_ids();
                            (current_agent, ui) =
                                session_for_available_agent(backend, &snapshot, adjacent)?;
                            observed_edb_mutation_revision = current_agent
                                .as_ref()
                                .and_then(|id| snapshot.agent(id))
                                .map(|agent| agent.mutation_revision)
                                .unwrap_or(0);
                            observed_prompt_submission_revision = current_agent
                                .as_ref()
                                .and_then(|id| snapshot.agent(id))
                                .map(|agent| agent.prompt_submission_revision)
                                .unwrap_or(0);
                            view = TuiView::Transcript;
                            preview_selection = None;
                            preview_sessions.clear();
                            preview_renderer.reset();
                        }
                    }
                    if draft_agent_before_action.as_ref() == current_agent.as_ref()
                        && ui.read_only_state.is_none()
                        && let Some(agent_id) = current_agent.as_ref()
                    {
                        sync_input_draft(commands, agent_id, &mut ui)?;
                    }
                    if exit {
                        ui.overlay = None;
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::ViewChanged,
                            view,
                        )?;
                        break;
                    }
                    ui.redraw(
                        &mut stdout,
                        current_events(&snapshot, current_agent.as_ref())?,
                        if ui.overlay.is_some() {
                            RedrawCause::InputChanged
                        } else {
                            RedrawCause::ViewChanged
                        },
                        view,
                    )?;
                    continue;
                }
                if view.is_tool_details() {
                    if key.code == KeyCode::Esc || (key.code == KeyCode::Char('o') && control) {
                        terminal.leave_detail_screen(&mut stdout)?;
                        view = TuiView::Transcript;
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::ViewChanged,
                            view,
                        )?;
                        continue;
                    }
                    if view == TuiView::ToolDetails {
                        match key.code {
                            KeyCode::PageUp | KeyCode::Up => {
                                let step = if key.code == KeyCode::PageUp { 10 } else { 1 };
                                ui.detail_scroll = ui.detail_scroll.saturating_add(step);
                                ui.redraw(
                                    &mut stdout,
                                    current_events(&snapshot, current_agent.as_ref())?,
                                    RedrawCause::DetailScrolled,
                                    view,
                                )?;
                            }
                            KeyCode::PageDown | KeyCode::Down => {
                                let step = if key.code == KeyCode::PageDown { 10 } else { 1 };
                                ui.detail_scroll = ui.detail_scroll.saturating_sub(step);
                                ui.redraw(
                                    &mut stdout,
                                    current_events(&snapshot, current_agent.as_ref())?,
                                    RedrawCause::DetailScrolled,
                                    view,
                                )?;
                            }
                            _ => {}
                        }
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Esc => {
                        if ui.pending_escape_rewind.is_none() {
                            let action = transcript_escape_action(current_events(
                                &snapshot,
                                current_agent.as_ref(),
                            )?)?;
                            match action {
                                TranscriptEscapeAction::Abort => {
                                    commands.submit(UiCommand::AbortTurn {
                                        agent_id: require_agent(current_agent.as_ref())?.clone(),
                                    })?;
                                }
                                TranscriptEscapeAction::Wait => {}
                                TranscriptEscapeAction::Rewind(prompt_id) => {
                                    ui.pending_escape_rewind = Some(prompt_id);
                                    commands.submit(UiCommand::RewindContext {
                                        agent_id: require_agent(current_agent.as_ref())?.clone(),
                                        event_id: prompt_id,
                                    })?;
                                }
                                TranscriptEscapeAction::Clear => ui.input.clear(),
                            }
                            if matches!(action, TranscriptEscapeAction::Clear) {
                                sync_input_draft(
                                    commands,
                                    require_agent(current_agent.as_ref())?,
                                    &mut ui,
                                )?;
                            }
                        }
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::InputChanged,
                            view,
                        )?;
                    }
                    KeyCode::Enter => {
                        let prompt = ui.input.trim().to_owned();
                        ui.input.clear();
                        if prompt.is_empty() {
                            if let Some(agent_id) = current_agent.as_ref() {
                                sync_input_draft(commands, agent_id, &mut ui)?;
                            }
                            ui.redraw(
                                &mut stdout,
                                current_events(&snapshot, current_agent.as_ref())?,
                                RedrawCause::InputChanged,
                                view,
                            )?;
                            continue;
                        }
                        if prompt == "/exit" {
                            if let Some(agent_id) = current_agent.as_ref() {
                                sync_input_draft(commands, agent_id, &mut ui)?;
                            }
                            break;
                        }
                        let Some(agent_id) = current_agent.as_ref() else {
                            ui.redraw(&mut stdout, &[], RedrawCause::InputChanged, view)?;
                            continue;
                        };
                        let UiCommandReceipt::UserPromptSubmitted {
                            prompt_revision,
                            input_draft_revision,
                        } = commands.submit(UiCommand::SubmitUserPrompt {
                            agent_id: agent_id.clone(),
                            content: prompt,
                        })?
                        else {
                            return Err(
                                "SubmitUserPrompt did not return its submission revision".into()
                            );
                        };
                        observed_prompt_submission_revision = prompt_revision;
                        ui.input_draft_revision = input_draft_revision;
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::InputChanged,
                            view,
                        )?;
                    }
                    KeyCode::Backspace => {
                        ui.input.pop();
                        sync_input_draft(
                            commands,
                            require_agent(current_agent.as_ref())?,
                            &mut ui,
                        )?;
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::InputChanged,
                            view,
                        )?;
                    }
                    KeyCode::Char('o') if control => {
                        terminal.enter_detail_screen(&mut stdout)?;
                        view = terminal.detail_view();
                        ui.detail_scroll = 0;
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::ViewChanged,
                            view,
                        )?;
                    }
                    KeyCode::Char(character) if !control => {
                        ui.input.push(character);
                        sync_input_draft(
                            commands,
                            require_agent(current_agent.as_ref())?,
                            &mut ui,
                        )?;
                        if ui.input == "/" {
                            ui.open_command_palette();
                        }
                        ui.redraw(
                            &mut stdout,
                            current_events(&snapshot, current_agent.as_ref())?,
                            RedrawCause::InputChanged,
                            view,
                        )?;
                    }
                    _ => {}
                }
            }
            TerminalEvent::Paste(text) => {
                if !view.accepts_input() || ui.read_only_state.is_some() {
                    continue;
                }
                let text = text.replace(['\r', '\n'], " ");
                if let Some(OverlayState::Commands { selected, .. }) = ui.overlay.as_mut() {
                    ui.input.push_str(&text);
                    *selected = 0;
                } else if ui.overlay.is_some() {
                    continue;
                } else {
                    ui.input.push_str(&text);
                }
                if ui.input.starts_with('/')
                    && let Some(scope) = ui.command_scope()
                {
                    ui.overlay = Some(OverlayState::Commands { scope, selected: 0 });
                }
                sync_input_draft(commands, require_agent(current_agent.as_ref())?, &mut ui)?;
                ui.redraw(
                    &mut stdout,
                    current_events(&snapshot, current_agent.as_ref())?,
                    RedrawCause::InputChanged,
                    view,
                )?;
            }
            TerminalEvent::Resize(_, _) => {
                if view == TuiView::TerminalPreview {
                    if let Some(agent_id) = current_agent.as_ref() {
                        refresh_terminal_preview(
                            backend,
                            agent_id,
                            &mut stdout,
                            &mut preview_selection,
                            &mut preview_sessions,
                            &mut preview_renderer,
                        )?;
                    }
                } else {
                    ui.redraw_if_terminal_size_changed(
                        &mut stdout,
                        current_events(&snapshot, current_agent.as_ref())?,
                        view,
                    )?;
                }
            }
            _ => {}
        }
    }
    if view.is_tool_details() {
        terminal.leave_detail_screen(&mut stdout)?;
    }
    ui.renderer.finish(&mut stdout)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum OverlayAction {
    Redraw,
    Close,
    Open(SlashCommand),
    SubmitModel(String),
    SubmitEffort(String),
    SubmitClear,
    OpenContextDetail(ContextAction),
    ConfirmContextClear,
    BackToContext {
        breakdown: Box<ContextUsageBreakdown>,
        selected: usize,
    },
    SubmitRewind(EventId),
    SubmitAgentAdd(String),
    SubmitAgentDelete(AgentId),
}

fn handle_overlay_key(
    overlay: &mut OverlayState,
    input: &mut String,
    key: KeyCode,
) -> OverlayAction {
    if key == KeyCode::Esc {
        if let OverlayState::ContextDetail {
            breakdown,
            selected,
            ..
        } = overlay
        {
            return OverlayAction::BackToContext {
                breakdown: breakdown.clone(),
                selected: *selected,
            };
        }
        return OverlayAction::Close;
    }
    match overlay {
        OverlayState::Commands { scope, selected } => {
            let matches = matching_commands(input, *scope);
            match key {
                KeyCode::Up => move_selection_up(selected, matches.len()),
                KeyCode::Down => move_selection_down(selected, matches.len()),
                KeyCode::Backspace => {
                    input.pop();
                    *selected = 0;
                    if input.is_empty() {
                        return OverlayAction::Close;
                    }
                }
                KeyCode::Char(character) => {
                    input.push(character);
                    *selected = 0;
                }
                KeyCode::Enter => {
                    return matches
                        .get(*selected)
                        .copied()
                        .map(OverlayAction::Open)
                        .unwrap_or(OverlayAction::Redraw);
                }
                _ => {}
            }
        }
        OverlayState::Model { choices, selected } => match key {
            KeyCode::Up => move_selection_up(selected, choices.len()),
            KeyCode::Down => move_selection_down(selected, choices.len()),
            KeyCode::Enter => {
                return choices
                    .get(*selected)
                    .cloned()
                    .map(OverlayAction::SubmitModel)
                    .unwrap_or(OverlayAction::Close);
            }
            _ => {}
        },
        OverlayState::Effort { choices, selected } => match key {
            KeyCode::Up => move_selection_up(selected, choices.len()),
            KeyCode::Down => move_selection_down(selected, choices.len()),
            KeyCode::Enter => {
                return choices
                    .get(*selected)
                    .cloned()
                    .map(OverlayAction::SubmitEffort)
                    .unwrap_or(OverlayAction::Close);
            }
            _ => {}
        },
        OverlayState::Clear { selected } => match key {
            KeyCode::Up | KeyCode::Down => *selected = 1_usize.saturating_sub(*selected),
            KeyCode::Enter if *selected == 0 => return OverlayAction::SubmitClear,
            KeyCode::Enter => return OverlayAction::Close,
            _ => {}
        },
        OverlayState::Context {
            breakdown,
            selected,
        } => {
            let actions = breakdown.actions();
            match key {
                KeyCode::Up => move_selection_up(selected, actions.len()),
                KeyCode::Down => move_selection_down(selected, actions.len()),
                KeyCode::Enter => {
                    return actions
                        .get(*selected)
                        .copied()
                        .map(|action| match action {
                            ContextAction::Clear => OverlayAction::ConfirmContextClear,
                            _ => OverlayAction::OpenContextDetail(action),
                        })
                        .unwrap_or(OverlayAction::Redraw);
                }
                _ => {}
            }
        }
        OverlayState::ContextDetail { scroll, .. } => match key {
            KeyCode::Up => *scroll = scroll.saturating_sub(1),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
            KeyCode::Down => *scroll = scroll.saturating_add(1),
            KeyCode::PageDown => *scroll = scroll.saturating_add(10),
            _ => {}
        },
        OverlayState::Rewind { choices, selected } => match key {
            KeyCode::Up => move_selection_up(selected, choices.len()),
            KeyCode::Down => move_selection_down(selected, choices.len()),
            KeyCode::Enter => {
                return choices
                    .get(*selected)
                    .map(|choice| OverlayAction::SubmitRewind(choice.event_id))
                    .unwrap_or(OverlayAction::Close);
            }
            _ => {}
        },
        OverlayState::AgentAdd { choices, selected } => match key {
            KeyCode::Up => move_selection_up(selected, choices.len()),
            KeyCode::Down => move_selection_down(selected, choices.len()),
            KeyCode::Enter => {
                return choices
                    .get(*selected)
                    .cloned()
                    .map(OverlayAction::SubmitAgentAdd)
                    .unwrap_or(OverlayAction::Close);
            }
            _ => {}
        },
        OverlayState::AgentDelete {
            id,
            blocker,
            selected,
            ..
        } => match key {
            KeyCode::Up | KeyCode::Down if blocker.is_none() => {
                *selected = 1_usize.saturating_sub(*selected)
            }
            KeyCode::Enter if blocker.is_none() && *selected == 0 => {
                return OverlayAction::SubmitAgentDelete(id.clone());
            }
            KeyCode::Enter => return OverlayAction::Close,
            _ => {}
        },
    }
    OverlayAction::Redraw
}

fn matching_commands(input: &str, scope: CommandScope) -> Vec<SlashCommand> {
    scope
        .commands()
        .iter()
        .copied()
        .filter(|command| command.name().starts_with(input))
        .collect()
}

fn move_selection_up(selected: &mut usize, count: usize) {
    if count > 0 {
        *selected = if *selected == 0 {
            count - 1
        } else {
            *selected - 1
        };
    }
}

fn observe_edb_mutation(observed_revision: &mut u64, current_revision: u64) -> bool {
    let changed = *observed_revision != current_revision;
    *observed_revision = current_revision;
    changed
}

fn observe_prompt_submission(observed_revision: &mut u64, current_revision: u64) -> bool {
    let should_clear = current_revision > *observed_revision;
    *observed_revision = current_revision;
    should_clear
}

fn move_selection_down(selected: &mut usize, count: usize) {
    if count > 0 {
        *selected = (*selected + 1) % count;
    }
}

fn rewind_choices(events: &[Event]) -> Result<Vec<RewindChoice>> {
    Ok(events
        .iter()
        .filter_map(|event| match event {
            Event::UserPrompt(prompt) => Some(RewindChoice {
                event_id: prompt.id,
                kind: RewindChoiceKind::UserPrompt(prompt.content.clone()),
            }),
            Event::ContextCleared(cleared) => Some(RewindChoice {
                event_id: cleared.id,
                kind: RewindChoiceKind::ContextCleared,
            }),
            Event::CompactStateUpdate(update) if update.state == CompactState::Completed => {
                Some(RewindChoice {
                    event_id: update.id,
                    kind: RewindChoiceKind::ContextCompacted,
                })
            }
            _ => None,
        })
        .rev()
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptEscapeAction {
    Abort,
    Wait,
    Rewind(EventId),
    Clear,
}

fn transcript_escape_action(events: &[Event]) -> Result<TranscriptEscapeAction> {
    Ok(match current_user_turn_state(events)? {
        Some(UserTurnState::Active(_)) => TranscriptEscapeAction::Abort,
        Some(UserTurnState::Aborting(_)) => TranscriptEscapeAction::Wait,
        Some(UserTurnState::Aborted(prompt_id)) => TranscriptEscapeAction::Rewind(prompt_id),
        Some(UserTurnState::Completed(_)) | None => TranscriptEscapeAction::Clear,
    })
}

const PANEL_ROWS: usize = 5;
const INPUT_ROW_OFFSET: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiView {
    Transcript,
    ToolDetails,
    ToolDetailsInline,
    TerminalPreview,
    WorkMap,
}

impl TuiView {
    fn is_tool_details(self) -> bool {
        matches!(self, Self::ToolDetails | Self::ToolDetailsInline)
    }

    fn accepts_input(self) -> bool {
        self == Self::Transcript
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedrawCause {
    Startup,
    EdbUpdated,
    InputChanged,
    TerminalResized,
    ViewChanged,
    ContextChanged,
    DetailScrolled,
}

fn requires_full_replay(
    cause: RedrawCause,
    initialized: bool,
    previous_width: usize,
    current_width: usize,
) -> bool {
    !initialized
        || previous_width != current_width
        || matches!(
            cause,
            RedrawCause::Startup
                | RedrawCause::TerminalResized
                | RedrawCause::ViewChanged
                | RedrawCause::ContextChanged
        )
}

fn terminal_size_changed(previous: Option<(u16, u16)>, current: (u16, u16)) -> bool {
    previous != Some(current)
}

fn workmap_view_needs_redraw(
    observed_events: usize,
    events: &[Event],
    edb_was_modified: bool,
) -> bool {
    edb_was_modified
        || observed_events > events.len()
        || events.get(observed_events..).is_some_and(|events| {
            events
                .iter()
                .any(|event| matches!(event, Event::WorkMapMutation(_) | Event::ContextCleared(_)))
        })
}

fn toggles_workmap_history(code: KeyCode, control: bool) -> bool {
    control && code == KeyCode::Char('o')
}

#[derive(Clone, Copy)]
struct RedrawRequest<'a> {
    cause: RedrawCause,
    view: TuiView,
    events: &'a [Event],
    input: &'a str,
    overlay: Option<&'a OverlayState>,
    agent_id: &'a str,
    orchestrator_name: &'a str,
    context_window: Option<u64>,
    api_activity: UiApiActivity,
    read_only_state: Option<ReadOnlyAgentState>,
    worker_events: &'a [Event],
}

struct TranscriptRenderer {
    rows: Vec<UiRow>,
    current_objective: Option<WorkMapObjectiveSnapshot>,
    initialized: bool,
    content_width: usize,
    terminal_size: Option<(u16, u16)>,
    animations: Option<LiveIndicators>,
    output_lock: Arc<Mutex<()>>,
    last_input_at_ms: Arc<AtomicU64>,
    workmap_history_expanded: bool,
    banner: StartupBanner,
}

impl TranscriptRenderer {
    fn new(banner: StartupBanner) -> Self {
        Self {
            rows: Vec::new(),
            current_objective: None,
            initialized: false,
            content_width: 0,
            terminal_size: None,
            animations: None,
            output_lock: Arc::new(Mutex::new(())),
            last_input_at_ms: Arc::new(AtomicU64::new(0)),
            workmap_history_expanded: false,
            banner,
        }
    }

    fn redraw(
        &mut self,
        stdout: &mut Stdout,
        projection: &mut ChatProjection,
        detail_scroll: &mut usize,
        request: RedrawRequest<'_>,
    ) -> Result<()> {
        if request.cause == RedrawCause::InputChanged {
            self.last_input_at_ms
                .store(current_timestamp_ms(), Ordering::Release);
        }
        let terminal_size = terminal::size()?;
        let (terminal_width, _) = terminal_size;
        let content_width = usize::from(terminal_width.saturating_sub(1).max(1));
        let event_prefix_changed = projection.next_order > request.events.len()
            || (projection.next_order > 0
                && request.events.get(projection.next_order - 1).map(Event::id)
                    != projection.last_event_id);
        let input_only = request.cause == RedrawCause::InputChanged
            && request.view == TuiView::Transcript
            && request.overlay.is_none()
            && self.initialized
            && self.content_width == content_width
            && !event_prefix_changed
            && projection.next_order == request.events.len();
        if input_only {
            let _output_guard = self
                .output_lock
                .lock()
                .map_err(|_| "TUI output lock was poisoned")?;
            rewrite_input_row(
                stdout,
                request.input,
                request.orchestrator_name,
                request.read_only_state,
                terminal_width,
            )?;
            self.terminal_size = Some(terminal_size);
            stdout.flush()?;
            return Ok(());
        }
        self.pause_animations();
        let workmap_changed = !self.initialized
            || event_prefix_changed
            || request
                .events
                .get(projection.next_order..)
                .is_some_and(|events| {
                    events.iter().any(|event| {
                        matches!(event, Event::WorkMapMutation(_) | Event::ContextCleared(_))
                    })
                });
        if workmap_changed {
            self.current_objective =
                WorkMapProjection::from_events(request.events)?.current_snapshot();
        }
        let cause = if request
            .events
            .get(projection.next_order..)
            .is_some_and(|events| {
                events
                    .iter()
                    .any(|event| matches!(event, Event::ContextCleared(_)))
            }) {
            RedrawCause::ContextChanged
        } else {
            request.cause
        };
        let full_replay =
            requires_full_replay(cause, self.initialized, self.content_width, content_width);
        if full_replay {
            *projection = ChatProjection::replay_events(request.events)?;
            self.banner.event_count = request.events.len();
            self.banner.model = projection
                .model_name
                .clone()
                .unwrap_or_else(|| "<unset>".into());
        } else {
            projection.consume_events(request.events)?;
        }
        projection.apply_api_activity(request.api_activity);
        projection.update_worker_activities(request.worker_events);

        self.content_width = content_width;
        self.terminal_size = Some(terminal_size);
        if let Some(overlay) = request.overlay {
            render_overlay(stdout, overlay, request.input)?;
            self.initialized = true;
            stdout.flush()?;
            return Ok(());
        }
        match request.view {
            TuiView::ToolDetails => {
                *detail_scroll = render_details(stdout, projection, *detail_scroll)?;
                return Ok(());
            }
            TuiView::ToolDetailsInline => {
                *detail_scroll = 0;
                render_details_inline(stdout, projection)?;
                return Ok(());
            }
            TuiView::TerminalPreview => {
                return Err("Terminal preview must use its dedicated renderer".into());
            }
            TuiView::WorkMap => {
                render_workmap(
                    stdout,
                    request.events,
                    request.agent_id,
                    terminal_size,
                    self.workmap_history_expanded,
                )?;
                self.rows.clear();
                self.initialized = false;
                stdout.flush()?;
                return Ok(());
            }
            TuiView::Transcript => {}
        }

        let mut rows = chat_rows(projection, content_width, false, current_timestamp_ms());
        append_current_objective_summary(&mut rows, self.current_objective.as_ref(), content_width);

        if full_replay {
            clear_for_full_redraw(stdout, self.initialized)?;
            write_rows(stdout, &self.banner.rows(content_width), content_width)?;
            write_rows(stdout, &rows, content_width)?;
            write_panel(
                stdout,
                projection,
                request.input,
                request.agent_id,
                request.orchestrator_name,
                request.context_window,
                request.api_activity,
                request.read_only_state,
                terminal_width,
            )?;
            self.rows = rows;
            self.initialized = true;
            stdout.flush()?;
            self.resume_animations(projection);
            return Ok(());
        }

        let common = common_row_prefix(&self.rows, &rows);
        let old_suffix = self.rows.len().saturating_sub(common);
        rewind_to_transcript_tail(stdout, old_suffix)?;
        write_rows(stdout, &rows[common..], content_width)?;
        write_panel(
            stdout,
            projection,
            request.input,
            request.agent_id,
            request.orchestrator_name,
            request.context_window,
            request.api_activity,
            request.read_only_state,
            terminal_width,
        )?;
        self.rows = rows;
        stdout.flush()?;
        self.resume_animations(projection);
        Ok(())
    }

    fn finish(&mut self, stdout: &mut Stdout) -> Result<()> {
        self.pause_animations();
        if self.initialized {
            rewind_to_transcript_tail(stdout, 0)?;
            queue!(stdout, ResetColor, SetAttribute(Attribute::Reset), Show)?;
            stdout.flush()?;
            self.initialized = false;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn redraw_status(
        &self,
        stdout: &mut Stdout,
        projection: &ChatProjection,
        agent_id: &str,
        orchestrator_name: &str,
        context_window: Option<u64>,
        api_activity: UiApiActivity,
        read_only_state: Option<ReadOnlyAgentState>,
    ) -> Result<()> {
        if !self.initialized {
            return Ok(());
        }
        let terminal_width = self.terminal_size.unwrap_or(terminal::size()?).0;
        let _output_guard = self
            .output_lock
            .lock()
            .map_err(|_| "TUI output lock was poisoned")?;
        rewrite_status_row(
            stdout,
            projection,
            agent_id,
            orchestrator_name,
            context_window,
            api_activity,
            read_only_state,
            terminal_width,
        )?;
        stdout.flush()?;
        Ok(())
    }

    fn pause_animations(&mut self) {
        if let Some(mut animations) = self.animations.take() {
            animations.stop();
        }
    }

    fn toggle_workmap_history(&mut self) {
        self.workmap_history_expanded = !self.workmap_history_expanded;
    }

    fn suspend_for_external_view(&mut self) {
        self.pause_animations();
        self.rows.clear();
        self.initialized = false;
        self.terminal_size = None;
    }

    fn resume_animations(&mut self, projection: &ChatProjection) {
        let row_count = self.rows.len();
        let distance_from_tail = |index: usize| {
            u16::try_from(
                row_count
                    .saturating_add(INPUT_ROW_OFFSET)
                    .saturating_sub(index),
            )
            .unwrap_or(u16::MAX)
        };
        let marker_distances = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.tone == RowTone::ToolRunning)
            .map(|(index, _)| distance_from_tail(index))
            .collect::<Vec<_>>();
        let status_distances = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.tone == RowTone::ToolRunningStatus)
            .map(|(index, _)| distance_from_tail(index))
            .collect::<Vec<_>>();
        let running_tools = projection
            .messages
            .iter()
            .filter(|message| message.kind == ChatBlockKind::ToolCall)
            .filter_map(|message| message.tool.as_ref())
            .filter(|tool| tool.result.is_none() && !tool.queued)
            .collect::<Vec<_>>();
        let tool_clocks = status_distances
            .into_iter()
            .zip(running_tools)
            .map(|(distance, tool)| RunningToolClock {
                distance,
                tool_name: tool.name.clone(),
                started_at_ms: tool.started_at_ms,
                timeout_ms: terminal_timeout(&tool.name, &tool.arguments),
                width: self.content_width,
            })
            .collect::<Vec<_>>();
        let api_active = api_is_active(projection.api_state);
        if !marker_distances.is_empty() || !tool_clocks.is_empty() || api_active {
            self.animations = Some(LiveIndicators::start(
                marker_distances,
                tool_clocks,
                api_active,
                Arc::clone(&self.output_lock),
                Arc::clone(&self.last_input_at_ms),
            ));
        }
    }
}

struct StartupBanner {
    workspace: String,
    system: String,
    agent: String,
    orchestrator: String,
    model: String,
    terminal_backend: String,
    event_count: usize,
    edb_size_bytes: u64,
}

impl StartupBanner {
    fn current(
        agent: &str,
        orchestrator: &str,
        model: &str,
        event_count: usize,
        edb_size_bytes: u64,
        terminal_backend: &str,
    ) -> Self {
        Self {
            workspace: env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "<unknown>".to_owned()),
            system: format!("{}/{}", env::consts::OS, env::consts::ARCH),
            agent: agent.to_owned(),
            orchestrator: orchestrator.to_owned(),
            model: model.to_owned(),
            terminal_backend: terminal_backend.to_owned(),
            event_count,
            edb_size_bytes,
        }
    }

    fn rows(&self, width: usize) -> Vec<UiRow> {
        let mut rows = ME_S_LOGO
            .into_iter()
            .map(|line| UiRow::new(line, RowTone::BannerLogo))
            .collect::<Vec<_>>();
        rows.push(UiRow::new("", RowTone::Spacer));
        rows.push(UiRow::new(
            format!("Welcome to ME-S v{}", env!("CARGO_PKG_VERSION")),
            RowTone::BannerTitle,
        ));
        append_banner_field(&mut rows, "Workspace", &self.workspace, width);
        append_banner_field(&mut rows, "System", &self.system, width);
        append_banner_field(&mut rows, "Agent", &self.agent, width);
        append_banner_field(&mut rows, "Orchestrator", &self.orchestrator, width);
        append_banner_field(&mut rows, "Model", &self.model, width);
        append_banner_field(
            &mut rows,
            "EDB",
            &format!(
                "{} events · {}",
                self.event_count,
                format_byte_size(self.edb_size_bytes)
            ),
            width,
        );
        append_banner_field(&mut rows, "Terminal", &self.terminal_backend, width);
        rows.push(UiRow::new("", RowTone::Spacer));
        rows
    }
}

fn format_byte_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match bytes {
        0..KIB => format!("{bytes} B"),
        KIB..MIB => format!("{:.1} KiB", bytes as f64 / KIB as f64),
        MIB..GIB => format!("{:.1} MiB", bytes as f64 / MIB as f64),
        _ => format!("{:.1} GiB", bytes as f64 / GIB as f64),
    }
}

const ME_S_LOGO: [&str; 6] = [
    "███╗   ███╗███████╗      ███████╗",
    "████╗ ████║██╔════╝      ██╔════╝",
    "██╔████╔██║█████╗  █████╗███████╗",
    "██║╚██╔╝██║██╔══╝  ╚════╝╚════██║",
    "██║ ╚═╝ ██║███████╗      ███████║",
    "╚═╝     ╚═╝╚══════╝      ╚══════╝",
];

fn append_banner_field(rows: &mut Vec<UiRow>, label: &str, value: &str, width: usize) {
    append_prefixed_rows(
        rows,
        &format!("  {label:<13}"),
        value,
        width,
        RowTone::BannerInfo,
    );
}

struct LiveIndicators {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunningToolClock {
    distance: u16,
    tool_name: String,
    started_at_ms: u64,
    timeout_ms: Option<u64>,
    width: usize,
}

impl LiveIndicators {
    fn start(
        marker_distances: Vec<u16>,
        tool_clocks: Vec<RunningToolClock>,
        api_active: bool,
        output_lock: Arc<Mutex<()>>,
        last_input_at_ms: Arc<AtomicU64>,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let mut stdout = io::stdout();
            let mut tick = 0;
            while worker_running.load(Ordering::Acquire) {
                let now_ms = current_timestamp_ms();
                if input_has_animation_priority(last_input_at_ms.load(Ordering::Acquire), now_ms) {
                    thread::park_timeout(ANIMATION_INTERVAL);
                    continue;
                }
                let Ok(_output_guard) = output_lock.lock() else {
                    break;
                };
                if tick % TOOL_ANIMATION_TICKS == 0 {
                    if !tool_clocks.is_empty() {
                        let _ = paint_running_tool_clocks(&mut stdout, &tool_clocks, now_ms);
                    }
                    let phase = tick / TOOL_ANIMATION_TICKS % BREATHING_PHASES.len();
                    let (color, intensity) = BREATHING_PHASES[phase];
                    if !marker_distances.is_empty() {
                        let _ = paint_breathing_markers(
                            &mut stdout,
                            &marker_distances,
                            color,
                            intensity,
                        );
                    }
                }
                if api_active {
                    let frame = api_spinner_frame_at(now_ms);
                    let _ = paint_api_spinner(&mut stdout, frame);
                }
                drop(_output_guard);
                tick = tick.wrapping_add(1);
                thread::park_timeout(ANIMATION_INTERVAL);
            }
        });
        Self {
            running,
            handle: Some(handle),
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl Drop for LiveIndicators {
    fn drop(&mut self) {
        self.stop();
    }
}

const BREATHING_PHASES: [(Color, Attribute); 6] = [
    (Color::White, Attribute::Bold),
    (Color::White, Attribute::NormalIntensity),
    (Color::Grey, Attribute::NormalIntensity),
    (Color::DarkGrey, Attribute::Dim),
    (Color::Grey, Attribute::NormalIntensity),
    (Color::White, Attribute::NormalIntensity),
];
const API_SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TOOL_ANIMATION_TICKS: usize = 2;
const INPUT_ANIMATION_QUIET_PERIOD_MS: u64 = 250;
const ANIMATION_INTERVAL: Duration = Duration::from_millis(100);

fn api_spinner_frame_at(timestamp_ms: u64) -> &'static str {
    let frame_duration_ms = u64::try_from(ANIMATION_INTERVAL.as_millis())
        .unwrap_or(1)
        .max(1);
    let frame = timestamp_ms / frame_duration_ms;
    API_SPINNER_FRAMES[frame as usize % API_SPINNER_FRAMES.len()]
}

fn input_has_animation_priority(last_input_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_input_at_ms) < INPUT_ANIMATION_QUIET_PERIOD_MS
}

fn paint_running_tool_clocks<W: Write>(
    stdout: &mut W,
    clocks: &[RunningToolClock],
    now_ms: u64,
) -> Result<()> {
    for clock in clocks {
        let mut rows = Vec::with_capacity(1);
        append_running_tool_status(
            &mut rows,
            &running_tool_status_text(
                &clock.tool_name,
                clock.started_at_ms,
                clock.timeout_ms,
                now_ms,
            ),
            clock.width,
        );
        let row = rows
            .first()
            .ok_or("running tool clock did not produce a row")?;
        queue!(
            stdout,
            SavePosition,
            MoveToColumn(0),
            MoveUp(clock.distance),
            Clear(ClearType::CurrentLine)
        )?;
        print_row(stdout, row, clock.width)?;
        queue!(stdout, RestorePosition)?;
    }
    stdout.flush()?;
    Ok(())
}

fn paint_breathing_markers<W: Write>(
    stdout: &mut W,
    distances: &[u16],
    color: Color,
    intensity: Attribute,
) -> io::Result<()> {
    for distance in distances {
        queue!(
            stdout,
            SavePosition,
            MoveToColumn(0),
            MoveUp(*distance),
            SetForegroundColor(color),
            SetAttribute(intensity),
            Print("●"),
            ResetColor,
            SetAttribute(Attribute::Reset),
            RestorePosition
        )?;
    }
    stdout.flush()
}

fn paint_api_spinner<W: Write>(stdout: &mut W, frame: &str) -> io::Result<()> {
    queue!(
        stdout,
        SavePosition,
        MoveToColumn(0),
        MoveDown(2),
        SetForegroundColor(STATUS_API_COLOR),
        SetAttribute(Attribute::Bold),
        Print(frame),
        ResetColor,
        SetAttribute(Attribute::Reset),
        RestorePosition
    )?;
    stdout.flush()
}

fn common_row_prefix(left: &[UiRow], right: &[UiRow]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn rewind_to_transcript_tail(stdout: &mut Stdout, mutable_rows: usize) -> Result<()> {
    let up = u16::try_from(mutable_rows.saturating_add(PANEL_ROWS - 1)).unwrap_or(u16::MAX);
    queue!(
        stdout,
        ResetColor,
        SetAttribute(Attribute::Reset),
        MoveToColumn(0),
        MoveDown(2),
        MoveUp(up),
        Clear(ClearType::FromCursorDown)
    )?;
    Ok(())
}

fn write_rows(stdout: &mut Stdout, rows: &[UiRow], width: usize) -> Result<()> {
    for row in rows {
        print_row(stdout, row, width)?;
        queue!(stdout, Print("\r\n"))?;
    }
    Ok(())
}

fn clear_for_full_redraw<W: Write>(stdout: &mut W, purge_scrollback: bool) -> io::Result<()> {
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    if purge_scrollback {
        queue!(stdout, Clear(ClearType::Purge))?;
    }
    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0), Hide)
}

#[cfg(test)]
fn render_terminal_preview<W: Write>(
    stdout: &mut W,
    frame: &TerminalFrame,
    host_size: (u16, u16),
) -> io::Result<()> {
    let visible_rows = host_size.1.saturating_sub(1);
    let scroll_top = u64::try_from(frame.rows.len())
        .unwrap_or(u64::MAX)
        .saturating_sub(u64::from(visible_rows));
    render_terminal_preview_frame(stdout, frame, host_size, scroll_top, true)
}

fn render_terminal_preview_frame<W: Write>(
    stdout: &mut W,
    frame: &TerminalFrame,
    host_size: (u16, u16),
    scroll_top: u64,
    clear_screen: bool,
) -> io::Result<()> {
    if clear_screen {
        clear_for_full_redraw(stdout, false)?;
    } else {
        queue!(stdout, Hide)?;
    }
    let content_height = host_size.1.saturating_sub(1);
    for y in 0..content_height {
        queue!(
            stdout,
            MoveTo(0, y),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Clear(ClearType::CurrentLine)
        )?;
    }

    let styles = frame
        .style_defs
        .iter()
        .map(|definition| (definition.id, &definition.style))
        .collect::<BTreeMap<_, _>>();
    for row in &frame.rows {
        let Some(relative_row) = row.row.checked_sub(scroll_top) else {
            continue;
        };
        let Ok(y) = u16::try_from(relative_row) else {
            continue;
        };
        if y >= content_height {
            continue;
        }
        print_terminal_preview_row(
            stdout,
            y,
            frame.width,
            host_size.0.min(frame.width),
            row,
            &styles,
        )?;
    }
    if host_size.1 > 0 {
        print_terminal_preview_status(stdout, &frame.session_id, host_size.1 - 1, host_size.0)?;
    }
    if frame.cursor.visible
        && frame.cursor.row >= scroll_top
        && frame.cursor.row < scroll_top.saturating_add(u64::from(content_height))
        && frame.cursor.col < host_size.0.min(frame.width)
    {
        queue!(
            stdout,
            MoveTo(
                frame.cursor.col,
                u16::try_from(frame.cursor.row - scroll_top)
                    .expect("visible terminal preview cursor row fits u16")
            ),
            Show
        )?;
    } else {
        queue!(stdout, Hide)?;
    }
    Ok(())
}

fn print_terminal_preview_row<W: Write>(
    stdout: &mut W,
    y: u16,
    terminal_width: u16,
    visible_width: u16,
    row: &crate::terminal::TerminalRowUpdate,
    styles: &BTreeMap<u32, &TerminalStyle>,
) -> io::Result<()> {
    queue!(
        stdout,
        MoveTo(0, y),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Clear(ClearType::CurrentLine)
    )?;
    let mut column = 0_u16;
    for run in &row.runs {
        if run.col >= terminal_width || run.col >= visible_width {
            continue;
        }
        if run.col > column {
            queue!(
                stdout,
                ResetColor,
                SetAttribute(Attribute::Reset),
                Print(" ".repeat(usize::from(run.col - column)))
            )?;
        }
        let default_style = TerminalStyle::default();
        let style = styles.get(&run.style).copied().unwrap_or(&default_style);
        set_terminal_style(stdout, style)?;
        let available = visible_width.saturating_sub(run.col);
        queue!(
            stdout,
            Print(take_display_width(&run.text, usize::from(available)))
        )?;
        column = run.col.saturating_add(run.width).min(visible_width);
    }
    if column < visible_width {
        queue!(
            stdout,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Print(" ".repeat(usize::from(visible_width - column)))
        )?;
    }
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn set_terminal_style<W: Write>(stdout: &mut W, style: &TerminalStyle) -> io::Result<()> {
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    if let Some(color) = &style.foreground {
        queue!(stdout, SetForegroundColor(terminal_color(color)))?;
    }
    if let Some(color) = &style.background {
        queue!(stdout, SetBackgroundColor(terminal_color(color)))?;
    }
    for attribute in [
        style.bold.then_some(Attribute::Bold),
        style.dim.then_some(Attribute::Dim),
        style.italic.then_some(Attribute::Italic),
        style.underline.then_some(Attribute::Underlined),
        style.inverse.then_some(Attribute::Reverse),
    ]
    .into_iter()
    .flatten()
    {
        queue!(stdout, SetAttribute(attribute))?;
    }
    Ok(())
}

fn terminal_color(color: &TerminalColor) -> Color {
    match color {
        TerminalColor::Indexed(value) => Color::AnsiValue(*value),
        TerminalColor::Rgb([r, g, b]) => Color::Rgb {
            r: *r,
            g: *g,
            b: *b,
        },
    }
}

fn print_terminal_preview_status<W: Write>(
    stdout: &mut W,
    session_id: &str,
    y: u16,
    width: u16,
) -> io::Result<()> {
    queue!(
        stdout,
        MoveTo(0, y),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Clear(ClearType::CurrentLine)
    )?;
    let mut remaining = usize::from(width);
    for (text, color) in [
        (" me-s", STATUS_PRODUCT_COLOR),
        (" · ", STATUS_HINT_COLOR),
        ("Terminal", STATUS_ORCHESTRATOR_COLOR),
        (" · ", STATUS_HINT_COLOR),
        (session_id, STATUS_MODEL_COLOR),
    ] {
        if remaining == 0 {
            break;
        }
        let visible = take_display_width(text, remaining);
        remaining = remaining.saturating_sub(display_width(&visible));
        queue!(stdout, SetForegroundColor(color), Print(visible))?;
    }
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))
}

fn render_overlay(stdout: &mut Stdout, overlay: &OverlayState, input: &str) -> Result<()> {
    if matches!(overlay, OverlayState::Context { .. }) {
        return render_context_overlay(stdout, overlay);
    }
    if matches!(overlay, OverlayState::ContextDetail { .. }) {
        return render_context_detail_overlay(stdout, overlay);
    }
    if matches!(overlay, OverlayState::AgentAdd { .. }) {
        return render_agent_add_overlay(stdout, overlay);
    }
    let (terminal_width, terminal_height) = terminal::size()?;
    let screen_width = usize::from(terminal_width.max(1));
    let box_width = screen_width
        .saturating_sub(4)
        .clamp(24, 76)
        .min(screen_width);
    let (title, description, choices, selected) = overlay_content(overlay, input);
    let maximum_choices = usize::from(terminal_height).saturating_sub(7).max(1);
    let visible_count = choices.len().min(maximum_choices);
    let maximum_start = choices.len().saturating_sub(visible_count);
    let start = selected
        .saturating_sub(visible_count.saturating_sub(1))
        .min(maximum_start);
    let end = start.saturating_add(visible_count);
    let inner = box_width.saturating_sub(2);
    let mut rows = Vec::new();
    rows.push(UiRow::new(
        framed_title(&title, box_width),
        RowTone::OverlayBorder,
    ));
    rows.push(UiRow::new(
        framed_line(&description, inner),
        RowTone::OverlayText,
    ));
    rows.push(UiRow::new(framed_line("", inner), RowTone::OverlayText));
    if choices.is_empty() {
        rows.push(UiRow::new(
            framed_line("没有可用选项", inner),
            RowTone::OverlayText,
        ));
    } else {
        for (index, choice) in choices[start..end].iter().enumerate() {
            let absolute = start + index;
            rows.push(UiRow::new(
                framed_line(choice, inner),
                if absolute == selected {
                    RowTone::OverlaySelected
                } else {
                    RowTone::OverlayText
                },
            ));
        }
    }
    rows.push(UiRow::new(
        framed_line("↑↓ 选择 · Enter 确认 · Esc 返回", inner),
        RowTone::OverlayHint,
    ));
    rows.push(UiRow::new(
        format!("╰{}╯", "─".repeat(inner)),
        RowTone::OverlayBorder,
    ));

    clear_for_full_redraw(stdout, false)?;
    let x = u16::try_from(screen_width.saturating_sub(box_width) / 2).unwrap_or(0);
    let y = terminal_height.saturating_sub(rows.len() as u16) / 2;
    for (offset, row) in rows.iter().enumerate() {
        queue!(stdout, MoveTo(x, y.saturating_add(offset as u16)))?;
        print_row(stdout, row, box_width)?;
    }
    queue!(stdout, Hide)?;
    Ok(())
}

fn render_agent_add_overlay(stdout: &mut Stdout, overlay: &OverlayState) -> Result<()> {
    let OverlayState::AgentAdd { choices, selected } = overlay else {
        return Err("agent add renderer requires an AgentAdd overlay".into());
    };
    let (terminal_width, terminal_height) = terminal::size()?;
    let screen_width = usize::from(terminal_width.max(1));
    let box_width = screen_width
        .saturating_sub(4)
        .clamp(32, 96)
        .min(screen_width);
    let rows = agent_add_overlay_rows(choices, *selected, box_width);
    render_centered_overlay_rows(stdout, &rows, box_width, terminal_height)
}

fn agent_add_overlay_rows(choices: &[String], selected: usize, box_width: usize) -> Vec<UiRow> {
    let inner = box_width.saturating_sub(2);
    let available = inner.saturating_sub(2).max(1);
    let mut rows = vec![UiRow::new(
        framed_title("创建新的会话？", box_width),
        RowTone::OverlayBorder,
    )];
    for line in wrap("选择 Agent 类型。创建后不可更改。", available) {
        rows.push(UiRow::new(framed_line(&line, inner), RowTone::OverlayText));
    }
    rows.push(UiRow::new(framed_line("", inner), RowTone::OverlayText));
    if choices.is_empty() {
        rows.push(UiRow::new(
            framed_line("没有可用 Agent", inner),
            RowTone::OverlayText,
        ));
    } else {
        for (index, orchestrator) in choices.iter().enumerate() {
            let (label, detail) = agent_type_presentation(orchestrator);
            rows.push(UiRow::new(
                framed_line(&label, inner),
                if index == selected {
                    RowTone::OverlaySelected
                } else {
                    RowTone::OverlayText
                },
            ));
            for line in wrap(detail, available.saturating_sub(2).max(1)) {
                rows.push(UiRow::new(
                    framed_line(&format!("  {line}"), inner),
                    RowTone::OverlayHint,
                ));
            }
        }
    }
    rows.push(UiRow::new(
        framed_line("↑↓ 选择 · Enter 创建 · Esc 取消", inner),
        RowTone::OverlayHint,
    ));
    rows.push(UiRow::new(
        format!("╰{}╯", "─".repeat(inner)),
        RowTone::OverlayBorder,
    ));
    rows
}

fn agent_type_presentation(orchestrator: &str) -> (String, &'static str) {
    match orchestrator {
        "main-agent" => (
            "标准 (main-agent)".into(),
            "单 Agent 模式，响应直接，Token 开销较低",
        ),
        "manager-agent" => (
            "协作 (manager-agent)".into(),
            "双 Agent 协作，适合复杂任务，减少主模型上下文占用，但总 Token 开销更高",
        ),
        "chatbot" => ("聊天 (chatbot)".into(), "仅进行对话，不使用工作工具"),
        _ => (orchestrator.to_owned(), "自定义 Agent"),
    }
}

fn render_context_overlay(stdout: &mut Stdout, overlay: &OverlayState) -> Result<()> {
    let OverlayState::Context {
        breakdown,
        selected,
    } = overlay
    else {
        return Err("context renderer requires a context overlay".into());
    };
    let (terminal_width, terminal_height) = terminal::size()?;
    let screen_width = usize::from(terminal_width.max(1));
    let box_width = screen_width
        .saturating_sub(4)
        .clamp(44, 96)
        .min(screen_width);
    let inner = box_width.saturating_sub(2);
    let mut rows = vec![UiRow::new(
        framed_title("Context", box_width),
        RowTone::OverlayBorder,
    )];
    rows.push(UiRow::new(
        framed_line(&context_usage_summary(breakdown), inner),
        RowTone::OverlayText,
    ));
    rows.push(context_usage_bar_row(breakdown, box_width));
    rows.push(UiRow::new(framed_line("", inner), RowTone::OverlayText));
    for category in breakdown.categories() {
        rows.push(context_category_row(breakdown, category, box_width));
    }
    let actions = breakdown.actions();
    if !actions.is_empty() {
        rows.push(UiRow::new(framed_line("", inner), RowTone::OverlayText));
        for (index, action) in actions.iter().enumerate() {
            rows.push(UiRow::new(
                framed_line(action.label(), inner),
                if index == *selected {
                    RowTone::OverlaySelected
                } else {
                    RowTone::OverlayText
                },
            ));
        }
    }
    rows.push(UiRow::new(
        framed_line("↑↓ 选择 · Enter 确认 · Esc 返回", inner),
        RowTone::OverlayHint,
    ));
    rows.push(UiRow::new(
        format!("╰{}╯", "─".repeat(inner)),
        RowTone::OverlayBorder,
    ));
    render_centered_overlay_rows(stdout, &rows, box_width, terminal_height)
}

fn render_context_detail_overlay(stdout: &mut Stdout, overlay: &OverlayState) -> Result<()> {
    let OverlayState::ContextDetail {
        title,
        content,
        markdown,
        scroll,
        ..
    } = overlay
    else {
        return Err("context detail renderer requires a detail overlay".into());
    };
    let (terminal_width, terminal_height) = terminal::size()?;
    let screen_width = usize::from(terminal_width.max(1));
    let box_width = screen_width
        .saturating_sub(4)
        .clamp(44, 110)
        .min(screen_width);
    let inner = box_width.saturating_sub(2);
    let content_width = inner.saturating_sub(2).max(1);
    let content_lines = if *markdown {
        agent_markdown_renderer::render(content, content_width)
            .into_iter()
            .map(|line| line.spans)
            .collect::<Vec<_>>()
    } else {
        wrap(content, content_width)
            .into_iter()
            .map(|line| vec![MarkdownSpan::new(line, MarkdownTextStyle::default())])
            .collect::<Vec<_>>()
    };
    let capacity = usize::from(terminal_height).saturating_sub(3).max(1);
    let start = (*scroll).min(content_lines.len().saturating_sub(capacity));
    let end = start.saturating_add(capacity).min(content_lines.len());
    let mut rows = vec![UiRow::new(
        framed_title(title, box_width),
        RowTone::OverlayBorder,
    )];
    if content_lines.is_empty() {
        rows.push(UiRow::new(
            framed_line("没有可显示的内容", inner),
            RowTone::OverlayText,
        ));
    } else {
        for spans in &content_lines[start..end] {
            rows.push(framed_markdown_row(spans, box_width));
        }
    }
    rows.push(UiRow::new(
        framed_line("↑↓/PgUp/PgDn 滚动 · Esc 返回", inner),
        RowTone::OverlayHint,
    ));
    rows.push(UiRow::new(
        format!("╰{}╯", "─".repeat(inner)),
        RowTone::OverlayBorder,
    ));
    render_centered_overlay_rows(stdout, &rows, box_width, terminal_height)
}

fn render_centered_overlay_rows(
    stdout: &mut Stdout,
    rows: &[UiRow],
    box_width: usize,
    terminal_height: u16,
) -> Result<()> {
    let screen_width = usize::from(terminal::size()?.0.max(1));
    clear_for_full_redraw(stdout, false)?;
    let x = u16::try_from(screen_width.saturating_sub(box_width) / 2).unwrap_or(0);
    let y = terminal_height.saturating_sub(rows.len() as u16) / 2;
    for (offset, row) in rows.iter().enumerate() {
        queue!(stdout, MoveTo(x, y.saturating_add(offset as u16)))?;
        print_row(stdout, row, box_width)?;
    }
    queue!(stdout, Hide)?;
    stdout.flush()?;
    Ok(())
}

fn context_usage_summary(breakdown: &ContextUsageBreakdown) -> String {
    let percentage = match (breakdown.total, breakdown.limit) {
        (Some(total), Some(limit)) if limit > 0 => {
            format!("{}%", total.saturating_mul(100) / limit)
        }
        _ => "—".into(),
    };
    format!(
        "{} / {}  ·  {percentage}",
        format_context_tokens(breakdown.total),
        breakdown
            .limit
            .map(format_context_limit)
            .unwrap_or_else(|| "—".into())
    )
}

fn context_usage_bar_row(breakdown: &ContextUsageBreakdown, box_width: usize) -> UiRow {
    let inner = box_width.saturating_sub(2);
    let bar_width = inner.saturating_sub(2);
    let denominator = breakdown.limit.or(breakdown.total).unwrap_or(0).max(1);
    let mut remaining = bar_width;
    let mut spans = vec![MarkdownSpan::new(
        "│ ",
        MarkdownTextStyle::colored(MarkdownColorRole::Accent),
    )];
    for category in breakdown.categories() {
        if category != ContextCategory::Remaining
            && (breakdown.total.is_some() || category == ContextCategory::Reserve)
        {
            let value = breakdown.value(category);
            let width = usize::try_from(
                value
                    .saturating_mul(bar_width as u64)
                    .saturating_add(denominator / 2)
                    / denominator,
            )
            .unwrap_or(bar_width)
            .min(remaining);
            if width > 0 {
                spans.push(MarkdownSpan::new(
                    "█".repeat(width),
                    MarkdownTextStyle::colored(category.color()),
                ));
                remaining -= width;
            }
        }
    }
    let mut remaining_style = MarkdownTextStyle::colored(MarkdownColorRole::Muted);
    remaining_style.dim = true;
    spans.push(MarkdownSpan::new("░".repeat(remaining), remaining_style));
    spans.push(MarkdownSpan::new(
        " │",
        MarkdownTextStyle::colored(MarkdownColorRole::Accent),
    ));
    UiRow::markdown(spans, RowTone::OverlayText)
}

fn context_category_row(
    breakdown: &ContextUsageBreakdown,
    category: ContextCategory,
    box_width: usize,
) -> UiRow {
    let value = breakdown.value(category);
    let formatted = if category == ContextCategory::Remaining && breakdown.total.is_none() {
        "—".into()
    } else if matches!(
        category,
        ContextCategory::Reserve | ContextCategory::Remaining
    ) {
        format_context_tokens(Some(value))
    } else if breakdown.total.is_some() {
        format_estimated_context_tokens(value)
    } else {
        "—".into()
    };
    let share = if category == ContextCategory::Remaining && breakdown.total.is_none() {
        None
    } else if matches!(
        category,
        ContextCategory::Reserve | ContextCategory::Remaining
    ) {
        breakdown
            .limit
            .filter(|limit| *limit > 0)
            .map(|limit| value as f64 / limit as f64 * 100.0)
    } else {
        breakdown
            .total
            .filter(|total| *total > 0)
            .map(|total| value as f64 / total as f64 * 100.0)
    };
    let share = share.map_or_else(|| "—".into(), |share| format!("{share:.1}%"));
    let content = format!("● {:<12} {:>10}  {:>6}", category.label(), formatted, share);
    let available = box_width.saturating_sub(4);
    let content = truncate(&content, available);
    let padding = " ".repeat(available.saturating_sub(display_width(&content)));
    UiRow::markdown(
        vec![
            MarkdownSpan::new("│ ", MarkdownTextStyle::colored(MarkdownColorRole::Accent)),
            MarkdownSpan::new("●", MarkdownTextStyle::colored(category.color()).bold()),
            MarkdownSpan::new(
                content.strip_prefix('●').unwrap_or(&content),
                Default::default(),
            ),
            MarkdownSpan::new(padding, Default::default()),
            MarkdownSpan::new(" │", MarkdownTextStyle::colored(MarkdownColorRole::Accent)),
        ],
        RowTone::OverlayText,
    )
}

fn framed_markdown_row(spans: &[MarkdownSpan], box_width: usize) -> UiRow {
    let available = box_width.saturating_sub(4);
    let mut content = Vec::with_capacity(spans.len() + 3);
    content.push(MarkdownSpan::new(
        "│ ",
        MarkdownTextStyle::colored(MarkdownColorRole::Accent),
    ));
    let mut used = 0;
    for span in spans {
        if used >= available {
            break;
        }
        let text = take_display_width(&span.text, available - used);
        used += display_width(&text);
        content.push(MarkdownSpan::new(text, span.style));
    }
    content.push(MarkdownSpan::new(
        " ".repeat(available.saturating_sub(used)),
        MarkdownTextStyle::default(),
    ));
    content.push(MarkdownSpan::new(
        " │",
        MarkdownTextStyle::colored(MarkdownColorRole::Accent),
    ));
    UiRow::markdown(content, RowTone::OverlayText)
}

fn overlay_content(overlay: &OverlayState, input: &str) -> (String, String, Vec<String>, usize) {
    match overlay {
        OverlayState::Commands { scope, selected } => (
            match scope {
                CommandScope::Interactive => "Commands",
                CommandScope::Worker => "Worker commands",
            }
            .into(),
            format!("筛选：{input}"),
            matching_commands(input, *scope)
                .into_iter()
                .map(|command| format!("{:<10} {}", command.name(), command.description()))
                .collect(),
            *selected,
        ),
        OverlayState::Effort { choices, selected } => (
            "Reasoning effort".into(),
            "选择后将从下一次模型请求开始生效".into(),
            choices.clone(),
            *selected,
        ),
        OverlayState::Model { choices, selected } => (
            "Model".into(),
            "选择后将从下一次模型请求开始生效".into(),
            choices.clone(),
            *selected,
        ),
        OverlayState::Clear { selected } => (
            "Clear context".into(),
            "确认清空当前上下文？".into(),
            vec!["清空上下文".into(), "取消".into()],
            *selected,
        ),
        OverlayState::Context { .. } | OverlayState::ContextDetail { .. } => {
            unreachable!("context overlays use their dedicated renderer")
        }
        OverlayState::Rewind { choices, selected } => (
            "Rewind".into(),
            "目标事件及其后内容将从 EDB 删除".into(),
            choices
                .iter()
                .map(|choice| match &choice.kind {
                    RewindChoiceKind::UserPrompt(content) => format!(
                        "#{:<5} {}",
                        choice.event_id,
                        content.split_whitespace().collect::<Vec<_>>().join(" ")
                    ),
                    RewindChoiceKind::ContextCleared => {
                        format!("#{:<5} 上下文清理", choice.event_id)
                    }
                    RewindChoiceKind::ContextCompacted => {
                        format!("#{:<5} 上下文压缩", choice.event_id)
                    }
                })
                .collect(),
            *selected,
        ),
        OverlayState::AgentAdd { choices, selected } => (
            "创建新的会话？".into(),
            "选择 Agent 类型。创建后不可更改。".into(),
            choices
                .iter()
                .map(|orchestrator| agent_type_presentation(orchestrator).0)
                .collect(),
            *selected,
        ),
        OverlayState::AgentDelete {
            id,
            path,
            blocker,
            selected,
        } => (
            "Delete agent".into(),
            blocker.as_ref().map_or_else(
                || format!("永久删除 {id} 及 EDB：{path}"),
                |reason| format!("{id} 当前不可删除：{reason}"),
            ),
            if blocker.is_some() {
                vec!["返回".into()]
            } else {
                vec!["永久删除".into(), "取消".into()]
            },
            *selected,
        ),
    }
}

fn framed_title(title: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    let title = truncate(&format!("─ {title} "), inner);
    format!(
        "╭{title}{}╮",
        "─".repeat(inner.saturating_sub(display_width(&title)))
    )
}

fn framed_line(content: &str, inner: usize) -> String {
    let available = inner.saturating_sub(2);
    let content = truncate(content, available);
    format!(
        "│ {content}{} │",
        " ".repeat(available.saturating_sub(display_width(&content)))
    )
}

#[allow(clippy::too_many_arguments)]
fn write_panel<W: Write>(
    stdout: &mut W,
    projection: &ChatProjection,
    input: &str,
    agent_id: &str,
    orchestrator_name: &str,
    context_window: Option<u64>,
    api_activity: UiApiActivity,
    read_only_state: Option<ReadOnlyAgentState>,
    terminal_width: u16,
) -> Result<()> {
    let width = usize::from(terminal_width.saturating_sub(1).max(1));
    let visible_input = read_only_state.map_or_else(
        || tail(input, terminal_width.saturating_sub(3) as usize),
        |state| read_only_input_hint(orchestrator_name, state),
    );
    let status_text = panel_status_text(
        projection,
        agent_id,
        orchestrator_name,
        context_window,
        api_activity,
        read_only_state,
    );
    let status = truncate(&status_text, width);
    let input_row = if read_only_state.is_some() {
        format!("  {visible_input}")
    } else {
        format!("› {visible_input}")
    };
    let panel: [UiRow; PANEL_ROWS] = [
        UiRow::new("", RowTone::Spacer),
        UiRow::new("─".repeat(width), RowTone::Separator),
        UiRow::new(input_row, RowTone::Input),
        UiRow::new("─".repeat(width), RowTone::Separator),
        UiRow::new(status, RowTone::Status),
    ];
    for row in &panel[..PANEL_ROWS - 1] {
        print_row(stdout, row, width)?;
        queue!(stdout, Print("\r\n"))?;
    }
    print_row(stdout, &panel[PANEL_ROWS - 1], width)?;

    if read_only_state.is_some() {
        queue!(stdout, MoveUp(2), MoveToColumn(0), Hide)?;
    } else {
        let cursor_x = u16::try_from(display_width(&visible_input).saturating_add(2))
            .unwrap_or(terminal_width.saturating_sub(1))
            .min(terminal_width.saturating_sub(1));
        queue!(stdout, MoveUp(2), MoveToColumn(cursor_x), Show)?;
    }
    Ok(())
}

fn panel_status_text(
    projection: &ChatProjection,
    agent_id: &str,
    orchestrator_name: &str,
    context_window: Option<u64>,
    api_activity: UiApiActivity,
    read_only_state: Option<ReadOnlyAgentState>,
) -> String {
    let model_name = projection.model_name.as_deref().unwrap_or("<unset>");
    let effort = projection.effort.as_deref().unwrap_or("<unset>");
    let spinner_frame = api_spinner_frame_at(current_timestamp_ms());
    match read_only_state {
        Some(state) => {
            let spinner = if api_activity.active || api_is_active(projection.api_state) {
                spinner_frame
            } else {
                " "
            };
            let context = format_context_usage(
                projection.api_usage.map(|usage| usage.total_tokens),
                context_window,
            );
            let context = format_status_activity(&context, api_activity);
            let controls = if orchestrator_name == "worker-agent" {
                "/ 命令 · Tab 切换"
            } else {
                "Tab 切换"
            };
            format!(
                "{spinner} me-s · {agent_id} · {orchestrator_name} · {model_name} · {effort} · {context} · {state}   只读 · {controls}"
            )
        }
        None => main_status_text(
            projection.api_state,
            agent_id,
            orchestrator_name,
            model_name,
            effort,
            projection.api_usage.map(|usage| usage.total_tokens),
            context_window,
            api_activity,
            spinner_frame,
        ),
    }
}

fn rewrite_input_row<W: Write>(
    stdout: &mut W,
    input: &str,
    orchestrator_name: &str,
    read_only_state: Option<ReadOnlyAgentState>,
    terminal_width: u16,
) -> Result<()> {
    let width = usize::from(terminal_width.saturating_sub(1).max(1));
    let visible_input = read_only_state.map_or_else(
        || tail(input, terminal_width.saturating_sub(3) as usize),
        |state| read_only_input_hint(orchestrator_name, state),
    );
    let input_row = if read_only_state.is_some() {
        format!("  {visible_input}")
    } else {
        format!("› {visible_input}")
    };
    queue!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    print_row(stdout, &UiRow::new(input_row, RowTone::Input), width)?;
    if read_only_state.is_some() {
        queue!(stdout, MoveToColumn(0), Hide)?;
    } else {
        let cursor_x = u16::try_from(display_width(&visible_input).saturating_add(2))
            .unwrap_or(terminal_width.saturating_sub(1))
            .min(terminal_width.saturating_sub(1));
        queue!(stdout, MoveToColumn(cursor_x), Show)?;
    }
    Ok(())
}

fn read_only_input_hint(orchestrator_name: &str, state: ReadOnlyAgentState) -> String {
    if orchestrator_name == "worker-agent" {
        format!("只读 Worker · {state} · / 命令 · Tab 切换页面")
    } else {
        format!("只读子 Agent · {state} · Tab 切换页面")
    }
}

#[allow(clippy::too_many_arguments)]
fn rewrite_status_row<W: Write>(
    stdout: &mut W,
    projection: &ChatProjection,
    agent_id: &str,
    orchestrator_name: &str,
    context_window: Option<u64>,
    api_activity: UiApiActivity,
    read_only_state: Option<ReadOnlyAgentState>,
    terminal_width: u16,
) -> Result<()> {
    let width = usize::from(terminal_width.saturating_sub(1).max(1));
    let status = truncate(
        &panel_status_text(
            projection,
            agent_id,
            orchestrator_name,
            context_window,
            api_activity,
            read_only_state,
        ),
        width,
    );
    queue!(
        stdout,
        SavePosition,
        MoveDown(2),
        MoveToColumn(0),
        Clear(ClearType::CurrentLine)
    )?;
    print_row(stdout, &UiRow::new(status, RowTone::Status), width)?;
    queue!(stdout, RestorePosition)?;
    Ok(())
}

fn render_details(
    stdout: &mut Stdout,
    projection: &ChatProjection,
    scroll_from_bottom: usize,
) -> Result<usize> {
    let (terminal_width, terminal_height) = terminal::size()?;
    let width = usize::from(terminal_width.saturating_sub(1).max(1));
    let rows = chat_rows(projection, width, true, current_timestamp_ms());
    let viewport = usize::from(terminal_height.saturating_sub(2));
    let maximum_scroll = rows.len().saturating_sub(viewport);
    let scroll = scroll_from_bottom.min(maximum_scroll);
    let end = rows.len().saturating_sub(scroll);
    let start = end.saturating_sub(viewport);

    clear_for_full_redraw(stdout, true)?;
    for (y, row) in rows[start..end].iter().enumerate() {
        print_row_at(stdout, y as u16, row, width)?;
    }
    if terminal_height > 0 {
        let status = format!(
            " 工具详情：全部展开 · {}/{} · ↑↓/PgUp/PgDn 滚动 · Ctrl+O 返回",
            start.saturating_add(1).min(rows.len()),
            rows.len()
        );
        print_row_at(
            stdout,
            terminal_height - 1,
            &UiRow::new(status, RowTone::Status),
            width,
        )?;
    }
    stdout.flush()?;
    Ok(scroll)
}

fn render_details_inline(stdout: &mut Stdout, projection: &ChatProjection) -> Result<()> {
    let (terminal_width, _) = terminal::size()?;
    let width = usize::from(terminal_width.saturating_sub(1).max(1));
    let rows = detail_inline_rows(projection, width, current_timestamp_ms());

    clear_for_full_redraw(stdout, true)?;
    write_rows(stdout, &rows, width)?;
    stdout.flush()?;
    Ok(())
}

fn detail_inline_rows(projection: &ChatProjection, width: usize, now_ms: u64) -> Vec<UiRow> {
    let mut rows = chat_rows(projection, width, true, now_ms);
    rows.push(UiRow::new("", RowTone::Spacer));
    rows.push(UiRow::new(
        " 工具详情：全部展开 · Ctrl+O 返回",
        RowTone::Status,
    ));
    rows
}

fn render_workmap<W: Write>(
    stdout: &mut W,
    events: &[Event],
    agent_id: &str,
    terminal_size: (u16, u16),
    history_expanded: bool,
) -> Result<()> {
    let width = usize::from(terminal_size.0.saturating_sub(1).max(1));
    let snapshot = WorkMapProjection::from_events(events)?.snapshot();
    let record_count = workmap_record_count(&snapshot);
    let rows = workmap_rows(&snapshot, width, history_expanded);

    clear_for_full_redraw(stdout, true)?;
    for row in &rows {
        print_row(stdout, row, width)?;
        queue!(stdout, Print("\r\n"))?;
    }
    print_row(stdout, &UiRow::new("", RowTone::Spacer), width)?;
    queue!(stdout, Print("\r\n"))?;
    let history_action = if history_expanded {
        "收起历史详情"
    } else {
        "展开历史详情"
    };
    let status = format!(
        " me-s · WorkMap · {agent_id} · {record_count} records · {} lines   Ctrl+O {history_action} · Tab 切换",
        rows.len()
    );
    print_row(stdout, &UiRow::new(status, RowTone::Status), width)?;
    queue!(stdout, Hide)?;
    stdout.flush()?;
    Ok(())
}

fn workmap_rows(snapshot: &WorkMapSnapshot, width: usize, history_expanded: bool) -> Vec<UiRow> {
    let objective_count = snapshot.history.len() + usize::from(snapshot.current.is_some());
    let plan_count = workmap_plan_count(snapshot);
    let note_count = workmap_note_count(snapshot);
    let memory_count = snapshot.memory.facts.len() + snapshot.memory.agreements.len();
    let mut rows = vec![UiRow::new("WorkMap", RowTone::BannerTitle)];
    let summary = format!(
        "完整工作地图 · {memory_count} memories · {objective_count} objectives · {plan_count} plans · {note_count} notes · Memory / History / Current",
    );
    rows.extend(
        wrap(&summary, width)
            .into_iter()
            .map(|line| UiRow::new(line, RowTone::BannerInfo)),
    );
    rows.push(UiRow::new("", RowTone::Spacer));

    append_workmap_memory(&mut rows, snapshot, width);

    append_workmap_section(&mut rows, "History", snapshot.history.len());
    for objective in &snapshot.history {
        append_workmap_history_objective(&mut rows, objective, width, history_expanded);
    }
    append_empty_workmap_section(&mut rows, snapshot.history.is_empty());

    append_workmap_section(
        &mut rows,
        "Current",
        usize::from(snapshot.current.is_some()),
    );
    if let Some(current) = &snapshot.current {
        append_workmap_current(&mut rows, current, width);
    } else {
        append_empty_workmap_section(&mut rows, true);
    }
    while rows.last().is_some_and(|row| row.tone == RowTone::Spacer) {
        rows.pop();
    }
    rows
}

fn append_current_objective_summary(
    rows: &mut Vec<UiRow>,
    current: Option<&WorkMapObjectiveSnapshot>,
    width: usize,
) {
    let Some(current) = current else {
        return;
    };
    if !rows.is_empty() && rows.last().is_some_and(|row| row.tone != RowTone::Spacer) {
        rows.push(UiRow::new("", RowTone::Spacer));
    }

    let marker = if current
        .plans
        .iter()
        .any(|plan| plan.plan.state == PlanState::Active)
    {
        "■"
    } else {
        "□"
    };
    append_prefixed_rows(
        rows,
        &format!("{marker} "),
        &current.objective.title,
        width,
        RowTone::Assistant,
    );
    if let Some(description) = &current.objective.description {
        append_prefixed_rows(rows, "  ", description, width, RowTone::ToolDetail);
    }
    for plan in &current.plans {
        let tone = if plan.plan.state == PlanState::Active {
            RowTone::Assistant
        } else {
            RowTone::ToolDetail
        };
        let title = if plan.notes.is_empty() {
            plan.plan.title.clone()
        } else {
            let label = if plan.notes.len() == 1 {
                "note"
            } else {
                "notes"
            };
            format!("{} ({} {label})", plan.plan.title, plan.notes.len())
        };
        append_prefixed_rows(
            rows,
            &format!("    {} ", workmap_plan_symbol(plan.plan.state)),
            &title,
            width,
            tone,
        );
    }
}

fn workmap_record_count(snapshot: &WorkMapSnapshot) -> usize {
    snapshot.history.len()
        + usize::from(snapshot.current.is_some())
        + workmap_plan_count(snapshot)
        + workmap_note_count(snapshot)
        + snapshot.memory.facts.len()
        + snapshot.memory.agreements.len()
}

fn append_workmap_memory(rows: &mut Vec<UiRow>, snapshot: &WorkMapSnapshot, width: usize) {
    let count = snapshot.memory.facts.len() + snapshot.memory.agreements.len();
    append_workmap_section(rows, "Memory", count);
    if count == 0 {
        append_empty_workmap_section(rows, true);
        return;
    }
    append_workmap_memory_group(rows, "Facts", &snapshot.memory.facts, width);
    append_workmap_memory_group(rows, "Agreements", &snapshot.memory.agreements, width);
    rows.push(UiRow::new("", RowTone::Spacer));
}

fn append_workmap_memory_group(
    rows: &mut Vec<UiRow>,
    name: &str,
    memories: &[WorkMapMemory],
    width: usize,
) {
    rows.push(UiRow::new(
        format!("  {name} ({})", memories.len()),
        RowTone::BannerInfo,
    ));
    if memories.is_empty() {
        rows.push(UiRow::new("    —", RowTone::ToolDetail));
        return;
    }
    for memory in memories {
        let label = workmap_memory_label(memory);
        append_prefixed_rows(
            rows,
            &format!("    {} [{label}] ", workmap_memory_symbol(memory.state)),
            &memory.content,
            width,
            if memory.state == MemoryState::Active {
                RowTone::Assistant
            } else {
                RowTone::ToolDetail
            },
        );
        if let Some(reason) = &memory.status_reason {
            append_prefixed_rows(rows, "      ", reason, width, RowTone::ToolDetail);
        }
    }
}

fn append_workmap_history_objective(
    rows: &mut Vec<UiRow>,
    snapshot: &WorkMapObjectiveSnapshot,
    width: usize,
    expanded: bool,
) {
    let objective = &snapshot.objective;
    append_prefixed_rows(
        rows,
        &format!("{} ", workmap_objective_symbol(objective.state)),
        &objective.title,
        width,
        RowTone::Assistant,
    );
    if expanded && let Some(description) = &objective.description {
        append_prefixed_rows(rows, "  ", description, width, RowTone::ToolDetail);
    }
    if let Some(reason) = &objective.status_reason {
        if expanded {
            append_workmap_field(rows, "Reason", reason, width, RowTone::ToolDetail);
        } else {
            append_prefixed_rows(rows, "  ", reason, width, RowTone::ToolDetail);
        }
    }
    if expanded {
        for plan in &snapshot.plans {
            append_workmap_plan(rows, plan, width);
        }
        rows.push(UiRow::new("", RowTone::Spacer));
    }
}

fn workmap_plan_count(snapshot: &WorkMapSnapshot) -> usize {
    snapshot
        .history
        .iter()
        .chain(snapshot.current.iter())
        .map(|objective| objective.plans.len())
        .sum()
}

fn workmap_note_count(snapshot: &WorkMapSnapshot) -> usize {
    snapshot
        .history
        .iter()
        .chain(snapshot.current.iter())
        .flat_map(|objective| &objective.plans)
        .map(|plan| plan.notes.len())
        .sum()
}

fn append_workmap_section(rows: &mut Vec<UiRow>, name: &str, count: usize) {
    if rows.last().is_some_and(|row| row.tone != RowTone::Spacer) {
        rows.push(UiRow::new("", RowTone::Spacer));
    }
    rows.push(UiRow::new(
        format!("{name} ({count})"),
        RowTone::BannerTitle,
    ));
}

fn append_empty_workmap_section(rows: &mut Vec<UiRow>, empty: bool) {
    if empty {
        rows.push(UiRow::new("  —", RowTone::ToolDetail));
        rows.push(UiRow::new("", RowTone::Spacer));
    }
}

fn append_workmap_current(
    rows: &mut Vec<UiRow>,
    snapshot: &WorkMapObjectiveSnapshot,
    width: usize,
) {
    let objective = &snapshot.objective;
    append_prefixed_rows(
        rows,
        "Objective: ",
        &objective.title,
        width,
        RowTone::Assistant,
    );
    if let Some(description) = &objective.description {
        append_prefixed_rows(rows, "  ", description, width, RowTone::ToolDetail);
    }
    for plan in &snapshot.plans {
        append_workmap_plan(rows, plan, width);
    }
    rows.push(UiRow::new("", RowTone::Spacer));
}

fn append_workmap_plan(rows: &mut Vec<UiRow>, snapshot: &WorkMapPlanSnapshot, width: usize) {
    let plan = &snapshot.plan;
    append_prefixed_rows(
        rows,
        &format!("  {} ", workmap_plan_symbol(plan.state)),
        &plan.title,
        width,
        RowTone::Assistant,
    );
    if let Some(description) = &plan.description {
        append_prefixed_rows(rows, "   ", description, width, RowTone::ToolDetail);
    }
    if let Some(outcome) = &plan.outcome {
        append_workmap_field(rows, "Outcome", outcome, width, RowTone::Assistant);
    }
    if let Some(verification) = &plan.verification {
        append_workmap_field(
            rows,
            "Verification",
            verification,
            width,
            RowTone::Assistant,
        );
    }
    if let Some(reason) = &plan.status_reason {
        append_workmap_field(rows, "Reason", reason, width, RowTone::ToolDetail);
    }
    for (index, note) in snapshot.notes.iter().enumerate() {
        let connector = if index + 1 == snapshot.notes.len() {
            "└─"
        } else {
            "├─"
        };
        let prefix = format!("    {connector} [{}] ", workmap_note_kind_label(note.kind));
        append_prefixed_rows(rows, &prefix, &note.content, width, RowTone::Assistant);
    }
}

fn append_workmap_field(
    rows: &mut Vec<UiRow>,
    label: &str,
    value: &str,
    width: usize,
    tone: RowTone,
) {
    append_prefixed_rows(
        rows,
        &format!("  {label:<14}: "),
        &workmap_display_value(value),
        width,
        tone,
    );
}

fn workmap_display_value(value: &str) -> String {
    let mut display = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => display.push('\n'),
            '\r' => display.push_str("\\r"),
            '\t' => display.push_str("\\t"),
            character if character.is_control() => {
                write!(display, "\\u{{{:x}}}", u32::from(character))
                    .expect("writing to String cannot fail");
            }
            character => display.push(character),
        }
    }
    display
}

fn workmap_plan_symbol(value: PlanState) -> &'static str {
    match value {
        PlanState::Planned => "□",
        PlanState::Active => "■",
        PlanState::Completed => "✓",
        PlanState::Cancelled | PlanState::Superseded => "×",
    }
}

fn workmap_memory_symbol(value: MemoryState) -> &'static str {
    match value {
        MemoryState::Active => "●",
        MemoryState::Superseded | MemoryState::Retracted => "×",
    }
}

fn workmap_memory_label(memory: &WorkMapMemory) -> String {
    let kind = match memory.kind {
        MemoryKind::Fact => match memory.basis {
            Some(MemoryBasis::UserStated) => "FACT · USER STATED",
            Some(MemoryBasis::Observed) => "FACT · OBSERVED",
            Some(MemoryBasis::Verified) => "FACT · VERIFIED",
            Some(MemoryBasis::Inferred) => "FACT · INFERRED",
            None => "FACT",
        },
        MemoryKind::Agreement => "AGREEMENT",
    };
    match memory.state {
        MemoryState::Active => kind.into(),
        MemoryState::Superseded => format!("SUPERSEDED · {kind}"),
        MemoryState::Retracted => format!("RETRACTED · {kind}"),
    }
}

fn workmap_objective_symbol(value: ObjectiveState) -> &'static str {
    match value {
        ObjectiveState::Active => "■",
        ObjectiveState::Completed => "✓",
        ObjectiveState::Cancelled | ObjectiveState::Superseded => "×",
    }
}

fn workmap_note_kind_label(value: NoteKind) -> &'static str {
    match value {
        NoteKind::Action => "ACTION",
        NoteKind::Finding => "FINDING",
        NoteKind::Decision => "DECISION",
        NoteKind::Validation => "VALIDATION",
        NoteKind::Adjustment => "ADJUSTMENT",
        NoteKind::Blocker => "BLOCKER",
        NoteKind::Next => "NEXT",
        NoteKind::Note => "NOTE",
    }
}

fn api_is_active(state: Option<ApiState>) -> bool {
    matches!(
        state,
        Some(ApiState::Requesting | ApiState::Streaming | ApiState::Retrying)
    )
}

#[allow(clippy::too_many_arguments)]
fn main_status_text(
    api_state: Option<ApiState>,
    agent_id: &str,
    orchestrator_name: &str,
    model_name: &str,
    effort: &str,
    context_tokens: Option<u64>,
    context_window: Option<u64>,
    api_activity: UiApiActivity,
    spinner_frame: &str,
) -> String {
    let spinner = if api_activity.active || api_is_active(api_state) {
        spinner_frame
    } else {
        " "
    };
    let context = format_status_activity(
        &format_context_usage(context_tokens, context_window),
        api_activity,
    );
    format!(
        "{spinner} me-s · {agent_id} · {orchestrator_name} · {model_name} · {effort} · {context}   Ctrl+O 工具详情 · Esc 中止/撤回/清空"
    )
}

fn format_status_activity(context: &str, api_activity: UiApiActivity) -> String {
    api_activity
        .active
        .then_some(api_activity.received_sse_events)
        .map_or_else(
            || context.to_owned(),
            |events| format!("{context}  ↓ {events}"),
        )
}

fn format_context_usage(context_tokens: Option<u64>, context_window: Option<u64>) -> String {
    let used = context_tokens
        .map(|tokens| format!("{:.1}k", tokens as f64 / 1_000.0))
        .unwrap_or_else(|| "—".to_owned());
    let total = context_window
        .map(format_context_limit)
        .unwrap_or_else(|| "—".to_owned());
    format!("{used}/{total}")
}

fn estimate_context_breakdown(
    events: &[Event],
    usage: Option<ApiUsage>,
    limit: Option<u64>,
    reserve: u64,
    memory_content: Option<String>,
    can_clear: bool,
) -> Result<ContextUsageBreakdown> {
    let effective = effective_conversation_events(events)?;
    let current_compact = effective.iter().find_map(|event| match event {
        Event::CompactStateUpdate(update) if update.state == CompactState::Completed => {
            Some((update.compact_id, update.content.clone()))
        }
        _ => None,
    });
    let compact_content = current_compact.as_ref().map(|(_, content)| content.clone());
    let compact_analysis = current_compact.as_ref().and_then(|(compact_id, _)| {
        events.iter().find_map(|event| match event {
            Event::CompactStateUpdate(update)
                if update.compact_id == *compact_id
                    && update.state == CompactState::StageCompleted
                    && update.stage == Some(crate::event::CompactStage::Analysis) =>
            {
                Some(update.content.clone())
            }
            _ => None,
        })
    });
    let Some(usage) = usage else {
        return Ok(ContextUsageBreakdown {
            total: None,
            limit,
            reserve,
            values: ContextTokenValues::default(),
            compact_content,
            compact_analysis,
            memory_content,
            can_clear,
        });
    };
    let Some(boundary) = latest_usage_boundary(events, usage)? else {
        return Ok(ContextUsageBreakdown {
            total: Some(usage.total_tokens),
            limit,
            reserve,
            values: ContextTokenValues {
                system: usage.total_tokens,
                ..ContextTokenValues::default()
            },
            compact_content,
            compact_analysis,
            memory_content,
            can_clear,
        });
    };
    let api_state_event_id = events[boundary].id();
    if let Some(estimate) = events.iter().find_map(|event| match event {
        Event::ContextUsageEstimate(estimate)
            if estimate.api_state_event_id == api_state_event_id
                && estimate.values.sum() == usage.total_tokens =>
        {
            Some(estimate)
        }
        _ => None,
    }) {
        return Ok(ContextUsageBreakdown {
            total: Some(usage.total_tokens),
            limit,
            reserve,
            values: estimate.values.into(),
            compact_content: compact_content.clone(),
            compact_analysis: compact_analysis.clone(),
            memory_content: compact_content
                .is_some()
                .then_some(memory_content)
                .flatten(),
            can_clear,
        });
    }
    Ok(ContextUsageBreakdown {
        total: Some(usage.total_tokens),
        limit,
        reserve,
        values: ContextTokenValues {
            system: usage.total_tokens,
            ..ContextTokenValues::default()
        },
        compact_content: compact_content.clone(),
        compact_analysis,
        memory_content: compact_content
            .is_some()
            .then_some(memory_content)
            .flatten(),
        can_clear,
    })
}

fn compact_detail_content(breakdown: &ContextUsageBreakdown) -> Option<String> {
    let summary = breakdown.compact_content.as_ref()?;
    Some(match breakdown.compact_analysis.as_deref() {
        Some(analysis) => format!("## Analysis\n\n{analysis}\n\n---\n\n## 压缩摘要\n\n{summary}"),
        None => format!("## 压缩摘要\n\n{summary}"),
    })
}

fn latest_usage_boundary(events: &[Event], expected: ApiUsage) -> Result<Option<usize>> {
    let effective = effective_conversation_events(events)?;
    let mut errored = BTreeSet::new();
    let mut boundary = None;
    for event in effective {
        let Event::ApiStateUpdate(update) = event else {
            continue;
        };
        if update.state == ApiState::Error {
            errored.insert(update.api_call_id);
        }
        let committed = update.state == ApiState::Completed
            || (update.state == ApiState::Interrupted && !errored.contains(&update.api_call_id));
        if committed && update.usage.is_some() {
            boundary = Some((update.id, update.usage));
        }
    }
    let Some((event_id, usage)) = boundary else {
        return Ok(None);
    };
    if usage != Some(expected) {
        return Ok(None);
    }
    Ok(events.iter().position(|event| event.id() == event_id))
}

fn format_context_tokens(tokens: Option<u64>) -> String {
    tokens
        .map(|tokens| format!("{:.1}k", tokens as f64 / 1_000.0))
        .unwrap_or_else(|| "—".to_owned())
}

fn format_estimated_context_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        format!("≈{tokens} tok")
    } else {
        format!("≈{:.1}k", tokens as f64 / 1_000.0)
    }
}

fn format_context_limit(tokens: u64) -> String {
    if tokens.is_multiple_of(1_000) {
        format!("{}k", tokens / 1_000)
    } else {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowTone {
    BannerLogo,
    BannerTitle,
    BannerInfo,
    User,
    UserPadding,
    Assistant,
    TurnToolbar,
    MutedBulletHead,
    Tool,
    ToolDetail,
    ToolRunning,
    ToolRunningStatus,
    ToolQueued,
    ToolSucceeded,
    ToolFailed,
    Separator,
    Spacer,
    Input,
    Status,
    OverlayBorder,
    OverlayText,
    OverlaySelected,
    OverlayHint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UiRow {
    text: String,
    tone: RowTone,
    markdown_spans: Option<Vec<MarkdownSpan>>,
}

impl UiRow {
    fn new(text: impl Into<String>, tone: RowTone) -> Self {
        Self {
            text: text.into(),
            tone,
            markdown_spans: None,
        }
    }

    fn markdown(spans: Vec<MarkdownSpan>, tone: RowTone) -> Self {
        let text = spans.iter().map(|span| span.text.as_str()).collect();
        Self {
            text,
            tone,
            markdown_spans: Some(spans),
        }
    }
}

fn print_row_at<W: Write>(stdout: &mut W, y: u16, row: &UiRow, width: usize) -> Result<()> {
    queue!(stdout, MoveTo(0, y))?;
    print_row(stdout, row, width)
}

fn print_row<W: Write>(stdout: &mut W, row: &UiRow, width: usize) -> Result<()> {
    if let Some(spans) = &row.markdown_spans {
        print_markdown_spans(stdout, spans, width)?;
        return Ok(());
    }
    let text = truncate(&row.text, width);
    let fill_background = row_fills_background(row.tone);
    let padded = if fill_background {
        format!(
            "{text}{}",
            " ".repeat(width.saturating_sub(display_width(&text)))
        )
    } else {
        text.clone()
    };
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    match row.tone {
        RowTone::BannerLogo => {
            queue!(
                stdout,
                SetForegroundColor(BANNER_LOGO_COLOR),
                SetAttribute(Attribute::Bold)
            )?;
        }
        RowTone::BannerTitle => {
            queue!(
                stdout,
                SetForegroundColor(Color::White),
                SetAttribute(Attribute::Bold)
            )?;
        }
        RowTone::BannerInfo => {
            queue!(
                stdout,
                SetForegroundColor(Color::Grey),
                SetAttribute(Attribute::NormalIntensity)
            )?;
        }
        RowTone::User => {
            queue!(
                stdout,
                SetForegroundColor(Color::Yellow),
                SetBackgroundColor(USER_BACKGROUND_COLOR)
            )?;
        }
        RowTone::UserPadding => {
            queue!(stdout, SetBackgroundColor(USER_BACKGROUND_COLOR))?;
        }
        RowTone::Assistant | RowTone::Tool | RowTone::Input => {
            queue!(stdout, SetForegroundColor(Color::White))?;
        }
        RowTone::TurnToolbar => {
            queue!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                SetAttribute(Attribute::NormalIntensity)
            )?;
        }
        RowTone::MutedBulletHead => {
            let rest = text.strip_prefix('●').unwrap_or(&text);
            queue!(
                stdout,
                SetForegroundColor(MUTED_BULLET_COLOR),
                SetAttribute(Attribute::Bold),
                Print("●"),
                SetAttribute(Attribute::NormalIntensity),
                SetForegroundColor(Color::White),
                Print(rest),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
            return Ok(());
        }
        RowTone::ToolDetail | RowTone::ToolRunningStatus => {
            queue!(
                stdout,
                SetForegroundColor(TOOL_DETAIL_COLOR),
                SetAttribute(Attribute::NormalIntensity)
            )?;
        }
        RowTone::ToolRunning
        | RowTone::ToolQueued
        | RowTone::ToolSucceeded
        | RowTone::ToolFailed => {
            let rest = text.strip_prefix('●').unwrap_or(&text);
            let color = tool_marker_color(row.tone);
            queue!(
                stdout,
                SetForegroundColor(color),
                SetAttribute(Attribute::Bold),
                Print("●"),
                SetForegroundColor(Color::White),
                Print(rest),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
            return Ok(());
        }
        RowTone::Separator => {
            queue!(
                stdout,
                SetForegroundColor(SEPARATOR_COLOR),
                SetAttribute(Attribute::NormalIntensity)
            )?;
        }
        RowTone::Spacer => {}
        RowTone::Status => {
            if print_main_status(stdout, &text)? {
                return Ok(());
            }
            queue!(stdout, SetForegroundColor(STATUS_HINT_COLOR))?;
        }
        RowTone::OverlayBorder => {
            queue!(stdout, SetForegroundColor(STATUS_API_COLOR))?;
        }
        RowTone::OverlayText => {
            queue!(stdout, SetForegroundColor(Color::White))?;
        }
        RowTone::OverlaySelected => {
            queue!(
                stdout,
                SetForegroundColor(Color::White),
                SetBackgroundColor(OVERLAY_SELECTED_BACKGROUND),
                SetAttribute(Attribute::Bold)
            )?;
        }
        RowTone::OverlayHint => {
            queue!(stdout, SetForegroundColor(Color::Grey))?;
        }
    }
    queue!(
        stdout,
        Print(padded),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn print_markdown_spans<W: Write>(
    stdout: &mut W,
    spans: &[MarkdownSpan],
    width: usize,
) -> Result<()> {
    let mut remaining = width;
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let span_width = display_width(&span.text);
        let text = take_display_width(&span.text, remaining);
        let printed_width = display_width(&text);
        remaining = remaining.saturating_sub(printed_width);
        if text.is_empty() {
            break;
        }
        apply_markdown_style(stdout, span.style)?;
        queue!(stdout, Print(text))?;
        if printed_width < span_width {
            break;
        }
    }
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn apply_markdown_style<W: Write>(stdout: &mut W, style: MarkdownTextStyle) -> io::Result<()> {
    queue!(
        stdout,
        ResetColor,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(markdown_color(style.color))
    )?;
    for attribute in [
        style.bold.then_some(Attribute::Bold),
        style.italic.then_some(Attribute::Italic),
        style.underlined.then_some(Attribute::Underlined),
        style.crossed_out.then_some(Attribute::CrossedOut),
        style.dim.then_some(Attribute::Dim),
    ]
    .into_iter()
    .flatten()
    {
        queue!(stdout, SetAttribute(attribute))?;
    }
    Ok(())
}

fn markdown_color(role: MarkdownColorRole) -> Color {
    match role {
        MarkdownColorRole::Primary => Color::White,
        MarkdownColorRole::Muted | MarkdownColorRole::Border => Color::DarkGrey,
        MarkdownColorRole::Accent | MarkdownColorRole::Link => Color::Rgb {
            r: 122,
            g: 162,
            b: 247,
        },
        MarkdownColorRole::Code => Color::Rgb {
            r: 158,
            g: 206,
            b: 106,
        },
        MarkdownColorRole::SyntaxComment => Color::Rgb {
            r: 106,
            g: 153,
            b: 85,
        },
        MarkdownColorRole::SyntaxString => Color::Rgb {
            r: 206,
            g: 145,
            b: 120,
        },
        MarkdownColorRole::SyntaxKeyword => Color::Rgb {
            r: 197,
            g: 134,
            b: 192,
        },
        MarkdownColorRole::SyntaxDeclaration | MarkdownColorRole::SyntaxConstant => Color::Rgb {
            r: 86,
            g: 156,
            b: 214,
        },
        MarkdownColorRole::SyntaxNumber => Color::Rgb {
            r: 181,
            g: 206,
            b: 168,
        },
        MarkdownColorRole::SyntaxType => Color::Rgb {
            r: 78,
            g: 201,
            b: 176,
        },
        MarkdownColorRole::SyntaxFunction => Color::Rgb {
            r: 220,
            g: 220,
            b: 170,
        },
        MarkdownColorRole::SyntaxVariable => Color::Rgb {
            r: 156,
            g: 220,
            b: 254,
        },
        MarkdownColorRole::Math => STATUS_API_COLOR,
        MarkdownColorRole::Success => Color::Green,
        MarkdownColorRole::Warning => Color::Yellow,
        MarkdownColorRole::Error => Color::Red,
    }
}

const MUTED_BULLET_COLOR: Color = Color::Grey;
const TOOL_DETAIL_COLOR: Color = Color::DarkGrey;
const SEPARATOR_COLOR: Color = Color::DarkGrey;
const STATUS_PRODUCT_COLOR: Color = Color::Rgb {
    r: 120,
    g: 100,
    b: 255,
};
const STATUS_ORCHESTRATOR_COLOR: Color = Color::Rgb {
    r: 255,
    g: 165,
    b: 70,
};
const STATUS_MODEL_COLOR: Color = Color::Rgb {
    r: 238,
    g: 218,
    b: 170,
};
const STATUS_API_COLOR: Color = Color::Rgb {
    r: 125,
    g: 220,
    b: 215,
};
const STATUS_HINT_COLOR: Color = Color::Grey;
const USER_BACKGROUND_COLOR: Color = Color::Rgb {
    r: 35,
    g: 35,
    b: 38,
};
const BANNER_LOGO_COLOR: Color = Color::Rgb {
    r: 120,
    g: 100,
    b: 255,
};
const OVERLAY_SELECTED_BACKGROUND: Color = Color::Rgb {
    r: 55,
    g: 55,
    b: 62,
};

fn row_fills_background(tone: RowTone) -> bool {
    matches!(
        tone,
        RowTone::User | RowTone::UserPadding | RowTone::OverlaySelected
    )
}

fn print_main_status<W: Write>(stdout: &mut W, text: &str) -> io::Result<bool> {
    let Some(segments) = main_status_segments(text) else {
        return Ok(false);
    };
    for (color, segment) in segments {
        queue!(stdout, SetForegroundColor(color), Print(segment))?;
    }
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(true)
}

fn main_status_segments(text: &str) -> Option<Vec<(Color, &str)>> {
    let (spinner, text) = text.split_at(text.chars().next()?.len_utf8());
    if !text.starts_with(" me-s") || (spinner != " " && !API_SPINNER_FRAMES.contains(&spinner)) {
        return None;
    }
    let mut segments = vec![(
        if spinner == " " {
            STATUS_HINT_COLOR
        } else {
            STATUS_API_COLOR
        },
        spinner,
    )];
    for (index, field) in text.split(" · ").enumerate() {
        if index > 0 {
            segments.push((STATUS_HINT_COLOR, " · "));
        }
        match index {
            0 => segments.push((STATUS_PRODUCT_COLOR, field)),
            1 => segments.push((STATUS_ORCHESTRATOR_COLOR, field)),
            2 => segments.push((STATUS_ORCHESTRATOR_COLOR, field)),
            3 => segments.push((STATUS_MODEL_COLOR, field)),
            4 => segments.push((STATUS_MODEL_COLOR, field)),
            5 => {
                let (api, hint) = field.split_once("   ").unwrap_or((field, ""));
                segments.push((STATUS_API_COLOR, api));
                if !hint.is_empty() {
                    segments.push((STATUS_HINT_COLOR, "   "));
                    segments.push((STATUS_HINT_COLOR, hint));
                }
            }
            _ => segments.push((STATUS_HINT_COLOR, field)),
        }
    }
    Some(segments)
}

fn tool_marker_color(tone: RowTone) -> Color {
    match tone {
        RowTone::ToolRunning => Color::White,
        RowTone::ToolQueued => Color::DarkGrey,
        RowTone::ToolSucceeded => Color::Green,
        RowTone::ToolFailed => Color::Red,
        _ => unreachable!("row tone has no tool marker"),
    }
}

fn chat_rows(
    projection: &ChatProjection,
    width: usize,
    tools_expanded: bool,
    now_ms: u64,
) -> Vec<UiRow> {
    let mut rows = Vec::new();
    let mut previous_visible = None;
    for message in &projection.messages {
        if !message_is_visible(message) {
            continue;
        }
        if let Some(previous) = previous_visible {
            if tool_card_precedes_assistant(previous, message) {
                rows.push(UiRow::new("", RowTone::Spacer));
                rows.push(UiRow::new("─".repeat(width), RowTone::Separator));
                rows.push(UiRow::new("", RowTone::Spacer));
            } else if message_blocks_need_gap(previous, message) {
                rows.push(UiRow::new("", RowTone::Spacer));
            }
        }
        match (&message.kind, &message.tool) {
            (ChatBlockKind::User, _) => {
                rows.push(UiRow::new("", RowTone::UserPadding));
                append_prefixed_rows(&mut rows, " ", &message.content, width, RowTone::User);
                rows.push(UiRow::new("", RowTone::UserPadding));
            }
            (ChatBlockKind::Assistant, _) => {
                append_assistant_markdown_rows(
                    &mut rows,
                    trim_model_boundary_newlines(&message.content),
                    width,
                );
            }
            (ChatBlockKind::TurnToolbar, None) => {
                rows.push(UiRow::new(
                    format!(
                        "  ▶ 用时 {} · {}",
                        message.content,
                        format_turn_completed_at(message.timestamp_ms),
                    ),
                    RowTone::TurnToolbar,
                ));
            }
            (ChatBlockKind::ToolCall, Some(tool)) => {
                rows.extend(tool_rows(tool, width, tools_expanded, now_ms));
            }
            (ChatBlockKind::WorkerActivity, Some(wait)) => {
                rows.extend(worker_activity_rows(
                    wait,
                    projection.worker_activities.get(&wait.id),
                    width,
                ));
            }
            (ChatBlockKind::SessionState | ChatBlockKind::StateNotice, None) => {
                let first_row = rows.len();
                append_prefixed_rows(&mut rows, "● ", &message.content, width, RowTone::Tool);
                rows[first_row].tone = RowTone::MutedBulletHead;
            }
            (ChatBlockKind::ToolCall, None)
            | (ChatBlockKind::WorkerActivity, None)
            | (ChatBlockKind::TurnToolbar, Some(_))
            | (ChatBlockKind::SessionState | ChatBlockKind::StateNotice, Some(_)) => {
                unreachable!("chat block kind does not match its payload")
            }
        }
        previous_visible = Some(message);
    }
    rows
}

fn append_assistant_markdown_rows(rows: &mut Vec<UiRow>, content: &str, width: usize) {
    const PREFIX_WIDTH: usize = 2;
    let content_width = width.saturating_sub(PREFIX_WIDTH).max(1);
    let rendered = agent_markdown_renderer::render(content, content_width);
    for (index, line) in rendered.into_iter().enumerate() {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        let (prefix, style, tone) = if index == 0 {
            (
                "● ",
                MarkdownTextStyle::colored(MarkdownColorRole::Muted).bold(),
                RowTone::MutedBulletHead,
            )
        } else {
            ("  ", MarkdownTextStyle::default(), RowTone::Assistant)
        };
        spans.push(MarkdownSpan::new(prefix, style));
        spans.extend(line.spans);
        rows.push(UiRow::markdown(spans, tone));
    }
}

fn message_is_visible(message: &ChatMessage) -> bool {
    match (&message.kind, &message.tool) {
        (ChatBlockKind::Assistant, _) => !trim_model_boundary_newlines(&message.content)
            .trim()
            .is_empty(),
        (ChatBlockKind::WorkerActivity, Some(wait)) => worker_wait_is_visible(wait),
        _ => true,
    }
}

fn message_blocks_need_gap(previous: &ChatMessage, current: &ChatMessage) -> bool {
    if previous.kind == ChatBlockKind::Assistant && current.kind == ChatBlockKind::TurnToolbar {
        return false;
    }
    if previous.kind == ChatBlockKind::ToolCall && current.kind == ChatBlockKind::ToolCall {
        return false;
    }
    previous.kind != current.kind
        || (previous.kind == current.kind
            && matches!(
                &previous.kind,
                ChatBlockKind::ToolCall
                    | ChatBlockKind::WorkerActivity
                    | ChatBlockKind::SessionState
                    | ChatBlockKind::StateNotice
            ))
}

fn format_turn_elapsed(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_turn_tokens(tokens: Option<u64>) -> String {
    tokens
        .map(|tokens| format!("{:.1}k", tokens as f64 / 1_000.0))
        .unwrap_or_else(|| "—".to_owned())
}

fn format_turn_completed_at(timestamp_ms: u64) -> String {
    format_turn_completed_at_relative(timestamp_ms, current_timestamp_ms())
}

fn format_turn_completed_at_relative(timestamp_ms: u64, now_ms: u64) -> String {
    let to_local = |value: u64| {
        i64::try_from(value)
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .map(|value| value.with_timezone(&Local))
    };
    let Some(completed) = to_local(timestamp_ms) else {
        return "—".to_owned();
    };
    let Some(now) = to_local(now_ms) else {
        return "—".to_owned();
    };
    let days_ago = now
        .date_naive()
        .signed_duration_since(completed.date_naive())
        .num_days()
        .max(0);
    let day = match days_ago {
        0 => "今天".to_owned(),
        1 => "昨天".to_owned(),
        2 => "前天".to_owned(),
        days => format!("{days} 天前"),
    };
    format!("{day} {}", completed.format("%H:%M"))
}

fn tool_card_precedes_assistant(previous: &ChatMessage, current: &ChatMessage) -> bool {
    matches!(
        previous.kind,
        ChatBlockKind::ToolCall | ChatBlockKind::WorkerActivity
    ) && previous.tool.is_some()
        && current.kind == ChatBlockKind::Assistant
}

fn trim_model_boundary_newlines(content: &str) -> &str {
    content.trim_matches(['\r', '\n'])
}

fn append_prefixed_rows(
    rows: &mut Vec<UiRow>,
    prefix: &str,
    content: &str,
    width: usize,
    tone: RowTone,
) {
    let prefix_width = display_width(prefix);
    let content_width = width.saturating_sub(prefix_width).max(1);
    let wrapped = wrap(content, content_width);
    for (index, line) in wrapped.into_iter().enumerate() {
        let prefix = if index == 0 {
            prefix.to_owned()
        } else {
            " ".repeat(prefix_width)
        };
        rows.push(UiRow::new(format!("{prefix}{line}"), tone));
    }
}

fn worker_activity_rows(
    wait: &ToolCard,
    activity: Option<&WorkerActivity>,
    _width: usize,
) -> Vec<UiRow> {
    let state = activity
        .map(|activity| activity.state)
        .unwrap_or_else(|| worker_wait_state(wait));
    let (title, tone) = match state {
        WorkerActivityState::Running => ("正在执行", RowTone::ToolRunning),
        WorkerActivityState::Completed => ("已完成", RowTone::ToolSucceeded),
        WorkerActivityState::Interrupted => ("已中断", RowTone::ToolFailed),
        WorkerActivityState::Failed => ("未完成", RowTone::ToolFailed),
    };
    let mut rows = vec![UiRow::new(format!("● {title}"), tone)];
    let Some(activity) = activity else {
        return rows;
    };
    for tool in &activity.tools {
        let marker_color = match tool.result.as_ref().map(|result| result.state) {
            None => MarkdownColorRole::Primary,
            Some(ToolResultState::Succeeded) => MarkdownColorRole::Success,
            Some(
                ToolResultState::Failed | ToolResultState::Cancelled | ToolResultState::Interrupted,
            ) => MarkdownColorRole::Error,
        };
        let brief = tool_brief(tool);
        let mut spans = vec![
            MarkdownSpan::new("  ● ", MarkdownTextStyle::colored(marker_color).bold()),
            MarkdownSpan::new(tool.name.clone(), MarkdownTextStyle::default()),
        ];
        if !brief.is_empty() {
            spans.push(MarkdownSpan::new(
                format!(" {brief}"),
                MarkdownTextStyle::colored(MarkdownColorRole::Muted),
            ));
        }
        rows.push(UiRow::markdown(spans, RowTone::ToolDetail));
    }
    rows
}

fn worker_wait_is_visible(wait: &ToolCard) -> bool {
    wait.result.is_none() || worker_wait_state(wait) != WorkerActivityState::Running
}

fn worker_wait_turn_id(wait: &ToolCard) -> Option<EventId> {
    wait.result.as_ref().and_then(|result| {
        serde_json::from_str::<serde_json::Value>(&result.detail)
            .ok()?
            .get("turn_id")?
            .as_u64()
    })
}

fn worker_wait_state(wait: &ToolCard) -> WorkerActivityState {
    let Some(result) = wait.result.as_ref() else {
        return WorkerActivityState::Running;
    };
    if result.state != ToolResultState::Succeeded {
        return WorkerActivityState::Failed;
    }
    match serde_json::from_str::<serde_json::Value>(&result.detail)
        .ok()
        .and_then(|value| {
            value
                .get("state")
                .and_then(|state| state.as_str())
                .map(str::to_owned)
        })
        .as_deref()
    {
        Some("completed") => WorkerActivityState::Completed,
        Some("interrupted" | "stopped") => WorkerActivityState::Interrupted,
        Some("wait_interrupted") => WorkerActivityState::Running,
        Some("failed" | "api_error") => WorkerActivityState::Failed,
        _ => WorkerActivityState::Running,
    }
}

fn tool_brief(tool: &ToolCard) -> String {
    if let Some(result) = tool.result.as_ref()
        && result.state != ToolResultState::Succeeded
    {
        let detail = result.detail.lines().next().unwrap_or_default().trim();
        return if detail.is_empty() {
            "失败".to_owned()
        } else {
            format!("失败: {detail}")
        };
    }
    let mut parts = Vec::new();
    if let Some(session_id) = tool
        .session_id
        .clone()
        .or_else(|| terminal_argument(&tool.arguments, "session_id"))
    {
        parts.push(session_id);
    }
    if let Some(input) = terminal_input(&tool.arguments) {
        parts.push(input);
    } else if let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&tool.arguments) {
        for key in [
            "path",
            "url",
            "page_id",
            "element_id",
            "query",
            "command",
            "instruction",
            "name",
        ] {
            let Some(value) = arguments.get(key) else {
                continue;
            };
            let value = match value {
                serde_json::Value::String(value) => value.clone(),
                serde_json::Value::Number(value) => value.to_string(),
                serde_json::Value::Bool(value) => value.to_string(),
                _ => continue,
            };
            if !value.is_empty() && !parts.contains(&value) {
                parts.push(value);
            }
            if parts.len() >= 2 {
                break;
            }
        }
    }
    parts.join(" ")
}

fn tool_rows(tool: &ToolCard, width: usize, expanded: bool, now_ms: u64) -> Vec<UiRow> {
    let (icon, tone) = match (tool.result.as_ref().map(|result| result.state), tool.queued) {
        (None, true) => ("●", RowTone::ToolQueued),
        (None, false) => ("●", RowTone::ToolRunning),
        (Some(ToolResultState::Succeeded), _) => ("●", RowTone::ToolSucceeded),
        (Some(ToolResultState::Failed), _) => ("●", RowTone::ToolFailed),
        (Some(ToolResultState::Cancelled), _) => ("●", RowTone::ToolFailed),
        (Some(ToolResultState::Interrupted), _) => ("●", RowTone::ToolFailed),
    };
    if !expanded {
        let marker_color = match tone {
            RowTone::ToolRunning => MarkdownColorRole::Primary,
            RowTone::ToolQueued => MarkdownColorRole::Muted,
            RowTone::ToolSucceeded => MarkdownColorRole::Success,
            RowTone::ToolFailed => MarkdownColorRole::Error,
            _ => unreachable!("tool summary has an invalid row tone"),
        };
        let visible_name = truncate_with_ellipsis(&tool.name, width.saturating_sub(2));
        let mut spans = vec![
            MarkdownSpan::new(
                format!("{icon} "),
                MarkdownTextStyle::colored(marker_color).bold(),
            ),
            MarkdownSpan::new(visible_name.clone(), MarkdownTextStyle::default()),
        ];
        let brief = tool_brief(tool);
        let available = width
            .saturating_sub(2 + display_width(&visible_name))
            .saturating_sub(1);
        if !brief.is_empty() && available > 0 {
            spans.push(MarkdownSpan::new(
                format!(" {}", truncate_with_ellipsis(&brief, available)),
                MarkdownTextStyle::colored(MarkdownColorRole::Muted),
            ));
        }
        return vec![UiRow::markdown(spans, tone)];
    }

    let mut rows = Vec::new();
    rows.push(UiRow::new(format!("{icon} {}", tool.name), tone));

    let argument_session = terminal_argument(&tool.arguments, "session_id");
    if let Some(session_id) = tool.session_id.as_deref().or(argument_session.as_deref()) {
        append_tool_field(&mut rows, "Session", session_id, width);
    }
    if let Some(input) = terminal_input(&tool.arguments) {
        append_tool_field(&mut rows, "Input", &input, width);
    }
    if tool.name == "WebBrowser.RequireHumanAction"
        && let Some(instruction) = terminal_argument(&tool.arguments, "instruction")
    {
        append_tool_field(&mut rows, "Action", &instruction, width);
    }

    let output = visible_tool_output(tool);
    if !output.is_empty() {
        append_tool_field(&mut rows, "Output", &output, width);
    }

    let finished_at = tool
        .result
        .as_ref()
        .map(|result| result.finished_at_ms)
        .unwrap_or(now_ms);
    let elapsed = finished_at.saturating_sub(tool.started_at_ms);
    if tool.queued {
        append_tool_status(&mut rows, "Queued", width);
    } else if tool.result.is_none() {
        append_running_tool_status(
            &mut rows,
            &running_tool_status_text(
                &tool.name,
                tool.started_at_ms,
                terminal_timeout(&tool.name, &tool.arguments),
                now_ms,
            ),
            width,
        );
    } else {
        append_tool_status(
            &mut rows,
            &format!("Time use: {}", format_duration(elapsed)),
            width,
        );
    }
    rows
}

fn append_tool_field(rows: &mut Vec<UiRow>, name: &str, value: &str, width: usize) {
    let label = format!("  ├ {name:<7}: ");
    let label_width = display_width(&label);
    let continuation = format!("  │ {}", " ".repeat(label_width.saturating_sub(4)));
    append_tool_item(rows, &label, &continuation, value, width);
}

fn append_tool_status(rows: &mut Vec<UiRow>, value: &str, width: usize) {
    append_tool_item(rows, "  └ ", "    ", value, width);
}

fn append_running_tool_status(rows: &mut Vec<UiRow>, value: &str, width: usize) {
    append_tool_item_with_tone(
        rows,
        "  └ ",
        "    ",
        value,
        width,
        RowTone::ToolRunningStatus,
    );
}

fn append_tool_item(
    rows: &mut Vec<UiRow>,
    prefix: &str,
    continuation: &str,
    value: &str,
    width: usize,
) {
    append_tool_item_with_tone(
        rows,
        prefix,
        continuation,
        value,
        width,
        RowTone::ToolDetail,
    );
}

fn append_tool_item_with_tone(
    rows: &mut Vec<UiRow>,
    prefix: &str,
    continuation: &str,
    value: &str,
    width: usize,
    tone: RowTone,
) {
    let available = width.saturating_sub(display_width(prefix)).max(1);
    let wrapped = wrap(value, available);
    for (index, line) in wrapped.into_iter().enumerate() {
        let prefix = if index == 0 { prefix } else { continuation };
        rows.push(UiRow::new(
            truncate(&format!("{prefix}{line}"), width),
            tone,
        ));
    }
}

fn running_tool_status_text(
    tool_name: &str,
    started_at_ms: u64,
    timeout_ms: Option<u64>,
    now_ms: u64,
) -> String {
    if tool_name == "WebBrowser.RequireHumanAction" {
        return format!(
            "等待浏览器人工操作 ... {}",
            format_duration(now_ms.saturating_sub(started_at_ms))
        );
    }
    let timeout = timeout_ms
        .map(|timeout| format!(" (timeout {})", format_duration(timeout)))
        .unwrap_or_default();
    format!(
        "Running ... {}{timeout}",
        format_duration(now_ms.saturating_sub(started_at_ms))
    )
}

fn terminal_argument(arguments: &str, name: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(name)?
        .as_str()
        .map(str::to_owned)
}

fn terminal_input(arguments: &str) -> Option<String> {
    let arguments = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let actions = arguments.get("input").and_then(|value| value.as_array())?;
    if actions.is_empty() {
        return None;
    }
    let mut visible = String::new();
    for action in actions {
        match action.get("type").and_then(|value| value.as_str()) {
            Some("text") => {
                let text = action.get("text").and_then(|value| value.as_str())?;
                for character in text.chars() {
                    match character {
                        '\r' | '\n' => visible.push('↵'),
                        '\t' => visible.push('⇥'),
                        '\u{1b}' => visible.push_str("Esc"),
                        '\u{7f}' => visible.push_str("Del"),
                        character if character.is_control() => {
                            write!(visible, "\\u{{{:04x}}}", character as u32)
                                .expect("writing to String cannot fail");
                        }
                        character => visible.push(character),
                    }
                }
            }
            Some("key") => visible.push_str(&terminal_key_action_label(action)?),
            _ => return None,
        }
    }
    Some(visible)
}

fn terminal_key_action_label(action: &serde_json::Value) -> Option<String> {
    let key = action.get("key").and_then(|value| value.as_str())?;
    let modifiers = action
        .get("modifiers")
        .and_then(|value| value.as_array())
        .map(|modifiers| {
            modifiers
                .iter()
                .filter_map(|modifier| modifier.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut label = String::new();
    for (modifier, display) in [("ctrl", "Ctrl"), ("alt", "Alt"), ("shift", "Shift")] {
        if modifiers.contains(modifier) {
            label.push_str(display);
            label.push('+');
        }
    }
    let plain_named_key = modifiers.is_empty();
    label.push_str(match key {
        "enter" if plain_named_key => "↵",
        "escape" if plain_named_key => "Esc",
        "tab" if plain_named_key => "⇥",
        "backspace" if plain_named_key => "⌫",
        "delete" if plain_named_key => "Del",
        "up" if plain_named_key => "↑",
        "down" if plain_named_key => "↓",
        "left" if plain_named_key => "←",
        "right" if plain_named_key => "→",
        "page_up" => "PageUp",
        "page_down" => "PageDown",
        "space" => "Space",
        key => {
            let mut characters = key.chars();
            if let (Some(character), None) = (characters.next(), characters.next()) {
                return Some(format!(
                    "{}{}{}",
                    label,
                    character.to_uppercase(),
                    terminal_key_repeat_label(action)
                ));
            }
            match key {
                "enter" => "Enter",
                "escape" => "Esc",
                "tab" => "Tab",
                "backspace" => "Backspace",
                "insert" => "Insert",
                "delete" => "Delete",
                "up" => "Up",
                "down" => "Down",
                "left" => "Left",
                "right" => "Right",
                "home" => "Home",
                "end" => "End",
                key => key,
            }
        }
    });
    label.push_str(&terminal_key_repeat_label(action));
    Some(label)
}

fn terminal_key_repeat_label(action: &serde_json::Value) -> String {
    match action
        .get("repeat")
        .and_then(|value| value.as_u64())
        .unwrap_or(1)
    {
        0 | 1 => String::new(),
        repeat => format!("×{repeat}"),
    }
}

fn terminal_timeout(tool_name: &str, arguments: &str) -> Option<u64> {
    let arguments = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    if !matches!(tool_name, "Terminal.Create" | "Terminal.Interact")
        && arguments.get("wait_ms").is_none()
        && arguments.get("max_wait_ms").is_none()
    {
        return None;
    }
    Some(
        arguments
            .get("max_wait_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(10_000),
    )
}

fn visible_tool_output(tool: &ToolCard) -> String {
    if !tool.output.trim().is_empty() {
        return tool.output.trim_end().to_owned();
    }
    if matches!(tool.name.as_str(), "Terminal.Create" | "Terminal.Interact")
        && tool
            .result
            .as_ref()
            .is_some_and(|result| result.state == ToolResultState::Succeeded)
    {
        return "(no terminal output)".to_owned();
    }
    tool.result
        .as_ref()
        .filter(|result| !result.detail.is_empty())
        .map(|result| result.detail.clone())
        .unwrap_or_default()
}

fn projected_tool_output(_stream: ToolOutputStream, content: &ToolInfoContent) -> (bool, String) {
    match content {
        ToolInfoContent::Text(content) => (false, content.clone()),
        ToolInfoContent::Terminal(update) => (false, update.plain_text()),
    }
}

fn truncate_with_ellipsis(content: &str, width: usize) -> String {
    if display_width(content) <= width {
        return content.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut output = take_display_width(content, keep);
    output.push('…');
    output
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds}ms");
    }
    if milliseconds.is_multiple_of(1_000) {
        return format!("{}s", milliseconds / 1_000);
    }
    format!("{:.1}s", milliseconds as f64 / 1_000.0)
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;

    for grapheme in text.graphemes(true) {
        if grapheme.contains('\n') || grapheme.contains('\r') {
            lines.push(std::mem::take(&mut line));
            used = 0;
            continue;
        }
        let visible = grapheme
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        if visible.is_empty() {
            continue;
        }
        let grapheme_width = display_width(&visible);
        if used + grapheme_width > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if grapheme_width <= width {
            line.push_str(&visible);
            used += grapheme_width;
        }
    }
    if !line.is_empty() || lines.is_empty() || !text.ends_with('\n') {
        lines.push(line);
    }
    lines
}

fn truncate(text: &str, width: usize) -> String {
    wrap(text, width).into_iter().next().unwrap_or_default()
}

fn tail(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut characters = Vec::new();
    for grapheme in text.graphemes(true).rev() {
        let grapheme_width = display_width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        if grapheme.chars().all(|character| !character.is_control()) {
            characters.push(grapheme);
            used += grapheme_width;
        }
    }
    characters.into_iter().rev().collect::<String>()
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn take_display_width(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        output.push_str(grapheme);
        used += grapheme_width;
    }
    output
}

struct TerminalGuard {
    alternate: bool,
    alternate_detail_screen: bool,
}

fn use_alternate_detail_screen(term_program: Option<&str>) -> bool {
    term_program != Some("Apple_Terminal")
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let term_program = env::var("TERM_PROGRAM").ok();
        Ok(Self {
            alternate: false,
            alternate_detail_screen: use_alternate_detail_screen(term_program.as_deref()),
        })
    }

    fn enter_detail_screen(&mut self, stdout: &mut Stdout) -> Result<()> {
        if self.alternate_detail_screen && !self.alternate {
            execute!(stdout, EnterAlternateScreen, Hide)?;
            self.alternate = true;
        }
        Ok(())
    }

    fn detail_view(&self) -> TuiView {
        if self.alternate_detail_screen {
            TuiView::ToolDetails
        } else {
            TuiView::ToolDetailsInline
        }
    }

    fn leave_detail_screen(&mut self, stdout: &mut Stdout) -> Result<()> {
        if self.alternate {
            execute!(stdout, LeaveAlternateScreen, Show)?;
            self.alternate = false;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.alternate {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
        let _ = execute!(
            io::stdout(),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workmap::{WorkMapMemorySnapshot, WorkMapNote, WorkMapObjective, WorkMapPlan};

    type TestNote<'a> = (NoteKind, &'a str);
    type TestPlan<'a> = (PlanState, &'a str, Vec<TestNote<'a>>);

    fn workmap_memory(
        id: &str,
        kind: MemoryKind,
        state: MemoryState,
        content: &str,
    ) -> WorkMapMemory {
        WorkMapMemory {
            id: id.into(),
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 2,
            kind,
            state,
            basis: (kind == MemoryKind::Fact).then_some(MemoryBasis::Verified),
            content: content.into(),
            status_reason: (state != MemoryState::Active).then(|| "memory changed".into()),
            replacement_id: (state == MemoryState::Superseded).then(|| "memory-ffffffff".into()),
        }
    }

    fn workmap_objective(
        id: &str,
        state: ObjectiveState,
        title: &str,
        plans: Vec<TestPlan<'_>>,
    ) -> WorkMapObjectiveSnapshot {
        WorkMapObjectiveSnapshot {
            objective: WorkMapObjective {
                id: id.into(),
                revision: 1,
                created_at_ms: 1,
                updated_at_ms: 2,
                state,
                title: title.into(),
                description: Some(format!("{title} description")),
                status_reason: matches!(
                    state,
                    ObjectiveState::Cancelled | ObjectiveState::Superseded
                )
                .then(|| "objective reason".into()),
            },
            plans: plans
                .into_iter()
                .enumerate()
                .map(
                    |(plan_index, (plan_state, plan_title, notes))| WorkMapPlanSnapshot {
                        plan: WorkMapPlan {
                            id: format!("plan-{plan_index:08x}"),
                            revision: 1,
                            created_at_ms: 1,
                            updated_at_ms: 2,
                            objective_id: id.into(),
                            order: plan_index as u64 + 1,
                            state: plan_state,
                            title: plan_title.into(),
                            description: Some(format!("{plan_title} description")),
                            outcome: (plan_state == PlanState::Completed)
                                .then(|| "verified outcome".into()),
                            verification: (plan_state == PlanState::Completed)
                                .then(|| "tests passed".into()),
                            status_reason: matches!(
                                plan_state,
                                PlanState::Cancelled | PlanState::Superseded
                            )
                            .then(|| "plan reason".into()),
                        },
                        notes: notes
                            .into_iter()
                            .enumerate()
                            .map(|(note_index, (kind, content))| WorkMapNote {
                                id: format!("note-{plan_index:04x}{note_index:04x}"),
                                revision: 1,
                                created_at_ms: 1,
                                updated_at_ms: 2,
                                plan_id: format!("plan-{plan_index:08x}"),
                                sequence: note_index as u64 + 1,
                                kind,
                                content: content.into(),
                            })
                            .collect(),
                    },
                )
                .collect(),
        }
    }

    fn preview_session(id: EventId) -> TerminalSessionPreview {
        TerminalSessionPreview {
            session_id: format!("pty-{id}"),
            creation_order: id,
            width: 120,
            height: 40,
            revision: id,
        }
    }

    fn preview_frame() -> TerminalFrame {
        TerminalFrame {
            session_id: "pty-27".into(),
            revision: 4,
            width: 8,
            height: 2,
            viewport: [0, 1],
            style_defs: vec![crate::terminal::TerminalStyleDefinition {
                id: 1,
                style: TerminalStyle {
                    foreground: Some(TerminalColor::Indexed(2)),
                    bold: true,
                    ..TerminalStyle::default()
                },
            }],
            rows: vec![
                crate::terminal::TerminalRowUpdate {
                    row: 0,
                    wrapped: false,
                    runs: vec![crate::terminal::TerminalRowRun {
                        col: 0,
                        width: 3,
                        text: "run".into(),
                        style: 1,
                    }],
                },
                crate::terminal::TerminalRowUpdate {
                    row: 1,
                    wrapped: false,
                    runs: vec![crate::terminal::TerminalRowRun {
                        col: 2,
                        width: 2,
                        text: "好".into(),
                        style: 0,
                    }],
                },
            ],
            cursor: crate::terminal::TerminalCursor {
                row: 1,
                col: 4,
                visible: true,
                underlying: String::new(),
                wide: false,
                wide_continuation: false,
            },
        }
    }

    #[test]
    fn tab_cycles_transcript_terminals_workmap_agents_and_back_to_transcript() {
        let sessions = vec![preview_session(10), preview_session(27)];
        let discovery_calls = std::cell::Cell::new(0);
        assert_eq!(
            terminal_sessions_for_tab(TuiView::TerminalPreview, &sessions, || {
                discovery_calls.set(discovery_calls.get() + 1);
                Vec::new()
            }),
            sessions
        );
        assert_eq!(discovery_calls.get(), 0);
        assert!(
            terminal_sessions_for_tab(TuiView::WorkMap, &sessions, || {
                discovery_calls.set(discovery_calls.get() + 1);
                sessions.clone()
            })
            .is_empty()
        );
        assert_eq!(discovery_calls.get(), 0);
        assert_eq!(
            terminal_sessions_for_tab(TuiView::Transcript, &[], || {
                discovery_calls.set(discovery_calls.get() + 1);
                sessions.clone()
            }),
            sessions
        );
        assert_eq!(discovery_calls.get(), 1);

        let first = next_terminal_preview(None, &sessions).unwrap();
        assert_eq!(first.session_id, "pty-10");
        let first = TerminalPreviewSelection::from(&first);
        let second = next_terminal_preview(Some(&first), &sessions).unwrap();
        assert_eq!(second.session_id, "pty-27");
        let second = TerminalPreviewSelection::from(&second);
        assert!(next_terminal_preview(Some(&second), &sessions).is_none());

        let ended = TerminalPreviewSelection {
            session_id: "pty-15".into(),
            creation_order: 15,
        };
        assert_eq!(
            next_terminal_preview(Some(&ended), &sessions)
                .unwrap()
                .session_id,
            "pty-27"
        );

        let agents = vec![
            AgentId::new("main").unwrap(),
            AgentId::new("agent-b").unwrap(),
            AgentId::new("agent-c").unwrap(),
        ];
        assert_eq!(
            next_agent_id(&agents, Some(&agents[0])),
            Some(agents[1].clone())
        );
        assert_eq!(
            next_agent_id(&agents, Some(&agents[2])),
            Some(agents[0].clone())
        );
        assert_eq!(next_agent_id(&agents[..1], Some(&agents[0])), None);

        assert_eq!(
            tab_destination(
                TuiView::Transcript,
                None,
                &sessions,
                &agents[..1],
                Some(&agents[0])
            ),
            TabDestination::Terminal(sessions[0].clone())
        );
        assert_eq!(
            tab_destination(
                TuiView::TerminalPreview,
                Some(&second),
                &sessions,
                &agents,
                Some(&agents[0])
            ),
            TabDestination::WorkMap
        );
        assert_eq!(
            tab_destination(TuiView::WorkMap, None, &sessions, &agents, Some(&agents[0])),
            TabDestination::Agent(agents[1].clone())
        );
        assert_eq!(
            tab_destination(
                TuiView::TerminalPreview,
                Some(&second),
                &sessions,
                &agents[..1],
                Some(&agents[0])
            ),
            TabDestination::WorkMap
        );
        assert_eq!(
            tab_destination(
                TuiView::WorkMap,
                None,
                &sessions,
                &agents[..1],
                Some(&agents[0])
            ),
            TabDestination::Transcript
        );
        assert_eq!(
            tab_destination(
                TuiView::Transcript,
                None,
                &[],
                &agents[..1],
                Some(&agents[0])
            ),
            TabDestination::WorkMap
        );
    }

    #[test]
    fn workmap_rows_show_history_then_current_plans_without_truncation() {
        let snapshot = WorkMapSnapshot {
            memory: WorkMapMemorySnapshot {
                facts: vec![workmap_memory(
                    "memory-00000001",
                    MemoryKind::Fact,
                    MemoryState::Active,
                    "The target filesystem is case-sensitive",
                )],
                agreements: vec![workmap_memory(
                    "memory-00000002",
                    MemoryKind::Agreement,
                    MemoryState::Retracted,
                    "Use the earlier output format",
                )],
            },
            history: vec![workmap_objective(
                "objective-00000001",
                ObjectiveState::Completed,
                "Past objective",
                vec![(
                    PlanState::Completed,
                    "Past plan",
                    vec![(NoteKind::Validation, "verified the historical result")],
                )],
            )],
            current: Some(workmap_objective(
                "objective-00000002",
                ObjectiveState::Active,
                "Deliver the complete requested outcome",
                vec![
                    (
                        PlanState::Completed,
                        "Establish constraints",
                        vec![(NoteKind::Finding, "found an important constraint")],
                    ),
                    (
                        PlanState::Active,
                        "Carry out the selected direction",
                        vec![
                            (NoteKind::Decision, "selected the next direction"),
                            (NoteKind::Next, "continue from this exact point"),
                        ],
                    ),
                    (PlanState::Planned, "Verify the result", Vec::new()),
                ],
            )),
        };

        let collapsed_rows = workmap_rows(&snapshot, 52, false);
        assert!(
            collapsed_rows
                .iter()
                .all(|row| display_width(&row.text) <= 52)
        );
        let rendered = collapsed_rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let history_position = rendered.find("History (1)").unwrap();
        let current_position = rendered.find("Current (1)").unwrap();
        let memory_position = rendered.find("Memory (2)").unwrap();
        assert!(memory_position < history_position);
        assert!(history_position < current_position);
        for expected in [
            "Memory (2)",
            "Facts (1)",
            "[FACT · VERIFIED] The target filesystem",
            "Agreements (1)",
            "[RETRACTED · AGREEMENT] Use the earlier",
            "memory changed",
            "History (1)",
            "✓ Past objective",
            "Current (1)",
            "Objective: Deliver the complete requested",
            "✓ Establish constraints",
            "Outcome       : verified outcome",
            "Verification  : tests passed",
            "[FINDING] found an important constraint",
            "■ Carry out the selected direction",
            "[NEXT] continue from this exact point",
            "□ Verify the result",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
        assert!(!rendered.contains("Past plan"));
        assert!(!rendered.contains("verified the historical result"));

        let expanded_rows = workmap_rows(&snapshot, 52, true);
        assert!(
            expanded_rows
                .iter()
                .all(|row| display_width(&row.text) <= 52)
        );
        let expanded = expanded_rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Past objective description",
            "✓ Past plan",
            "Past plan description",
            "Outcome       : verified outcome",
            "Verification  : tests passed",
            "[VALIDATION] verified the historical result",
            "Objective: Deliver the complete requested",
        ] {
            assert!(
                expanded.contains(expected),
                "missing {expected:?}\n{expanded}"
            );
        }
    }

    #[test]
    fn workmap_ctrl_o_toggles_history_details_only_with_control() {
        assert!(toggles_workmap_history(KeyCode::Char('o'), true));
        assert!(!toggles_workmap_history(KeyCode::Char('o'), false));
        assert!(!toggles_workmap_history(KeyCode::Char('x'), true));
    }

    #[test]
    fn transcript_shows_objective_activity_and_plan_note_counts_without_note_content() {
        let current = workmap_objective(
            "objective-00000001",
            ObjectiveState::Active,
            "调查 MiniMax H3 模型",
            vec![
                (
                    PlanState::Active,
                    "检索官方资料与发布信息",
                    vec![
                        (NoteKind::Action, "不应显示的内部行动"),
                        (NoteKind::Finding, "不应显示的内部发现"),
                        (NoteKind::Validation, "不应显示的内部验证"),
                    ],
                ),
                (
                    PlanState::Planned,
                    "整理模型能力、使用方式和限制",
                    vec![(NoteKind::Next, "不应显示的内部下一步")],
                ),
                (PlanState::Planned, "核对结论并交付简明调研摘要", vec![]),
            ],
        );
        let mut rows = vec![UiRow::new("● WebBrowser.Navigate", RowTone::ToolSucceeded)];
        append_current_objective_summary(&mut rows, Some(&current), 100);
        let rendered = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("■ 调查 MiniMax H3 模型"));
        assert!(!rendered.contains("目标:"));
        assert!(rendered.contains("  调查 MiniMax H3 模型 description"));
        assert!(rendered.contains("    ■ 检索官方资料与发布信息 (3 notes)"));
        assert!(rendered.contains("    □ 整理模型能力、使用方式和限制 (1 note)"));
        assert!(rendered.contains("    □ 核对结论并交付简明调研摘要"));
        assert!(!rendered.contains("不应显示的内部行动"));
        assert!(!rendered.contains("不应显示的内部发现"));
        assert!(!rendered.contains("不应显示的内部验证"));
        assert!(!rendered.contains("不应显示的内部下一步"));
        assert_eq!(rows[1], UiRow::new("", RowTone::Spacer));
        assert_eq!(rows[2].tone, RowTone::Assistant);

        let inactive = workmap_objective(
            "objective-00000002",
            ObjectiveState::Active,
            "等待下一阶段",
            vec![(PlanState::Planned, "尚未开始", vec![])],
        );
        let mut idle = Vec::new();
        append_current_objective_summary(&mut idle, Some(&inactive), 100);
        assert!(idle[0].text.starts_with("□ 等待下一阶段"));
        assert_eq!(idle[0].tone, RowTone::Assistant);

        let mut absent = vec![UiRow::new("unchanged", RowTone::Assistant)];
        append_current_objective_summary(&mut absent, None, 100);
        assert_eq!(absent, vec![UiRow::new("unchanged", RowTone::Assistant)]);
    }

    #[test]
    fn workmap_page_replays_persisted_mutations_and_rewind() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("map work").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        let arguments = r#"{"objective":{"title":"Persist the route"},"plans":[{"title":"Persistent current"},{"title":"Future route"}]}"#;
        let call = edb
            .append_tool_call(
                api,
                prompt,
                "provider-map",
                crate::workmap::START,
                arguments,
            )
            .unwrap();
        let started =
            crate::workmap::execute(crate::workmap::START, arguments, call, &mut edb).unwrap();
        let plan_id = started["current"]["plans"][0]["plan"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        edb.append_tool_result(call, ToolResultState::Succeeded, None, "ok")
            .unwrap();

        let rewind_target = edb.next_event_id();
        let prompt = edb.append_user_prompt("progress").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        let arguments =
            format!(r#"{{"plan_id":"{plan_id}","kind":"finding","content":"Durable finding"}}"#);
        let call = edb
            .append_tool_call(
                api,
                prompt,
                "provider-progress",
                crate::workmap::ADD_NOTE,
                &arguments,
            )
            .unwrap();
        crate::workmap::execute(crate::workmap::ADD_NOTE, &arguments, call, &mut edb).unwrap();

        let mut rendered = Vec::new();
        render_workmap(&mut rendered, edb.events(), "main", (80, 12), false).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("Persistent current"));
        assert!(rendered.contains("Future route"));
        assert!(rendered.contains("Durable finding"));
        assert!(rendered.contains("Ctrl+O 展开历史详情"));

        let mut expanded = Vec::new();
        render_workmap(&mut expanded, edb.events(), "main", (80, 12), true).unwrap();
        let expanded = String::from_utf8(expanded).unwrap();
        assert!(expanded.contains("Ctrl+O 收起历史详情"));

        edb.rewind_to_event(rewind_target).unwrap();
        let mut rewound = Vec::new();
        render_workmap(&mut rewound, edb.events(), "main", (80, 12), false).unwrap();
        let rewound = String::from_utf8(rewound).unwrap();
        assert!(rewound.contains("Persistent current"));
        assert!(rewound.contains("Future route"));
        assert!(!rewound.contains("Durable finding"));
    }

    #[test]
    fn empty_and_narrow_workmap_views_remain_complete() {
        assert_eq!(
            workmap_display_value("a\0b\tc\rd\ne"),
            "a\\u{0}b\\tc\\rd\ne"
        );
        let empty = WorkMapSnapshot::default();
        let empty_text = workmap_rows(&empty, 24, false)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        for section in ["Memory (0)", "History (0)", "Current (0)"] {
            assert!(empty_text.contains(section));
        }
        assert!(empty_text.find("Memory (0)").unwrap() < empty_text.find("History (0)").unwrap());
        assert!(empty_text.find("History (0)").unwrap() < empty_text.find("Current (0)").unwrap());

        let snapshot = WorkMapSnapshot {
            current: Some(workmap_objective(
                "objective-00000001",
                ObjectiveState::Active,
                "Narrow objective",
                vec![(
                    PlanState::Active,
                    "Narrow current plan",
                    vec![(NoteKind::Adjustment, "abcdefghijklmnopqrstuvwxyz0123456789")],
                )],
            )),
            ..WorkMapSnapshot::default()
        };
        let rows = workmap_rows(&snapshot, 24, false);
        assert!(rows.iter().all(|row| display_width(&row.text) <= 24));
        let text = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let compact = text
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect::<String>();
        assert!(
            compact.contains("abcdefghijklmnopqrstuvwxyz0123456789"),
            "{text}"
        );
    }

    #[test]
    fn asynchronously_removed_current_agent_selects_an_adjacent_survivor() {
        let main = AgentId::new("main").unwrap();
        let first = AgentId::new("agent-a").unwrap();
        let current = AgentId::new("agent-b").unwrap();
        let next = AgentId::new("agent-c").unwrap();
        let previous = vec![main.clone(), first.clone(), current.clone(), next.clone()];

        assert_eq!(
            reconcile_agent_selection(
                &previous,
                &[main.clone(), first.clone(), next.clone()],
                Some(&current),
            ),
            (Some(next.clone()), true)
        );
        assert_eq!(
            reconcile_agent_selection(
                &previous,
                &[main.clone(), first.clone(), current.clone()],
                Some(&next),
            ),
            (Some(current.clone()), true)
        );
        assert_eq!(
            reconcile_agent_selection(&previous, &[], Some(&current)),
            (None, true)
        );
        assert_eq!(
            reconcile_agent_selection(&previous, &previous, Some(&current)),
            (Some(current), false)
        );
        assert_eq!(
            reconcile_agent_selection(&previous, &previous, None),
            (None, false)
        );
    }

    #[test]
    fn child_agent_view_is_explicitly_read_only() {
        assert!(TuiView::Transcript.accepts_input());
        assert!(!TuiView::TerminalPreview.accepts_input());
        assert!(!TuiView::WorkMap.accepts_input());
        assert!(!TuiView::ToolDetails.accepts_input());

        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::SubAgent, "main-agent", Some("main".into()), None)
            .unwrap();
        let prompt = edb.append_user_prompt("work").unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        assert_eq!(
            projected_child_state(edb.events()).unwrap(),
            ReadOnlyAgentState::Working
        );
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Completed, "")
            .unwrap();
        assert_eq!(
            projected_child_state(edb.events()).unwrap(),
            ReadOnlyAgentState::Completed
        );

        let mut session =
            TuiSession::new("agent-readonly", "main-agent", &[], 0, "/bin/bash", &[]).unwrap();
        session.read_only_state = Some(ReadOnlyAgentState::Working);
        assert_eq!(session.read_only_state, Some(ReadOnlyAgentState::Working));
        assert!(session.input.is_empty());
        assert!(session.overlay.is_none());

        let mut output = Vec::new();
        write_panel(
            &mut output,
            &ChatProjection::default(),
            "must not appear",
            "agent-readonly",
            "main-agent",
            None,
            UiApiActivity::default(),
            Some(ReadOnlyAgentState::Working),
            100,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("只读子 Agent · working · Tab 切换页面"));
        assert!(!output.contains("must not appear"));
        assert!(output.contains("\u{1b}[2A"));
        assert!(output.contains("\u{1b}[?25l"));

        let mut worker_output = Vec::new();
        write_panel(
            &mut worker_output,
            &ChatProjection::default(),
            "must not appear",
            "worker",
            "worker-agent",
            None,
            UiApiActivity::default(),
            Some(ReadOnlyAgentState::Working),
            100,
        )
        .unwrap();
        let worker_output = String::from_utf8(worker_output).unwrap();
        assert!(worker_output.contains("只读 Worker · working · / 命令 · Tab 切换页面"));
        assert!(!worker_output.contains("must not appear"));
    }

    #[test]
    fn ordinary_input_rewrite_touches_only_the_single_input_row() {
        let mut output = Vec::new();
        rewrite_input_row(&mut output, "abcdefghi123456", "main-agent", None, 100).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("abcdefghi123456"));
        assert!(output.contains("\u{1b}[2K"));
        assert!(!output.contains('\n'));
        assert!(!output.contains(&"─".repeat(20)));
        assert!(!output.contains("Ctrl+O"));
    }

    #[test]
    fn input_temporarily_has_priority_over_live_animation() {
        assert!(input_has_animation_priority(1_000, 1_000));
        assert!(input_has_animation_priority(1_000, 1_249));
        assert!(!input_has_animation_priority(1_000, 1_250));
    }

    #[test]
    fn terminal_preview_renders_cells_styles_cursor_and_status_without_scaling() {
        let frame = preview_frame();
        let mut output = Vec::new();
        render_terminal_preview(&mut output, &frame, (40, 5)).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("run"));
        assert!(output.contains("好"));
        assert!(output.contains(" me-s"));
        assert!(output.contains("Terminal"));
        assert!(output.contains("pty-27"));
        assert!(output.contains("\u{1b}[1m"), "{output:?}");
        assert!(output.contains("\u{1b}[2;5H"), "{output:?}");
        assert_eq!(
            terminal_color(&TerminalColor::Indexed(2)),
            Color::AnsiValue(2)
        );
    }

    #[test]
    fn terminal_preview_skips_unchanged_revisions_and_does_not_clear_live_updates() {
        let mut renderer = TerminalPreviewRenderer::default();
        let mut frame = preview_frame();
        let mut output = Vec::new();
        renderer.redraw(&mut output, &frame, (12, 5)).unwrap();
        let initial_length = output.len();
        let session = TerminalSessionPreview {
            session_id: frame.session_id.clone(),
            creation_order: 27,
            width: frame.width,
            height: frame.height,
            revision: frame.revision,
        };
        assert!(renderer.is_current(&session, (12, 5)));
        renderer.redraw(&mut output, &frame, (12, 5)).unwrap();
        assert_eq!(output.len(), initial_length);

        frame.revision += 1;
        let changed_session = TerminalSessionPreview {
            revision: frame.revision,
            ..session
        };
        assert!(!renderer.is_current(&changed_session, (12, 5)));
        renderer.redraw(&mut output, &frame, (12, 5)).unwrap();
        let update = &output[initial_length..];
        assert!(!update.windows(4).any(|bytes| bytes == b"\x1b[2J"));
        assert!(String::from_utf8_lossy(update).contains("run"));
    }

    #[test]
    fn undersized_terminal_preview_clips_without_resizing_or_rejecting_the_frame() {
        let mut frame = preview_frame();
        frame.width = 81;
        let mut output = Vec::new();
        render_terminal_preview(&mut output, &frame, (4, 3)).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("请扩大窗口"));
        assert!(output.contains("run"));
    }

    #[test]
    fn terminal_preview_scrolls_full_history_and_only_follows_output_at_the_bottom() {
        let mut frame = preview_frame();
        frame.height = 2;
        frame.viewport = [4, 5];
        frame.rows = (0_u64..6)
            .map(|row| crate::terminal::TerminalRowUpdate {
                row,
                wrapped: false,
                runs: vec![crate::terminal::TerminalRowRun {
                    col: 0,
                    width: 5,
                    text: format!("row-{row}"),
                    style: 0,
                }],
            })
            .collect();
        frame.cursor.row = 5;

        let mut renderer = TerminalPreviewRenderer::default();
        renderer.redraw(&mut Vec::new(), &frame, (12, 3)).unwrap();
        assert_eq!(renderer.scroll_top, 4);
        assert!(renderer.follow_bottom);

        renderer.scroll_up(2);
        renderer.redraw(&mut Vec::new(), &frame, (12, 3)).unwrap();
        assert_eq!(renderer.scroll_top, 2);
        assert!(!renderer.follow_bottom);

        frame.revision += 1;
        frame.viewport = [5, 6];
        frame.rows.push(crate::terminal::TerminalRowUpdate {
            row: 6,
            wrapped: false,
            runs: vec![crate::terminal::TerminalRowRun {
                col: 0,
                width: 5,
                text: "row-6".into(),
                style: 0,
            }],
        });
        frame.cursor.row = 6;
        renderer.redraw(&mut Vec::new(), &frame, (12, 3)).unwrap();
        assert_eq!(renderer.scroll_top, 2);
        assert!(!renderer.follow_bottom);

        renderer.scroll_end();
        renderer.redraw(&mut Vec::new(), &frame, (12, 3)).unwrap();
        assert_eq!(renderer.scroll_top, 5);
        assert!(renderer.follow_bottom);
    }

    #[test]
    fn startup_banner_contains_runtime_environment_and_spacing() {
        let banner = StartupBanner {
            workspace: "/workspace".to_owned(),
            system: "macos/aarch64".to_owned(),
            agent: "main".to_owned(),
            orchestrator: "main-agent".to_owned(),
            model: "test-model".to_owned(),
            terminal_backend: "/bin/bash".to_owned(),
            event_count: 42,
            edb_size_bytes: 1536,
        };
        let rows = banner.rows(80);
        assert_eq!(rows[0], UiRow::new(ME_S_LOGO[0], RowTone::BannerLogo));
        assert!(rows.iter().any(|row| row.text
            == format!("Welcome to ME-S v{}", env!("CARGO_PKG_VERSION"))
            && row.tone == RowTone::BannerTitle));
        assert!(rows.iter().any(|row| row.text.contains("Agent")
            && row.text.contains("main")
            && row.tone == RowTone::BannerInfo));
        assert_eq!(
            BANNER_LOGO_COLOR,
            Color::Rgb {
                r: 120,
                g: 100,
                b: 255
            }
        );
        let text = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for value in [
            "/workspace",
            "macos/aarch64",
            "main-agent",
            "test-model",
            "42 events · 1.5 KiB",
            "/bin/bash",
        ] {
            assert!(text.contains(value));
        }
        assert_eq!(rows.last(), Some(&UiRow::new("", RowTone::Spacer)));
    }

    #[test]
    fn edb_size_uses_compact_binary_units() {
        assert_eq!(format_byte_size(0), "0 B");
        assert_eq!(format_byte_size(1023), "1023 B");
        assert_eq!(format_byte_size(1024), "1.0 KiB");
        assert_eq!(format_byte_size(1536), "1.5 KiB");
        assert_eq!(format_byte_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_byte_size(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn tui_detects_edb_structure_changes_by_polling_revision() {
        let mut observed_revision = 4;
        assert!(!observe_edb_mutation(&mut observed_revision, 4));
        assert!(observe_edb_mutation(&mut observed_revision, 5));
        assert_eq!(observed_revision, 5);
        assert!(!observe_edb_mutation(&mut observed_revision, 5));
    }

    #[test]
    fn tui_clears_input_only_for_a_new_prompt_submission_revision() {
        let mut observed_revision = 4;
        assert!(!observe_prompt_submission(&mut observed_revision, 4));
        assert!(observe_prompt_submission(&mut observed_revision, 5));
        assert_eq!(observed_revision, 5);
        assert!(!observe_prompt_submission(&mut observed_revision, 5));
        assert!(!observe_prompt_submission(&mut observed_revision, 0));
        assert_eq!(observed_revision, 0);
    }

    #[test]
    fn tui_session_restores_the_runtime_owned_input_draft() {
        let mut session = TuiSession::new("main", "main-agent", &[], 0, "/bin/bash", &[]).unwrap();
        session.input = "shared draft\nsecond line".into();
        session.input_draft_revision = 7;
        assert_eq!(session.input, "shared draft\nsecond line");
        assert_eq!(session.input_draft_revision, 7);
    }

    #[test]
    fn redraw_policy_upgrades_layout_changes_to_full_edb_replay() {
        for cause in [
            RedrawCause::Startup,
            RedrawCause::TerminalResized,
            RedrawCause::ViewChanged,
            RedrawCause::ContextChanged,
        ] {
            assert!(requires_full_replay(cause, true, 80, 80));
        }
        for cause in [
            RedrawCause::EdbUpdated,
            RedrawCause::InputChanged,
            RedrawCause::DetailScrolled,
        ] {
            assert!(!requires_full_replay(cause, true, 80, 80));
            assert!(requires_full_replay(cause, true, 80, 79));
        }
        assert!(requires_full_replay(RedrawCause::EdbUpdated, false, 0, 80));
        assert!(terminal_size_changed(None, (80, 24)));
        assert!(terminal_size_changed(Some((80, 24)), (79, 24)));
        assert!(terminal_size_changed(Some((80, 24)), (80, 25)));
        assert!(!terminal_size_changed(Some((80, 24)), (80, 24)));
        assert!(!use_alternate_detail_screen(Some("Apple_Terminal")));
        assert!(use_alternate_detail_screen(Some("iTerm.app")));
        assert!(use_alternate_detail_screen(None));
        assert_eq!(
            TerminalGuard {
                alternate: false,
                alternate_detail_screen: false,
            }
            .detail_view(),
            TuiView::ToolDetailsInline
        );
        assert_eq!(
            TerminalGuard {
                alternate: false,
                alternate_detail_screen: true,
            }
            .detail_view(),
            TuiView::ToolDetails
        );
    }

    #[test]
    fn full_redraw_clears_scrollback_screen_and_resets_cursor() {
        let mut output = Vec::new();
        clear_for_full_redraw(&mut output, true).unwrap();
        let output = String::from_utf8(output).unwrap();
        let purge = output.find("\u{1b}[3J").unwrap();
        let clear = output.find("\u{1b}[2J").unwrap();
        let home = output.find("\u{1b}[1;1H").unwrap();
        assert!(purge < clear);
        assert!(clear < home);

        let mut startup = Vec::new();
        clear_for_full_redraw(&mut startup, false).unwrap();
        let startup = String::from_utf8(startup).unwrap();
        assert!(!startup.contains("\u{1b}[3J"));
        assert!(startup.contains("\u{1b}[2J"));
    }

    #[test]
    fn workmap_page_redraws_only_for_its_content_or_edb_rewind() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("ordinary conversation").unwrap();
        assert!(!workmap_view_needs_redraw(0, edb.events(), false));
        assert!(workmap_view_needs_redraw(0, edb.events(), true));
        assert!(workmap_view_needs_redraw(2, edb.events(), false));

        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        let observed = edb.len();
        let call = edb
            .append_tool_call(
                api,
                prompt,
                "provider-workmap-redraw",
                crate::workmap::START,
                r#"{"objective":{"title":"Preserve valuable state"},"plans":[{"title":"valuable state"}]}"#,
            )
            .unwrap();
        crate::workmap::execute(
            crate::workmap::START,
            r#"{"objective":{"title":"Preserve valuable state"},"plans":[{"title":"valuable state"}]}"#,
            call,
            &mut edb,
        )
        .unwrap();
        assert!(workmap_view_needs_redraw(observed, edb.events(), false));
    }

    #[test]
    fn full_and_incremental_projection_match() {
        let mut edb = EventDataBase::new();
        let mut incremental = ChatProjection::default();
        edb.append_user_prompt("hello").unwrap();
        incremental.consume(&edb).unwrap();
        let api_call_id = edb.append_api_requesting(0).unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_api_state(api_call_id, 0, ApiState::Streaming, "")
            .unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_model_context_item(
            api_call_id,
            0,
            "codex-oauth",
            r#"{"type":"reasoning","encrypted_content":"opaque","summary":[]}"#,
        )
        .unwrap();
        incremental.consume(&edb).unwrap();
        assert_eq!(incremental.messages.len(), 1);
        edb.append_assist_response(0, "a", false).unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_assist_response(0, "b", true).unwrap();
        incremental.consume(&edb).unwrap();
        let tool_call_id = edb
            .append_tool_call(api_call_id, 0, "provider-1", "Terminal.Create", "{}")
            .unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_api_state(api_call_id, 0, ApiState::Completed, "")
            .unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_terminal_update(tool_call_id, crate::terminal::test_update("ok"))
            .unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_terminal_update(tool_call_id, crate::terminal::test_update("full\nscreen"))
            .unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_tool_result(
            tool_call_id,
            crate::event::ToolResultState::Succeeded,
            Some(0),
            format!(
                r#"{{"session_id":"pty-{tool_call_id}","state":"running","shell":"/bin/bash"}}"#
            ),
        )
        .unwrap();
        incremental.consume(&edb).unwrap();

        let full = ChatProjection::replay_events(edb.events()).unwrap();
        assert_eq!(full, incremental);
        assert_eq!(full.messages[1].content, "ab");
        let tool = full.messages[2].tool.as_ref().unwrap();
        let expected_session = format!("pty-{tool_call_id}");
        assert_eq!(tool.name, "Terminal.Create");
        assert_eq!(tool.session_id.as_deref(), Some(expected_session.as_str()));
        assert_eq!(tool.output, "000000: ok000000: full\n000001: screen");
        assert_eq!(
            tool.result.as_ref().map(|result| result.state),
            Some(ToolResultState::Succeeded)
        );
        assert_eq!(tool.result.as_ref().unwrap().exit_code, Some(0));
        assert_eq!(full.messages.len(), 3);
        assert_eq!(full.api_state, Some(ApiState::Completed));
        assert_eq!(full.api_usage, None);
        assert!(
            full.messages
                .windows(2)
                .all(|messages| messages[0].timestamp_ms <= messages[1].timestamp_ms)
        );
    }

    #[test]
    fn completed_turn_appends_elapsed_toolbar_after_only_the_final_answer() {
        let started_at_ms = 10_000;
        let completed_at_ms = started_at_ms + 3_600_000 + 43 * 60_000 + 3_000;
        let events = vec![
            Event::ModelChanged(crate::event::ModelChangedEvent {
                id: 0,
                timestamp_ms: started_at_ms - 1,
                model: "test-model".into(),
                cause: ModelChangeCause::Initial,
            }),
            Event::UserPrompt(crate::event::UserPromptEvent {
                id: 1,
                timestamp_ms: started_at_ms,
                content: "complete the work".into(),
            }),
            Event::AgentTurn(crate::event::AgentTurnEvent {
                id: 2,
                timestamp_ms: started_at_ms,
                turn_id: 2,
                prompt_id: 1,
                state: AgentTurnState::Started,
                detail: String::new(),
            }),
            Event::AssistResponse(crate::event::AssistResponseEvent {
                id: 3,
                timestamp_ms: started_at_ms + 1_000,
                prompt_id: 1,
                content: "I will use a tool".into(),
                finished: true,
            }),
            Event::ApiStateUpdate(crate::event::ApiStateUpdateEvent {
                id: 4,
                timestamp_ms: started_at_ms + 1_500,
                api_call_id: 99,
                prompt_id: 1,
                state: ApiState::Completed,
                retry_count: 0,
                retry_limit: 10,
                usage: Some(ApiUsage {
                    input_tokens: 4_000,
                    output_tokens: 600,
                    total_tokens: 4_600,
                }),
                detail: String::new(),
            }),
            Event::ToolCall(crate::event::ToolCallEvent {
                id: 5,
                timestamp_ms: started_at_ms + 2_000,
                api_call_id: 99,
                prompt_id: 1,
                provider_call_id: "provider-call".into(),
                name: "File.Read".into(),
                arguments: r#"{"path":"input.txt"}"#.into(),
            }),
            Event::ToolCallResult(crate::event::ToolCallResultEvent {
                id: 6,
                timestamp_ms: started_at_ms + 3_000,
                tool_call_id: 5,
                state: ToolResultState::Succeeded,
                exit_code: None,
                detail: "content".into(),
            }),
            Event::AssistResponse(crate::event::AssistResponseEvent {
                id: 7,
                timestamp_ms: completed_at_ms - 1,
                prompt_id: 1,
                content: "final answer".into(),
                finished: true,
            }),
            Event::ApiStateUpdate(crate::event::ApiStateUpdateEvent {
                id: 8,
                timestamp_ms: completed_at_ms - 1,
                api_call_id: 100,
                prompt_id: 1,
                state: ApiState::Completed,
                retry_count: 0,
                retry_limit: 10,
                usage: Some(ApiUsage {
                    input_tokens: 7_000,
                    output_tokens: 1_000,
                    total_tokens: 8_000,
                }),
                detail: String::new(),
            }),
            Event::AgentTurn(crate::event::AgentTurnEvent {
                id: 9,
                timestamp_ms: completed_at_ms,
                turn_id: 2,
                prompt_id: 1,
                state: AgentTurnState::Completed,
                detail: String::new(),
            }),
        ];

        let mut incremental = ChatProjection::default();
        incremental.consume_events(&events[..9]).unwrap();
        assert!(
            incremental
                .messages
                .iter()
                .all(|message| message.kind != ChatBlockKind::TurnToolbar)
        );
        incremental.consume_events(&events).unwrap();
        let full = ChatProjection::replay_events(&events).unwrap();
        assert_eq!(incremental, full);
        assert_eq!(full.messages.len(), 5);
        assert_eq!(full.messages[1].kind, ChatBlockKind::Assistant);
        assert_eq!(full.messages[2].kind, ChatBlockKind::ToolCall);
        assert_eq!(full.messages[3].kind, ChatBlockKind::Assistant);
        assert_eq!(full.messages[4].kind, ChatBlockKind::TurnToolbar);
        assert_eq!(full.messages[4].content, "1h 43m 03s · 4.0k");
        let completed_label = format_turn_completed_at(completed_at_ms);

        let rows = chat_rows(&full, 80, false, completed_at_ms);
        let final_answer = rows
            .iter()
            .position(|row| row.text == "● final answer")
            .unwrap();
        assert_eq!(
            rows[final_answer + 1],
            UiRow::new(
                format!("  ▶ 用时 1h 43m 03s · 4.0k · {completed_label}"),
                RowTone::TurnToolbar,
            )
        );
        assert_ne!(rows[final_answer + 1].tone, RowTone::Spacer);

        incremental.consume_events(&events[..2]).unwrap();
        assert!(
            incremental
                .messages
                .iter()
                .all(|message| message.kind != ChatBlockKind::TurnToolbar)
        );

        let mut cleared_events = events.clone();
        cleared_events.push(Event::ContextCleared(crate::event::ContextClearedEvent {
            id: 10,
            timestamp_ms: completed_at_ms + 1,
        }));
        let cleared = ChatProjection::replay_events(&cleared_events).unwrap();
        assert!(
            cleared
                .messages
                .iter()
                .all(|message| message.kind != ChatBlockKind::TurnToolbar)
        );
    }

    #[test]
    fn interrupted_and_failed_turns_never_append_elapsed_toolbar() {
        for terminal_state in [AgentTurnState::Interrupted, AgentTurnState::Failed] {
            let events = vec![
                Event::UserPrompt(crate::event::UserPromptEvent {
                    id: 0,
                    timestamp_ms: 1_000,
                    content: "work".into(),
                }),
                Event::AgentTurn(crate::event::AgentTurnEvent {
                    id: 1,
                    timestamp_ms: 1_000,
                    turn_id: 0,
                    prompt_id: 0,
                    state: AgentTurnState::Started,
                    detail: String::new(),
                }),
                Event::AssistResponse(crate::event::AssistResponseEvent {
                    id: 2,
                    timestamp_ms: 2_000,
                    prompt_id: 0,
                    content: "partial answer".into(),
                    finished: false,
                }),
                Event::AgentTurn(crate::event::AgentTurnEvent {
                    id: 3,
                    timestamp_ms: 3_000,
                    turn_id: 0,
                    prompt_id: 0,
                    state: terminal_state,
                    detail: "stopped".into(),
                }),
            ];
            let projection = ChatProjection::replay_events(&events).unwrap();
            assert!(
                projection
                    .messages
                    .iter()
                    .all(|message| message.kind != ChatBlockKind::TurnToolbar)
            );
        }
    }

    #[test]
    fn turn_elapsed_format_is_compact_and_stable() {
        assert_eq!(format_turn_elapsed(0), "0s");
        assert_eq!(format_turn_elapsed(2_999), "2s");
        assert_eq!(format_turn_elapsed(59_999), "59s");
        assert_eq!(format_turn_elapsed(60_000), "1m 00s");
        assert_eq!(format_turn_elapsed(3_723_000), "1h 02m 03s");
        assert_eq!(format_turn_elapsed(6_183_999), "1h 43m 03s");
        assert_eq!(format_turn_tokens(Some(0)), "0.0k");
        assert_eq!(format_turn_tokens(Some(12_649)), "12.6k");
        assert_eq!(format_turn_tokens(Some(103_000)), "103.0k");
        assert_eq!(format_turn_tokens(None), "—");
    }

    #[test]
    fn turn_completion_time_uses_local_calendar_days() {
        use chrono::{Days, TimeZone as _};

        let now = Local
            .with_ymd_and_hms(2026, 8, 13, 0, 5, 0)
            .single()
            .unwrap();
        let completed_at = |days_ago: u64| {
            let date = now
                .date_naive()
                .checked_sub_days(Days::new(days_ago))
                .unwrap();
            date.and_hms_opt(23, 24, 0)
                .unwrap()
                .and_local_timezone(Local)
                .single()
                .unwrap()
                .timestamp_millis() as u64
        };
        let now_ms = now.timestamp_millis() as u64;

        assert_eq!(
            format_turn_completed_at_relative(completed_at(0), now_ms),
            "今天 23:24"
        );
        assert_eq!(
            format_turn_completed_at_relative(completed_at(1), now_ms),
            "昨天 23:24"
        );
        assert_eq!(
            format_turn_completed_at_relative(completed_at(2), now_ms),
            "前天 23:24"
        );
        assert_eq!(
            format_turn_completed_at_relative(completed_at(5), now_ms),
            "5 天前 23:24"
        );
        assert_eq!(
            format_turn_completed_at_relative(completed_at(132), now_ms),
            "132 天前 23:24"
        );
    }

    #[test]
    fn completed_turn_tokens_measure_context_growth_without_recounting_tool_rounds() {
        let mut completed = BTreeMap::from([
            (
                10,
                (
                    1,
                    Some(ApiUsage {
                        input_tokens: 25_000,
                        output_tokens: 1_000,
                        total_tokens: 26_000,
                    }),
                ),
            ),
            (
                11,
                (
                    1,
                    Some(ApiUsage {
                        input_tokens: 37_000,
                        output_tokens: 600,
                        total_tokens: 37_600,
                    }),
                ),
            ),
            (
                12,
                (
                    2,
                    Some(ApiUsage {
                        input_tokens: 98_000,
                        output_tokens: 1_000,
                        total_tokens: 99_000,
                    }),
                ),
            ),
        ]);
        assert_eq!(
            completed_turn_context_growth(&completed, 1, Some(25_000)),
            Some(12_600)
        );
        assert_eq!(
            completed_turn_context_growth(&completed, 1, None),
            Some(12_600)
        );
        assert_eq!(
            completed_turn_context_growth(&completed, 2, Some(97_000)),
            Some(2_000)
        );
        assert_eq!(completed_turn_context_growth(&completed, 3, None), None);

        completed.insert(13, (1, None));
        assert_eq!(
            completed_turn_context_growth(&completed, 1, Some(25_000)),
            None
        );
        assert_eq!(
            completed_turn_context_growth(&completed, 2, Some(97_000)),
            Some(2_000)
        );
    }

    #[test]
    fn control_plane_tool_lifecycles_are_hidden_from_chat_projection() {
        let mut edb = EventDataBase::new();
        let prompt_id = edb.append_user_prompt("investigate input latency").unwrap();
        let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
        let call_id = edb
            .append_tool_call(
                api_call_id,
                prompt_id,
                "provider-title",
                crate::agent_title::TOOL_NAME,
                r#"{"title":"调查输入延迟"}"#,
            )
            .unwrap();
        let workmap_call_id = edb
            .append_tool_call(
                api_call_id,
                prompt_id,
                "provider-workmap",
                "WorkMap.Read",
                "{}",
            )
            .unwrap();
        let worker_call_id = edb
            .append_tool_call(
                api_call_id,
                prompt_id,
                "provider-worker",
                "Worker.Wait",
                r#"{"max_wait_ms":1000}"#,
            )
            .unwrap();
        edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
            .unwrap();
        edb.append_agent_title_changed(call_id, "调查输入延迟")
            .unwrap();
        edb.append_tool_result(
            call_id,
            ToolResultState::Succeeded,
            None,
            r#"{"title":"调查输入延迟"}"#,
        )
        .unwrap();
        edb.append_tool_info(workmap_call_id, ToolOutputStream::Stdout, "workmap output")
            .unwrap();
        edb.append_tool_result(
            workmap_call_id,
            ToolResultState::Succeeded,
            None,
            r#"{"current":null}"#,
        )
        .unwrap();
        edb.append_tool_info(worker_call_id, ToolOutputStream::Stdout, "worker progress")
            .unwrap();
        edb.append_tool_result(
            worker_call_id,
            ToolResultState::Succeeded,
            None,
            r#"{"state":"completed"}"#,
        )
        .unwrap();

        let projection = ChatProjection::replay_events(edb.events()).unwrap();
        assert_eq!(projection.messages.len(), 2);
        assert_eq!(projection.messages[0].kind, ChatBlockKind::User);
        assert_eq!(projection.messages[0].content, "investigate input latency");
        assert_eq!(projection.messages[1].kind, ChatBlockKind::WorkerActivity);
        assert_eq!(
            projection.messages[1].tool.as_ref().unwrap().id,
            worker_call_id
        );

        let mut incremental = ChatProjection::default();
        incremental.consume(&edb).unwrap();
        assert_eq!(incremental.messages, projection.messages);
    }

    #[test]
    fn worker_wait_projects_the_worker_turn_tools_and_terminal_state() {
        let mut worker = EventDataBase::new();
        worker
            .append_agent_kind_def(
                AgentKind::SubAgent,
                "worker-agent",
                Some("manager".into()),
                None,
            )
            .unwrap();
        let prompt_id = worker.append_manager_prompt("inspect").unwrap();
        worker
            .append_agent_turn(prompt_id, prompt_id, AgentTurnState::Started, "")
            .unwrap();
        let api_call_id = worker.append_api_requesting(prompt_id).unwrap();
        worker
            .append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
            .unwrap();
        let file_call = worker
            .append_tool_call(
                api_call_id,
                prompt_id,
                "file-stat",
                "File.Stat",
                r#"{"path":"src/main.rs"}"#,
            )
            .unwrap();
        worker
            .append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
            .unwrap();
        worker
            .append_tool_result(
                file_call,
                ToolResultState::Succeeded,
                None,
                r#"{"size":42}"#,
            )
            .unwrap();

        let wait_started_at_ms = 0;
        let wait = ToolCard {
            id: 99,
            api_call_id: 98,
            name: crate::agent_toolbox::WORKER_WAIT.into(),
            arguments: r#"{"max_wait_ms":1000}"#.into(),
            started_at_ms: wait_started_at_ms,
            queued: false,
            session_id: None,
            output: String::new(),
            result: None,
        };
        let running = project_worker_activity(worker.events(), &wait).unwrap();
        assert_eq!(running.state, WorkerActivityState::Running);
        assert_eq!(running.tools.len(), 1);
        assert_eq!(running.tools[0].name, "File.Stat");
        assert_eq!(tool_brief(&running.tools[0]), "src/main.rs");
        assert_eq!(
            worker_activity_rows(&wait, Some(&running), 80)[0],
            UiRow::new("● 正在执行", RowTone::ToolRunning)
        );

        let current_directory_search = ToolCard {
            id: 100,
            api_call_id: 98,
            name: "File.Search".into(),
            arguments: r#"{"path":".","query":"NTC"}"#.into(),
            started_at_ms: 0,
            queued: false,
            session_id: None,
            output: String::new(),
            result: None,
        };
        assert_eq!(tool_brief(&current_directory_search), ". NTC");
        let search_rows = worker_activity_rows(
            &wait,
            Some(&WorkerActivity {
                state: WorkerActivityState::Running,
                tools: vec![current_directory_search],
            }),
            80,
        );
        assert_eq!(search_rows[1].text, "  ● File.Search . NTC");
        assert_eq!(
            search_rows[1].markdown_spans.as_ref().unwrap()[0]
                .style
                .color,
            MarkdownColorRole::Primary
        );

        let failed_tool = ToolCard {
            id: 101,
            api_call_id: 98,
            name: "File.Read".into(),
            arguments: r#"{"path":"missing.txt"}"#.into(),
            started_at_ms: 0,
            queued: false,
            session_id: None,
            output: String::new(),
            result: Some(ToolCardResult {
                state: ToolResultState::Failed,
                exit_code: None,
                detail: "not found".into(),
                finished_at_ms: 1,
            }),
        };
        let failed_rows = worker_activity_rows(
            &wait,
            Some(&WorkerActivity {
                state: WorkerActivityState::Running,
                tools: vec![failed_tool],
            }),
            80,
        );
        assert_eq!(
            failed_rows[1].markdown_spans.as_ref().unwrap()[0]
                .style
                .color,
            MarkdownColorRole::Error
        );

        let historical_cutoff = worker.events().last().unwrap().timestamp_ms();
        let second_api_call_id = worker.append_api_requesting(prompt_id).unwrap();
        let terminal_call = worker
            .append_tool_call(
                second_api_call_id,
                prompt_id,
                "terminal-status",
                "Terminal.Status",
                r#"{"session_id":"pty-1"}"#,
            )
            .unwrap();
        worker
            .append_api_state(second_api_call_id, prompt_id, ApiState::Completed, "")
            .unwrap();
        worker
            .append_tool_result(
                terminal_call,
                ToolResultState::Succeeded,
                None,
                r#"{"state":"running"}"#,
            )
            .unwrap();
        worker
            .append_agent_turn(prompt_id, prompt_id, AgentTurnState::Completed, "")
            .unwrap();
        let mut historical_wait = wait.clone();
        historical_wait.result = Some(ToolCardResult {
            state: ToolResultState::Succeeded,
            exit_code: None,
            detail: r#"{"state":"working"}"#.into(),
            finished_at_ms: historical_cutoff,
        });
        let timed_out = project_worker_activity(worker.events(), &historical_wait).unwrap();
        assert_eq!(timed_out.state, WorkerActivityState::Running);
        assert_eq!(timed_out.tools.len(), 2);
        assert!(!worker_wait_is_visible(&historical_wait));
        historical_wait.result.as_mut().unwrap().detail =
            r#"{"state":"wait_interrupted","reason":"follow_up"}"#.into();
        assert!(!worker_wait_is_visible(&historical_wait));

        worker.append_context_cleared().unwrap();
        let mut completed_wait = wait.clone();
        completed_wait.result = Some(ToolCardResult {
            state: ToolResultState::Succeeded,
            exit_code: None,
            detail: format!(r#"{{"state":"completed","turn_id":{prompt_id}}}"#),
            finished_at_ms: worker.events().last().unwrap().timestamp_ms(),
        });
        let next_prompt_id = worker.append_manager_prompt("next assignment").unwrap();
        worker
            .append_agent_turn(next_prompt_id, next_prompt_id, AgentTurnState::Started, "")
            .unwrap();
        completed_wait.result.as_mut().unwrap().finished_at_ms = u64::MAX;
        let completed = project_worker_activity(worker.events(), &completed_wait).unwrap();
        assert_eq!(completed.state, WorkerActivityState::Completed);
        assert_eq!(completed.tools.len(), 2);
        assert!(worker_wait_is_visible(&completed_wait));
        let rows = worker_activity_rows(&completed_wait, Some(&completed), 80);
        assert_eq!(rows[0], UiRow::new("● 已完成", RowTone::ToolSucceeded));
        assert!(rows[1].text.contains("File.Stat src/main.rs"));
        assert!(rows[2].text.contains("Terminal.Status pty-1"));
        assert!(rows.iter().all(|row| !row.text.contains('·')));
        assert!(rows.iter().skip(1).all(|row| row.text.starts_with("  ● ")));
        assert_eq!(
            rows[1].markdown_spans.as_ref().unwrap()[0].style.color,
            MarkdownColorRole::Success
        );
        assert!(!rows[1].markdown_spans.as_ref().unwrap()[1].style.bold);
        for (state, title) in [("interrupted", "● 已中断"), ("failed", "● 未完成")] {
            let mut terminal_wait = completed_wait.clone();
            terminal_wait.result.as_mut().unwrap().detail =
                format!(r#"{{"state":"{state}","turn_id":{prompt_id}}}"#);
            let activity = project_worker_activity(worker.events(), &terminal_wait).unwrap();
            assert_eq!(activity.tools.len(), 2);
            let rows = worker_activity_rows(&terminal_wait, Some(&activity), 80);
            assert_eq!(rows[0].text, title);
            assert_eq!(rows.len(), 3);
        }

        let mut failed_wait = wait.clone();
        failed_wait.result = Some(ToolCardResult {
            state: ToolResultState::Failed,
            exit_code: None,
            detail: "worker failed".into(),
            finished_at_ms: u64::MAX,
        });
        assert_eq!(
            worker_activity_rows(&failed_wait, None, 80)[0],
            UiRow::new("● 未完成", RowTone::ToolFailed)
        );
        let mut interrupted_wait = wait.clone();
        interrupted_wait.result = Some(ToolCardResult {
            state: ToolResultState::Succeeded,
            exit_code: None,
            detail: r#"{"state":"interrupted"}"#.into(),
            finished_at_ms: u64::MAX,
        });
        assert_eq!(
            worker_activity_rows(&interrupted_wait, None, 80)[0],
            UiRow::new("● 已中断", RowTone::ToolFailed)
        );
    }

    #[test]
    fn api_error_rolls_back_provisional_stream_while_interruption_commits_partial_stream() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("model").unwrap();
        edb.append_initial_reasoning_effort("high").unwrap();
        let prompt_id = edb.append_user_prompt("retry").unwrap();
        let failed_call = edb.append_api_requesting(prompt_id).unwrap();
        edb.append_api_state(failed_call, prompt_id, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt_id, "discarded", false)
            .unwrap();

        let mut projection = ChatProjection::default();
        projection.consume(&edb).unwrap();
        assert!(projection.messages.iter().any(|message| {
            message.kind == ChatBlockKind::Assistant && message.content == "discarded"
        }));

        edb.append_api_state_with_usage(
            failed_call,
            prompt_id,
            ApiState::Error,
            Some(ApiUsage {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
            }),
            "network failure",
        )
        .unwrap();
        projection.consume(&edb).unwrap();
        assert!(
            !projection
                .messages
                .iter()
                .any(|message| message.content == "discarded")
        );
        assert!(projection.messages.iter().any(|message| {
            message.kind == ChatBlockKind::StateNotice
                && message.content == "API 错误：network failure"
        }));
        assert_eq!(projection.api_usage, None);

        edb.append_api_retrying(failed_call, prompt_id, 1, 10, "network failure")
            .unwrap();
        projection.consume(&edb).unwrap();
        assert!(projection.messages.iter().any(|message| {
            message.kind == ChatBlockKind::StateNotice && message.content == "API 正在重试 1/10"
        }));

        let successful_call = edb.append_api_requesting(prompt_id).unwrap();
        edb.append_api_state(successful_call, prompt_id, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt_id, "kept", true).unwrap();
        edb.append_api_state(successful_call, prompt_id, ApiState::Completed, "")
            .unwrap();
        projection.consume(&edb).unwrap();
        assert!(projection.messages.iter().any(|message| {
            message.kind == ChatBlockKind::Assistant && message.content == "kept"
        }));
        assert_eq!(
            projection,
            ChatProjection::replay_events(edb.events()).unwrap()
        );

        let interrupted_prompt = edb.append_user_prompt("stop").unwrap();
        let interrupted_call = edb.append_api_requesting(interrupted_prompt).unwrap();
        edb.append_api_state(
            interrupted_call,
            interrupted_prompt,
            ApiState::Streaming,
            "",
        )
        .unwrap();
        edb.append_assist_response(interrupted_prompt, "useful partial", false)
            .unwrap();
        edb.append_api_state_with_usage(
            interrupted_call,
            interrupted_prompt,
            ApiState::Interrupted,
            Some(ApiUsage {
                input_tokens: 120,
                output_tokens: 4,
                total_tokens: 124,
            }),
            "user requested turn abort",
        )
        .unwrap();
        projection.consume(&edb).unwrap();

        assert!(projection.messages.iter().any(|message| {
            message.kind == ChatBlockKind::Assistant && message.content == "useful partial"
        }));
        assert_eq!(
            projection.api_usage,
            Some(ApiUsage {
                input_tokens: 120,
                output_tokens: 4,
                total_tokens: 124,
            })
        );
        assert_eq!(
            projection,
            ChatProjection::replay_events(edb.events()).unwrap()
        );
    }

    #[test]
    fn follow_up_prompt_projects_as_user_input() {
        let mut edb = EventDataBase::new();
        let prompt_id = edb.append_user_prompt("first").unwrap();
        edb.append_follow_up_prompt(prompt_id, "while running")
            .unwrap();

        let mut projection = ChatProjection::default();
        projection.consume(&edb).unwrap();

        assert_eq!(projection.messages.len(), 2);
        assert_eq!(projection.messages[0].kind, ChatBlockKind::User);
        assert_eq!(projection.messages[0].content, "first");
        assert_eq!(projection.messages[1].kind, ChatBlockKind::User);
        assert_eq!(projection.messages[1].content, "while running");
    }

    #[test]
    fn internal_agent_prompts_replay_as_read_only_input_blocks() {
        let events = vec![
            Event::ManagerPrompt(crate::event::ManagerPromptEvent {
                id: 0,
                timestamp_ms: 10,
                content: "manager instruction".into(),
            }),
            Event::ParentAgentPrompt(crate::event::ParentAgentPromptEvent {
                id: 1,
                timestamp_ms: 11,
                content: "parent assignment".into(),
            }),
        ];
        let projection = ChatProjection::replay_events(&events).unwrap();
        assert_eq!(projection.messages.len(), 2);
        assert_eq!(projection.messages[0].kind, ChatBlockKind::User);
        assert_eq!(projection.messages[0].content, "manager instruction");
        assert_eq!(projection.messages[1].kind, ChatBlockKind::User);
        assert_eq!(projection.messages[1].content, "parent assignment");
    }

    #[test]
    fn workmap_pending_reminder_is_internal_and_not_rendered() {
        let events = vec![
            Event::UserPrompt(crate::event::UserPromptEvent {
                id: 1,
                timestamp_ms: 10,
                content: "continue".into(),
            }),
            Event::WorkMapPendingReminder(crate::event::WorkMapPendingReminderEvent {
                id: 2,
                timestamp_ms: 11,
                prompt_id: 1,
            }),
        ];
        let projection = ChatProjection::replay_events(&events).unwrap();
        assert_eq!(projection.messages.len(), 1);
        assert_eq!(projection.messages[0].kind, ChatBlockKind::User);
        assert_eq!(projection.messages[0].content, "continue");
    }

    #[test]
    fn aborted_turn_stays_visible_until_a_separate_rewind_restores_its_prompt() {
        let mut edb = EventDataBase::new();
        let prompt_id = edb.append_user_prompt("try again").unwrap();
        let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
        edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt_id, "partial", false)
            .unwrap();
        let mut incremental = ChatProjection::default();
        incremental.consume(&edb).unwrap();
        assert_eq!(incremental.messages.len(), 2);

        edb.append_user_turn_aborted(prompt_id).unwrap();
        incremental.consume(&edb).unwrap();
        edb.append_api_state(
            api_call_id,
            prompt_id,
            ApiState::Interrupted,
            "user requested turn abort",
        )
        .unwrap();
        incremental.consume(&edb).unwrap();
        assert_eq!(incremental.messages.len(), 2);
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Rewind(prompt_id)
        );

        let mutation = edb.rewind_to_event(prompt_id).unwrap();
        incremental.consume(&edb).unwrap();

        assert!(incremental.messages.is_empty());
        assert_eq!(
            incremental,
            ChatProjection::replay_events(edb.events()).unwrap()
        );
        assert_eq!(
            mutation,
            EdbMutation::Rewind {
                target_event_id: prompt_id,
                restored_prompt_content: Some("try again".into()),
            }
        );
    }

    #[test]
    fn escape_action_is_driven_by_replayed_turn_state_and_rewinds_abort_chain() {
        let mut edb = EventDataBase::new();
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Clear
        );

        let first = edb.append_user_prompt("first").unwrap();
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Abort
        );
        let first_api = edb.append_api_requesting(first).unwrap();
        edb.append_api_state(first_api, first, ApiState::Streaming, "")
            .unwrap();
        edb.append_user_turn_aborted(first).unwrap();
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Wait
        );
        edb.append_api_state(
            first_api,
            first,
            ApiState::Interrupted,
            "user requested turn abort",
        )
        .unwrap();
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Rewind(first)
        );

        let second = edb.append_user_prompt("second").unwrap();
        let second_api = edb.append_api_requesting(second).unwrap();
        edb.append_api_state(second_api, second, ApiState::Streaming, "")
            .unwrap();
        edb.append_user_turn_aborted(second).unwrap();
        edb.append_api_state(
            second_api,
            second,
            ApiState::Interrupted,
            "user requested turn abort",
        )
        .unwrap();
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Rewind(second)
        );

        edb.rewind_to_event(second).unwrap();
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Rewind(first)
        );
        edb.rewind_to_event(first).unwrap();
        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Clear
        );
    }

    #[test]
    fn a_normally_completed_turn_stops_escape_from_reaching_older_aborts() {
        let mut edb = EventDataBase::new();
        let aborted = edb.append_user_prompt("aborted").unwrap();
        edb.append_user_turn_aborted(aborted).unwrap();
        let completed = edb.append_user_prompt("completed").unwrap();
        let api_call_id = edb.append_api_requesting(completed).unwrap();
        edb.append_api_state(api_call_id, completed, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(completed, "done", true).unwrap();
        edb.append_api_state(api_call_id, completed, ApiState::Completed, "")
            .unwrap();

        assert_eq!(
            transcript_escape_action(edb.events()).unwrap(),
            TranscriptEscapeAction::Clear
        );
    }

    #[test]
    fn slash_palette_filters_wraps_and_opens_commands() {
        assert_eq!(
            matching_commands("/", CommandScope::Interactive),
            SlashCommand::INTERACTIVE
        );
        assert_eq!(
            matching_commands("/e", CommandScope::Interactive),
            vec![SlashCommand::Effort, SlashCommand::Exit]
        );
        assert_eq!(
            matching_commands("/m", CommandScope::Interactive),
            vec![SlashCommand::Model]
        );
        assert_eq!(
            matching_commands("/context", CommandScope::Interactive),
            vec![SlashCommand::Context]
        );
        assert_eq!(SlashCommand::AgentAdd.name(), "/new_session");
        assert_eq!(SlashCommand::AgentDelete.name(), "/delete_session");
        assert_eq!(
            matching_commands("/new", CommandScope::Interactive),
            vec![SlashCommand::AgentAdd]
        );
        assert_eq!(
            matching_commands("/delete", CommandScope::Interactive),
            vec![SlashCommand::AgentDelete]
        );
        assert!(matching_commands("/agent-", CommandScope::Interactive).is_empty());
        assert_eq!(
            matching_commands("/rew", CommandScope::Interactive),
            vec![SlashCommand::Rewind]
        );
        assert!(matching_commands("/unknown", CommandScope::Interactive).is_empty());

        let mut input = "/".to_owned();
        let mut overlay = OverlayState::Commands {
            scope: CommandScope::Interactive,
            selected: 0,
        };
        assert_eq!(
            handle_overlay_key(&mut overlay, &mut input, KeyCode::Up),
            OverlayAction::Redraw
        );
        assert_eq!(
            overlay,
            OverlayState::Commands {
                scope: CommandScope::Interactive,
                selected: SlashCommand::INTERACTIVE.len() - 1
            }
        );
        assert_eq!(
            handle_overlay_key(&mut overlay, &mut input, KeyCode::Down),
            OverlayAction::Redraw
        );
        assert_eq!(
            overlay,
            OverlayState::Commands {
                scope: CommandScope::Interactive,
                selected: 0
            }
        );

        for character in "rewind".chars() {
            handle_overlay_key(&mut overlay, &mut input, KeyCode::Char(character));
        }
        assert_eq!(input, "/rewind");
        assert_eq!(
            handle_overlay_key(&mut overlay, &mut input, KeyCode::Enter),
            OverlayAction::Open(SlashCommand::Rewind)
        );
    }

    #[test]
    fn worker_reuses_the_palette_with_worker_only_commands() {
        assert_eq!(
            matching_commands("/", CommandScope::Worker),
            SlashCommand::WORKER
        );
        assert_eq!(
            matching_commands("/s", CommandScope::Worker),
            vec![SlashCommand::Stop]
        );
        assert_eq!(
            matching_commands("/context", CommandScope::Worker),
            vec![SlashCommand::Context]
        );
        assert!(matching_commands("/new_session", CommandScope::Worker).is_empty());
        assert!(matching_commands("/delete_session", CommandScope::Worker).is_empty());
        assert!(matching_commands("/rewind", CommandScope::Worker).is_empty());
        assert!(matching_commands("/exit", CommandScope::Worker).is_empty());

        let mut worker =
            TuiSession::new("worker", "worker-agent", &[], 0, "/bin/bash", &[]).unwrap();
        worker.read_only_state = Some(ReadOnlyAgentState::Working);
        assert_eq!(worker.command_scope(), Some(CommandScope::Worker));
        assert!(worker.open_command_palette());
        assert_eq!(worker.input, "/");
        assert_eq!(
            worker.overlay,
            Some(OverlayState::Commands {
                scope: CommandScope::Worker,
                selected: 0
            })
        );
        assert_eq!(worker.read_only_state, Some(ReadOnlyAgentState::Working));

        let mut ordinary_child =
            TuiSession::new("child", "main-agent", &[], 0, "/bin/bash", &[]).unwrap();
        ordinary_child.read_only_state = Some(ReadOnlyAgentState::Working);
        assert_eq!(ordinary_child.command_scope(), None);
        assert!(!ordinary_child.open_command_palette());
        assert!(ordinary_child.overlay.is_none());
    }

    #[test]
    fn command_modals_return_the_selected_control_action() {
        let mut input = String::new();
        let mut model = OverlayState::Model {
            choices: vec!["first".into(), "second".into()],
            selected: 0,
        };
        handle_overlay_key(&mut model, &mut input, KeyCode::Down);
        assert_eq!(
            handle_overlay_key(&mut model, &mut input, KeyCode::Enter),
            OverlayAction::SubmitModel("second".into())
        );

        let mut effort = OverlayState::Effort {
            choices: vec!["low".into(), "high".into(), "max".into()],
            selected: 0,
        };
        handle_overlay_key(&mut effort, &mut input, KeyCode::Down);
        assert_eq!(
            handle_overlay_key(&mut effort, &mut input, KeyCode::Enter),
            OverlayAction::SubmitEffort("high".into())
        );

        let mut clear = OverlayState::Clear { selected: 0 };
        assert_eq!(
            handle_overlay_key(&mut clear, &mut input, KeyCode::Enter),
            OverlayAction::SubmitClear
        );
        handle_overlay_key(&mut clear, &mut input, KeyCode::Down);
        assert_eq!(
            handle_overlay_key(&mut clear, &mut input, KeyCode::Enter),
            OverlayAction::Close
        );

        let mut rewind = OverlayState::Rewind {
            choices: vec![RewindChoice {
                event_id: 42,
                kind: RewindChoiceKind::UserPrompt("target".into()),
            }],
            selected: 0,
        };
        assert_eq!(
            handle_overlay_key(&mut rewind, &mut input, KeyCode::Enter),
            OverlayAction::SubmitRewind(42)
        );
        assert_eq!(
            handle_overlay_key(&mut rewind, &mut input, KeyCode::Esc),
            OverlayAction::Close
        );

        let id = AgentId::new("agent-a1").unwrap();
        let mut add = OverlayState::AgentAdd {
            choices: vec![
                "main-agent".into(),
                "manager-agent".into(),
                "chatbot".into(),
            ],
            selected: 0,
        };
        let rows = agent_add_overlay_rows(
            match &add {
                OverlayState::AgentAdd { choices, .. } => choices,
                _ => unreachable!(),
            },
            0,
            140,
        );
        let copy = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(copy.contains("创建新的会话？"));
        assert!(copy.contains("选择 Agent 类型。创建后不可更改。"));
        assert!(copy.contains("标准 (main-agent)"));
        assert!(copy.contains("单 Agent 模式，响应直接，Token 开销较低"));
        assert!(copy.contains("协作 (manager-agent)"));
        assert!(
            copy.contains("双 Agent 协作，适合复杂任务，减少主模型上下文占用，但总 Token 开销更高")
        );
        assert!(copy.contains("聊天 (chatbot)"));
        assert!(copy.contains("仅进行对话，不使用工作工具"));
        assert!(rows.iter().any(|row| {
            row.text.contains("标准 (main-agent)") && row.tone == RowTone::OverlaySelected
        }));
        for width in [32, 48, 96] {
            assert!(
                agent_add_overlay_rows(
                    match &add {
                        OverlayState::AgentAdd { choices, .. } => choices,
                        _ => unreachable!(),
                    },
                    0,
                    width,
                )
                .iter()
                .all(|row| display_width(&row.text) <= width)
            );
        }
        assert_eq!(
            handle_overlay_key(&mut add, &mut input, KeyCode::Enter),
            OverlayAction::SubmitAgentAdd("main-agent".into())
        );
        handle_overlay_key(&mut add, &mut input, KeyCode::Down);
        assert_eq!(
            handle_overlay_key(&mut add, &mut input, KeyCode::Enter),
            OverlayAction::SubmitAgentAdd("manager-agent".into())
        );
        handle_overlay_key(&mut add, &mut input, KeyCode::Down);
        assert_eq!(
            handle_overlay_key(&mut add, &mut input, KeyCode::Enter),
            OverlayAction::SubmitAgentAdd("chatbot".into())
        );
        assert_eq!(
            handle_overlay_key(&mut add, &mut input, KeyCode::Esc),
            OverlayAction::Close
        );
        let mut delete = OverlayState::AgentDelete {
            id: id.clone(),
            path: "/workspace/.me/edb/agent-a1.edb".into(),
            blocker: None,
            selected: 0,
        };
        assert_eq!(
            handle_overlay_key(&mut delete, &mut input, KeyCode::Enter),
            OverlayAction::SubmitAgentDelete(id)
        );
        let mut blocked = OverlayState::AgentDelete {
            id: AgentId::new("busy").unwrap(),
            path: "/workspace/.me/edb/busy.edb".into(),
            blocker: Some("Agent loop 正在运行".into()),
            selected: 0,
        };
        assert_eq!(
            handle_overlay_key(&mut blocked, &mut input, KeyCode::Enter),
            OverlayAction::Close
        );
    }

    #[test]
    fn rewind_choices_only_include_current_physical_branch_targets() {
        let mut edb = EventDataBase::new();
        let first = edb.append_user_prompt("first\nmessage").unwrap();
        edb.append_assist_response(first, "answer", true).unwrap();
        let discarded = edb.append_user_prompt("discarded").unwrap();
        edb.append_assist_response(discarded, "discarded answer", true)
            .unwrap();
        edb.rewind_to_event(discarded).unwrap();
        let branch = edb.append_user_prompt("branch").unwrap();

        let choices = rewind_choices(edb.events()).unwrap();
        assert_eq!(
            choices,
            vec![
                RewindChoice {
                    event_id: branch,
                    kind: RewindChoiceKind::UserPrompt("branch".into()),
                },
                RewindChoice {
                    event_id: first,
                    kind: RewindChoiceKind::UserPrompt("first\nmessage".into()),
                },
            ]
        );
        assert!(choices.iter().all(|choice| choice.event_id != discarded));

        let overlay = OverlayState::Rewind {
            choices,
            selected: 1,
        };
        let (_, description, rows, selected) = overlay_content(&overlay, "");
        assert!(description.contains("目标事件及其后"));
        assert!(rows.iter().any(|row| row.contains("first message")));
        assert_eq!(selected, 1);
    }

    #[test]
    fn rewind_choices_can_cross_a_clear_barrier_on_the_current_physical_branch() {
        let mut edb = EventDataBase::new();
        let before_clear = edb.append_user_prompt("before clear").unwrap();
        edb.append_assist_response(before_clear, "answer", true)
            .unwrap();
        let clear = edb.append_context_cleared().unwrap();
        let after_clear = edb.append_user_prompt("after clear").unwrap();

        let effective = effective_ui_events(edb.events()).unwrap();
        assert_eq!(effective.len(), 2);
        assert!(matches!(effective[0], Event::ContextCleared(event) if event.id == clear));
        assert!(matches!(effective[1], Event::UserPrompt(prompt) if prompt.id == after_clear));
        assert_eq!(
            rewind_choices(edb.events()).unwrap(),
            vec![
                RewindChoice {
                    event_id: after_clear,
                    kind: RewindChoiceKind::UserPrompt("after clear".into()),
                },
                RewindChoice {
                    event_id: clear,
                    kind: RewindChoiceKind::ContextCleared,
                },
                RewindChoice {
                    event_id: before_clear,
                    kind: RewindChoiceKind::UserPrompt("before clear".into()),
                },
            ]
        );

        let overlay = OverlayState::Rewind {
            choices: rewind_choices(edb.events()).unwrap(),
            selected: 1,
        };
        let (_, _, rows, _) = overlay_content(&overlay, "");
        assert!(rows.iter().any(|row| row.contains("上下文清理")));
    }

    #[test]
    fn clear_and_rewind_replay_ui_while_effort_remains_stateful() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("test").unwrap();
        edb.append_initial_reasoning_effort("unset").unwrap();
        let old = edb.append_user_prompt("old").unwrap();
        edb.append_assist_response(old, "old answer", true).unwrap();
        edb.append_reasoning_effort_changed("high").unwrap();
        let mut projection = ChatProjection::default();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.messages.len(), 3);

        let clear = edb.append_context_cleared().unwrap();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.messages.len(), 1);
        assert_eq!(projection.messages[0].kind, ChatBlockKind::StateNotice);
        assert_eq!(projection.messages[0].content, "上下文已清空");
        assert_eq!(projection.effort.as_deref(), Some("high"));

        let target = edb.append_user_prompt("target").unwrap();
        edb.append_assist_response(target, "draft", true).unwrap();
        edb.append_reasoning_effort_changed("max").unwrap();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.messages.len(), 4);
        edb.rewind_to_event(target).unwrap();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.messages.len(), 1);
        assert_eq!(projection.messages[0].content, "上下文已清空");
        assert_eq!(projection.effort.as_deref(), Some("high"));
        assert_eq!(
            projection,
            ChatProjection::replay_events(edb.events()).unwrap()
        );

        edb.rewind_to_event(clear).unwrap();
        projection.consume(&edb).unwrap();
        assert!(
            projection
                .messages
                .iter()
                .any(|message| message.content == "old")
        );
        assert!(
            projection
                .messages
                .iter()
                .any(|message| message.content == "old answer")
        );
        assert!(
            projection
                .messages
                .iter()
                .all(|message| message.content != "上下文已清空")
        );
    }

    #[test]
    fn clone_completed_notice_matches_between_incremental_and_full_replay() {
        let events = vec![Event::CloneCompleted(crate::event::CloneCompletedEvent {
            id: 0,
            timestamp_ms: 1_000,
            title: "原会话 (1)".into(),
        })];
        let mut incremental = ChatProjection::default();
        incremental.consume_events(&events).unwrap();
        let replayed = ChatProjection::replay_events(&events).unwrap();

        assert_eq!(incremental, replayed);
        assert_eq!(incremental.messages.len(), 1);
        assert_eq!(incremental.messages[0].kind, ChatBlockKind::StateNotice);
        assert_eq!(
            incremental.messages[0].content,
            "克隆完成。新会话：原会话 (1)"
        );
    }

    #[test]
    fn model_and_effort_state_replay_independently_from_conversation() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("first").unwrap();
        edb.append_initial_reasoning_effort("low").unwrap();
        let mut projection = ChatProjection::replay_events(edb.events()).unwrap();
        assert!(projection.messages.is_empty());
        assert_eq!(projection.model_name.as_deref(), Some("first"));
        assert_eq!(projection.effort.as_deref(), Some("low"));

        let prompt = edb.append_user_prompt("discarded").unwrap();
        edb.append_assist_response(prompt, "discarded answer", true)
            .unwrap();
        edb.append_model_changed("second").unwrap();
        edb.append_reasoning_effort_fallback().unwrap();
        edb.rewind_to_event(prompt).unwrap();
        projection.consume(&edb).unwrap();

        assert_eq!(projection.model_name.as_deref(), Some("first"));
        assert_eq!(projection.effort.as_deref(), Some("low"));
        assert!(projection.messages.is_empty());
        assert_eq!(
            projection,
            ChatProjection::replay_events(edb.events()).unwrap()
        );
    }

    #[test]
    fn api_terminal_usage_projects_and_model_change_discards_stale_usage() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("first").unwrap();
        edb.append_initial_reasoning_effort("high").unwrap();
        let prompt_id = edb.append_user_prompt("hello").unwrap();
        let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
        edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt_id, "world", true)
            .unwrap();
        edb.append_api_state_with_usage(
            api_call_id,
            prompt_id,
            ApiState::Completed,
            Some(ApiUsage {
                input_tokens: 50_000,
                output_tokens: 6_100,
                total_tokens: 56_100,
            }),
            "",
        )
        .unwrap();

        let mut projection = ChatProjection::replay_events(edb.events()).unwrap();
        assert_eq!(projection.api_state, Some(ApiState::Completed));
        assert_eq!(
            projection.api_usage,
            Some(ApiUsage {
                input_tokens: 50_000,
                output_tokens: 6_100,
                total_tokens: 56_100,
            })
        );

        let target = edb.append_user_prompt("discard this branch").unwrap();
        edb.append_model_changed("second").unwrap();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.model_name.as_deref(), Some("second"));
        assert_eq!(projection.api_usage, None);

        edb.rewind_to_event(target).unwrap();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.model_name.as_deref(), Some("first"));
        assert_eq!(
            projection.api_usage,
            Some(ApiUsage {
                input_tokens: 50_000,
                output_tokens: 6_100,
                total_tokens: 56_100,
            })
        );
        assert_eq!(
            projection,
            ChatProjection::replay_events(edb.events()).unwrap()
        );
    }

    #[test]
    fn rewind_rolls_context_usage_back_to_the_effective_branch() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("model").unwrap();
        edb.append_initial_reasoning_effort("high").unwrap();

        let first = edb.append_user_prompt("first").unwrap();
        let first_call = edb.append_api_requesting(first).unwrap();
        edb.append_api_state(first_call, first, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(first, "one", true).unwrap();
        edb.append_api_state_with_usage(
            first_call,
            first,
            ApiState::Completed,
            Some(ApiUsage {
                input_tokens: 8,
                output_tokens: 2,
                total_tokens: 10,
            }),
            "",
        )
        .unwrap();

        let second = edb.append_user_prompt("second").unwrap();
        let second_call = edb.append_api_requesting(second).unwrap();
        edb.append_api_state(second_call, second, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(second, "two", true).unwrap();
        edb.append_api_state_with_usage(
            second_call,
            second,
            ApiState::Completed,
            Some(ApiUsage {
                input_tokens: 24,
                output_tokens: 6,
                total_tokens: 30,
            }),
            "",
        )
        .unwrap();

        let mut projection = ChatProjection::replay_events(edb.events()).unwrap();
        assert_eq!(projection.api_usage.unwrap().total_tokens, 30);

        edb.rewind_to_event(second).unwrap();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.api_usage.unwrap().total_tokens, 10);

        edb.rewind_to_event(first).unwrap();
        projection.consume(&edb).unwrap();
        assert_eq!(projection.api_usage, None);
        assert_eq!(
            projection,
            ChatProjection::replay_events(edb.events()).unwrap()
        );
    }

    #[test]
    fn rewind_mutation_only_closes_the_pending_tui_request() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("test").unwrap();
        edb.append_initial_reasoning_effort("low").unwrap();
        let target = edb.append_user_prompt("edit this prompt").unwrap();
        let mut session =
            TuiSession::new("main", "main-agent", edb.events(), 0, "/bin/bash", &[]).unwrap();
        assert_eq!(session.current_effort(), Some("low"));

        session.pending_escape_rewind = Some(target);
        let mutation = edb.rewind_to_event(target).unwrap();
        session
            .apply_new_event_effects(edb.events(), 64, Some(&mutation))
            .unwrap();
        assert!(session.input.is_empty());
        assert_eq!(session.pending_escape_rewind, None);
        let restored =
            TuiSession::new("main", "main-agent", edb.events(), 64, "/bin/bash", &[]).unwrap();
        assert!(restored.input.is_empty());

        edb.append_reasoning_effort_changed("high").unwrap();
        session.projection.consume(&edb).unwrap();
        session
            .apply_new_event_effects(edb.events(), 96, None)
            .unwrap();
        assert!(session.input.is_empty());
        assert_eq!(session.current_effort(), Some("high"));

        edb.append_user_prompt("replacement").unwrap();
        session
            .apply_new_event_effects(edb.events(), 128, None)
            .unwrap();
        let restored =
            TuiSession::new("main", "main-agent", edb.events(), 128, "/bin/bash", &[]).unwrap();
        assert!(restored.input.is_empty());
    }

    #[test]
    fn rewinding_a_context_clear_does_not_synthesize_a_prompt_draft() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("test").unwrap();
        edb.append_initial_reasoning_effort("low").unwrap();
        let prompt = edb.append_user_prompt("visible again").unwrap();
        edb.append_assist_response(prompt, "answer", true).unwrap();
        let clear = edb.append_context_cleared().unwrap();
        let mut session =
            TuiSession::new("main", "main-agent", edb.events(), 0, "/bin/bash", &[]).unwrap();
        session.input = "unchanged draft".into();

        let mutation = edb.rewind_to_event(clear).unwrap();
        assert_eq!(
            mutation,
            EdbMutation::Rewind {
                target_event_id: clear,
                restored_prompt_content: None,
            }
        );
        session
            .apply_new_event_effects(edb.events(), 64, Some(&mutation))
            .unwrap();
        assert_eq!(session.input, "unchanged draft");
    }

    #[test]
    fn compact_lifecycle_notice_uses_transient_sse_activity_and_terminal_state() {
        let compact_event = |id, state, stage, detail: &str| {
            Event::CompactStateUpdate(crate::event::CompactStateUpdateEvent {
                id,
                timestamp_ms: id * 10,
                compact_id: 1,
                tool_call_id: 0,
                prompt_id: 0,
                kind: CompactKind::MainAgentMultiTurn,
                total_stages: 6,
                state,
                stage,
                content: if state == CompactState::StageCompleted {
                    "stage output".into()
                } else {
                    String::new()
                },
                detail: detail.into(),
            })
        };
        let api_event =
            |id: EventId, api_call_id: EventId, state: ApiState, output_tokens: Option<u64>| {
                Event::ApiStateUpdate(crate::event::ApiStateUpdateEvent {
                    id,
                    timestamp_ms: id * 10,
                    api_call_id,
                    prompt_id: 0,
                    state,
                    retry_count: 0,
                    retry_limit: 10,
                    usage: output_tokens.map(|output_tokens| ApiUsage {
                        input_tokens: 20_000,
                        output_tokens,
                        total_tokens: 20_000 + output_tokens,
                    }),
                    detail: String::new(),
                })
            };
        let events = vec![
            compact_event(1, CompactState::Started, None, ""),
            api_event(2, 100, ApiState::Completed, Some(1_234)),
            compact_event(
                3,
                CompactState::StageCompleted,
                Some(crate::event::CompactStage::Analysis),
                "",
            ),
            api_event(4, 101, ApiState::Error, Some(2_366)),
            api_event(5, 101, ApiState::Interrupted, None),
            compact_event(6, CompactState::Failed, None, "failed"),
        ];

        let mut projection = ChatProjection::default();
        projection.consume_events(&events[..1]).unwrap();
        assert_eq!(projection.messages[0].content, "正在压缩 (1/6) ...");
        assert!(projection.apply_api_activity(UiApiActivity {
            active: true,
            received_sse_events: 37,
        }));
        assert_eq!(projection.messages[0].content, "正在压缩 (1/6) ... ↓ 37");
        assert!(projection.apply_api_activity(UiApiActivity::default()));
        projection.consume_events(&events[..2]).unwrap();
        assert_eq!(projection.messages[0].content, "正在压缩 (1/6) ...");
        projection.consume_events(&events[..3]).unwrap();
        assert_eq!(projection.messages[0].content, "正在压缩 (2/6) ...");
        projection.consume_events(&events[..4]).unwrap();
        assert_eq!(projection.messages[0].content, "正在压缩 (2/6) ...");
        projection.consume_events(&events).unwrap();
        assert_eq!(projection.messages[0].content, "压缩失败");
        assert_eq!(projection, ChatProjection::replay_events(&events).unwrap());

        for (state, expected) in [
            (CompactState::Completed, "上下文已压缩"),
            (CompactState::Interrupted, "压缩中断"),
        ] {
            let terminal = vec![
                compact_event(1, CompactState::Started, None, ""),
                compact_event(2, state, None, "terminal"),
            ];
            let replayed = ChatProjection::replay_events(&terminal).unwrap();
            assert_eq!(replayed.messages.len(), 1);
            assert_eq!(replayed.messages[0].content, expected);
        }
        assert_eq!(
            compact_progress_text(1, 1, Some(100)),
            "正在压缩 (1/1) ... ↓ 100"
        );
        assert_eq!(compact_progress_text(6, 1, None), "正在压缩 (1/6) ...");
        assert_eq!(
            compact_progress_text(7, 7, Some(321)),
            "正在压缩 (7/7) ... ↓ 321"
        );
    }

    #[test]
    fn api_activity_count_is_independent_from_chat_projection() {
        let active = UiApiActivity {
            active: true,
            received_sse_events: 37,
        };
        assert_eq!(
            format_status_activity("12.3k/500k", active),
            "12.3k/500k  ↓ 37"
        );
        assert_eq!(
            format_status_activity("12.3k/500k", UiApiActivity::default()),
            "12.3k/500k"
        );
    }

    #[test]
    fn compact_completion_is_a_notice_and_rewind_target_without_a_tool_card() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("visible history").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let tool = edb
            .append_tool_call(api, prompt, "compact-call", "Compact", "{}")
            .unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
            .unwrap();
        let compact = edb
            .append_compact_started(tool, prompt, crate::event::CompactKind::WorkerSingleTurn)
            .unwrap();
        let summary_api = edb.append_api_requesting(prompt).unwrap();
        edb.append_api_state(summary_api, prompt, ApiState::Streaming, "")
            .unwrap();
        edb.append_api_state(summary_api, prompt, ApiState::Completed, "")
            .unwrap();
        let completed = edb
            .append_compact_terminal(
                compact,
                CompactState::Completed,
                "Summary:\ncontinuation",
                "",
            )
            .unwrap();

        let projection = ChatProjection::replay_events(edb.events()).unwrap();
        assert!(
            projection
                .messages
                .iter()
                .any(|message| message.content == "visible history")
        );
        assert!(
            projection
                .messages
                .iter()
                .any(|message| message.content == "上下文已压缩")
        );
        assert!(!projection.messages.iter().any(|message| {
            message
                .tool
                .as_ref()
                .is_some_and(|tool| tool.name == "Compact")
        }));
        assert!(rewind_choices(edb.events()).unwrap().iter().any(|choice| {
            choice.event_id == completed && choice.kind == RewindChoiceKind::ContextCompacted
        }));

        edb.rewind_to_event(completed).unwrap();
        let replayed = ChatProjection::replay_events(edb.events()).unwrap();
        assert!(
            replayed
                .messages
                .iter()
                .all(|message| message.content != "上下文已压缩")
        );
    }

    #[test]
    fn renders_running_and_completed_terminal_cards() {
        let running = ToolCard {
            id: 7,
            api_call_id: 6,
            name: "Terminal.Interact".to_owned(),
            arguments: r#"{"session_id":"pty-4","input":[{"type":"text","text":"pwd"},{"type":"key","key":"enter"}],"max_wait_ms":3000}"#.to_owned(),
            started_at_ms: 1_000,
            queued: false,
            session_id: Some("pty-4".to_owned()),
            output: String::new(),
            result: None,
        };
        let running_rows = tool_rows(&running, 100, false, 2_200);
        assert_eq!(running_rows.len(), 1);
        assert_eq!(running_rows[0].tone, RowTone::ToolRunning);
        let running_text = running_rows
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(running_text, "● Terminal.Interact pty-4 pwd↵");

        let running_detail = tool_rows(&running, 100, true, 2_200)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(running_detail.contains("Session: pty-4"));
        assert!(running_detail.contains("Input  : pwd↵"));
        assert!(running_detail.contains("Running ... 1.2s (timeout 3s)"));

        let human_action = ToolCard {
            id: 8,
            api_call_id: 6,
            name: "WebBrowser.RequireHumanAction".to_owned(),
            arguments:
                r#"{"page_id":"p0000001","instruction":"Complete the visible verification"}"#
                    .to_owned(),
            started_at_ms: 1_000,
            queued: false,
            session_id: None,
            output: String::new(),
            result: None,
        };
        let human_action_text = tool_rows(&human_action, 100, false, 2_200)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            human_action_text,
            "● WebBrowser.RequireHumanAction p0000001 Complete the visible verification"
        );

        let completed = ToolCard {
            output: "/root\nproject".to_owned(),
            result: Some(ToolCardResult {
                state: ToolResultState::Succeeded,
                exit_code: Some(0),
                detail: "completed".to_owned(),
                finished_at_ms: 3_500,
            }),
            ..running.clone()
        };
        let completed_rows = tool_rows(&completed, 100, false, 9_999);
        assert_eq!(completed_rows[0].tone, RowTone::ToolSucceeded);
        let collapsed = completed_rows
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(collapsed, "● Terminal.Interact pty-4 pwd↵");

        let expanded = tool_rows(&completed, 100, true, 9_999)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("Output : /root"));
        assert!(expanded.contains("│          project"));
        assert!(expanded.contains("Time use: 2.5s"));

        let failed = ToolCard {
            result: Some(ToolCardResult {
                state: ToolResultState::Failed,
                exit_code: Some(1),
                detail: "failed".to_owned(),
                finished_at_ms: 2_000,
            }),
            ..running
        };
        assert_eq!(
            tool_rows(&failed, 100, false, 9_999)[0].tone,
            RowTone::ToolFailed
        );

        let empty = ToolCard {
            output: String::new(),
            result: Some(ToolCardResult {
                state: ToolResultState::Succeeded,
                exit_code: None,
                detail: r#"{"session_id":"pty-4","mode":"delta","state":"running"}"#.to_owned(),
                finished_at_ms: 2_000,
            }),
            ..completed
        };
        let empty = tool_rows(&empty, 100, true, 9_999)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(empty.contains("Output : (no terminal output)"));
        assert!(!empty.contains(r#""mode":"delta""#));
    }

    #[test]
    fn multi_tool_batch_renders_only_the_current_call_as_running() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("model").unwrap();
        edb.append_initial_reasoning_effort("unset").unwrap();
        let prompt_id = edb.append_user_prompt("run both").unwrap();
        let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
        edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
            .unwrap();
        edb.append_assist_response(prompt_id, "", true).unwrap();
        let first = edb
            .append_tool_call(
                api_call_id,
                prompt_id,
                "call-1",
                "File.Stat",
                r#"{"path":"a"}"#,
            )
            .unwrap();
        let second = edb
            .append_tool_call(
                api_call_id,
                prompt_id,
                "call-2",
                "File.Stat",
                r#"{"path":"b"}"#,
            )
            .unwrap();
        edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
            .unwrap();

        let mut projection = ChatProjection::replay_events(edb.events()).unwrap();
        let tools = projection
            .messages
            .iter()
            .filter_map(|message| message.tool.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 2);
        assert!(!tools[0].queued);
        assert!(tools[1].queued);
        assert_eq!(
            tool_rows(tools[0], 80, false, 1_000)[0].tone,
            RowTone::ToolRunning
        );
        let queued_rows = tool_rows(tools[1], 80, false, 1_000);
        assert_eq!(queued_rows[0].tone, RowTone::ToolQueued);
        assert_eq!(queued_rows.len(), 1);
        assert_eq!(queued_rows[0].text, "● File.Stat b");

        edb.append_tool_result(first, ToolResultState::Failed, None, "failed")
            .unwrap();
        let second_started_at = edb.events().last().unwrap().timestamp_ms();
        projection.consume(&edb).unwrap();
        let tools = projection
            .messages
            .iter()
            .filter_map(|message| message.tool.as_ref())
            .collect::<Vec<_>>();
        assert!(tools[0].result.is_some());
        assert!(!tools[1].queued);
        assert_eq!(tools[1].started_at_ms, second_started_at);
        assert_eq!(
            tool_rows(tools[1], 80, false, second_started_at)[0].tone,
            RowTone::ToolRunning
        );

        edb.append_tool_result(second, ToolResultState::Succeeded, None, "ok")
            .unwrap();
        projection.consume(&edb).unwrap();
        assert!(
            projection
                .messages
                .iter()
                .filter_map(|message| message.tool.as_ref())
                .all(|tool| tool.result.is_some())
        );
    }

    #[test]
    fn terminal_input_renders_ordered_semantic_keys_without_control_characters() {
        let arguments = serde_json::json!({
            "session_id": "pty-4",
            "input": [
                {"type": "key", "key": "escape"},
                {"type": "text", "text": ":wq"},
                {"type": "key", "key": "c", "modifiers": ["ctrl"]},
                {"type": "key", "key": "left", "modifiers": ["shift", "ctrl"], "repeat": 3},
                {"type": "key", "key": "enter"}
            ]
        })
        .to_string();
        let visible = terminal_input(&arguments).unwrap();
        assert_eq!(visible, "Esc:wqCtrl+CCtrl+Shift+Left×3↵");
        assert!(!visible.chars().any(char::is_control));
    }

    #[test]
    fn structured_terminal_line_projection_appends_and_preserves_empty_patches() {
        let update = ToolInfoContent::Terminal(crate::terminal::test_update("old output"));
        let (replace, content) = projected_tool_output(ToolOutputStream::Terminal, &update);
        assert!(!replace);
        assert_eq!(content, "000000: old output");

        let mut cursor_only = crate::terminal::test_update("");
        cursor_only.rows.clear();
        let cursor_only = ToolInfoContent::Terminal(cursor_only);
        let (replace, content) = projected_tool_output(ToolOutputStream::Terminal, &cursor_only);
        assert!(!replace);
        assert!(content.is_empty());
    }

    #[test]
    fn collapsed_tool_call_is_one_summary_line_and_expands_to_complete_details() {
        let session_id = format!("pty-{}", "session".repeat(12));
        let input = format!("printf {}", "argument ".repeat(12));
        let tool = ToolCard {
            id: 8,
            api_call_id: 7,
            name: "Terminal.Interact".to_owned(),
            arguments: serde_json::json!({
                "session_id": session_id,
                "input": [
                    {"type": "text", "text": input},
                    {"type": "key", "key": "enter"}
                ]
            })
            .to_string(),
            started_at_ms: 1_000,
            queued: false,
            session_id: None,
            output: "first output line\nsecond output line with more content".to_owned(),
            result: Some(ToolCardResult {
                state: ToolResultState::Succeeded,
                exit_code: Some(0),
                detail: String::new(),
                finished_at_ms: 2_000,
            }),
        };
        let width = 60;
        let collapsed = tool_rows(&tool, width, false, 2_000);
        assert_eq!(collapsed.len(), 1);
        assert!(!collapsed[0].text.contains(['\r', '\n']));
        assert!(display_width(&collapsed[0].text) <= width);
        assert!(collapsed[0].text.starts_with("● Terminal.Interact pty-"));
        assert!(collapsed[0].text.ends_with('…'));

        let expanded = tool_rows(&tool, width, true, 2_000);
        assert!(expanded.len() > collapsed.len());
        let mut expanded_status = Vec::new();
        append_tool_status(
            &mut expanded_status,
            "Running with an intentionally long status message",
            32,
        );
        assert!(expanded_status.len() > 1);
    }

    #[test]
    fn apple_terminal_detail_rows_emit_the_complete_expanded_transcript() {
        let output = (0..80)
            .map(|line| format!("terminal output line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let projection = ChatProjection {
            messages: vec![ChatMessage {
                kind: ChatBlockKind::ToolCall,
                content: String::new(),
                timestamp_ms: 1_000,
                tool: Some(ToolCard {
                    id: 1,
                    api_call_id: 0,
                    name: "Terminal.Interact".to_owned(),
                    arguments: r#"{"session_id":"pty-1","input":[{"type":"text","text":"tree"},{"type":"key","key":"enter"}]}"#.to_owned(),
                    started_at_ms: 1_000,
                    queued: false,
                    session_id: Some("pty-1".to_owned()),
                    output,
                    result: Some(ToolCardResult {
                        state: ToolResultState::Succeeded,
                        exit_code: Some(0),
                        detail: String::new(),
                        finished_at_ms: 2_000,
                    }),
                }),
            }],
            ..ChatProjection::default()
        };

        let rows = detail_inline_rows(&projection, 80, 2_000);
        assert!(rows.len() > 80);
        assert!(
            rows.iter()
                .any(|row| row.text.contains("terminal output line 0"))
        );
        assert!(
            rows.iter()
                .any(|row| row.text.contains("terminal output line 79"))
        );
        assert_eq!(
            rows.get(rows.len() - 2),
            Some(&UiRow::new("", RowTone::Spacer))
        );
        assert_eq!(
            rows.last(),
            Some(&UiRow::new(
                " 工具详情：全部展开 · Ctrl+O 返回",
                RowTone::Status,
            ))
        );
    }

    #[test]
    fn live_indicators_style_tool_breathing_and_api_spinner() {
        assert_eq!(tool_marker_color(RowTone::ToolRunning), Color::White);
        assert_eq!(tool_marker_color(RowTone::ToolSucceeded), Color::Green);
        assert_eq!(tool_marker_color(RowTone::ToolFailed), Color::Red);

        let mut running = Vec::new();
        print_row(
            &mut running,
            &UiRow::new("● Terminal.Interact", RowTone::ToolRunning),
            80,
        )
        .unwrap();
        assert!(running.windows(4).any(|bytes| bytes == b"\x1b[1m"));
        assert!(!running.windows(5).any(|bytes| bytes == b"\x1b[22m"));
        assert!(!running.windows(4).any(|bytes| bytes == b"\x1b[5m"));

        assert_eq!(TOOL_DETAIL_COLOR, Color::DarkGrey);
        let mut detail = Vec::new();
        print_row(
            &mut detail,
            &UiRow::new("  ├ Output  : result", RowTone::ToolDetail),
            80,
        )
        .unwrap();
        assert!(!detail.windows(4).any(|bytes| bytes == b"\x1b[1m"));

        assert_eq!(SEPARATOR_COLOR, Color::DarkGrey);
        let mut separator = Vec::new();
        print_row(
            &mut separator,
            &UiRow::new("────────", RowTone::Separator),
            80,
        )
        .unwrap();
        assert!(!separator.windows(4).any(|bytes| bytes == b"\x1b[2m"));

        let mut bright = Vec::new();
        paint_breathing_markers(&mut bright, &[3], Color::White, Attribute::Bold).unwrap();
        let mut dim = Vec::new();
        paint_breathing_markers(&mut dim, &[3], Color::DarkGrey, Attribute::Dim).unwrap();
        assert_ne!(BREATHING_PHASES[0], BREATHING_PHASES[3]);
        assert_ne!(bright, dim);
        assert!(String::from_utf8(bright).unwrap().contains('●'));
        assert!(String::from_utf8(dim).unwrap().contains('●'));

        let clocks = [RunningToolClock {
            distance: 2,
            tool_name: "Terminal.Interact".to_owned(),
            started_at_ms: 1_000,
            timeout_ms: Some(3_000),
            width: 80,
        }];
        let mut first_clock = Vec::new();
        paint_running_tool_clocks(&mut first_clock, &clocks, 2_200).unwrap();
        let first_clock = String::from_utf8(first_clock).unwrap();
        assert!(first_clock.contains("Running ... 1.2s (timeout 3s)"));
        assert!(first_clock.contains("\u{1b}[2K"));
        let mut second_clock = Vec::new();
        paint_running_tool_clocks(&mut second_clock, &clocks, 3_900).unwrap();
        let second_clock = String::from_utf8(second_clock).unwrap();
        assert!(second_clock.contains("Running ... 2.9s (timeout 3s)"));
        assert_ne!(first_clock, second_clock);

        let mut spinner = Vec::new();
        paint_api_spinner(&mut spinner, API_SPINNER_FRAMES[3]).unwrap();
        let spinner = String::from_utf8(spinner).unwrap();
        assert!(spinner.contains(API_SPINNER_FRAMES[3]));
        assert_ne!(API_SPINNER_FRAMES[0], API_SPINNER_FRAMES[1]);
    }

    #[test]
    fn api_spinner_frame_is_driven_only_by_time() {
        assert_eq!(api_spinner_frame_at(0), API_SPINNER_FRAMES[0]);
        assert_eq!(api_spinner_frame_at(99), API_SPINNER_FRAMES[0]);
        assert_eq!(api_spinner_frame_at(100), API_SPINNER_FRAMES[1]);
        assert_eq!(api_spinner_frame_at(999), API_SPINNER_FRAMES[9]);
        assert_eq!(api_spinner_frame_at(1_000), API_SPINNER_FRAMES[0]);

        let frame = api_spinner_frame_at(400);
        for received_sse_events in [0, 1, 100, 100_000] {
            let text = main_status_text(
                Some(ApiState::Streaming),
                "main",
                "main-agent",
                "model",
                "unset",
                Some(12_345),
                Some(500_000),
                UiApiActivity {
                    active: true,
                    received_sse_events,
                },
                frame,
            );
            assert!(text.starts_with(&format!("{frame} me")));
        }
    }

    #[test]
    fn status_bar_has_no_background_and_uses_segment_colors() {
        let text = main_status_text(
            Some(ApiState::Completed),
            "main",
            "main-agent",
            "cometapi-deepseek-v4-flash",
            "high",
            Some(56_100),
            Some(500_000),
            UiApiActivity::default(),
            API_SPINNER_FRAMES[0],
        );
        let segments = main_status_segments(&text).unwrap();
        assert!(text.contains("cometapi-deepseek-v4-flash · high · 56.1k/500k"));
        assert!(!text.contains("effort high"));
        assert_eq!(
            segments
                .iter()
                .map(|(_, segment)| *segment)
                .collect::<String>(),
            text
        );
        for (expected_text, expected_color) in [
            (" me-s", STATUS_PRODUCT_COLOR),
            ("main", STATUS_ORCHESTRATOR_COLOR),
            ("main-agent", STATUS_ORCHESTRATOR_COLOR),
            ("cometapi-deepseek-v4-flash", STATUS_MODEL_COLOR),
            ("high", STATUS_MODEL_COLOR),
            ("56.1k/500k", STATUS_API_COLOR),
            ("Ctrl+O 工具详情", STATUS_HINT_COLOR),
            ("Esc 中止/撤回/清空", STATUS_HINT_COLOR),
        ] {
            assert!(
                segments
                    .iter()
                    .any(|(color, segment)| *color == expected_color && *segment == expected_text),
                "missing styled segment {expected_text:?}"
            );
        }
        assert!(!row_fills_background(RowTone::Status));
        assert!(row_fills_background(RowTone::User));
        assert!(row_fills_background(RowTone::UserPadding));
    }

    #[test]
    fn api_spinner_appears_only_while_the_api_is_active() {
        for state in [
            Some(ApiState::Requesting),
            Some(ApiState::Streaming),
            Some(ApiState::Retrying),
        ] {
            assert!(api_is_active(state));
            let text = main_status_text(
                state,
                "main",
                "main-agent",
                "model",
                "high",
                Some(12_345),
                Some(500_000),
                UiApiActivity {
                    active: true,
                    received_sse_events: 321,
                },
                API_SPINNER_FRAMES[4],
            );
            assert!(text.starts_with("⠼ me"));
            assert!(text.contains("↓ 321"));
            assert_eq!(
                main_status_segments(&text).unwrap()[0],
                (STATUS_API_COLOR, "⠼")
            );
        }
        for state in [
            None,
            Some(ApiState::Completed),
            Some(ApiState::Error),
            Some(ApiState::Interrupted),
        ] {
            assert!(!api_is_active(state));
            let text = main_status_text(
                state,
                "main",
                "main-agent",
                "model",
                "unset",
                None,
                Some(1_048_576),
                UiApiActivity::default(),
                API_SPINNER_FRAMES[4],
            );
            assert!(text.starts_with("  me"));
            assert!(text.contains("—/1048.6k"));
            assert_eq!(
                main_status_segments(&text).unwrap()[0],
                (STATUS_HINT_COLOR, " ")
            );
        }
    }

    #[test]
    fn context_usage_formats_provider_total_and_model_limit_in_k() {
        assert_eq!(
            format_context_usage(Some(56_100), Some(500_000)),
            "56.1k/500k"
        );
        assert_eq!(format_context_usage(Some(19), Some(272_000)), "0.0k/272k");
        assert_eq!(format_context_usage(None, Some(1_000_000)), "—/1000k");
        assert_eq!(format_context_usage(Some(14), None), "0.0k/—");
    }

    #[test]
    fn context_breakdown_matches_webui_categories_and_real_usage_total() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("model").unwrap();
        let first = edb.append_user_prompt("first request").unwrap();
        let first_api = edb.append_api_requesting(first).unwrap();
        let compact_call = edb
            .append_tool_call(first_api, first, "compact", "Compact", "{}")
            .unwrap();
        edb.append_tool_result(compact_call, ToolResultState::Succeeded, None, "ok")
            .unwrap();
        let compact = edb
            .append_compact_started(compact_call, first, CompactKind::MainAgentMultiTurn)
            .unwrap();
        let compact_stages = [
            "reviewed earlier context before producing the summary",
            "1. Primary Request and Intent\nintent",
            "2. Key Technical Context and Decisions\ndecisions",
            "3. Files, Code, and Artifacts\nfiles",
            "4. Problems, Investigations, and Resolutions\nproblems",
            "5. Current State and Continuation Plan\nnext",
        ];
        for (stage, content) in crate::event::CompactStage::MULTI_TURN
            .into_iter()
            .zip(compact_stages)
        {
            edb.append_compact_stage(compact, stage, content).unwrap();
        }
        let compact_summary =
            crate::compact::merge_multi_turn_summary(compact_stages.into_iter().skip(1));
        edb.append_compact_terminal(compact, CompactState::Completed, compact_summary, "")
            .unwrap();

        let second = edb.append_user_prompt("second request").unwrap();
        let tool_api = edb.append_api_requesting(second).unwrap();
        let file_call = edb
            .append_tool_call(tool_api, second, "read", "File.Read", r#"{"path":"a.txt"}"#)
            .unwrap();
        edb.append_api_state_with_usage(
            tool_api,
            second,
            ApiState::Completed,
            Some(ApiUsage {
                input_tokens: 30_000,
                output_tokens: 1_000,
                total_tokens: 31_000,
            }),
            "",
        )
        .unwrap();
        edb.append_tool_result(file_call, ToolResultState::Succeeded, None, "file contents")
            .unwrap();
        let final_api = edb.append_api_requesting(second).unwrap();
        edb.append_assist_response(second, "final answer", true)
            .unwrap();
        let usage = ApiUsage {
            input_tokens: 39_000,
            output_tokens: 1_000,
            total_tokens: 40_000,
        };
        let completed = edb
            .append_api_state_with_usage(final_api, second, ApiState::Completed, Some(usage), "")
            .unwrap();
        edb.append_context_usage_estimate(
            completed,
            crate::event::ContextTokenUsage {
                system: 20_000,
                compact: 4_000,
                memory: 3_000,
                user: 5_000,
                model: 4_000,
                tool: 4_000,
            },
        )
        .unwrap();

        let memory = turn_history::latest_snapshot(edb.events()).unwrap();
        let breakdown = estimate_context_breakdown(
            edb.events(),
            Some(usage),
            Some(100_000),
            10_000,
            memory,
            true,
        )
        .unwrap();
        assert_eq!(breakdown.total, Some(40_000));
        assert_eq!(breakdown.limit, Some(100_000));
        assert_eq!(breakdown.reserve, 10_000);
        assert_eq!(breakdown.values.sum(), 40_000);
        assert!(breakdown.values.system > 0);
        assert!(breakdown.values.compact > 0);
        assert!(breakdown.values.memory > 0);
        assert!(breakdown.values.user > 0);
        assert!(breakdown.values.model > 0);
        assert!(breakdown.values.tool > 0);
        assert!(
            breakdown
                .compact_content
                .as_deref()
                .unwrap()
                .contains("Primary Request")
        );
        assert_eq!(
            breakdown.compact_analysis.as_deref(),
            Some("reviewed earlier context before producing the summary")
        );
        let detail = compact_detail_content(&breakdown).unwrap();
        assert!(detail.starts_with("## Analysis\n\nreviewed earlier context"));
        assert!(detail.contains("## 压缩摘要\n\n1. Primary Request and Intent"));
        assert!(
            breakdown
                .memory_content
                .as_deref()
                .unwrap()
                .contains("first request")
        );
        assert_eq!(
            breakdown.actions(),
            vec![
                ContextAction::CompactDetail,
                ContextAction::MemoryDetail,
                ContextAction::Clear,
            ]
        );
        let bar = context_usage_bar_row(&breakdown, 80);
        assert_eq!(display_width(&bar.text), 80);
        assert_eq!(context_usage_summary(&breakdown), "40.0k / 100k  ·  40%");
    }

    #[test]
    fn context_breakdown_prefers_the_persisted_normalized_estimate() {
        let mut edb = EventDataBase::new();
        edb.append_initial_model("model").unwrap();
        let prompt = edb.append_user_prompt("tiny prompt").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let usage = ApiUsage {
            input_tokens: 9_000,
            output_tokens: 1_000,
            total_tokens: 10_000,
        };
        let completed = edb
            .append_api_state_with_usage(api, prompt, ApiState::Completed, Some(usage), "")
            .unwrap();
        let expected = crate::event::ContextTokenUsage {
            system: 6_000,
            compact: 0,
            memory: 0,
            user: 2_000,
            model: 1_000,
            tool: 1_000,
        };
        edb.append_context_usage_estimate(completed, expected)
            .unwrap();

        let breakdown =
            estimate_context_breakdown(edb.events(), Some(usage), Some(100_000), 0, None, true)
                .unwrap();
        assert_eq!(breakdown.values, expected.into());
        assert_eq!(breakdown.values.sum(), usage.total_tokens);
    }

    #[test]
    fn readonly_context_is_viewable_without_a_clear_action() {
        let breakdown = ContextUsageBreakdown {
            total: Some(20_000),
            limit: Some(100_000),
            reserve: 10_000,
            values: ContextTokenValues {
                system: 20_000,
                ..ContextTokenValues::default()
            },
            compact_content: Some("summary".into()),
            compact_analysis: Some("analysis".into()),
            memory_content: None,
            can_clear: false,
        };
        assert_eq!(breakdown.actions(), vec![ContextAction::CompactDetail]);
        let mut overlay = OverlayState::Context {
            breakdown: Box::new(breakdown),
            selected: 0,
        };
        let mut input = String::new();
        assert_eq!(
            handle_overlay_key(&mut overlay, &mut input, KeyCode::Enter),
            OverlayAction::OpenContextDetail(ContextAction::CompactDetail)
        );
    }

    #[test]
    fn chat_rows_style_user_and_assistant_messages() {
        let projection = ChatProjection {
            messages: vec![
                ChatMessage {
                    kind: ChatBlockKind::User,
                    content: "你好\n世界".to_owned(),
                    timestamp_ms: 1,
                    tool: None,
                },
                ChatMessage {
                    kind: ChatBlockKind::Assistant,
                    content: "你好。".to_owned(),
                    timestamp_ms: 2,
                    tool: None,
                },
            ],
            ..ChatProjection::default()
        };
        let rows = chat_rows(&projection, 40, false, 2);
        assert_eq!(
            rows.iter()
                .map(|row| (row.text.as_str(), row.tone))
                .collect::<Vec<_>>(),
            vec![
                ("", RowTone::UserPadding),
                (" 你好", RowTone::User),
                (" 世界", RowTone::User),
                ("", RowTone::UserPadding),
                ("", RowTone::Spacer),
                ("● 你好。", RowTone::MutedBulletHead),
            ]
        );
        assert!(rows.iter().all(|row| !row.text.contains("You")));
        assert!(rows.iter().all(|row| !row.text.contains("Assist")));
        let mut user = Vec::new();
        print_row(&mut user, &rows[1], 40).unwrap();
        assert!(!user.windows(4).any(|bytes| bytes == b"\x1b[1m"));
        assert_eq!(
            USER_BACKGROUND_COLOR,
            Color::Rgb {
                r: 35,
                g: 35,
                b: 38
            }
        );
        let mut assistant = Vec::new();
        print_row(&mut assistant, rows.last().unwrap(), 40).unwrap();
        assert!(assistant.windows(4).any(|bytes| bytes == b"\x1b[1m"));
        assert_eq!(MUTED_BULLET_COLOR, Color::Grey);
        assert_eq!(display_width("me 你好"), 7);
    }

    #[test]
    fn state_notices_and_different_visible_blocks_have_exactly_one_blank_line() {
        let projection = ChatProjection {
            messages: vec![
                ChatMessage {
                    kind: ChatBlockKind::Assistant,
                    content: "你的本机 IP 地址是 192.0.2.123".to_owned(),
                    timestamp_ms: 1,
                    tool: None,
                },
                ChatMessage {
                    kind: ChatBlockKind::SessionState,
                    content: "Session pty-16 lost".to_owned(),
                    timestamp_ms: 2,
                    tool: None,
                },
                ChatMessage {
                    kind: ChatBlockKind::StateNotice,
                    content: "模型已变更为 gpt-5.6-terra".to_owned(),
                    timestamp_ms: 3,
                    tool: None,
                },
                ChatMessage {
                    kind: ChatBlockKind::StateNotice,
                    content: "effort 已变更为 high".to_owned(),
                    timestamp_ms: 4,
                    tool: None,
                },
            ],
            ..ChatProjection::default()
        };

        let rows = chat_rows(&projection, 80, false, 4);
        assert_eq!(
            rows.iter()
                .map(|row| (row.text.as_str(), row.tone))
                .collect::<Vec<_>>(),
            vec![
                ("● 你的本机 IP 地址是 192.0.2.123", RowTone::MutedBulletHead,),
                ("", RowTone::Spacer),
                ("● Session pty-16 lost", RowTone::MutedBulletHead),
                ("", RowTone::Spacer),
                ("● 模型已变更为 gpt-5.6-terra", RowTone::MutedBulletHead,),
                ("", RowTone::Spacer),
                ("● effort 已变更为 high", RowTone::MutedBulletHead),
            ]
        );
    }

    #[test]
    fn consecutive_tool_calls_are_compact_but_other_block_boundaries_keep_their_gap() {
        let tool = ChatMessage {
            kind: ChatBlockKind::ToolCall,
            content: String::new(),
            timestamp_ms: 1,
            tool: None,
        };
        let notice = ChatMessage {
            kind: ChatBlockKind::StateNotice,
            content: "notice".into(),
            timestamp_ms: 2,
            tool: None,
        };

        assert!(!message_blocks_need_gap(&tool, &tool));
        assert!(message_blocks_need_gap(&tool, &notice));
        assert!(message_blocks_need_gap(&notice, &tool));
    }

    #[test]
    fn assistant_markdown_uses_agent_markdown_renderer() {
        let markdown = "# Heading\n\n**bold** and `inline code`\n\n| A | B |\n|---|---|";
        let projection = ChatProjection {
            messages: vec![ChatMessage {
                kind: ChatBlockKind::Assistant,
                content: markdown.to_owned(),
                timestamp_ms: 1,
                tool: None,
            }],
            ..ChatProjection::default()
        };
        let rows = chat_rows(&projection, 80, false, 1);

        let visible = rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>();
        assert_eq!(visible[0], "● Heading");
        assert!(visible.contains(&"  bold and inline code"));
        assert!(visible.iter().any(|line| line.starts_with("  ┌")));
        assert!(visible.iter().any(|line| line.starts_with("  └")));
        assert!(visible.iter().all(|line| {
            ["**", "`", "|---|", "# Heading"]
                .into_iter()
                .all(|marker| !line.contains(marker))
        }));
        assert!(rows.iter().all(|row| display_width(&row.text) <= 80));
        assert!(
            rows.iter()
                .filter_map(|row| row.markdown_spans.as_ref())
                .flatten()
                .any(|span| span.text == "bold" && span.style.bold)
        );
        assert!(
            rows.iter()
                .filter_map(|row| row.markdown_spans.as_ref())
                .flatten()
                .any(|span| {
                    span.text == "inline code" && span.style.color == MarkdownColorRole::Code
                })
        );
        assert!(rows.iter().all(|row| !row.text.contains('\u{1b}')));

        let mut painted = Vec::new();
        for row in &rows {
            print_row(&mut painted, row, 80).unwrap();
        }
        let painted = String::from_utf8(painted).unwrap();
        assert!(!painted.contains("\u{1b}[48;"));
    }

    #[test]
    fn assistant_cjk_sentence_emits_terminal_bold_style() {
        let content = "一句话：**别生吃、别半生不熟，炖熟煮透就完全可以放心吃。**";
        let projection = ChatProjection {
            messages: vec![ChatMessage {
                kind: ChatBlockKind::Assistant,
                content: content.to_owned(),
                timestamp_ms: 1,
                tool: None,
            }],
            ..ChatProjection::default()
        };
        let rows = chat_rows(&projection, 120, false, 1);
        let spans = rows[0].markdown_spans.as_ref().unwrap();
        assert!(spans.iter().any(|span| {
            span.text == "别生吃、别半生不熟，炖熟煮透就完全可以放心吃。" && span.style.bold
        }));

        let mut painted = Vec::new();
        print_row(&mut painted, &rows[0], 120).unwrap();
        let painted = String::from_utf8(painted).unwrap();
        assert!(painted.contains("\u{1b}[1m"));
        assert!(!painted.contains("**"));
    }

    #[test]
    fn syntax_colors_follow_the_vscode_dark_plus_palette() {
        for (role, rgb) in [
            (MarkdownColorRole::SyntaxComment, (106, 153, 85)),
            (MarkdownColorRole::SyntaxString, (206, 145, 120)),
            (MarkdownColorRole::SyntaxKeyword, (197, 134, 192)),
            (MarkdownColorRole::SyntaxDeclaration, (86, 156, 214)),
            (MarkdownColorRole::SyntaxNumber, (181, 206, 168)),
            (MarkdownColorRole::SyntaxType, (78, 201, 176)),
            (MarkdownColorRole::SyntaxFunction, (220, 220, 170)),
            (MarkdownColorRole::SyntaxVariable, (156, 220, 254)),
            (MarkdownColorRole::SyntaxConstant, (86, 156, 214)),
        ] {
            assert_eq!(
                markdown_color(role),
                Color::Rgb {
                    r: rgb.0,
                    g: rgb.1,
                    b: rgb.2
                }
            );
        }
    }

    #[test]
    fn streamed_markdown_is_rendered_from_accumulated_projection_source() {
        let mut edb = EventDataBase::new();
        let prompt_id = edb.append_user_prompt("stream").unwrap();
        edb.append_assist_response(prompt_id, "**hel", false)
            .unwrap();
        let mut projection = ChatProjection::default();
        projection.consume(&edb).unwrap();

        edb.append_assist_response(prompt_id, "lo**", true).unwrap();
        projection.consume(&edb).unwrap();
        let rows = chat_rows(&projection, 40, false, 2);

        assert_eq!(projection.messages[1].content, "**hello**");
        assert_eq!(
            edb.events()
                .iter()
                .filter_map(|event| match event {
                    Event::AssistResponse(response) => Some(response.content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["**hel", "lo**"]
        );
        let assistant = rows
            .iter()
            .find(|row| row.tone == RowTone::MutedBulletHead)
            .unwrap();
        assert_eq!(assistant.text, "● hello");
        assert!(
            assistant
                .markdown_spans
                .as_ref()
                .unwrap()
                .iter()
                .any(|span| span.text == "hello" && span.style.bold)
        );
    }

    #[test]
    fn tool_card_to_assistant_has_one_blank_line_around_full_width_separator() {
        let projection = ChatProjection {
            messages: vec![
                ChatMessage {
                    kind: ChatBlockKind::ToolCall,
                    content: String::new(),
                    timestamp_ms: 1,
                    tool: Some(ToolCard {
                        id: 1,
                        api_call_id: 0,
                        name: "Terminal.Status".to_owned(),
                        arguments: "{}".to_owned(),
                        started_at_ms: 1,
                        queued: false,
                        session_id: Some("pty-1".to_owned()),
                        output: String::new(),
                        result: Some(ToolCardResult {
                            state: ToolResultState::Succeeded,
                            exit_code: Some(0),
                            detail: "done".to_owned(),
                            finished_at_ms: 2,
                        }),
                    }),
                },
                ChatMessage {
                    kind: ChatBlockKind::Assistant,
                    content: "continued".to_owned(),
                    timestamp_ms: 3,
                    tool: None,
                },
            ],
            ..ChatProjection::default()
        };

        let width = 24;
        let rows = chat_rows(&projection, width, false, 3);
        let separator = rows
            .iter()
            .position(|row| row.tone == RowTone::Separator)
            .unwrap();

        assert_eq!(rows[separator].text, "─".repeat(width));
        assert_eq!(display_width(&rows[separator].text), width);
        assert_eq!(rows[separator - 1], UiRow::new("", RowTone::Spacer));
        assert_ne!(rows[separator - 2].tone, RowTone::Spacer);
        assert_eq!(rows[separator + 1], UiRow::new("", RowTone::Spacer));
        assert_eq!(rows[separator + 2].text, "● continued");
        assert_eq!(rows[separator + 2].tone, RowTone::MutedBulletHead);
    }

    #[test]
    fn trims_model_boundary_newlines_but_preserves_internal_newlines() {
        let projection = ChatProjection {
            messages: vec![
                ChatMessage {
                    kind: ChatBlockKind::Assistant,
                    content: "\r\nbefore\n\nmiddle\n\n\r".to_owned(),
                    timestamp_ms: 1,
                    tool: None,
                },
                ChatMessage {
                    kind: ChatBlockKind::Assistant,
                    content: "after".to_owned(),
                    timestamp_ms: 2,
                    tool: None,
                },
            ],
            ..ChatProjection::default()
        };
        let rows = chat_rows(&projection, 40, false, 2);
        assert_eq!(
            rows.iter()
                .map(|row| (row.text.as_str(), row.tone))
                .collect::<Vec<_>>(),
            vec![
                ("● before", RowTone::MutedBulletHead),
                ("  ", RowTone::Assistant),
                ("  middle", RowTone::Assistant),
                ("● after", RowTone::MutedBulletHead),
            ]
        );
        assert_eq!(trim_model_boundary_newlines("\r\ncontent\n\r"), "content");
        assert_eq!(wrap("line\n", 40), vec!["line"]);
        assert_eq!(wrap("line\n\n", 40), vec!["line", ""]);
        assert_eq!(wrap("\n\n", 40), vec!["", ""]);
    }

    #[test]
    fn transcript_diff_keeps_the_unchanged_prefix() {
        let first = vec![
            UiRow::new("one", RowTone::Assistant),
            UiRow::new("two", RowTone::Assistant),
        ];
        let appended = vec![
            UiRow::new("one", RowTone::Assistant),
            UiRow::new("two", RowTone::Assistant),
            UiRow::new("three", RowTone::Assistant),
        ];
        let changed = vec![
            UiRow::new("one", RowTone::Assistant),
            UiRow::new("changed", RowTone::Assistant),
        ];
        assert_eq!(common_row_prefix(&first, &appended), 2);
        assert_eq!(common_row_prefix(&first, &changed), 1);

        let plain = UiRow::markdown(
            vec![MarkdownSpan::new("same", MarkdownTextStyle::default())],
            RowTone::Assistant,
        );
        let bold = UiRow::markdown(
            vec![MarkdownSpan::new(
                "same",
                MarkdownTextStyle::default().bold(),
            )],
            RowTone::Assistant,
        );
        assert_eq!(plain.text, bold.text);
        assert_eq!(common_row_prefix(&[plain], &[bold]), 0);
    }

    #[test]
    fn expanded_mode_applies_to_every_tool_card() {
        let card = |id, output: &str| ChatMessage {
            kind: ChatBlockKind::ToolCall,
            content: String::new(),
            timestamp_ms: id,
            tool: Some(ToolCard {
                id,
                api_call_id: id.saturating_sub(1),
                name: "Terminal.Interact".to_owned(),
                arguments: "{}".to_owned(),
                started_at_ms: id,
                queued: false,
                session_id: None,
                output: output.to_owned(),
                result: Some(ToolCardResult {
                    state: ToolResultState::Succeeded,
                    exit_code: Some(0),
                    detail: String::new(),
                    finished_at_ms: id + 1,
                }),
            }),
        };
        let projection = ChatProjection {
            messages: vec![
                card(1, "one\ntwo"),
                ChatMessage {
                    kind: ChatBlockKind::Assistant,
                    content: "\r\n \n".to_owned(),
                    timestamp_ms: 2,
                    tool: None,
                },
                card(3, "three\nfour"),
            ],
            ..ChatProjection::default()
        };

        let collapsed = chat_rows(&projection, 80, false, 10);
        assert!(
            collapsed
                .iter()
                .all(|row| !matches!(row.tone, RowTone::Assistant | RowTone::MutedBulletHead))
        );
        assert_eq!(
            collapsed
                .iter()
                .filter(|row| row.tone == RowTone::Spacer)
                .count(),
            0
        );
        assert_eq!(
            collapsed
                .iter()
                .filter(|row| row.tone == RowTone::ToolSucceeded)
                .count(),
            2
        );
        assert!(collapsed.iter().all(|row| !row.text.contains("Output")));

        let expanded = chat_rows(&projection, 80, true, 10);
        let expanded = expanded
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("Output : one"));
        assert!(expanded.contains("│          two"));
        assert!(expanded.contains("Output : three"));
        assert!(expanded.contains("│          four"));
    }
}
