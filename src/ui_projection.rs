use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    Result,
    event::{
        AgentTurnState, ApiState, ApiUsage, CompactStage, CompactState, ContextTokenUsage, Event,
        EventBase, EventId, SystemStaticPromptMode, ToolInfoContent, effective_conversation_events,
        effective_ui_events, latest_context_usage_event,
    },
    ui_backend::{ChatToolPresentation, UiAgentSnapshot, UiSnapshot, tool_chat_presentation},
    workmap::{WorkMapMemorySnapshot, WorkMapObjectiveSnapshot, WorkMapProjection},
    workspace::AgentId,
};

const TRANSITION_LIMIT: usize = 1024;
const PROJECTION_SCHEMA: &str = "ui-projection-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiProjectionQueryError {
    AgentNotFound(String),
    StaleRevision {
        expected: String,
        current: String,
    },
    InvalidRange {
        start: usize,
        end: usize,
        count: usize,
    },
}

impl std::fmt::Display for UiProjectionQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentNotFound(agent_id) => {
                write!(formatter, "Agent {agent_id} does not exist")
            }
            Self::StaleRevision { expected, current } => write!(
                formatter,
                "UI projection revision changed: expected {expected}, current {current}"
            ),
            Self::InvalidRange { start, end, count } => write!(
                formatter,
                "invalid UI projection range [{start},{end}) for {count} items"
            ),
        }
    }
}

impl std::error::Error for UiProjectionQueryError {}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UiProjectionCursor {
    pub agent_id: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub window: Option<UiProjectionWindow>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UiProjectionWindow {
    pub start: usize,
    pub end: usize,
    pub count: usize,
    pub follow_tail: bool,
}

impl UiProjectionWindow {
    fn range(&self, state: &UiProjectionState, known: Option<&str>) -> Option<(usize, usize)> {
        let count = state.count;
        let tail = || Some((count.saturating_sub(64), count));
        if self.start > self.end || self.end > self.count || self.end - self.start > 192 {
            return tail();
        }
        if count == 0 {
            return None;
        }
        if known.is_none()
            || self.start == self.end
            || self.start >= count
            || (self.follow_tail && self.end < count)
        {
            return tail();
        }
        if known == Some(state.revision.as_str()) {
            return None;
        }
        let Some(changed) = state.changed_from else {
            return if self.end > count { tail() } else { None };
        };
        if self.end == self.count {
            let start = changed.min(count);
            return if start >= self.start && count - start <= 192 {
                Some((start, count))
            } else {
                tail()
            };
        }
        if changed >= self.end && self.end <= count {
            return None;
        }
        let start = self.start.min(count);
        let end = start
            .saturating_add((self.end - self.start).clamp(64, 192))
            .min(count);
        Some((changed.clamp(start, end), end))
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct UiProjectionState {
    pub agent_id: String,
    pub edb_id: String,
    pub revision: String,
    pub source_event_count: usize,
    pub source_mutation_revision: u64,
    pub last_event_id: Option<EventId>,
    pub count: usize,
    pub changed_from: Option<usize>,
    pub summary: UiProjectionSummary,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub api_state: Option<ApiState>,
    pub api_usage: Option<ApiUsage>,
    pub turn_state: Option<UiTurnState>,
    pub workmap: UiWorkMapProjection,
    pub context: UiContextProjection,
    pub system_prompt: UiSystemPromptProjection,
    pub compact_activity: Option<UiCompactActivityProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<UiProjectionRange>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct UiProjectionRange {
    pub agent_id: String,
    pub revision: String,
    pub start: usize,
    pub end: usize,
    pub count: usize,
    pub hash: String,
    pub projections: Vec<UiPartProjection>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UiProjectionHash {
    pub agent_id: String,
    pub revision: String,
    pub start: usize,
    pub end: usize,
    pub count: usize,
    pub hash: String,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct UiProjectionSummary {
    pub turn_state: Option<AgentTurnState>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UiTurnState {
    pub state: String,
    pub prompt_id: EventId,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct UiContextProjection {
    pub total: Option<u64>,
    pub values: ContextTokenUsage,
    pub compact_content: Option<String>,
    pub compact_analysis: Option<String>,
    pub memory_content: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UiSystemPromptChangeProjection {
    pub id: EventId,
    pub mode: SystemStaticPromptMode,
    pub content: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UiSystemPromptProjection {
    pub mode: SystemStaticPromptMode,
    pub content: Option<String>,
    pub event_id: Option<EventId>,
    pub changes: Vec<UiSystemPromptChangeProjection>,
}

impl Default for UiSystemPromptProjection {
    fn default() -> Self {
        Self {
            mode: SystemStaticPromptMode::Default,
            content: None,
            event_id: None,
            changes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct UiWorkMapProjection {
    pub memory: WorkMapMemorySnapshot,
    pub history: Vec<WorkMapObjectiveSnapshot>,
    pub current: Option<WorkMapObjectiveSnapshot>,
    #[serde(rename = "recordCount")]
    pub record_count: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UiCompactActivityProjection {
    pub compact_id: EventId,
    pub kind: String,
    pub total_stages: usize,
    pub stage: usize,
    pub message_key: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum UiPartProjection {
    #[serde(rename = "user")]
    User {
        key: String,
        revision: EventId,
        content: String,
        timestamp: u64,
        #[serde(rename = "eventId")]
        event_id: EventId,
        rewindable: bool,
    },
    #[serde(rename = "assistant")]
    Assistant {
        key: String,
        revision: EventId,
        content: String,
        timestamp: u64,
    },
    #[serde(rename = "notice")]
    Notice {
        key: String,
        revision: EventId,
        content: String,
        timestamp: u64,
    },
    #[serde(rename = "session")]
    Session {
        key: String,
        revision: EventId,
        content: String,
        timestamp: u64,
    },
    #[serde(rename = "turn-toolbar")]
    TurnToolbar {
        key: String,
        revision: EventId,
        timestamp: u64,
        #[serde(rename = "finalAnswerEventId")]
        final_answer_event_id: EventId,
        #[serde(rename = "promptId")]
        prompt_id: EventId,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
        #[serde(rename = "tokenCount")]
        token_count: Option<u64>,
    },
    #[serde(rename = "tool")]
    Tool {
        key: String,
        revision: EventId,
        timestamp: u64,
        tool: UiToolProjection,
    },
    #[serde(rename = "worker-activity")]
    WorkerActivity {
        key: String,
        revision: EventId,
        timestamp: u64,
        tool: UiToolProjection,
    },
}

impl UiPartProjection {
    pub fn key(&self) -> &str {
        match self {
            Self::User { key, .. }
            | Self::Assistant { key, .. }
            | Self::Notice { key, .. }
            | Self::Session { key, .. }
            | Self::TurnToolbar { key, .. }
            | Self::Tool { key, .. }
            | Self::WorkerActivity { key, .. } => key,
        }
    }

    fn tool_mut(&mut self) -> Option<&mut UiToolProjection> {
        match self {
            Self::Tool { tool, .. } | Self::WorkerActivity { tool, .. } => Some(tool),
            _ => None,
        }
    }

    fn tool(&self) -> Option<&UiToolProjection> {
        match self {
            Self::Tool { tool, .. } | Self::WorkerActivity { tool, .. } => Some(tool),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UiToolProjection {
    pub id: EventId,
    pub api_call_id: EventId,
    pub name: String,
    pub arguments: String,
    pub args: Value,
    pub started: u64,
    pub queued: bool,
    pub session_id: Option<String>,
    pub output: String,
    pub updates: Vec<ToolInfoContent>,
    pub result: Option<UiToolResultProjection>,
    pub revision: EventId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<UiWorkerActivityProjection>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiToolResultProjection {
    pub state: crate::event::ToolResultState,
    pub exit_code: Option<i32>,
    pub detail: String,
    pub finished: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct UiWorkerActivityProjection {
    pub state: String,
    pub tools: Vec<UiToolProjection>,
    pub revision: EventId,
}

#[derive(Default)]
pub struct UiProjectionCache {
    stores: HashMap<AgentId, UiProjectionStore>,
}

impl UiProjectionCache {
    pub fn states(
        &mut self,
        snapshot: &UiSnapshot,
        cursors: &[UiProjectionCursor],
    ) -> Result<Vec<UiProjectionState>> {
        let visible = snapshot.agent_ids().into_iter().collect::<BTreeSet<_>>();
        self.stores.retain(|id, _| visible.contains(id));
        let cursors = cursors
            .iter()
            .map(|cursor| (cursor.agent_id.as_str(), cursor))
            .collect::<HashMap<_, _>>();
        let mut states = Vec::with_capacity(snapshot.agents.len());
        for agent in &snapshot.agents {
            let cursor = cursors.get(agent.id.as_str());
            let known = cursor.and_then(|cursor| cursor.revision.as_deref());
            let store = self.synchronize(snapshot, &agent.id)?;
            let mut state = store.state(agent, known);
            if let Some(window) = cursor.and_then(|cursor| cursor.window.as_ref()) {
                if let Some((start, end)) = window.range(&state, known) {
                    state.range = Some(store.range(agent, start, end)?);
                }
            }
            states.push(state);
        }
        Ok(states)
    }

    pub fn range(
        &mut self,
        snapshot: &UiSnapshot,
        agent_id: &AgentId,
        start: usize,
        end: usize,
        expected_revision: &str,
    ) -> Result<UiProjectionRange> {
        let agent = snapshot
            .agent(agent_id)
            .ok_or_else(|| UiProjectionQueryError::AgentNotFound(agent_id.to_string()))?;
        let store = self.synchronize(snapshot, agent_id)?;
        store.require_revision(expected_revision)?;
        store.range(agent, start, end)
    }

    pub fn hash(
        &mut self,
        snapshot: &UiSnapshot,
        agent_id: &AgentId,
        start: usize,
        end: usize,
        expected_revision: &str,
    ) -> Result<UiProjectionHash> {
        let range = self.range(snapshot, agent_id, start, end, expected_revision)?;
        Ok(UiProjectionHash {
            agent_id: range.agent_id,
            revision: range.revision,
            start: range.start,
            end: range.end,
            count: range.count,
            hash: range.hash,
        })
    }

    fn synchronize<'a>(
        &'a mut self,
        snapshot: &UiSnapshot,
        agent_id: &AgentId,
    ) -> Result<&'a UiProjectionStore> {
        let agent = snapshot
            .agent(agent_id)
            .ok_or_else(|| UiProjectionQueryError::AgentNotFound(agent_id.to_string()))?;
        let worker = snapshot.agents.iter().find(|candidate| {
            candidate.orchestrator_name == "worker-agent"
                && candidate.parent_agent_id.as_ref() == Some(agent_id)
        });
        match self.stores.get_mut(agent_id) {
            Some(store) => store.synchronize(agent, worker)?,
            None => {
                self.stores
                    .insert(agent_id.clone(), UiProjectionStore::new(agent, worker)?);
            }
        }
        Ok(self.stores.get(agent_id).expect("projection store exists"))
    }
}

struct UiProjectionTransition {
    from: String,
    to: String,
    changed_from: Option<usize>,
}

struct UiProjectionStore {
    revision: String,
    mutation_revision: u64,
    event_count: usize,
    last_event_id: Option<EventId>,
    last_event_hash: Option<String>,
    data: ProjectionData,
    transitions: VecDeque<UiProjectionTransition>,
}

impl UiProjectionStore {
    fn new(agent: &UiAgentSnapshot, worker: Option<&UiAgentSnapshot>) -> Result<Self> {
        let mut data = ProjectionData::from_events(&agent.events)?;
        data.enrich_worker_activities(worker.map(|worker| worker.events.as_ref()));
        Ok(Self {
            revision: projection_revision(agent, worker),
            mutation_revision: agent.mutation_revision,
            event_count: agent.events.len(),
            last_event_id: agent.events.last().map(Event::id),
            last_event_hash: agent.events.last().map(EventBase::getHash),
            data,
            transitions: VecDeque::new(),
        })
    }

    fn synchronize(
        &mut self,
        agent: &UiAgentSnapshot,
        worker: Option<&UiAgentSnapshot>,
    ) -> Result<()> {
        let revision = projection_revision(agent, worker);
        if revision == self.revision {
            return Ok(());
        }
        let previous_revision = self.revision.clone();
        let prefix_valid = self.mutation_revision == agent.mutation_revision
            && self.event_count <= agent.events.len()
            && (self.event_count == 0
                || agent
                    .events
                    .get(self.event_count - 1)
                    .map(EventBase::getHash)
                    .as_ref()
                    == self.last_event_hash.as_ref());
        let appended = prefix_valid.then(|| &agent.events[self.event_count..]);
        let requires_replay = appended.is_none_or(|events| {
            events.iter().any(|event| {
                matches!(event, Event::ContextCleared(_))
                    || matches!(event, Event::CompactStateUpdate(update) if update.state == CompactState::Completed)
                    || matches!(event, Event::ApiStateUpdate(update) if update.state == ApiState::Error)
            })
        });
        let mut changed_from = if requires_replay {
            self.data = ProjectionData::from_events(&agent.events)?;
            (!self.data.messages.is_empty()).then_some(0)
        } else {
            let mut changed = None;
            let mut context_dirty = false;
            for event in appended.expect("validated append suffix") {
                changed = min_changed(changed, self.data.project_event(event)?);
                self.data.observe_physical_event(event);
                context_dirty |= context_event(event);
            }
            if context_dirty {
                self.data.context = context_projection(&agent.events, self.data.api_usage)?;
            }
            changed
        };
        changed_from = min_changed(
            changed_from,
            self.data
                .enrich_worker_activities(worker.map(|worker| worker.events.as_ref())),
        );
        self.revision = revision;
        self.mutation_revision = agent.mutation_revision;
        self.event_count = agent.events.len();
        self.last_event_id = agent.events.last().map(Event::id);
        self.last_event_hash = agent.events.last().map(EventBase::getHash);
        self.transitions.push_back(UiProjectionTransition {
            from: previous_revision,
            to: self.revision.clone(),
            changed_from,
        });
        while self.transitions.len() > TRANSITION_LIMIT {
            self.transitions.pop_front();
        }
        Ok(())
    }

    fn state(&self, agent: &UiAgentSnapshot, known: Option<&str>) -> UiProjectionState {
        UiProjectionState {
            range: None,
            agent_id: agent.id.to_string(),
            edb_id: agent.edb_id.clone(),
            revision: self.revision.clone(),
            source_event_count: self.event_count,
            source_mutation_revision: self.mutation_revision,
            last_event_id: self.last_event_id,
            count: self.data.messages.len(),
            changed_from: self.changed_from(known),
            summary: self.data.summary.clone(),
            model: self.data.model.clone(),
            effort: self.data.effort.clone(),
            api_state: self.data.api_state,
            api_usage: self.data.api_usage,
            turn_state: self.data.turn_state(),
            workmap: self.data.workmap_projection(),
            context: self.data.context.clone(),
            system_prompt: self.data.system_prompt.clone(),
            compact_activity: self.data.compact_activity.as_ref().map(|activity| {
                UiCompactActivityProjection {
                    compact_id: activity.compact_id,
                    kind: activity.kind.clone(),
                    total_stages: activity.total_stages,
                    stage: activity.stage,
                    message_key: self.data.messages[activity.message_index].key().to_owned(),
                }
            }),
        }
    }

    fn changed_from(&self, known: Option<&str>) -> Option<usize> {
        let Some(mut revision) = known else {
            return (!self.data.messages.is_empty()).then_some(0);
        };
        if revision == self.revision {
            return None;
        }
        let mut changed = None;
        for _ in 0..=self.transitions.len() {
            let Some(transition) = self.transitions.iter().find(|item| item.from == revision)
            else {
                return (!self.data.messages.is_empty()).then_some(0);
            };
            changed = min_changed(changed, transition.changed_from);
            revision = &transition.to;
            if revision == self.revision {
                return changed;
            }
        }
        (!self.data.messages.is_empty()).then_some(0)
    }

    fn require_revision(&self, expected: &str) -> Result<()> {
        if expected == self.revision {
            Ok(())
        } else {
            Err(UiProjectionQueryError::StaleRevision {
                expected: expected.to_owned(),
                current: self.revision.clone(),
            }
            .into())
        }
    }

    fn range(
        &self,
        agent: &UiAgentSnapshot,
        start: usize,
        end: usize,
    ) -> Result<UiProjectionRange> {
        if start > end || end > self.data.messages.len() {
            return Err(UiProjectionQueryError::InvalidRange {
                start,
                end,
                count: self.data.messages.len(),
            }
            .into());
        }
        let projections = self.data.messages[start..end].to_vec();
        let hash = projection_hash(&projections)?;
        Ok(UiProjectionRange {
            agent_id: agent.id.to_string(),
            revision: self.revision.clone(),
            start,
            end,
            count: self.data.messages.len(),
            hash,
            projections,
        })
    }
}

#[derive(Default)]
struct ProjectionData {
    messages: Vec<UiPartProjection>,
    api_state: Option<ApiState>,
    api_usage: Option<ApiUsage>,
    model: Option<String>,
    effort: Option<String>,
    summary: UiProjectionSummary,
    workmap: WorkMapProjection,
    context: UiContextProjection,
    system_prompt: UiSystemPromptProjection,
    active_assistant: Option<(EventId, usize)>,
    active_tools: BTreeMap<EventId, usize>,
    turn_started_at: BTreeMap<EventId, u64>,
    turn_context_baseline: BTreeMap<EventId, Option<u64>>,
    last_assistant_by_prompt: BTreeMap<EventId, usize>,
    completed_api_usage: BTreeMap<EventId, (EventId, Option<ApiUsage>)>,
    errored_api_calls: BTreeSet<EventId>,
    completed_compact_tools: BTreeSet<EventId>,
    hidden_tools: BTreeSet<EventId>,
    compact_activity: Option<CompactActivity>,
    turn: Option<TurnTracker>,
}

struct CompactActivity {
    compact_id: EventId,
    kind: String,
    total_stages: usize,
    stage: usize,
    message_index: usize,
}

struct TurnTracker {
    prompt_id: EventId,
    aborted: bool,
    terminal: bool,
    api_states: BTreeMap<EventId, ApiState>,
    open_tools: BTreeSet<EventId>,
    latest_api_call_id: Option<EventId>,
    latest_api_has_tool: bool,
    latest_api_has_final: bool,
}

impl ProjectionData {
    fn from_events(events: &[Event]) -> Result<Self> {
        let effective = effective_ui_events(events)?;
        let mut projection = Self {
            completed_compact_tools: effective
                .iter()
                .filter_map(|event| match event {
                    Event::CompactStateUpdate(update)
                        if update.state == CompactState::Completed =>
                    {
                        Some(update.tool_call_id)
                    }
                    _ => None,
                })
                .collect(),
            hidden_tools: effective
                .iter()
                .filter_map(|event| match event {
                    Event::ToolCall(call)
                        if tool_chat_presentation(&call.name) == ChatToolPresentation::Hidden =>
                    {
                        Some(call.id)
                    }
                    _ => None,
                })
                .collect(),
            ..Default::default()
        };
        for event in effective {
            projection.project_event(event)?;
        }
        projection.model = projection.model.or_else(|| {
            events.iter().rev().find_map(|event| match event {
                Event::ModelChanged(changed) => Some(changed.model.clone()),
                _ => None,
            })
        });
        projection.effort = projection.effort.or_else(|| {
            events.iter().rev().find_map(|event| match event {
                Event::ReasoningEffortChanged(changed) => Some(changed.effort.clone()),
                _ => None,
            })
        });
        projection.summary = agent_summary(events);
        projection.system_prompt = system_prompt_projection(events);
        projection.context = context_projection(events, projection.api_usage)?;
        Ok(projection)
    }

    fn project_event(&mut self, event: &Event) -> Result<Option<usize>> {
        let changed = match event {
            Event::EdbIdGeneration(_)
            | Event::AgentKindDef(_)
            | Event::SystemPrompt(_)
            | Event::ModelContextItem(_)
            | Event::WorkMapPendingReminder(_)
            | Event::AgentTitleChanged(_)
            | Event::ImageContent(_)
            | Event::ContextUsageEstimate(_) => None,
            Event::WorkMapMutation(update) => {
                self.workmap.apply(update)?;
                None
            }
            Event::AgentTurn(turn) => {
                let mut changed = None;
                if turn.state == AgentTurnState::Completed
                    && let Some(started) = self.turn_started_at.get(&turn.prompt_id).copied()
                    && let Some(index) = self.last_assistant_by_prompt.get(&turn.prompt_id).copied()
                    && index + 1 == self.messages.len()
                    && self
                        .messages
                        .get(index)
                        .is_some_and(message_is_renderable_assistant)
                {
                    let token_count = completed_turn_context_growth(
                        &self.completed_api_usage,
                        turn.prompt_id,
                        self.turn_context_baseline
                            .get(&turn.prompt_id)
                            .copied()
                            .flatten(),
                    );
                    changed = Some(self.append(UiPartProjection::TurnToolbar {
                        key: format!("turn-toolbar:{}", turn.turn_id),
                        revision: turn.id,
                        timestamp: turn.timestamp_ms,
                        final_answer_event_id: turn.id,
                        prompt_id: turn.prompt_id,
                        duration_ms: turn.timestamp_ms.saturating_sub(started),
                        token_count,
                    }));
                }
                self.finish_turn(turn.prompt_id, turn.state);
                changed
            }
            Event::ModelChanged(changed) => {
                self.model = Some(changed.model.clone());
                self.api_usage = None;
                (changed.cause != crate::event::ModelChangeCause::Initial).then(|| {
                    self.add_notice(
                        format!("模型已变更为 {}", changed.model),
                        changed.id,
                        changed.timestamp_ms,
                    )
                })
            }
            Event::ReasoningEffortChanged(changed) => {
                self.effort = Some(changed.effort.clone());
                (changed.cause != crate::event::ReasoningEffortChangeCause::Initial).then(|| {
                    let content = if changed.cause
                        == crate::event::ReasoningEffortChangeCause::ModelUnsupported
                    {
                        "思考强度不支持，已退回 unset".to_owned()
                    } else {
                        format!("effort 已变更为 {}", changed.effort)
                    };
                    self.add_notice(content, changed.id, changed.timestamp_ms)
                })
            }
            Event::UserPrompt(prompt) => {
                self.begin_turn(prompt.id);
                self.turn_started_at.insert(prompt.id, prompt.timestamp_ms);
                self.turn_context_baseline
                    .insert(prompt.id, self.api_usage.map(|usage| usage.total_tokens));
                let index = self.append(UiPartProjection::User {
                    key: format!("user:{}", prompt.id),
                    revision: prompt.id,
                    content: prompt.content.clone(),
                    timestamp: prompt.timestamp_ms,
                    event_id: prompt.id,
                    rewindable: true,
                });
                self.active_assistant = None;
                Some(index)
            }
            Event::ManagerPrompt(prompt) => {
                Some(self.add_agent_prompt(prompt.id, prompt.timestamp_ms, &prompt.content))
            }
            Event::ParentAgentPrompt(prompt) => {
                Some(self.add_agent_prompt(prompt.id, prompt.timestamp_ms, &prompt.content))
            }
            Event::FollowUpPrompt(prompt) => {
                let index = self.append(UiPartProjection::User {
                    key: format!("user:{}", prompt.id),
                    revision: prompt.id,
                    content: prompt.content.clone(),
                    timestamp: prompt.timestamp_ms,
                    event_id: prompt.id,
                    rewindable: false,
                });
                self.active_assistant = None;
                Some(index)
            }
            Event::AssistResponse(response) => {
                let mut changed = None;
                if !response.content.is_empty() {
                    let index = match self.active_assistant {
                        Some((prompt_id, index)) if prompt_id == response.prompt_id => index,
                        _ => {
                            let index = self.append(UiPartProjection::Assistant {
                                key: format!("assistant:{}:{}", response.prompt_id, response.id),
                                revision: response.id,
                                content: String::new(),
                                timestamp: response.timestamp_ms,
                            });
                            self.active_assistant = Some((response.prompt_id, index));
                            self.last_assistant_by_prompt
                                .insert(response.prompt_id, index);
                            index
                        }
                    };
                    let Some(UiPartProjection::Assistant {
                        content, revision, ..
                    }) = self.messages.get_mut(index)
                    else {
                        return Err("active Assistant projection is invalid".into());
                    };
                    content.push_str(&response.content);
                    *revision = response.id;
                    changed = Some(index);
                }
                if response.finished {
                    self.active_assistant = None;
                    self.finish_assistant(response.prompt_id);
                }
                changed
            }
            Event::ApiStateUpdate(update) => {
                self.api_state = Some(update.state);
                self.update_api_state(update.api_call_id, update.prompt_id, update.state);
                match update.state {
                    ApiState::Completed => {
                        self.api_usage = update.usage;
                        self.completed_api_usage
                            .insert(update.api_call_id, (update.prompt_id, update.usage));
                        None
                    }
                    ApiState::Error => {
                        self.errored_api_calls.insert(update.api_call_id);
                        Some(self.add_notice(
                            format!("API 错误：{}", update.detail),
                            update.id,
                            update.timestamp_ms,
                        ))
                    }
                    ApiState::Retrying => Some(self.add_notice(
                        format!("API 正在重试 {}/{}", update.retry_count, update.retry_limit),
                        update.id,
                        update.timestamp_ms,
                    )),
                    ApiState::Interrupted => {
                        if !self.errored_api_calls.contains(&update.api_call_id) {
                            self.api_usage = update.usage;
                            None
                        } else {
                            Some(self.add_notice(
                                format!("API 已中断：{}", update.detail),
                                update.id,
                                update.timestamp_ms,
                            ))
                        }
                    }
                    ApiState::Requesting | ApiState::Streaming => None,
                }
            }
            Event::ToolCall(call) => {
                self.open_tool(call.id, call.api_call_id, call.prompt_id);
                let presentation = tool_chat_presentation(&call.name);
                if presentation == ChatToolPresentation::Hidden {
                    self.hidden_tools.insert(call.id);
                    None
                } else if self.completed_compact_tools.contains(&call.id) {
                    None
                } else {
                    let queued = self.active_tools.values().any(|index| {
                        self.messages
                            .get(*index)
                            .and_then(UiPartProjection::tool)
                            .is_some_and(|tool| tool.api_call_id == call.api_call_id)
                    });
                    let tool = tool_projection(
                        call.id,
                        call.api_call_id,
                        &call.name,
                        &call.arguments,
                        call.timestamp_ms,
                        queued,
                    );
                    let projection = if presentation == ChatToolPresentation::WorkerActivity {
                        UiPartProjection::WorkerActivity {
                            key: format!("tool:{}", call.id),
                            revision: call.id,
                            timestamp: call.timestamp_ms,
                            tool,
                        }
                    } else {
                        UiPartProjection::Tool {
                            key: format!("tool:{}", call.id),
                            revision: call.id,
                            timestamp: call.timestamp_ms,
                            tool,
                        }
                    };
                    let index = self.append(projection);
                    self.active_tools.insert(call.id, index);
                    Some(index)
                }
            }
            Event::ToolInfoUpdate(info) => {
                if self.hidden_tools.contains(&info.tool_call_id)
                    || self.completed_compact_tools.contains(&info.tool_call_id)
                {
                    None
                } else if let Some(index) = self.active_tools.get(&info.tool_call_id).copied() {
                    let Some(tool) = self
                        .messages
                        .get_mut(index)
                        .and_then(UiPartProjection::tool_mut)
                    else {
                        return Err("active tool projection is invalid".into());
                    };
                    tool.updates.push(info.content.clone());
                    tool.output.push_str(&tool_info_text(&info.content));
                    tool.revision = info.id;
                    Some(index)
                } else {
                    None
                }
            }
            Event::ToolCallResult(result) => {
                self.close_tool(result.tool_call_id);
                if self.hidden_tools.contains(&result.tool_call_id)
                    || self.completed_compact_tools.contains(&result.tool_call_id)
                {
                    None
                } else if let Some(index) = self.active_tools.remove(&result.tool_call_id) {
                    let Some(tool) = self
                        .messages
                        .get_mut(index)
                        .and_then(UiPartProjection::tool_mut)
                    else {
                        return Err("active tool result projection is invalid".into());
                    };
                    if tool.session_id.is_none() && tool.name == "Terminal.Create" {
                        tool.session_id = serde_json::from_str::<Value>(&result.detail)
                            .ok()
                            .and_then(|detail| {
                                detail
                                    .get("session_id")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            });
                    }
                    tool.result = Some(UiToolResultProjection {
                        state: result.state,
                        exit_code: result.exit_code,
                        detail: result.detail.clone(),
                        finished: result.timestamp_ms,
                    });
                    tool.revision = result.id;
                    let api_call_id = tool.api_call_id;
                    let mut changed = Some(index);
                    if let Some(next_index) =
                        self.active_tools.values().copied().find(|candidate| {
                            self.messages
                                .get(*candidate)
                                .and_then(UiPartProjection::tool)
                                .is_some_and(|candidate| {
                                    candidate.api_call_id == api_call_id && candidate.queued
                                })
                        })
                        && let Some(next) = self
                            .messages
                            .get_mut(next_index)
                            .and_then(UiPartProjection::tool_mut)
                    {
                        next.queued = false;
                        next.started = result.timestamp_ms;
                        next.revision = result.id;
                        changed = min_changed(changed, Some(next_index));
                    }
                    changed
                } else {
                    None
                }
            }
            Event::TerminalSessionCreated(created) => {
                if let Some(index) = self.active_tools.get(&created.tool_call_id).copied()
                    && let Some(tool) = self
                        .messages
                        .get_mut(index)
                        .and_then(UiPartProjection::tool_mut)
                {
                    tool.session_id = Some(created.session_id.clone());
                    tool.revision = created.id;
                    Some(index)
                } else {
                    None
                }
            }
            Event::TerminalSessionState(update) => Some(self.append(UiPartProjection::Session {
                key: format!("session:{}", update.id),
                revision: update.id,
                content: format!(
                    "Session {} {} · exit_code={} · {}",
                    update.session_id,
                    enum_lower(&update.state),
                    update
                        .exit_code
                        .map_or_else(|| "None".to_owned(), |code| code.to_string()),
                    update.detail
                ),
                timestamp: update.timestamp_ms,
            })),
            Event::UserTurnAborted(aborted) => {
                self.abort_turn(aborted.prompt_id);
                None
            }
            Event::ContextCleared(cleared) => {
                self.workmap = WorkMapProjection::default();
                Some(self.add_notice("上下文已清空".to_owned(), cleared.id, cleared.timestamp_ms))
            }
            Event::SystemStaticPromptChange(change) => Some(self.add_notice(
                if change.mode == SystemStaticPromptMode::Custom {
                    format!(
                        "系统提示词已更新\n{}",
                        change.content.as_deref().unwrap_or_default()
                    )
                } else {
                    "系统提示词已恢复默认".to_owned()
                },
                change.id,
                change.timestamp_ms,
            )),
            Event::CompactStateUpdate(update) => match update.state {
                CompactState::Started => Some(self.begin_compact(update)),
                CompactState::StageCompleted => self.advance_compact(update),
                CompactState::Completed => {
                    self.api_usage = None;
                    self.turn = None;
                    self.turn_context_baseline.insert(update.prompt_id, None);
                    self.completed_api_usage
                        .retain(|_, (prompt_id, _)| *prompt_id != update.prompt_id);
                    Some(self.finish_compact(update, "上下文已压缩"))
                }
                CompactState::Failed => Some(self.finish_compact(update, "压缩失败")),
                CompactState::Interrupted => Some(self.finish_compact(update, "压缩中断")),
            },
            Event::CloneCompleted(completed) => Some(self.add_notice(
                format!("克隆完成。新会话：{}", completed.title),
                completed.id,
                completed.timestamp_ms,
            )),
        };
        Ok(changed)
    }

    fn observe_physical_event(&mut self, event: &Event) {
        if let Event::AgentTurn(turn) = event {
            self.summary.turn_state = Some(turn.state);
        }
        if let Event::SystemStaticPromptChange(change) = event {
            self.system_prompt.mode = change.mode;
            self.system_prompt.content = change.content.clone();
            self.system_prompt.event_id = Some(change.id);
            self.system_prompt
                .changes
                .push(UiSystemPromptChangeProjection {
                    id: change.id,
                    mode: change.mode,
                    content: change.content.clone(),
                });
        }
    }

    fn append(&mut self, projection: UiPartProjection) -> usize {
        let index = self.messages.len();
        self.messages.push(projection);
        index
    }

    fn add_notice(&mut self, content: String, id: EventId, timestamp: u64) -> usize {
        self.append(UiPartProjection::Notice {
            key: format!("notice:{id}"),
            revision: id,
            content,
            timestamp,
        })
    }

    fn add_agent_prompt(&mut self, id: EventId, timestamp: u64, content: &str) -> usize {
        self.begin_turn(id);
        self.turn_started_at.insert(id, timestamp);
        self.turn_context_baseline
            .insert(id, self.api_usage.map(|usage| usage.total_tokens));
        let index = self.append(UiPartProjection::User {
            key: format!("agent-prompt:{id}"),
            revision: id,
            content: content.to_owned(),
            timestamp,
            event_id: id,
            rewindable: false,
        });
        self.active_assistant = None;
        index
    }

    fn begin_turn(&mut self, prompt_id: EventId) {
        self.turn = Some(TurnTracker {
            prompt_id,
            aborted: false,
            terminal: false,
            api_states: BTreeMap::new(),
            open_tools: BTreeSet::new(),
            latest_api_call_id: None,
            latest_api_has_tool: false,
            latest_api_has_final: false,
        });
    }

    fn update_api_state(&mut self, api_call_id: EventId, prompt_id: EventId, state: ApiState) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.prompt_id == prompt_id)
        else {
            return;
        };
        turn.api_states.insert(api_call_id, state);
        match state {
            ApiState::Requesting => {
                turn.latest_api_call_id = Some(api_call_id);
                turn.latest_api_has_tool = false;
                turn.latest_api_has_final = false;
                turn.terminal = false;
            }
            ApiState::Streaming | ApiState::Retrying => turn.terminal = false,
            ApiState::Error | ApiState::Interrupted => turn.terminal = true,
            ApiState::Completed => {}
        }
    }

    fn open_tool(&mut self, id: EventId, api_call_id: EventId, prompt_id: EventId) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.prompt_id == prompt_id)
        else {
            return;
        };
        turn.open_tools.insert(id);
        if turn.latest_api_call_id == Some(api_call_id) {
            turn.latest_api_has_tool = true;
        }
        turn.terminal = false;
    }

    fn close_tool(&mut self, id: EventId) {
        if let Some(turn) = self.turn.as_mut() {
            turn.open_tools.remove(&id);
        }
    }

    fn finish_assistant(&mut self, prompt_id: EventId) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.prompt_id == prompt_id)
        else {
            return;
        };
        turn.latest_api_has_final = true;
        if !turn.latest_api_has_tool {
            turn.terminal = true;
        }
    }

    fn finish_turn(&mut self, prompt_id: EventId, state: AgentTurnState) {
        if state != AgentTurnState::Started
            && let Some(turn) = self
                .turn
                .as_mut()
                .filter(|turn| turn.prompt_id == prompt_id)
        {
            turn.terminal = true;
        }
    }

    fn abort_turn(&mut self, prompt_id: EventId) {
        if let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| turn.prompt_id == prompt_id)
        {
            turn.aborted = true;
        }
    }

    fn turn_state(&self) -> Option<UiTurnState> {
        let turn = self.turn.as_ref()?;
        let state = if turn.aborted {
            let settled = turn.api_states.values().all(|state| {
                matches!(
                    state,
                    ApiState::Completed | ApiState::Error | ApiState::Interrupted
                )
            }) && turn.open_tools.is_empty();
            if settled { "aborted" } else { "aborting" }
        } else {
            let api_active = turn
                .latest_api_call_id
                .and_then(|id| turn.api_states.get(&id))
                .is_some_and(|state| {
                    matches!(
                        state,
                        ApiState::Requesting | ApiState::Streaming | ApiState::Retrying
                    )
                });
            if api_active || !turn.open_tools.is_empty() || !turn.terminal {
                "active"
            } else {
                "completed"
            }
        };
        Some(UiTurnState {
            state: state.to_owned(),
            prompt_id: turn.prompt_id,
        })
    }

    fn begin_compact(&mut self, update: &crate::event::CompactStateUpdateEvent) -> usize {
        let total_stages = compact_stage_count(update);
        let index = self.append(UiPartProjection::Notice {
            key: format!("compact:{}", update.compact_id),
            revision: update.id,
            content: compact_progress_text(total_stages, 1),
            timestamp: update.timestamp_ms,
        });
        self.compact_activity = Some(CompactActivity {
            compact_id: update.compact_id,
            kind: enum_name(&update.kind),
            total_stages,
            stage: 1,
            message_index: index,
        });
        index
    }

    fn advance_compact(&mut self, update: &crate::event::CompactStateUpdateEvent) -> Option<usize> {
        let activity = self
            .compact_activity
            .as_mut()
            .filter(|activity| activity.compact_id == update.compact_id)?;
        activity.stage = (activity.stage + 1).min(activity.total_stages);
        let index = activity.message_index;
        if let Some(UiPartProjection::Notice {
            revision, content, ..
        }) = self.messages.get_mut(index)
        {
            *revision = update.id;
            *content = compact_progress_text(activity.total_stages, activity.stage);
        }
        Some(index)
    }

    fn finish_compact(
        &mut self,
        update: &crate::event::CompactStateUpdateEvent,
        content: &str,
    ) -> usize {
        if let Some(activity) = self
            .compact_activity
            .take()
            .filter(|activity| activity.compact_id == update.compact_id)
        {
            if let Some(UiPartProjection::Notice {
                revision,
                content: message_content,
                timestamp,
                ..
            }) = self.messages.get_mut(activity.message_index)
            {
                *revision = update.id;
                *message_content = content.to_owned();
                *timestamp = update.timestamp_ms;
            }
            activity.message_index
        } else {
            self.add_notice(content.to_owned(), update.id, update.timestamp_ms)
        }
    }

    fn workmap_projection(&self) -> UiWorkMapProjection {
        let snapshot = self.workmap.snapshot();
        let memory = self.workmap.active_memory_snapshot();
        let history = snapshot.history;
        let current = snapshot.current;
        let objective_count = history.len() + usize::from(current.is_some());
        let plan_count = history
            .iter()
            .chain(current.iter())
            .map(|objective| objective.plans.len())
            .sum::<usize>();
        let note_count = history
            .iter()
            .chain(current.iter())
            .flat_map(|objective| &objective.plans)
            .map(|plan| plan.notes.len())
            .sum::<usize>();
        let record_count = objective_count
            + plan_count
            + note_count
            + memory.facts.len()
            + memory.agreements.len();
        UiWorkMapProjection {
            memory,
            history,
            current,
            record_count,
        }
    }

    fn enrich_worker_activities(&mut self, worker_events: Option<&[Event]>) -> Option<usize> {
        let turns = worker_events.map(worker_turns).unwrap_or_default();
        let mut changed = None;
        for (index, message) in self.messages.iter_mut().enumerate() {
            let tool = match message {
                UiPartProjection::WorkerActivity { tool, .. } => tool,
                _ => continue,
            };
            let activity = worker_activity_for_wait(tool, &turns);
            if tool.activity != activity {
                tool.activity = activity;
                changed = min_changed(changed, Some(index));
            }
        }
        changed
    }
}

fn tool_projection(
    id: EventId,
    api_call_id: EventId,
    name: &str,
    arguments: &str,
    timestamp: u64,
    queued: bool,
) -> UiToolProjection {
    let args = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    UiToolProjection {
        id,
        api_call_id,
        name: name.to_owned(),
        arguments: arguments.to_owned(),
        args,
        started: timestamp,
        queued,
        session_id,
        output: String::new(),
        updates: Vec::new(),
        result: None,
        revision: id,
        activity: None,
    }
}

#[derive(Default)]
struct WorkerTurn {
    prompt_id: EventId,
    timestamp: u64,
    tools: Vec<UiToolProjection>,
    revision: EventId,
}

fn worker_turns(events: &[Event]) -> Vec<WorkerTurn> {
    let mut turns = Vec::<WorkerTurn>::new();
    let mut active = BTreeMap::<EventId, (usize, usize)>::new();
    for event in events {
        match event {
            Event::ManagerPrompt(prompt) => {
                turns.push(WorkerTurn {
                    prompt_id: prompt.id,
                    timestamp: prompt.timestamp_ms,
                    tools: Vec::new(),
                    revision: prompt.id,
                });
                active.clear();
            }
            Event::ToolCall(call)
                if tool_chat_presentation(&call.name) == ChatToolPresentation::Standard
                    && !turns.is_empty() =>
            {
                let turn_index = turns.len() - 1;
                let queued = active.values().any(|(active_turn, tool_index)| {
                    *active_turn == turn_index
                        && turns[turn_index].tools[*tool_index].api_call_id == call.api_call_id
                });
                let tool_index = turns[turn_index].tools.len();
                turns[turn_index].tools.push(tool_projection(
                    call.id,
                    call.api_call_id,
                    &call.name,
                    &call.arguments,
                    call.timestamp_ms,
                    queued,
                ));
                turns[turn_index].revision = call.id;
                active.insert(call.id, (turn_index, tool_index));
            }
            Event::ToolInfoUpdate(update) => {
                if let Some((turn_index, tool_index)) = active.get(&update.tool_call_id).copied() {
                    let tool = &mut turns[turn_index].tools[tool_index];
                    tool.output.push_str(&tool_info_text(&update.content));
                    tool.updates.push(update.content.clone());
                    tool.revision = update.id;
                    turns[turn_index].revision = update.id;
                }
            }
            Event::ToolCallResult(result) => {
                if let Some((turn_index, tool_index)) = active.remove(&result.tool_call_id) {
                    let api_call_id;
                    {
                        let tool = &mut turns[turn_index].tools[tool_index];
                        if tool.session_id.is_none() && tool.name == "Terminal.Create" {
                            tool.session_id = serde_json::from_str::<Value>(&result.detail)
                                .ok()
                                .and_then(|detail| {
                                    detail
                                        .get("session_id")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned)
                                });
                        }
                        tool.result = Some(UiToolResultProjection {
                            state: result.state,
                            exit_code: result.exit_code,
                            detail: result.detail.clone(),
                            finished: result.timestamp_ms,
                        });
                        tool.revision = result.id;
                        api_call_id = tool.api_call_id;
                    }
                    turns[turn_index].revision = result.id;
                    if let Some((_, next_index)) =
                        active.values().copied().find(|(active_turn, candidate)| {
                            *active_turn == turn_index
                                && turns[turn_index].tools[*candidate].api_call_id == api_call_id
                                && turns[turn_index].tools[*candidate].queued
                        })
                    {
                        let next = &mut turns[turn_index].tools[next_index];
                        next.queued = false;
                        next.started = result.timestamp_ms;
                        next.revision = result.id;
                    }
                }
            }
            Event::TerminalSessionCreated(created) => {
                if let Some((turn_index, tool_index)) = active.get(&created.tool_call_id).copied() {
                    let tool = &mut turns[turn_index].tools[tool_index];
                    tool.session_id = Some(created.session_id.clone());
                    tool.revision = created.id;
                    turns[turn_index].revision = created.id;
                }
            }
            _ => {}
        }
    }
    turns
}

fn worker_activity_for_wait(
    wait: &UiToolProjection,
    turns: &[WorkerTurn],
) -> Option<UiWorkerActivityProjection> {
    let target_turn = wait
        .result
        .as_ref()
        .and_then(|result| serde_json::from_str::<Value>(&result.detail).ok())
        .and_then(|detail| detail.get("turn_id").and_then(Value::as_u64));
    let cutoff = wait
        .result
        .as_ref()
        .map_or(u64::MAX, |result| result.finished);
    let turn = target_turn
        .and_then(|id| turns.iter().find(|turn| turn.prompt_id == id))
        .or_else(|| turns.iter().rev().find(|turn| turn.timestamp <= cutoff))?;
    Some(UiWorkerActivityProjection {
        state: worker_wait_state(wait),
        tools: turn.tools.clone(),
        revision: turn.revision,
    })
}

fn worker_wait_state(wait: &UiToolProjection) -> String {
    let Some(result) = &wait.result else {
        return "running".to_owned();
    };
    if enum_lower(&result.state) != "succeeded" {
        return "failed".to_owned();
    }
    let state = serde_json::from_str::<Value>(&result.detail)
        .ok()
        .and_then(|detail| {
            detail
                .get("state")
                .and_then(Value::as_str)
                .map(|state| state.to_ascii_lowercase())
        })
        .unwrap_or_default();
    match state.as_str() {
        "completed" => "completed",
        "interrupted" | "stopped" => "interrupted",
        "wait_interrupted" => "running",
        "failed" | "api_error" => "failed",
        _ => "running",
    }
    .to_owned()
}

fn agent_summary(events: &[Event]) -> UiProjectionSummary {
    UiProjectionSummary {
        turn_state: events.iter().rev().find_map(|event| match event {
            Event::AgentTurn(turn) => Some(turn.state),
            _ => None,
        }),
    }
}

fn system_prompt_projection(events: &[Event]) -> UiSystemPromptProjection {
    let changes = events
        .iter()
        .filter_map(|event| match event {
            Event::SystemStaticPromptChange(change) => Some(UiSystemPromptChangeProjection {
                id: change.id,
                mode: change.mode,
                content: change.content.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    match changes.last().cloned() {
        Some(latest) => UiSystemPromptProjection {
            mode: latest.mode,
            content: latest.content,
            event_id: Some(latest.id),
            changes,
        },
        None => UiSystemPromptProjection::default(),
    }
}

fn context_projection(events: &[Event], usage: Option<ApiUsage>) -> Result<UiContextProjection> {
    let effective = effective_conversation_events(events)?;
    let completed = effective.iter().find_map(|event| match event {
        Event::CompactStateUpdate(update) if update.state == CompactState::Completed => {
            Some(update)
        }
        _ => None,
    });
    let compact_content = completed.map(|update| update.content.clone());
    let compact_analysis = completed.and_then(|completed| {
        events.iter().find_map(|event| match event {
            Event::CompactStateUpdate(update)
                if update.compact_id == completed.compact_id
                    && update.state == CompactState::StageCompleted
                    && update.stage == Some(CompactStage::Analysis) =>
            {
                Some(update.content.clone())
            }
            _ => None,
        })
    });
    let memory_content = if compact_content.is_some() {
        crate::turn_history::latest_snapshot(events)?
    } else {
        None
    };
    let Some(usage) = usage else {
        return Ok(UiContextProjection {
            total: None,
            values: ContextTokenUsage::default(),
            compact_content,
            compact_analysis,
            memory_content,
        });
    };
    let mut values = ContextTokenUsage {
        system: usage.total_tokens,
        ..Default::default()
    };
    if let Some(boundary) = latest_context_usage_event(events)
        && boundary
            .usage
            .is_some_and(|value| value.total_tokens == usage.total_tokens)
        && let Some(estimate) = events.iter().find_map(|event| match event {
            Event::ContextUsageEstimate(estimate) if estimate.api_state_event_id == boundary.id => {
                Some(estimate)
            }
            _ => None,
        })
        && estimate.values.sum() == usage.total_tokens
    {
        values = estimate.values;
    }
    Ok(UiContextProjection {
        total: Some(usage.total_tokens),
        values,
        compact_content,
        compact_analysis,
        memory_content,
    })
}

fn context_event(event: &Event) -> bool {
    matches!(
        event,
        Event::ApiStateUpdate(_)
            | Event::ContextUsageEstimate(_)
            | Event::ModelChanged(_)
            | Event::CompactStateUpdate(_)
            | Event::ContextCleared(_)
    )
}

fn completed_turn_context_growth(
    usages: &BTreeMap<EventId, (EventId, Option<ApiUsage>)>,
    prompt_id: EventId,
    baseline: Option<u64>,
) -> Option<u64> {
    let mut matching = usages
        .values()
        .filter(|(usage_prompt_id, _)| *usage_prompt_id == prompt_id);
    let first = matching.next()?.1?;
    let mut final_usage = first;
    for (_, usage) in matching {
        final_usage = (*usage)?;
    }
    let baseline = baseline.unwrap_or(first.input_tokens);
    Some(final_usage.total_tokens.saturating_sub(baseline))
}

fn compact_stage_count(update: &crate::event::CompactStateUpdateEvent) -> usize {
    let persisted = usize::from(update.total_stages);
    if persisted > 0 {
        persisted
    } else if matches!(
        enum_name(&update.kind).as_str(),
        "MainAgentMultiTurn" | "ManagerMultiTurn"
    ) {
        6
    } else {
        1
    }
}

fn compact_progress_text(total: usize, stage: usize) -> String {
    format!(
        "正在压缩 ({}/{}) ...",
        stage.max(1).min(total.max(1)),
        total.max(1)
    )
}

fn tool_info_text(content: &ToolInfoContent) -> String {
    match content {
        ToolInfoContent::Text(text) => text.clone(),
        ToolInfoContent::Terminal(update) => terminal_line_update_text(update),
    }
}

fn terminal_line_update_text(update: &crate::terminal::TerminalLineUpdate) -> String {
    update
        .rows
        .iter()
        .map(|row| {
            let mut output = String::new();
            let mut column = 0_u16;
            for run in &row.runs {
                if run.col > column {
                    output.extend(std::iter::repeat_n(' ', usize::from(run.col - column)));
                }
                output.push_str(&run.text);
                column = run.col.saturating_add(run.width);
            }
            format!("{:06}: {}", row.row, output.trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_is_renderable_assistant(message: &UiPartProjection) -> bool {
    let UiPartProjection::Assistant { content, .. } = message else {
        return false;
    };
    content.chars().any(|character| {
        !character.is_whitespace()
            && !character.is_control()
            && !is_default_ignorable(character as u32)
    })
}

fn is_default_ignorable(code: u32) -> bool {
    matches!(
        code,
        0x00ad
            | 0x034f
            | 0x061c
            | 0x115f..=0x1160
            | 0x17b4..=0x17b5
            | 0x180b..=0x180f
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x206f
            | 0x3164
            | 0xfe00..=0xfe0f
            | 0xfeff
            | 0xffa0
            | 0xfff0..=0xfff8
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
            | 0xe0000..=0xe0fff
    )
}

fn projection_revision(agent: &UiAgentSnapshot, worker: Option<&UiAgentSnapshot>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PROJECTION_SCHEMA.as_bytes());
    update_revision_source(&mut hasher, agent);
    if let Some(worker) = worker {
        update_revision_source(&mut hasher, worker);
    }
    format!("{:x}", hasher.finalize())
}

fn update_revision_source(hasher: &mut Sha256, agent: &UiAgentSnapshot) {
    for value in [
        agent.id.as_str(),
        agent.edb_id.as_str(),
        &agent.mutation_revision.to_string(),
        &agent.events.len().to_string(),
    ] {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    if let Some(event) = agent.events.last() {
        let hash = event.getHash();
        hasher.update((hash.len() as u64).to_le_bytes());
        hasher.update(hash.as_bytes());
    } else {
        hasher.update(0_u64.to_le_bytes());
    }
}

fn projection_hash(projections: &[UiPartProjection]) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(projections)?)
    ))
}

fn min_changed(left: Option<usize>, right: Option<usize>) -> Option<usize> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn enum_name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn enum_lower(value: &impl Serialize) -> String {
    enum_name(value).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::event::{CompactKind, EventDataBase, ToolOutputStream, ToolResultState};

    fn agent_named(
        id: &str,
        kind: crate::event::AgentKind,
        orchestrator_name: &str,
        parent_agent_id: Option<&str>,
        events: Vec<Event>,
        mutation_revision: u64,
    ) -> UiAgentSnapshot {
        let edb_id = crate::event::edb_id(&events).unwrap().to_owned();
        UiAgentSnapshot {
            id: AgentId::new(id).unwrap(),
            title: Some(id.to_owned()),
            kind,
            parent_agent_id: parent_agent_id.map(|id| AgentId::new(id).unwrap()),
            orchestrator_name: orchestrator_name.to_owned(),
            edb_path: format!("{id}.edb").into(),
            edb_id,
            edb_size_bytes: 0,
            mutation_revision,
            last_mutation: None,
            prompt_submission_revision: 0,
            input_draft: String::new(),
            input_draft_revision: 0,
            events: events.into(),
        }
    }

    fn agent(events: Vec<Event>, mutation_revision: u64) -> UiAgentSnapshot {
        agent_named(
            "main",
            crate::event::AgentKind::Primary,
            "main-agent",
            None,
            events,
            mutation_revision,
        )
    }

    fn snapshot(agents: Vec<UiAgentSnapshot>) -> UiSnapshot {
        UiSnapshot {
            revision: 0,
            environment: Arc::new(crate::ui_backend::UiEnvironment {
                workspace: ".".into(),
                os: "test".into(),
                arch: "test".into(),
            }),
            agents,
            models: Vec::<crate::ui_backend::UiModelOption>::new().into(),
            orchestrators: Vec::<String>::new().into(),
            default_orchestrator: "main-agent".into(),
        }
    }

    #[test]
    fn projection_is_incremental_and_range_hash_is_revision_bound() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        let prompt = edb.append_user_prompt("hello").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_assist_response(prompt, "one", false).unwrap();
        let initial = agent(edb.events().to_vec(), 0);
        let mut store = UiProjectionStore::new(&initial, None).unwrap();
        let first_revision = store.revision.clone();
        assert_eq!(store.data.messages.len(), 2);

        edb.append_assist_response(prompt, " two", true).unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        let completed = agent(edb.events().to_vec(), 0);
        store.synchronize(&completed, None).unwrap();
        assert_ne!(store.revision, first_revision);
        assert_eq!(store.changed_from(Some(&first_revision)), Some(1));
        let range = store.range(&completed, 1, 2).unwrap();
        assert_eq!(range.hash.len(), 64);
        assert!(store.require_revision(&first_revision).is_err());
        assert!(matches!(
            &range.projections[0],
            UiPartProjection::Assistant { content, .. } if content == "one two"
        ));
        let rebuilt = UiProjectionStore::new(&completed, None).unwrap();
        assert_eq!(
            store.state(&completed, None),
            rebuilt.state(&completed, None)
        );
    }

    #[test]
    fn sync_window_keeps_streaming_state_and_body_on_one_revision() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        let prompt = edb.append_user_prompt("hello").unwrap();
        edb.append_api_requesting(prompt).unwrap();
        let mut cache = UiProjectionCache::default();
        let mut cursor = UiProjectionCursor {
            agent_id: "main".into(),
            revision: None,
            window: Some(UiProjectionWindow {
                start: 0,
                end: 0,
                count: 0,
                follow_tail: true,
            }),
        };
        for index in 1..=40 {
            edb.append_assist_response(prompt, "line\n", false).unwrap();
            let snapshot = snapshot(vec![agent(edb.events().to_vec(), 0)]);
            let state = cache
                .states(&snapshot, &[cursor.clone()])
                .unwrap()
                .remove(0);
            let range = state.range.as_ref().unwrap();
            assert_eq!(range.revision, state.revision);
            assert_eq!(range.count, state.count);
            assert_eq!(range.end, state.count);
            assert_eq!(range.start, if index == 1 { 0 } else { 1 });
            assert!(matches!(range.projections.last().unwrap(),
                UiPartProjection::Assistant { content, .. } if *content == "line\n".repeat(index)));
            cursor.revision = Some(state.revision.clone());
            cursor.window = Some(UiProjectionWindow {
                start: 0,
                end: state.count,
                count: state.count,
                follow_tail: true,
            });
            assert!(
                cache.states(&snapshot, &[cursor.clone()]).unwrap()[0]
                    .range
                    .is_none()
            );
            // Metadata-only consumers never acquire message bodies.
            assert!(cache.states(&snapshot, &[]).unwrap()[0].range.is_none());
        }
    }

    #[test]
    fn sync_window_is_bounded_and_handles_history_shrink_and_changed_prefix() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        for _ in 0..300 {
            edb.append_user_prompt("history").unwrap();
        }
        let agent = agent(edb.events().to_vec(), 0);
        let store = UiProjectionStore::new(&agent, None).unwrap();
        let mut state = store.state(&agent, None);
        let mut window = UiProjectionWindow {
            start: 0,
            end: 0,
            count: 0,
            follow_tail: true,
        };
        assert_eq!(window.range(&state, None), Some((236, 300)));
        window = UiProjectionWindow {
            start: 40,
            end: 104,
            count: 300,
            follow_tail: false,
        };
        state.changed_from = Some(299);
        assert_eq!(window.range(&state, Some("old")), None);
        state.changed_from = Some(70);
        assert_eq!(window.range(&state, Some("old")), Some((70, 104)));
        state.changed_from = Some(0);
        assert_eq!(window.range(&state, Some("old")), Some((40, 104)));
        state.count = 20;
        assert_eq!(window.range(&state, Some("old")), Some((0, 20)));
        state.count = 0;
        assert_eq!(window.range(&state, Some("old")), None);
        state.count = 300;
        window.end = usize::MAX;
        assert_eq!(window.range(&state, Some("old")), Some((236, 300)));
    }

    #[test]
    fn public_projection_matches_shared_webui_message_shape() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        edb.append_initial_model("model-a").unwrap();
        edb.append_initial_reasoning_effort("high").unwrap();
        let prompt = edb.append_user_prompt("hello").unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_assist_response(prompt, "I will check.\n", false)
            .unwrap();
        let tool = edb
            .append_tool_call(api, prompt, "provider-tool", "Terminal.Create", "{}")
            .unwrap();
        edb.append_tool_info(tool, ToolOutputStream::Stdout, "ready\n")
            .unwrap();
        edb.append_tool_result(tool, ToolResultState::Succeeded, Some(0), "{}")
            .unwrap();
        edb.append_assist_response(prompt, "done", true).unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Completed, "")
            .unwrap();

        let projection = ProjectionData::from_events(edb.events()).unwrap();
        let json = serde_json::to_value(&projection.messages).unwrap();
        assert_eq!(json[0]["kind"], "user");
        assert_eq!(json[0]["content"], "hello");
        assert_eq!(json[1]["kind"], "assistant");
        assert_eq!(json[1]["content"], "I will check.\ndone");
        assert_eq!(json[2]["kind"], "tool");
        assert_eq!(json[2]["tool"]["name"], "Terminal.Create");
        assert_eq!(json[2]["tool"]["output"], "ready\n");
        assert_eq!(json[2]["tool"]["result"]["state"], "Succeeded");
    }

    #[test]
    fn same_length_source_prefix_hash_change_forces_full_replay() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        let prompt = edb.append_user_prompt("hello").unwrap();
        edb.append_assist_response(prompt, "before", true).unwrap();
        let initial = agent(edb.events().to_vec(), 0);
        let mut store = UiProjectionStore::new(&initial, None).unwrap();
        let initial_revision = store.revision.clone();

        let mut changed_events = edb.events().to_vec();
        let Some(Event::AssistResponse(response)) = changed_events.last_mut() else {
            panic!("fixture must end in an Assistant response");
        };
        response.content = "after".into();
        let changed = agent(changed_events, 0);
        store.synchronize(&changed, None).unwrap();

        assert_ne!(store.revision, initial_revision);
        assert_eq!(store.changed_from(Some(&initial_revision)), Some(0));
        assert!(matches!(
            &store.data.messages[1],
            UiPartProjection::Assistant { content, .. } if content == "after"
        ));
    }

    #[test]
    fn replay_boundaries_match_clean_reconstruction() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        let prompt = edb.append_user_prompt("hello").unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        edb.append_assist_response(prompt, "discard me", false)
            .unwrap();
        let before_error = agent(edb.events().to_vec(), 0);
        let mut store = UiProjectionStore::new(&before_error, None).unwrap();

        edb.append_api_state(api, prompt, ApiState::Error, "boom")
            .unwrap();
        let after_error = agent(edb.events().to_vec(), 0);
        store.synchronize(&after_error, None).unwrap();
        let rebuilt_after_error = UiProjectionStore::new(&after_error, None).unwrap();
        assert_eq!(
            store.state(&after_error, None),
            rebuilt_after_error.state(&after_error, None)
        );
        assert!(
            store
                .data
                .messages
                .iter()
                .all(|message| !matches!(message, UiPartProjection::Assistant { .. }))
        );
        assert!(store.data.messages.iter().any(|message| {
            matches!(message, UiPartProjection::Notice { content, .. } if content == "API 错误：boom")
        }));

        edb.append_context_cleared().unwrap();
        let after_clear = agent(edb.events().to_vec(), 0);
        store.synchronize(&after_clear, None).unwrap();
        let rebuilt_after_clear = UiProjectionStore::new(&after_clear, None).unwrap();
        assert_eq!(
            store.state(&after_clear, None),
            rebuilt_after_clear.state(&after_clear, None)
        );
        assert!(matches!(
            store.data.messages.as_slice(),
            [UiPartProjection::Notice { content, .. }] if content == "上下文已清空"
        ));
    }

    #[test]
    fn completed_compact_hides_its_tool_and_matches_full_replay() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        let prompt = edb.append_user_prompt("compact").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let tool = edb
            .append_tool_call(api, prompt, "compact", crate::compact::TOOL_NAME, "{}")
            .unwrap();
        edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
            .unwrap();
        let compact = edb
            .append_compact_started(tool, prompt, CompactKind::ChatbotSingleTurn)
            .unwrap();
        let started = agent(edb.events().to_vec(), 0);
        let mut store = UiProjectionStore::new(&started, None).unwrap();
        assert!(
            store
                .data
                .messages
                .iter()
                .any(|message| matches!(message, UiPartProjection::Tool { .. }))
        );

        edb.append_compact_terminal(compact, CompactState::Completed, "summary", "")
            .unwrap();
        let completed = agent(edb.events().to_vec(), 0);
        store.synchronize(&completed, None).unwrap();
        let rebuilt = UiProjectionStore::new(&completed, None).unwrap();
        assert_eq!(
            store.state(&completed, None),
            rebuilt.state(&completed, None)
        );
        assert!(
            store
                .data
                .messages
                .iter()
                .all(|message| !matches!(message, UiPartProjection::Tool { .. }))
        );
        assert!(store.data.messages.iter().any(|message| {
            matches!(message, UiPartProjection::Notice { content, .. } if content == "上下文已压缩")
        }));
    }

    #[test]
    fn worker_change_updates_parent_revision_and_only_the_wait_item() {
        let mut worker = EventDataBase::new();
        worker
            .append_agent_kind_def(
                crate::event::AgentKind::SubAgent,
                "worker-agent",
                Some("main".into()),
                Some("worker".into()),
            )
            .unwrap();
        let worker_prompt = worker.append_manager_prompt("inspect").unwrap();
        let worker_api = worker.append_api_requesting(worker_prompt).unwrap();
        let worker_tool = worker
            .append_tool_call(
                worker_api,
                worker_prompt,
                "read",
                "File.Read",
                r#"{"path":"a.txt"}"#,
            )
            .unwrap();

        let mut parent = EventDataBase::new();
        parent
            .append_agent_kind_def(crate::event::AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        let parent_prompt = parent.append_user_prompt("delegate").unwrap();
        let parent_api = parent.append_api_requesting(parent_prompt).unwrap();
        let wait = parent
            .append_tool_call(parent_api, parent_prompt, "wait", "Worker.Wait", "{}")
            .unwrap();
        parent
            .append_tool_result(
                wait,
                ToolResultState::Succeeded,
                None,
                format!(r#"{{"turn_id":{worker_prompt},"state":"completed"}}"#),
            )
            .unwrap();

        let first_snapshot = snapshot(vec![
            agent_named(
                "main",
                crate::event::AgentKind::Primary,
                "main-agent",
                None,
                parent.events().to_vec(),
                0,
            ),
            agent_named(
                "worker",
                crate::event::AgentKind::SubAgent,
                "worker-agent",
                Some("main"),
                worker.events().to_vec(),
                0,
            ),
        ]);
        let mut cache = UiProjectionCache::default();
        let first_states = cache.states(&first_snapshot, &[]).unwrap();
        let first_parent = first_states
            .iter()
            .find(|state| state.agent_id == "main")
            .unwrap();
        let first_revision = first_parent.revision.clone();
        let first_range = cache
            .range(
                &first_snapshot,
                &AgentId::new("main").unwrap(),
                0,
                first_parent.count,
                &first_revision,
            )
            .unwrap();
        assert!(matches!(
            &first_range.projections[1],
            UiPartProjection::WorkerActivity { tool, .. }
                if tool.activity.as_ref().is_some_and(|activity|
                    activity.tools.len() == 1 && activity.tools[0].output.is_empty())
        ));

        worker
            .append_tool_info(worker_tool, ToolOutputStream::Stdout, "ready")
            .unwrap();
        let second_snapshot = snapshot(vec![
            agent_named(
                "main",
                crate::event::AgentKind::Primary,
                "main-agent",
                None,
                parent.events().to_vec(),
                0,
            ),
            agent_named(
                "worker",
                crate::event::AgentKind::SubAgent,
                "worker-agent",
                Some("main"),
                worker.events().to_vec(),
                0,
            ),
        ]);
        let second_states = cache
            .states(
                &second_snapshot,
                &[UiProjectionCursor {
                    agent_id: "main".into(),
                    revision: Some(first_revision.clone()),
                    window: None,
                }],
            )
            .unwrap();
        let second_parent = second_states
            .iter()
            .find(|state| state.agent_id == "main")
            .unwrap();
        assert_ne!(second_parent.revision, first_revision);
        assert_eq!(second_parent.changed_from, Some(1));
        let second_range = cache
            .range(
                &second_snapshot,
                &AgentId::new("main").unwrap(),
                1,
                2,
                &second_parent.revision,
            )
            .unwrap();
        assert!(matches!(
            &second_range.projections[0],
            UiPartProjection::WorkerActivity { tool, .. }
                if tool.activity.as_ref().is_some_and(|activity|
                    activity.tools[0].output == "ready")
        ));
        let hash = cache
            .hash(
                &second_snapshot,
                &AgentId::new("main").unwrap(),
                1,
                2,
                &second_parent.revision,
            )
            .unwrap();
        assert_eq!(hash.hash, second_range.hash);
    }

    #[test]
    fn shared_webui_equivalence_fixture_matches_rust_projection() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/ui_projection_equivalence.json",
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let events: Vec<Event> = serde_json::from_value(case["events"].clone()).unwrap();
            let data = ProjectionData::from_events(&events).unwrap();
            let turn_state = data.turn_state();
            let workmap = data.workmap_projection();
            let actual = serde_json::json!({
                "messages": &data.messages,
                "apiState": &data.api_state,
                "apiUsage": &data.api_usage,
                "model": &data.model,
                "effort": &data.effort,
                "turnState": turn_state,
                "summary": &data.summary,
                "workmap": workmap,
                "context": &data.context,
                "systemPrompt": &data.system_prompt,
            });
            assert_eq!(
                actual,
                case["projection"],
                "projection fixture {} diverged",
                case["name"].as_str().unwrap()
            );
        }
    }
}
