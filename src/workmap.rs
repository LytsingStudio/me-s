use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Result,
    event::{Event, EventDataBase, EventId, WorkMapMutationEvent},
    toolbox::{DEFAULT_TOOL_RESULT_TOKEN_LIMIT, ToolboxExecutionError, ToolboxTool, api_safe_name},
};

pub const WORKMAP_TOOLBOX_NAME: &str = "WorkMap";
pub const READ: &str = "WorkMap.Read";
pub const READ_HISTORY: &str = "WorkMap.ReadHistory";
pub const START: &str = "WorkMap.Start";
pub const UPDATE_PLAN_STATE: &str = "WorkMap.UpdatePlanState";
pub const ADD_NOTE: &str = "WorkMap.AddNote";
pub const CHANGE_PLAN: &str = "WorkMap.ChangePlan";
pub const ADD_PLAN: &str = "WorkMap.AddPlan";
pub const CLOSE_OBJECTIVE: &str = "WorkMap.CloseObjective";
pub const ADD_MEMORY: &str = "WorkMap.AddMemory";
pub const INVALIDATE_MEMORY: &str = "WorkMap.InvalidateMemory";

const READ_RESULT_TOKEN_LIMIT: usize = 16 * 1024;

const LOCAL_TOOLS: [&str; 10] = [
    "Read",
    "ReadHistory",
    "Start",
    "UpdatePlanState",
    "AddNote",
    "ChangePlan",
    "AddPlan",
    "CloseObjective",
    "AddMemory",
    "InvalidateMemory",
];

const TOOLBOX_BRIEF: &str = r#"WorkMap is this Agent's private, persistent map for substantial work. It has three views:

- Memory: the maintained current state of active, globally applicable Facts and explicit Agreements expected to remain useful after the current Objective ends and across future Objectives.
- Current: at most one active Objective containing an ordered Plan list and every durable Note attached to those Plans.
- History: closed Objectives, each retaining its complete Plans and Notes.

WorkMap is mandatory for substantial work: external tools, research, file changes, debugging, multiple meaningful steps, or work that may continue across turns. It is optional only for greetings, casual conversation, and genuinely trivial one-step answers. It records concise, externally useful working state, not hidden chain-of-thought, every command, verbatim output, or token-by-token reasoning.

If another system section defines this Agent as a dedicated Worker, apply all WorkMap rules only to the concrete operational request received from the Manager. Do not adopt the user's broader objective or independently create business, design, implementation, review-judgment, or acceptance-judgment plans. The WorkMap may organize mechanical execution, explicitly specified review or acceptance procedures, and evidence transmission inside the request, but it cannot expand or reinterpret that request. This may include collecting image evidence while leaving its content uninspected for the Manager. A completed Worker Plan means only that its specified operation ran and its evidence was returned; it never means the underlying deliverable was reviewed, correct, accepted, or ready.

For a dedicated Worker, later references in this toolbox to the user's request or input mean the Manager's concrete request or input, and `final answer` means the Worker's report to the Manager. They never grant access to, or ownership of, the actual user's broader objective.

### Memory: active global state

Memory is not an indefinitely growing history. Active Memory is the authoritative current global state presented by WorkMap.Read. Retraction and replacement records may preserve earlier forms internally, but inactive entries are not current context and must not be treated as authoritative.

Admission rules:

1. Add only concise information that is globally applicable and expected to remain valid and useful after the current Objective ends and across future Objectives. Relevance across multiple Plans within the current Objective is insufficient.
2. Never store objective-specific requests or constraints, single-turn discussion, execution plans, progress, temporary decisions, local trade-offs, evidence, validation results, or completed-task status in Memory; keep them in Current's Objective, Plans, Notes, or the conversation.
3. Facts require a basis: user_stated, observed, verified, or inferred. Agreements qualify only when the user explicitly establishes a requirement, preference, or mutually established decision as globally applicable beyond the current Objective; never label an Agent assumption as an Agreement.
4. If unsure whether information is global and cross-Objective, do not call AddMemory. Do not duplicate or substantially overlap an existing active entry.

Maintenance rules:

1. Keep an active entry unchanged when it is still accurate, clear, non-duplicated, and globally useful. Age alone is never a reason to remove it.
2. Use InvalidateMemory without a replacement when an entry is clearly obsolete and will not be used again, was incorrectly classified because it is actually Objective-specific or temporary, is redundant after consolidation, or has lost so much essential context that it is clearly unusable.
3. Use InvalidateMemory with an atomic replacement when the subject was renamed or the global Fact, requirement, or Agreement changed. A replacement must independently satisfy the same global cross-Objective eligibility rule; never replace old Memory with Objective-specific content.
4. Consolidate duplicate or substantially overlapping entries so one complete eligible current statement remains active, then retract the redundant entries. Do not add a third summary while leaving the duplicates active.
5. If an unclear entry may still matter but cannot be interpreted safely, do not guess its meaning, invent a replacement, or silently retract it. Leave it unchanged until the user or reliable evidence resolves the ambiguity; ask for clarification when correct maintenance depends on it.
6. Inspecting or maintaining Memory does not require a mutation. Do not rewrite accurate entries merely to demonstrate maintenance, normalize style, shorten valid prose, or refresh wording.

Examples:

- If a long-lived subject changes from one name to another, supersede the old entry with the same current rule under the new name; do not keep both names active.
- If a global default changes from allowing an action to requiring approval, replace the old Fact or Agreement with the new current rule.
- `Finished the second step of the current migration` is progress for Current, not Memory. If it was added to Memory by mistake, retract it without a replacement.
- If two active Agreements both say that dates use the same standard, keep one complete statement and retract the redundant one instead of adding another summary.
- An old Agreement that remains accurate, clear, and globally useful stays unchanged even if it has not been referenced recently.
- If an entry says `keep the legacy rule for it` and the subject of `it` is unknown but could still matter, do not invent the subject or silently remove the entry; seek clarification when needed.

### Before substantial work

The mandatory first-message SetTitle call is the only ordering exception to this section. When the first-message title reminder is present, call SetTitle first; after it succeeds, follow the WorkMap sequence below before any other non-WorkMap action.

1. Call WorkMap.Read before the first non-WorkMap action other than that mandatory SetTitle call. Its Memory section contains only active entries and is the authoritative durable context.
2. If Current exists, resume its active Plan and respect its remaining planned route unless the user or evidence changes it.
3. If Current is empty, call WorkMap.Start with one precise overall Objective and a realistic ordered Plan list. The first Plan becomes active and the others remain planned.
4. Use WorkMap.ReadHistory only when earlier closed work is actually needed. It is not a routine startup or completion call.

Plans are sequential stages of one Objective. Keep their scopes distinct and at comparable granularity. Internal actions, findings, decisions, validation, adjustments, blockers, and continuation points are Notes under the relevant Plan, not extra Plans. The route may be changed explicitly with ChangePlan, AddPlan, or terminal Plan states; never silently discard planned work.

### While working

Treat Current as a live execution record, not a plan written once and revisited only at completion. While carrying out a multi-step Plan, use AddNote throughout the work: after each meaningful action, record what was done and its material result before moving on; record findings when established, decisions when the route is chosen, validation immediately after checking, and adjustments, blockers, or exact continuation points when they arise. Keep enough chronological detail that another capable Agent could resume from WorkMap without reconstructing the process from the conversation. Prefer one precise Note per meaningful boundary and combine only tightly related information. Do not postpone Notes until the end, mechanically log every command or verbatim output, or expose hidden chain-of-thought.

Note kinds are action, finding, decision, validation, adjustment, blocker, next, and note. Notes are immutable and chronological.

Use UpdatePlanState to complete, cancel, or supersede a Plan. Completed requires a concrete outcome and real verification; cancelled or superseded requires a reason. When the active Plan reaches a terminal state, ME automatically activates the next planned Plan. When no open Plans remain and at least one Plan completed, the Objective automatically enters History as completed. If the entire Objective is abandoned or replaced, use CloseObjective; it atomically closes every open Plan and moves the Objective to History.

### Final answers

Before a normal final answer for substantial work:

1. Add every valuable result, finding, decision, validation, blocker, route change, and exact continuation point not yet represented.
2. Resolve every planned or active Plan truthfully. Continue required work; explicitly cancel, supersede, change, or replace work only when the route genuinely changed.
3. A successful UpdatePlanState result is already the authoritative Current projection; never call Read after it merely to inspect or confirm state. If `current` is non-null, continue its active Plan. If `current` is null, the Objective is complete and you may proceed directly to the final answer. When final state was not established by UpdatePlanState, call WorkMap.Read as the final audit; Current must be null.

Do not call ReadHistory for the final audit. ReadHistory exists only to recover genuinely relevant earlier work.

One exception permits an early final answer while Current remains: progress genuinely requires user input or a user decision, an external condition prevents continuation, or the user explicitly requested a handoff. Preserve the real Current state, call Read, and explicitly state what is deferred, why, and what will resume it.

A successful context compaction makes earlier WorkMap tool results stale, but WorkMap itself persists. Immediately after compaction, call WorkMap.Read before any non-WorkMap action and use the fresh result as authoritative."#;

pub fn is_workmap_tool(name: &str) -> bool {
    matches!(
        name,
        READ | READ_HISTORY
            | START
            | UPDATE_PLAN_STATE
            | ADD_NOTE
            | CHANGE_PLAN
            | ADD_PLAN
            | CLOSE_OBJECTIVE
            | ADD_MEMORY
            | INVALIDATE_MEMORY
    )
}

pub fn operation_tool_name(operation: WorkMapOperation) -> &'static str {
    match operation {
        WorkMapOperation::Started => START,
        WorkMapOperation::PlanStateUpdated => UPDATE_PLAN_STATE,
        WorkMapOperation::NoteAdded => ADD_NOTE,
        WorkMapOperation::PlanChanged => CHANGE_PLAN,
        WorkMapOperation::PlanAdded => ADD_PLAN,
        WorkMapOperation::ObjectiveClosed => CLOSE_OBJECTIVE,
        WorkMapOperation::MemoryAdded => ADD_MEMORY,
        WorkMapOperation::MemoryInvalidated => INVALIDATE_MEMORY,
    }
}

pub fn catalog_parts() -> (Vec<ToolboxTool>, (String, String)) {
    let tools = LOCAL_TOOLS
        .into_iter()
        .map(|local_name| {
            let full_name = format!("{WORKMAP_TOOLBOX_NAME}.{local_name}");
            ToolboxTool {
                toolbox: WORKMAP_TOOLBOX_NAME.into(),
                local_name: local_name.into(),
                api_name: api_safe_name(&full_name),
                full_name,
                input_schema: input_schema(local_name),
                output_schema: output_schema(local_name),
                result_token_limit: if local_name == "Read" {
                    READ_RESULT_TOKEN_LIMIT
                } else {
                    DEFAULT_TOOL_RESULT_TOKEN_LIMIT
                },
                instructions: instructions(local_name).into(),
                route: route(local_name).into(),
                examples: examples(local_name).into(),
            }
        })
        .collect();
    (tools, (WORKMAP_TOOLBOX_NAME.into(), TOOLBOX_BRIEF.into()))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveState {
    Active,
    Completed,
    Cancelled,
    Superseded,
}

impl ObjectiveState {
    fn is_history(self) -> bool {
        self != Self::Active
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Planned,
    Active,
    Completed,
    Cancelled,
    Superseded,
}

impl PlanState {
    fn is_open(self) -> bool {
        matches!(self, Self::Planned | Self::Active)
    }

    fn is_terminal(self) -> bool {
        !self.is_open()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Action,
    Finding,
    Decision,
    Validation,
    Adjustment,
    Blocker,
    Next,
    Note,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Agreement,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBasis {
    UserStated,
    Observed,
    Verified,
    Inferred,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryState {
    Active,
    Superseded,
    Retracted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkMapObjective {
    pub id: String,
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub state: ObjectiveState,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkMapPlan {
    pub id: String,
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub objective_id: String,
    pub order: u64,
    pub state: PlanState,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkMapNote {
    pub id: String,
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub plan_id: String,
    pub sequence: u64,
    pub kind: NoteKind,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkMapMemory {
    pub id: String,
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub kind: MemoryKind,
    pub state: MemoryState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<MemoryBasis>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "record")]
pub enum WorkMapRecord {
    Objective(WorkMapObjective),
    Plan(WorkMapPlan),
    Note(WorkMapNote),
    Memory(WorkMapMemory),
}

impl WorkMapRecord {
    pub fn id(&self) -> &str {
        match self {
            Self::Objective(record) => &record.id,
            Self::Plan(record) => &record.id,
            Self::Note(record) => &record.id,
            Self::Memory(record) => &record.id,
        }
    }

    pub fn revision(&self) -> u64 {
        match self {
            Self::Objective(record) => record.revision,
            Self::Plan(record) => record.revision,
            Self::Note(record) => record.revision,
            Self::Memory(record) => record.revision,
        }
    }

    pub fn created_at_ms(&self) -> u64 {
        match self {
            Self::Objective(record) => record.created_at_ms,
            Self::Plan(record) => record.created_at_ms,
            Self::Note(record) => record.created_at_ms,
            Self::Memory(record) => record.created_at_ms,
        }
    }

    pub fn updated_at_ms(&self) -> u64 {
        match self {
            Self::Objective(record) => record.updated_at_ms,
            Self::Plan(record) => record.updated_at_ms,
            Self::Note(record) => record.updated_at_ms,
            Self::Memory(record) => record.updated_at_ms,
        }
    }

    pub(crate) fn stamp(&mut self, timestamp_ms: u64) {
        let (revision, created_at_ms, updated_at_ms) = match self {
            Self::Objective(record) => (
                record.revision,
                &mut record.created_at_ms,
                &mut record.updated_at_ms,
            ),
            Self::Plan(record) => (
                record.revision,
                &mut record.created_at_ms,
                &mut record.updated_at_ms,
            ),
            Self::Note(record) => (
                record.revision,
                &mut record.created_at_ms,
                &mut record.updated_at_ms,
            ),
            Self::Memory(record) => (
                record.revision,
                &mut record.created_at_ms,
                &mut record.updated_at_ms,
            ),
        };
        if revision == 1 {
            *created_at_ms = timestamp_ms;
        }
        *updated_at_ms = timestamp_ms;
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkMapOperation {
    Started,
    PlanStateUpdated,
    NoteAdded,
    PlanChanged,
    PlanAdded,
    ObjectiveClosed,
    MemoryAdded,
    MemoryInvalidated,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkMapMutation {
    pub operation: WorkMapOperation,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    pub records: Vec<WorkMapRecord>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkMapPlanSnapshot {
    pub plan: WorkMapPlan,
    pub notes: Vec<WorkMapNote>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkMapObjectiveSnapshot {
    pub objective: WorkMapObjective,
    pub plans: Vec<WorkMapPlanSnapshot>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkMapHistorySummary {
    pub objective: WorkMapObjective,
    pub plan_count: usize,
    pub note_count: usize,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct WorkMapMemorySnapshot {
    pub facts: Vec<WorkMapMemory>,
    pub agreements: Vec<WorkMapMemory>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct WorkMapSnapshot {
    pub memory: WorkMapMemorySnapshot,
    pub history: Vec<WorkMapObjectiveSnapshot>,
    pub current: Option<WorkMapObjectiveSnapshot>,
}

#[derive(Clone, Debug, Default)]
pub struct WorkMapProjection {
    records: BTreeMap<String, WorkMapRecord>,
}

impl WorkMapProjection {
    pub fn from_events(events: &[Event]) -> Result<Self> {
        let mut projection = Self::default();
        for event in events {
            match event {
                Event::ContextCleared(_) => projection = Self::default(),
                Event::WorkMapMutation(event) => projection.apply(event)?,
                _ => {}
            }
        }
        Ok(projection)
    }

    pub(crate) fn apply(&mut self, event: &WorkMapMutationEvent) -> Result<()> {
        validate_mutation_shape(&event.mutation)?;
        validate_operation_transition(&event.mutation, &self.records)?;
        for record in &event.mutation.records {
            validate_record(record, &self.records)?;
            if record.updated_at_ms() != event.timestamp_ms {
                return Err(format!(
                    "WorkMap record {} timestamp does not match event {}",
                    record.id(),
                    event.id
                )
                .into());
            }
            match self.records.get(record.id()) {
                Some(previous) => {
                    if record.revision() != previous.revision() + 1 {
                        return Err(format!(
                            "WorkMap record {} revision {} does not follow {}",
                            record.id(),
                            record.revision(),
                            previous.revision()
                        )
                        .into());
                    }
                    if record.created_at_ms() != previous.created_at_ms() {
                        return Err(format!(
                            "WorkMap record {} changed created_at_ms",
                            record.id()
                        )
                        .into());
                    }
                    if std::mem::discriminant(record) != std::mem::discriminant(previous) {
                        return Err(format!("WorkMap record {} changed kind", record.id()).into());
                    }
                }
                None => {
                    if record.revision() != 1 || record.created_at_ms() != event.timestamp_ms {
                        return Err(format!(
                            "new WorkMap record {} must begin at revision 1",
                            record.id()
                        )
                        .into());
                    }
                }
            }
        }
        for record in &event.mutation.records {
            self.records.insert(record.id().into(), record.clone());
        }
        validate_projection_relations(&self.records)
    }

    pub fn snapshot(&self) -> WorkMapSnapshot {
        let mut history = self
            .objectives()
            .into_iter()
            .filter(|objective| objective.state.is_history())
            .filter_map(|objective| self.objective_snapshot(&objective.id))
            .collect::<Vec<_>>();
        history.sort_by(|left, right| {
            (left.objective.updated_at_ms, &left.objective.id)
                .cmp(&(right.objective.updated_at_ms, &right.objective.id))
        });
        WorkMapSnapshot {
            memory: self.memory_snapshot(),
            current: self
                .active_objective()
                .and_then(|objective| self.objective_snapshot(&objective.id)),
            history,
        }
    }

    pub fn memory_snapshot(&self) -> WorkMapMemorySnapshot {
        let mut memory = self
            .records
            .values()
            .filter_map(|record| match record {
                WorkMapRecord::Memory(memory) => Some(memory.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        memory.sort_by_key(|record| (record.created_at_ms, record.id.clone()));
        WorkMapMemorySnapshot {
            facts: memory
                .iter()
                .filter(|record| record.kind == MemoryKind::Fact)
                .cloned()
                .collect(),
            agreements: memory
                .into_iter()
                .filter(|record| record.kind == MemoryKind::Agreement)
                .collect(),
        }
    }

    pub fn active_memory_snapshot(&self) -> WorkMapMemorySnapshot {
        let mut memory = self.memory_snapshot();
        memory
            .facts
            .retain(|record| record.state == MemoryState::Active);
        memory
            .agreements
            .retain(|record| record.state == MemoryState::Active);
        memory
    }

    pub fn current_snapshot(&self) -> Option<WorkMapObjectiveSnapshot> {
        self.active_objective()
            .and_then(|objective| self.objective_snapshot(&objective.id))
    }

    pub fn history_summaries(&self) -> Vec<WorkMapHistorySummary> {
        self.snapshot()
            .history
            .into_iter()
            .map(|snapshot| WorkMapHistorySummary {
                plan_count: snapshot.plans.len(),
                note_count: snapshot.plans.iter().map(|plan| plan.notes.len()).sum(),
                objective: snapshot.objective,
            })
            .collect()
    }

    pub fn history_objective(&self, id: &str) -> Option<WorkMapObjectiveSnapshot> {
        match self.records.get(id) {
            Some(WorkMapRecord::Objective(objective)) if objective.state.is_history() => {
                self.objective_snapshot(id)
            }
            _ => None,
        }
    }

    fn objective_snapshot(&self, id: &str) -> Option<WorkMapObjectiveSnapshot> {
        let WorkMapRecord::Objective(objective) = self.records.get(id)? else {
            return None;
        };
        let plans = self
            .plans_for(id)
            .into_iter()
            .map(|plan| WorkMapPlanSnapshot {
                notes: self.notes_for(&plan.id),
                plan: plan.clone(),
            })
            .collect();
        Some(WorkMapObjectiveSnapshot {
            objective: objective.clone(),
            plans,
        })
    }

    fn active_objective(&self) -> Option<&WorkMapObjective> {
        self.records.values().find_map(|record| match record {
            WorkMapRecord::Objective(objective) if objective.state == ObjectiveState::Active => {
                Some(objective)
            }
            _ => None,
        })
    }

    fn objectives(&self) -> Vec<&WorkMapObjective> {
        self.records
            .values()
            .filter_map(|record| match record {
                WorkMapRecord::Objective(objective) => Some(objective),
                _ => None,
            })
            .collect()
    }

    fn plans_for(&self, objective_id: &str) -> Vec<&WorkMapPlan> {
        let mut plans = self
            .records
            .values()
            .filter_map(|record| match record {
                WorkMapRecord::Plan(plan) if plan.objective_id == objective_id => Some(plan),
                _ => None,
            })
            .collect::<Vec<_>>();
        plans.sort_by_key(|plan| (plan.order, plan.id.as_str()));
        plans
    }

    fn notes_for(&self, plan_id: &str) -> Vec<WorkMapNote> {
        let mut notes = self
            .records
            .values()
            .filter_map(|record| match record {
                WorkMapRecord::Note(note) if note.plan_id == plan_id => Some(note.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        notes.sort_by_key(|note| (note.sequence, note.id.clone()));
        notes
    }

    fn plan(&self, id: &str) -> Option<&WorkMapPlan> {
        match self.records.get(id) {
            Some(WorkMapRecord::Plan(plan)) => Some(plan),
            _ => None,
        }
    }

    fn memory(&self, id: &str) -> Option<&WorkMapMemory> {
        match self.records.get(id) {
            Some(WorkMapRecord::Memory(memory)) => Some(memory),
            _ => None,
        }
    }

    fn next_note_sequence(&self, plan_id: &str) -> u64 {
        self.notes_for(plan_id)
            .last()
            .map(|note| note.sequence + 1)
            .unwrap_or(1)
    }

    fn contains(&self, id: &str) -> bool {
        self.records.contains_key(id)
    }
}

pub fn execute(
    full_name: &str,
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let projection = WorkMapProjection::from_events(edb.events()).map_err(protocol_error)?;
    match full_name {
        READ => {
            parse_arguments::<EmptyInput>(arguments)?;
            Ok(json!({
                "memory": projection.active_memory_snapshot(),
                "current": projection.current_snapshot()
            }))
        }
        READ_HISTORY => read_history(arguments, &projection),
        START => start(arguments, tool_call_id, edb, &projection),
        UPDATE_PLAN_STATE => update_plan_state(arguments, tool_call_id, edb, &projection),
        ADD_NOTE => add_note(arguments, tool_call_id, edb, &projection),
        CHANGE_PLAN => change_plan(arguments, tool_call_id, edb, &projection),
        ADD_PLAN => add_plan(arguments, tool_call_id, edb, &projection),
        CLOSE_OBJECTIVE => close_objective(arguments, tool_call_id, edb, &projection),
        ADD_MEMORY => add_memory(arguments, tool_call_id, edb, &projection),
        INVALIDATE_MEMORY => invalidate_memory(arguments, tool_call_id, edb, &projection),
        _ => Err(tool_error(
            "unknown_tool",
            format!("native WorkMap tool {full_name} does not exist"),
        )),
    }
}

pub fn persisted_mutation_result(events: &[Event], tool_call_id: EventId) -> Option<Value> {
    let (index, event) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match event {
            Event::WorkMapMutation(event) if event.tool_call_id == tool_call_id => {
                Some((index, event))
            }
            _ => None,
        })?;
    let projection = WorkMapProjection::from_events(&events[..=index]).ok()?;
    if event.mutation.operation == WorkMapOperation::PlanStateUpdated {
        return Some(json!({"current": projection.current_snapshot()}));
    }
    Some(json!({
        "memory": projection.active_memory_snapshot(),
        "current": projection.current_snapshot(),
        "records": event.mutation.records
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemInput {
    title: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadHistoryInput {
    #[serde(default)]
    objective_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    objective: ItemInput,
    plans: Vec<ItemInput>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PlanTerminalState {
    Completed,
    Cancelled,
    Superseded,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlanStateInput {
    plan_id: String,
    state: PlanTerminalState,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    verification: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddNoteInput {
    plan_id: String,
    kind: NoteKind,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePlanInput {
    plan_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    clear_description: bool,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddPlanInput {
    plan: ItemInput,
    #[serde(default)]
    after_plan_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ObjectiveCloseState {
    Cancelled,
    Superseded,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseObjectiveInput {
    state: ObjectiveCloseState,
    reason: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryInput {
    kind: MemoryKind,
    #[serde(default)]
    basis: Option<MemoryBasis>,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidateMemoryInput {
    memory_id: String,
    reason: String,
    #[serde(default)]
    replacement: Option<MemoryInput>,
}

fn read_history(
    arguments: &str,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: ReadHistoryInput = parse_arguments(arguments)?;
    match input.objective_id {
        Some(id) => {
            validate_id(&id, "objective").map_err(invalid_arguments)?;
            let objective = projection.history_objective(&id).ok_or_else(|| {
                tool_error(
                    "history_not_found",
                    format!("closed WorkMap Objective {id} does not exist"),
                )
            })?;
            Ok(json!({"objective": objective}))
        }
        None => Ok(json!({"objectives": projection.history_summaries()})),
    }
}

fn start(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: StartInput = parse_arguments(arguments)?;
    if projection.active_objective().is_some() {
        return Err(tool_error(
            "objective_already_active",
            "WorkMap already has a Current Objective; resume or close it instead of Start",
        ));
    }
    if input.plans.is_empty() || input.plans.len() > 100 {
        return Err(invalid_arguments(
            "plans must contain between 1 and 100 items",
        ));
    }
    validate_item("objective", &input.objective).map_err(invalid_arguments)?;
    for plan in &input.plans {
        validate_item("plan", plan).map_err(invalid_arguments)?;
    }
    let mut reserved = BTreeSet::new();
    let objective = new_objective(
        input.objective,
        edb.next_event_id(),
        projection,
        &mut reserved,
    );
    reserved.insert(objective.id.clone());
    let mut records = vec![WorkMapRecord::Objective(objective.clone())];
    for (index, plan) in input.plans.into_iter().enumerate() {
        records.push(WorkMapRecord::Plan(new_plan(
            plan,
            &objective.id,
            index as u64 + 1,
            if index == 0 {
                PlanState::Active
            } else {
                PlanState::Planned
            },
            edb.next_event_id(),
            index as u64,
            projection,
            &mut reserved,
        )));
    }
    append_mutation(edb, tool_call_id, WorkMapOperation::Started, "", records)
}

fn update_plan_state(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: UpdatePlanStateInput = parse_arguments(arguments)?;
    let objective = projection.active_objective().ok_or_else(no_current)?;
    let mut plan = current_objective_plan(projection, objective, &input.plan_id)?.clone();
    match input.state {
        PlanTerminalState::Completed if input.reason.is_some() => {
            return Err(invalid_arguments(
                "reason is not accepted when state is completed",
            ));
        }
        PlanTerminalState::Cancelled | PlanTerminalState::Superseded
            if input.outcome.is_some() || input.verification.is_some() =>
        {
            return Err(invalid_arguments(
                "outcome and verification are accepted only when state is completed",
            ));
        }
        _ => {}
    }
    let mutation_reason = input.reason.clone().unwrap_or_default();
    let previous_state = plan.state;
    plan.revision += 1;
    match input.state {
        PlanTerminalState::Completed => {
            if previous_state != PlanState::Active {
                return Err(tool_error(
                    "plan_not_active",
                    "only the active Plan can be completed",
                ));
            }
            plan.state = PlanState::Completed;
            plan.outcome = clean_required("outcome", input.outcome)?;
            plan.verification = clean_required("verification", input.verification)?;
            plan.status_reason = None;
        }
        PlanTerminalState::Cancelled | PlanTerminalState::Superseded => {
            if !previous_state.is_open() {
                return Err(tool_error(
                    "plan_already_closed",
                    format!("WorkMap Plan {} is already closed", plan.id),
                ));
            }
            plan.state = match input.state {
                PlanTerminalState::Cancelled => PlanState::Cancelled,
                PlanTerminalState::Superseded => PlanState::Superseded,
                PlanTerminalState::Completed => unreachable!(),
            };
            plan.outcome = None;
            plan.verification = None;
            plan.status_reason = clean_required("reason", input.reason)?;
        }
    }
    validate_plan(&plan).map_err(invalid_arguments)?;

    let mut records = vec![WorkMapRecord::Plan(plan.clone())];
    if previous_state == PlanState::Active
        && let Some(next) = projection
            .plans_for(&objective.id)
            .into_iter()
            .filter(|candidate| candidate.id != plan.id && candidate.state == PlanState::Planned)
            .min_by_key(|candidate| candidate.order)
    {
        let mut next = next.clone();
        next.revision += 1;
        next.state = PlanState::Active;
        records.push(WorkMapRecord::Plan(next));
    }

    let effective = effective_plans(projection, &objective.id, &records);
    let has_open = effective.iter().any(|plan| plan.state.is_open());
    let has_completed = effective
        .iter()
        .any(|plan| plan.state == PlanState::Completed);
    if !has_open && has_completed {
        let mut completed = objective.clone();
        completed.revision += 1;
        completed.state = ObjectiveState::Completed;
        completed.status_reason = None;
        records.push(WorkMapRecord::Objective(completed));
    }

    append_mutation(
        edb,
        tool_call_id,
        WorkMapOperation::PlanStateUpdated,
        &mutation_reason,
        records,
    )
}

fn add_note(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: AddNoteInput = parse_arguments(arguments)?;
    require_nonempty("content", &input.content).map_err(invalid_arguments)?;
    let objective = projection.active_objective().ok_or_else(no_current)?;
    let plan = current_objective_plan(projection, objective, &input.plan_id)?;
    let note = WorkMapNote {
        id: new_id("note", edb.next_event_id(), 0, projection, &BTreeSet::new()),
        revision: 1,
        created_at_ms: 0,
        updated_at_ms: 0,
        plan_id: plan.id.clone(),
        sequence: projection.next_note_sequence(&plan.id),
        kind: input.kind,
        content: input.content,
    };
    append_mutation(
        edb,
        tool_call_id,
        WorkMapOperation::NoteAdded,
        "",
        vec![WorkMapRecord::Note(note)],
    )
}

fn change_plan(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: ChangePlanInput = parse_arguments(arguments)?;
    require_nonempty("reason", &input.reason).map_err(invalid_arguments)?;
    if input.description.is_some() && input.clear_description {
        return Err(invalid_arguments(
            "description cannot be set and cleared together",
        ));
    }
    if input.title.is_none() && input.description.is_none() && !input.clear_description {
        return Err(invalid_arguments("ChangePlan contains no change"));
    }
    let objective = projection.active_objective().ok_or_else(no_current)?;
    let mut plan = current_objective_plan(projection, objective, &input.plan_id)?.clone();
    if !plan.state.is_open() {
        return Err(tool_error(
            "plan_already_closed",
            "closed Plans cannot be changed",
        ));
    }
    plan.revision += 1;
    if let Some(title) = input.title {
        plan.title = title;
    }
    if input.clear_description {
        plan.description = None;
    } else if input.description.is_some() {
        plan.description = clean_optional(input.description);
    }
    validate_plan(&plan).map_err(invalid_arguments)?;
    append_mutation(
        edb,
        tool_call_id,
        WorkMapOperation::PlanChanged,
        &input.reason,
        vec![WorkMapRecord::Plan(plan)],
    )
}

fn add_plan(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: AddPlanInput = parse_arguments(arguments)?;
    validate_item("plan", &input.plan).map_err(invalid_arguments)?;
    let objective = projection.active_objective().ok_or_else(no_current)?;
    let plans = projection.plans_for(&objective.id);
    if plans.len() >= 100 {
        return Err(invalid_arguments("an Objective cannot exceed 100 Plans"));
    }
    let insertion_index = match input.after_plan_id.as_deref() {
        Some(id) => plans
            .iter()
            .position(|plan| plan.id == id)
            .map(|index| index + 1)
            .ok_or_else(|| {
                tool_error(
                    "plan_not_found",
                    format!("WorkMap Plan {id} does not belong to Current"),
                )
            })?,
        None => plans.len(),
    };
    if let Some(active_index) = plans
        .iter()
        .position(|plan| plan.state == PlanState::Active)
        && insertion_index <= active_index
    {
        return Err(tool_error(
            "invalid_insertion_point",
            "a new Plan cannot be inserted before the active Plan",
        ));
    }

    let has_open = plans.iter().any(|plan| plan.state.is_open());
    let mut records = Vec::new();
    for plan in plans.iter().skip(insertion_index) {
        let mut shifted = (*plan).clone();
        shifted.revision += 1;
        shifted.order += 1;
        records.push(WorkMapRecord::Plan(shifted));
    }
    let mut reserved = records
        .iter()
        .map(|record| record.id().to_owned())
        .collect::<BTreeSet<_>>();
    records.push(WorkMapRecord::Plan(new_plan(
        input.plan,
        &objective.id,
        insertion_index as u64 + 1,
        if has_open {
            PlanState::Planned
        } else {
            PlanState::Active
        },
        edb.next_event_id(),
        insertion_index as u64,
        projection,
        &mut reserved,
    )));
    append_mutation(edb, tool_call_id, WorkMapOperation::PlanAdded, "", records)
}

fn close_objective(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: CloseObjectiveInput = parse_arguments(arguments)?;
    require_nonempty("reason", &input.reason).map_err(invalid_arguments)?;
    let objective = projection.active_objective().ok_or_else(no_current)?;
    let objective_state = match input.state {
        ObjectiveCloseState::Cancelled => ObjectiveState::Cancelled,
        ObjectiveCloseState::Superseded => ObjectiveState::Superseded,
    };
    let plan_state = match input.state {
        ObjectiveCloseState::Cancelled => PlanState::Cancelled,
        ObjectiveCloseState::Superseded => PlanState::Superseded,
    };
    let mut closed = objective.clone();
    closed.revision += 1;
    closed.state = objective_state;
    closed.status_reason = Some(input.reason.clone());
    let mut records = vec![WorkMapRecord::Objective(closed)];
    for plan in projection.plans_for(&objective.id) {
        if plan.state.is_open() {
            let mut plan = plan.clone();
            plan.revision += 1;
            plan.state = plan_state;
            plan.outcome = None;
            plan.verification = None;
            plan.status_reason = Some(input.reason.clone());
            records.push(WorkMapRecord::Plan(plan));
        }
    }
    append_mutation(
        edb,
        tool_call_id,
        WorkMapOperation::ObjectiveClosed,
        &input.reason,
        records,
    )
}

fn add_memory(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: MemoryInput = parse_arguments(arguments)?;
    validate_memory_input(&input).map_err(invalid_arguments)?;
    let memory = new_memory(
        input,
        edb.next_event_id(),
        0,
        projection,
        &mut BTreeSet::new(),
    );
    append_mutation(
        edb,
        tool_call_id,
        WorkMapOperation::MemoryAdded,
        "",
        vec![WorkMapRecord::Memory(memory)],
    )
}

fn invalidate_memory(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
    projection: &WorkMapProjection,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: InvalidateMemoryInput = parse_arguments(arguments)?;
    validate_id(&input.memory_id, "memory").map_err(invalid_arguments)?;
    require_nonempty("reason", &input.reason).map_err(invalid_arguments)?;
    if let Some(replacement) = &input.replacement {
        validate_memory_input(replacement).map_err(invalid_arguments)?;
    }
    let mut previous = projection
        .memory(&input.memory_id)
        .ok_or_else(|| tool_error("memory_not_found", "WorkMap Memory does not exist"))?
        .clone();
    if previous.state != MemoryState::Active {
        return Err(tool_error(
            "memory_inactive",
            "only active WorkMap Memory can be invalidated",
        ));
    }

    previous.revision += 1;
    previous.status_reason = Some(input.reason.clone());
    let mut records = Vec::with_capacity(2);
    if let Some(replacement) = input.replacement {
        let mut reserved = BTreeSet::new();
        let replacement = new_memory(
            replacement,
            edb.next_event_id(),
            0,
            projection,
            &mut reserved,
        );
        previous.state = MemoryState::Superseded;
        previous.replacement_id = Some(replacement.id.clone());
        records.push(WorkMapRecord::Memory(previous));
        records.push(WorkMapRecord::Memory(replacement));
    } else {
        previous.state = MemoryState::Retracted;
        previous.replacement_id = None;
        records.push(WorkMapRecord::Memory(previous));
    }
    append_mutation(
        edb,
        tool_call_id,
        WorkMapOperation::MemoryInvalidated,
        &input.reason,
        records,
    )
}

fn append_mutation(
    edb: &mut EventDataBase,
    tool_call_id: EventId,
    operation: WorkMapOperation,
    reason: &str,
    records: Vec<WorkMapRecord>,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let mutation = WorkMapMutation {
        operation,
        reason: reason.into(),
        records,
    };
    edb.append_workmap_mutation(tool_call_id, mutation)
        .map_err(protocol_error)?;
    persisted_mutation_result(edb.events(), tool_call_id)
        .ok_or_else(|| protocol_error("persisted WorkMap mutation is missing"))
}

fn new_objective(
    input: ItemInput,
    event_id: EventId,
    projection: &WorkMapProjection,
    reserved: &mut BTreeSet<String>,
) -> WorkMapObjective {
    WorkMapObjective {
        id: new_id("objective", event_id, 0, projection, reserved),
        revision: 1,
        created_at_ms: 0,
        updated_at_ms: 0,
        state: ObjectiveState::Active,
        title: input.title,
        description: clean_optional(input.description),
        status_reason: None,
    }
}

fn new_memory(
    input: MemoryInput,
    event_id: EventId,
    ordinal: u64,
    projection: &WorkMapProjection,
    reserved: &mut BTreeSet<String>,
) -> WorkMapMemory {
    let id = new_id("memory", event_id, ordinal, projection, reserved);
    reserved.insert(id.clone());
    WorkMapMemory {
        id,
        revision: 1,
        created_at_ms: 0,
        updated_at_ms: 0,
        kind: input.kind,
        state: MemoryState::Active,
        basis: input.basis,
        content: input.content,
        status_reason: None,
        replacement_id: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn new_plan(
    input: ItemInput,
    objective_id: &str,
    order: u64,
    state: PlanState,
    event_id: EventId,
    ordinal: u64,
    projection: &WorkMapProjection,
    reserved: &mut BTreeSet<String>,
) -> WorkMapPlan {
    let id = new_id("plan", event_id, ordinal, projection, reserved);
    reserved.insert(id.clone());
    WorkMapPlan {
        id,
        revision: 1,
        created_at_ms: 0,
        updated_at_ms: 0,
        objective_id: objective_id.into(),
        order,
        state,
        title: input.title,
        description: clean_optional(input.description),
        outcome: None,
        verification: None,
        status_reason: None,
    }
}

fn current_objective_plan<'a>(
    projection: &'a WorkMapProjection,
    objective: &WorkMapObjective,
    id: &str,
) -> std::result::Result<&'a WorkMapPlan, ToolboxExecutionError> {
    match projection.plan(id) {
        Some(plan) if plan.objective_id == objective.id => Ok(plan),
        _ => Err(tool_error(
            "plan_not_found",
            format!("WorkMap Plan {id} does not belong to Current"),
        )),
    }
}

fn effective_plans(
    projection: &WorkMapProjection,
    objective_id: &str,
    updates: &[WorkMapRecord],
) -> Vec<WorkMapPlan> {
    let mut plans = projection
        .plans_for(objective_id)
        .into_iter()
        .cloned()
        .map(|plan| (plan.id.clone(), plan))
        .collect::<BTreeMap<_, _>>();
    for record in updates {
        if let WorkMapRecord::Plan(plan) = record {
            plans.insert(plan.id.clone(), plan.clone());
        }
    }
    plans.into_values().collect()
}

fn validate_mutation_shape(mutation: &WorkMapMutation) -> Result<()> {
    if mutation.records.is_empty() {
        return Err("WorkMap mutation has no records".into());
    }
    let mut ids = BTreeSet::new();
    if mutation
        .records
        .iter()
        .any(|record| !ids.insert(record.id()))
    {
        return Err("WorkMap mutation repeats a record ID".into());
    }
    let valid = match mutation.operation {
        WorkMapOperation::Started => {
            mutation.records.len() >= 2
                && matches!(mutation.records.first(), Some(WorkMapRecord::Objective(_)))
                && mutation.records[1..]
                    .iter()
                    .all(|record| matches!(record, WorkMapRecord::Plan(_)))
        }
        WorkMapOperation::PlanStateUpdated => mutation
            .records
            .iter()
            .all(|record| matches!(record, WorkMapRecord::Plan(_) | WorkMapRecord::Objective(_))),
        WorkMapOperation::NoteAdded => {
            matches!(&mutation.records[..], [WorkMapRecord::Note(_)])
        }
        WorkMapOperation::PlanChanged => {
            matches!(&mutation.records[..], [WorkMapRecord::Plan(_)])
        }
        WorkMapOperation::PlanAdded => mutation
            .records
            .iter()
            .all(|record| matches!(record, WorkMapRecord::Plan(_))),
        WorkMapOperation::ObjectiveClosed => {
            matches!(mutation.records.first(), Some(WorkMapRecord::Objective(_)))
                && mutation.records[1..]
                    .iter()
                    .all(|record| matches!(record, WorkMapRecord::Plan(_)))
        }
        WorkMapOperation::MemoryAdded => {
            matches!(&mutation.records[..], [WorkMapRecord::Memory(_)])
        }
        WorkMapOperation::MemoryInvalidated => matches!(
            &mutation.records[..],
            [WorkMapRecord::Memory(_)] | [WorkMapRecord::Memory(_), WorkMapRecord::Memory(_)]
        ),
    };
    if !valid {
        return Err(format!(
            "invalid records for WorkMap operation {:?}",
            mutation.operation
        )
        .into());
    }
    if matches!(
        mutation.operation,
        WorkMapOperation::PlanChanged
            | WorkMapOperation::ObjectiveClosed
            | WorkMapOperation::MemoryInvalidated
    ) {
        require_nonempty("WorkMap mutation reason", &mutation.reason)?;
    }
    Ok(())
}

fn validate_operation_transition(
    mutation: &WorkMapMutation,
    existing: &BTreeMap<String, WorkMapRecord>,
) -> Result<()> {
    let active_objective = active_objective_in(existing);
    match mutation.operation {
        WorkMapOperation::Started => {
            if active_objective.is_some() {
                return Err("WorkMap Started requires no Current Objective".into());
            }
            let [WorkMapRecord::Objective(objective), plans @ ..] = &mutation.records[..] else {
                return Err("invalid WorkMap Started records".into());
            };
            if existing.contains_key(&objective.id)
                || objective.state != ObjectiveState::Active
                || plans.is_empty()
            {
                return Err("invalid WorkMap Started transition".into());
            }
            for (index, record) in plans.iter().enumerate() {
                let WorkMapRecord::Plan(plan) = record else {
                    unreachable!()
                };
                let expected_state = if index == 0 {
                    PlanState::Active
                } else {
                    PlanState::Planned
                };
                if existing.contains_key(&plan.id)
                    || plan.objective_id != objective.id
                    || plan.order != index as u64 + 1
                    || plan.state != expected_state
                {
                    return Err("invalid WorkMap Started Plan transition".into());
                }
            }
        }
        WorkMapOperation::NoteAdded => match &mutation.records[..] {
            [WorkMapRecord::Note(note)]
                if !existing.contains_key(&note.id)
                    && active_objective.is_some_and(|objective| {
                        matches!(
                            existing.get(&note.plan_id),
                            Some(WorkMapRecord::Plan(plan))
                                if plan.objective_id == objective.id
                        )
                    }) => {}
            _ => return Err("invalid WorkMap NoteAdded transition".into()),
        },
        WorkMapOperation::PlanChanged => match &mutation.records[..] {
            [WorkMapRecord::Plan(plan)]
                if active_objective.is_some_and(|objective| plan.objective_id == objective.id)
                    && matches!(
                        existing.get(&plan.id),
                        Some(WorkMapRecord::Plan(previous))
                            if previous.state.is_open()
                                && previous.state == plan.state
                                && previous.order == plan.order
                                && previous.objective_id == plan.objective_id
                    ) => {}
            _ => return Err("invalid WorkMap PlanChanged transition".into()),
        },
        WorkMapOperation::PlanAdded => {
            let Some(objective) = active_objective else {
                return Err("WorkMap PlanAdded requires a Current Objective".into());
            };
            let new = mutation
                .records
                .iter()
                .filter_map(|record| match record {
                    WorkMapRecord::Plan(plan) if !existing.contains_key(&plan.id) => Some(plan),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if new.len() != 1 {
                return Err("invalid WorkMap PlanAdded transition".into());
            }
            let new = new[0];
            let previous = existing
                .values()
                .filter_map(|record| match record {
                    WorkMapRecord::Plan(plan) if plan.objective_id == objective.id => Some(plan),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let expected_state = if previous.iter().any(|plan| plan.state.is_open()) {
                PlanState::Planned
            } else {
                PlanState::Active
            };
            if new.objective_id != objective.id
                || new.order == 0
                || new.order > previous.len() as u64 + 1
                || new.state != expected_state
            {
                return Err("invalid new WorkMap Plan placement".into());
            }
            let expected_shifted = previous
                .iter()
                .filter(|plan| plan.order >= new.order)
                .map(|plan| plan.id.as_str())
                .collect::<BTreeSet<_>>();
            let actual_shifted = mutation
                .records
                .iter()
                .filter_map(|record| match record {
                    WorkMapRecord::Plan(plan) if plan.id != new.id => Some(plan.id.as_str()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            if actual_shifted != expected_shifted
                || mutation.records.iter().any(|record| match record {
                    WorkMapRecord::Plan(plan) if plan.id == new.id => false,
                    WorkMapRecord::Plan(plan) => match existing.get(&plan.id) {
                        Some(WorkMapRecord::Plan(previous)) => {
                            !same_plan_definition(previous, plan)
                                || plan.order != previous.order + 1
                                || plan.state != previous.state
                                || plan.outcome != previous.outcome
                                || plan.verification != previous.verification
                                || plan.status_reason != previous.status_reason
                        }
                        _ => true,
                    },
                    _ => true,
                })
            {
                return Err("invalid shifted WorkMap Plan records".into());
            }
        }
        WorkMapOperation::PlanStateUpdated => {
            validate_plan_state_transition(mutation, existing, active_objective)?;
        }
        WorkMapOperation::ObjectiveClosed => {
            let Some(active) = active_objective else {
                return Err("WorkMap ObjectiveClosed requires a Current Objective".into());
            };
            let [WorkMapRecord::Objective(closed), plans @ ..] = &mutation.records[..] else {
                return Err("invalid WorkMap ObjectiveClosed records".into());
            };
            if closed.id != active.id
                || !matches!(
                    closed.state,
                    ObjectiveState::Cancelled | ObjectiveState::Superseded
                )
                || !same_objective_definition(active, closed)
                || closed.status_reason.as_deref() != Some(mutation.reason.as_str())
            {
                return Err("invalid WorkMap ObjectiveClosed transition".into());
            }
            let expected_open = existing
                .values()
                .filter_map(|record| match record {
                    WorkMapRecord::Plan(plan)
                        if plan.objective_id == active.id && plan.state.is_open() =>
                    {
                        Some(plan.id.as_str())
                    }
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            let actual = plans
                .iter()
                .filter_map(|record| match record {
                    WorkMapRecord::Plan(plan) => Some(plan.id.as_str()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            let expected_plan_state = if closed.state == ObjectiveState::Cancelled {
                PlanState::Cancelled
            } else {
                PlanState::Superseded
            };
            if actual != expected_open
                || plans.iter().any(|record| match record {
                    WorkMapRecord::Plan(plan) => {
                        !matches!(existing.get(&plan.id), Some(WorkMapRecord::Plan(previous)) if same_plan_definition(previous, plan) && previous.order == plan.order)
                            || plan.objective_id != active.id
                            || plan.state != expected_plan_state
                            || plan.status_reason.as_deref() != Some(mutation.reason.as_str())
                    }
                    _ => true,
                })
            {
                return Err("invalid WorkMap ObjectiveClosed Plan transition".into());
            }
        }
        WorkMapOperation::MemoryAdded => match &mutation.records[..] {
            [WorkMapRecord::Memory(memory)]
                if mutation.reason.is_empty()
                    && !existing.contains_key(&memory.id)
                    && memory.state == MemoryState::Active => {}
            _ => return Err("invalid WorkMap MemoryAdded transition".into()),
        },
        WorkMapOperation::MemoryInvalidated => {
            validate_memory_invalidation(mutation, existing)?;
        }
    }
    Ok(())
}

fn validate_memory_invalidation(
    mutation: &WorkMapMutation,
    existing: &BTreeMap<String, WorkMapRecord>,
) -> Result<()> {
    let WorkMapRecord::Memory(invalidated) = &mutation.records[0] else {
        unreachable!()
    };
    let Some(WorkMapRecord::Memory(previous)) = existing.get(&invalidated.id) else {
        return Err("invalidated WorkMap Memory does not exist".into());
    };
    if previous.state != MemoryState::Active
        || !same_memory_definition(previous, invalidated)
        || invalidated.status_reason.as_deref() != Some(mutation.reason.as_str())
    {
        return Err("invalid WorkMap MemoryInvalidated transition".into());
    }
    match &mutation.records[1..] {
        [] if invalidated.state == MemoryState::Retracted
            && invalidated.replacement_id.is_none() => {}
        [WorkMapRecord::Memory(replacement)]
            if invalidated.state == MemoryState::Superseded
                && invalidated.replacement_id.as_deref() == Some(replacement.id.as_str())
                && replacement.state == MemoryState::Active
                && !existing.contains_key(&replacement.id) => {}
        _ => return Err("invalid WorkMap Memory replacement transition".into()),
    }
    Ok(())
}

fn validate_plan_state_transition(
    mutation: &WorkMapMutation,
    existing: &BTreeMap<String, WorkMapRecord>,
    active_objective: Option<&WorkMapObjective>,
) -> Result<()> {
    let Some(objective) = active_objective else {
        return Err("WorkMap PlanStateUpdated requires a Current Objective".into());
    };
    let plans = mutation
        .records
        .iter()
        .filter_map(|record| match record {
            WorkMapRecord::Plan(plan) => Some(plan),
            _ => None,
        })
        .collect::<Vec<_>>();
    let objectives = mutation
        .records
        .iter()
        .filter_map(|record| match record {
            WorkMapRecord::Objective(objective) => Some(objective),
            _ => None,
        })
        .collect::<Vec<_>>();
    let terminal = plans
        .iter()
        .filter(|plan| match existing.get(&plan.id) {
            Some(WorkMapRecord::Plan(previous)) => {
                previous.state.is_open() && plan.state.is_terminal()
            }
            _ => false,
        })
        .copied()
        .collect::<Vec<_>>();
    let activated = plans
        .iter()
        .filter(|plan| match existing.get(&plan.id) {
            Some(WorkMapRecord::Plan(previous)) => {
                previous.state == PlanState::Planned && plan.state == PlanState::Active
            }
            _ => false,
        })
        .copied()
        .collect::<Vec<_>>();
    if terminal.len() != 1 || activated.len() > 1 || plans.len() != terminal.len() + activated.len()
    {
        return Err("invalid WorkMap PlanStateUpdated Plan transitions".into());
    }
    let target = terminal[0];
    let Some(WorkMapRecord::Plan(previous)) = existing.get(&target.id) else {
        unreachable!()
    };
    if target.objective_id != objective.id
        || !same_plan_definition(previous, target)
        || target.order != previous.order
        || (target.state == PlanState::Completed && previous.state != PlanState::Active)
        || plans.iter().any(|plan| {
            plan.objective_id != objective.id
                || !matches!(
                    existing.get(&plan.id),
                    Some(WorkMapRecord::Plan(previous))
                        if previous.objective_id == plan.objective_id
                            && same_plan_definition(previous, plan)
                            && previous.order == plan.order
                )
        })
    {
        return Err("invalid WorkMap PlanStateUpdated target".into());
    }
    match target.state {
        PlanState::Completed if !mutation.reason.is_empty() => {
            return Err("completed WorkMap Plan cannot carry a mutation reason".into());
        }
        PlanState::Cancelled | PlanState::Superseded
            if target.status_reason.as_deref() != Some(mutation.reason.as_str()) =>
        {
            return Err("closed WorkMap Plan reason does not match its mutation".into());
        }
        _ => {}
    }

    let effective = effective_plans_from_records(existing, &objective.id, &mutation.records);
    let open = effective
        .iter()
        .filter(|plan| plan.state.is_open())
        .collect::<Vec<_>>();
    let active = open
        .iter()
        .filter(|plan| plan.state == PlanState::Active)
        .collect::<Vec<_>>();
    if open.is_empty() {
        let has_completed = effective
            .iter()
            .any(|plan| plan.state == PlanState::Completed);
        if has_completed {
            if objectives.len() != 1
                || objectives[0].id != objective.id
                || objectives[0].state != ObjectiveState::Completed
                || !same_objective_definition(objective, objectives[0])
            {
                return Err("completed Plans must close the Objective".into());
            }
        } else if !objectives.is_empty() {
            return Err("uncompleted Plans cannot complete the Objective".into());
        }
    } else {
        if active.len() != 1 || !objectives.is_empty() {
            return Err("open Plans require exactly one active Plan".into());
        }
        if previous.state == PlanState::Active {
            let expected = open
                .iter()
                .filter(|plan| plan.state == PlanState::Active)
                .min_by_key(|plan| plan.order)
                .map(|plan| plan.id.as_str());
            if activated.len() != 1 || Some(activated[0].id.as_str()) != expected {
                return Err("closing the active Plan must activate the next Plan".into());
            }
        } else if !activated.is_empty() {
            return Err("closing a future Plan cannot activate another Plan".into());
        }
    }
    Ok(())
}

fn same_objective_definition(left: &WorkMapObjective, right: &WorkMapObjective) -> bool {
    left.id == right.id
        && left.title == right.title
        && left.description == right.description
        && left.created_at_ms == right.created_at_ms
}

fn same_plan_definition(left: &WorkMapPlan, right: &WorkMapPlan) -> bool {
    left.id == right.id
        && left.objective_id == right.objective_id
        && left.title == right.title
        && left.description == right.description
        && left.created_at_ms == right.created_at_ms
}

fn same_memory_definition(left: &WorkMapMemory, right: &WorkMapMemory) -> bool {
    left.id == right.id
        && left.kind == right.kind
        && left.basis == right.basis
        && left.content == right.content
        && left.created_at_ms == right.created_at_ms
}

fn effective_plans_from_records(
    existing: &BTreeMap<String, WorkMapRecord>,
    objective_id: &str,
    updates: &[WorkMapRecord],
) -> Vec<WorkMapPlan> {
    let mut plans = existing
        .values()
        .filter_map(|record| match record {
            WorkMapRecord::Plan(plan) if plan.objective_id == objective_id => {
                Some((plan.id.clone(), plan.clone()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    for record in updates {
        if let WorkMapRecord::Plan(plan) = record {
            plans.insert(plan.id.clone(), plan.clone());
        }
    }
    plans.into_values().collect()
}

fn validate_record(
    record: &WorkMapRecord,
    existing: &BTreeMap<String, WorkMapRecord>,
) -> Result<()> {
    match record {
        WorkMapRecord::Objective(objective) => validate_objective(objective),
        WorkMapRecord::Plan(plan) => validate_plan(plan),
        WorkMapRecord::Note(note) => {
            validate_id(&note.id, "note")?;
            validate_id(&note.plan_id, "plan")?;
            require_nonempty("WorkMap Note content", &note.content)?;
            if note.revision != 1 {
                return Err("WorkMap Notes are immutable revision 1 records".into());
            }
            if !matches!(existing.get(&note.plan_id), Some(WorkMapRecord::Plan(_))) {
                return Err(format!(
                    "WorkMap Note {} references missing Plan {}",
                    note.id, note.plan_id
                )
                .into());
            }
            let expected = existing
                .values()
                .filter_map(|record| match record {
                    WorkMapRecord::Note(previous) if previous.plan_id == note.plan_id => {
                        Some(previous.sequence)
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0)
                + 1;
            if note.sequence != expected {
                return Err(format!(
                    "WorkMap Note {} sequence {} must be {}",
                    note.id, note.sequence, expected
                )
                .into());
            }
            Ok(())
        }
        WorkMapRecord::Memory(memory) => validate_memory(memory),
    }
}

fn validate_objective(objective: &WorkMapObjective) -> Result<()> {
    validate_id(&objective.id, "objective")?;
    require_nonempty("WorkMap Objective title", &objective.title)?;
    validate_optional("WorkMap Objective description", &objective.description)?;
    match objective.state {
        ObjectiveState::Active | ObjectiveState::Completed => {
            if objective.status_reason.is_some() {
                return Err("active/completed Objective cannot have status_reason".into());
            }
        }
        ObjectiveState::Cancelled | ObjectiveState::Superseded => {
            require_nonempty(
                "closed WorkMap Objective status_reason",
                objective.status_reason.as_deref().unwrap_or(""),
            )?;
        }
    }
    Ok(())
}

fn validate_plan(plan: &WorkMapPlan) -> Result<()> {
    validate_id(&plan.id, "plan")?;
    validate_id(&plan.objective_id, "objective")?;
    require_nonempty("WorkMap Plan title", &plan.title)?;
    validate_optional("WorkMap Plan description", &plan.description)?;
    if plan.order == 0 {
        return Err("WorkMap Plan order must be greater than zero".into());
    }
    match plan.state {
        PlanState::Planned | PlanState::Active => {
            if plan.outcome.is_some() || plan.verification.is_some() || plan.status_reason.is_some()
            {
                return Err("open WorkMap Plan contains closing metadata".into());
            }
        }
        PlanState::Completed => {
            require_nonempty(
                "completed WorkMap Plan outcome",
                plan.outcome.as_deref().unwrap_or(""),
            )?;
            require_nonempty(
                "completed WorkMap Plan verification",
                plan.verification.as_deref().unwrap_or(""),
            )?;
            if plan.status_reason.is_some() {
                return Err("completed WorkMap Plan cannot have status_reason".into());
            }
        }
        PlanState::Cancelled | PlanState::Superseded => {
            require_nonempty(
                "cancelled/superseded WorkMap Plan status_reason",
                plan.status_reason.as_deref().unwrap_or(""),
            )?;
            if plan.outcome.is_some() || plan.verification.is_some() {
                return Err("cancelled/superseded Plan cannot have completion metadata".into());
            }
        }
    }
    Ok(())
}

fn validate_memory(memory: &WorkMapMemory) -> Result<()> {
    validate_id(&memory.id, "memory")?;
    require_nonempty("WorkMap Memory content", &memory.content)?;
    match memory.kind {
        MemoryKind::Fact if memory.basis.is_none() => {
            return Err("WorkMap Fact requires a basis".into());
        }
        MemoryKind::Agreement if memory.basis.is_some() => {
            return Err("WorkMap Agreement cannot have a fact basis".into());
        }
        _ => {}
    }
    match memory.state {
        MemoryState::Active => {
            if memory.status_reason.is_some() || memory.replacement_id.is_some() {
                return Err("active WorkMap Memory cannot have closing metadata".into());
            }
        }
        MemoryState::Superseded => {
            require_nonempty(
                "superseded WorkMap Memory status_reason",
                memory.status_reason.as_deref().unwrap_or(""),
            )?;
            let replacement = memory.replacement_id.as_deref().unwrap_or("");
            validate_id(replacement, "memory")?;
            if replacement == memory.id {
                return Err("WorkMap Memory cannot replace itself".into());
            }
        }
        MemoryState::Retracted => {
            require_nonempty(
                "retracted WorkMap Memory status_reason",
                memory.status_reason.as_deref().unwrap_or(""),
            )?;
            if memory.replacement_id.is_some() {
                return Err("retracted WorkMap Memory cannot have a replacement".into());
            }
        }
    }
    Ok(())
}

fn validate_projection_relations(records: &BTreeMap<String, WorkMapRecord>) -> Result<()> {
    let active_objectives = records
        .values()
        .filter(|record| {
            matches!(
                record,
                WorkMapRecord::Objective(objective) if objective.state == ObjectiveState::Active
            )
        })
        .count();
    if active_objectives > 1 {
        return Err("WorkMap contains more than one Current Objective".into());
    }

    let objectives = records
        .values()
        .filter_map(|record| match record {
            WorkMapRecord::Objective(objective) => Some(objective),
            _ => None,
        })
        .collect::<Vec<_>>();
    for objective in objectives {
        let mut orders = BTreeSet::new();
        let plans = records
            .values()
            .filter_map(|record| match record {
                WorkMapRecord::Plan(plan) if plan.objective_id == objective.id => Some(plan),
                _ => None,
            })
            .collect::<Vec<_>>();
        if plans.is_empty() || plans.iter().any(|plan| !orders.insert(plan.order)) {
            return Err(format!(
                "WorkMap Objective {} has no Plans or duplicate Plan order",
                objective.id
            )
            .into());
        }
        let active = plans
            .iter()
            .filter(|plan| plan.state == PlanState::Active)
            .collect::<Vec<_>>();
        let planned = plans
            .iter()
            .filter(|plan| plan.state == PlanState::Planned)
            .collect::<Vec<_>>();
        if objective.state == ObjectiveState::Active {
            if !planned.is_empty() && active.len() != 1 || active.len() > 1 {
                return Err("Current Objective has invalid active Plan count".into());
            }
            if let Some(active) = active.first()
                && planned.iter().any(|plan| plan.order < active.order)
            {
                return Err("planned Plan cannot precede the active Plan".into());
            }
        } else if plans.iter().any(|plan| plan.state.is_open()) {
            return Err("closed Objective contains an open Plan".into());
        }
    }

    for record in records.values() {
        match record {
            WorkMapRecord::Plan(plan)
                if !matches!(
                    records.get(&plan.objective_id),
                    Some(WorkMapRecord::Objective(_))
                ) =>
            {
                return Err(format!(
                    "WorkMap Plan {} references missing Objective {}",
                    plan.id, plan.objective_id
                )
                .into());
            }
            WorkMapRecord::Note(note)
                if !matches!(records.get(&note.plan_id), Some(WorkMapRecord::Plan(_))) =>
            {
                return Err(format!(
                    "WorkMap Note {} references missing Plan {}",
                    note.id, note.plan_id
                )
                .into());
            }
            WorkMapRecord::Memory(memory)
                if memory.state == MemoryState::Superseded
                    && !matches!(
                        memory
                            .replacement_id
                            .as_deref()
                            .and_then(|id| records.get(id)),
                        Some(WorkMapRecord::Memory(_))
                    ) =>
            {
                return Err(format!(
                    "WorkMap Memory {} references a missing replacement",
                    memory.id
                )
                .into());
            }
            _ => {}
        }
    }
    Ok(())
}

fn active_objective_in(records: &BTreeMap<String, WorkMapRecord>) -> Option<&WorkMapObjective> {
    records.values().find_map(|record| match record {
        WorkMapRecord::Objective(objective) if objective.state == ObjectiveState::Active => {
            Some(objective)
        }
        _ => None,
    })
}

fn new_id(
    prefix: &str,
    event_id: EventId,
    ordinal: u64,
    projection: &WorkMapProjection,
    reserved: &BTreeSet<String>,
) -> String {
    for salt in 0_u64.. {
        let digest =
            blake3::hash(format!("me:workmap:v2:{prefix}:{event_id}:{ordinal}:{salt}").as_bytes());
        let suffix = u32::from_le_bytes(digest.as_bytes()[..4].try_into().unwrap());
        let id = format!("{prefix}-{suffix:08x}");
        if !projection.contains(&id) && !reserved.contains(&id) {
            return id;
        }
    }
    unreachable!("unbounded WorkMap ID search")
}

fn validate_id(id: &str, prefix: &str) -> Result<()> {
    let Some(suffix) = id.strip_prefix(&format!("{prefix}-")) else {
        return Err(format!("invalid WorkMap {prefix} ID {id}").into());
    };
    if suffix.len() != 8
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(format!("invalid WorkMap {prefix} ID {id}").into());
    }
    Ok(())
}

fn validate_item(name: &str, input: &ItemInput) -> Result<()> {
    require_nonempty(&format!("{name} title"), &input.title)?;
    validate_optional(&format!("{name} description"), &input.description)
}

fn validate_memory_input(input: &MemoryInput) -> Result<()> {
    require_nonempty("memory content", &input.content)?;
    match input.kind {
        MemoryKind::Fact if input.basis.is_none() => Err("fact requires basis".into()),
        MemoryKind::Agreement if input.basis.is_some() => {
            Err("agreement does not accept basis".into())
        }
        _ => Ok(()),
    }
}

fn validate_optional(name: &str, value: &Option<String>) -> Result<()> {
    if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
        Err(format!("{name} cannot be blank").into())
    } else {
        Ok(())
    }
}

fn clean_required(
    name: &str,
    value: Option<String>,
) -> std::result::Result<Option<String>, ToolboxExecutionError> {
    let value = value.filter(|value| !value.trim().is_empty());
    if value.is_none() {
        return Err(invalid_arguments(format!("{name} cannot be empty")));
    }
    Ok(value)
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn require_nonempty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(format!("{name} cannot be empty").into())
    } else {
        Ok(())
    }
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(
    arguments: &str,
) -> std::result::Result<T, ToolboxExecutionError> {
    serde_json::from_str(arguments).map_err(invalid_arguments)
}

fn no_current() -> ToolboxExecutionError {
    tool_error("no_current", "WorkMap has no Current Objective")
}

fn invalid_arguments(error: impl std::fmt::Display) -> ToolboxExecutionError {
    tool_error("invalid_arguments", error.to_string())
}

fn protocol_error(error: impl std::fmt::Display) -> ToolboxExecutionError {
    ToolboxExecutionError::Protocol(error.to_string())
}

fn tool_error(code: impl Into<String>, message: impl Into<String>) -> ToolboxExecutionError {
    ToolboxExecutionError::Tool {
        code: code.into(),
        message: message.into(),
        retryable: false,
        tip: None,
    }
}

fn object_schema(required: &[&str], properties: Value) -> Value {
    json!({
        "type":"object",
        "required":required,
        "properties":properties,
        "additionalProperties":false
    })
}

fn item_schema() -> Value {
    object_schema(
        &["title"],
        json!({
            "title":{"type":"string","minLength":1},
            "description":{"type":"string","minLength":1}
        }),
    )
}

fn memory_input_schema() -> Value {
    object_schema(
        &["kind", "content"],
        json!({
            "kind":{"enum":["fact","agreement"]},
            "basis":{"enum":["user_stated","observed","verified","inferred"]},
            "content":{"type":"string","minLength":1}
        }),
    )
}

fn input_schema(tool: &str) -> Value {
    let plan_id = json!({"type":"string","pattern":"^plan-[0-9a-f]{8}$"});
    match tool {
        "Read" => object_schema(&[], json!({})),
        "ReadHistory" => object_schema(
            &[],
            json!({"objective_id":{"type":"string","pattern":"^objective-[0-9a-f]{8}$"}}),
        ),
        "Start" => object_schema(
            &["objective", "plans"],
            json!({
                "objective":item_schema(),
                "plans":{"type":"array","minItems":1,"maxItems":100,"items":item_schema()}
            }),
        ),
        "UpdatePlanState" => object_schema(
            &["plan_id", "state"],
            json!({
                "plan_id":plan_id,
                "state":{"enum":["completed","cancelled","superseded"]},
                "outcome":{"type":"string","minLength":1},
                "verification":{"type":"string","minLength":1},
                "reason":{"type":"string","minLength":1}
            }),
        ),
        "AddNote" => object_schema(
            &["plan_id", "kind", "content"],
            json!({
                "plan_id":plan_id,
                "kind":{"enum":["action","finding","decision","validation","adjustment","blocker","next","note"]},
                "content":{"type":"string","minLength":1}
            }),
        ),
        "ChangePlan" => object_schema(
            &["plan_id", "reason"],
            json!({
                "plan_id":plan_id,
                "title":{"type":"string","minLength":1},
                "description":{"type":"string","minLength":1},
                "clear_description":{"type":"boolean"},
                "reason":{"type":"string","minLength":1}
            }),
        ),
        "AddPlan" => object_schema(
            &["plan"],
            json!({"plan":item_schema(),"after_plan_id":plan_id}),
        ),
        "CloseObjective" => object_schema(
            &["state", "reason"],
            json!({
                "state":{"enum":["cancelled","superseded"]},
                "reason":{"type":"string","minLength":1}
            }),
        ),
        "AddMemory" => memory_input_schema(),
        "InvalidateMemory" => object_schema(
            &["memory_id", "reason"],
            json!({
                "memory_id":{"type":"string","pattern":"^memory-[0-9a-f]{8}$"},
                "reason":{"type":"string","minLength":1},
                "replacement":memory_input_schema()
            }),
        ),
        _ => object_schema(&[], json!({})),
    }
}

fn output_schema(tool: &str) -> Value {
    match tool {
        "Read" => object_schema(&["memory", "current"], json!({"memory":{},"current":{}})),
        "ReadHistory" => json!({"type":"object"}),
        "UpdatePlanState" => object_schema(&["current"], json!({"current":{}})),
        _ => object_schema(
            &["memory", "current", "records"],
            json!({"memory":{},"current":{},"records":{"type":"array","minItems":1}}),
        ),
    }
}

fn instructions(tool: &str) -> &'static str {
    match tool {
        "Read" => {
            "Return active Memory and the complete Current Objective with all ordered Plans and Notes, or null."
        }
        "ReadHistory" => {
            "Without objective_id return closed Objective summaries; with objective_id return that complete closed Objective."
        }
        "Start" => {
            "Create one Current Objective and an ordered non-empty Plan list. Input must be {\"objective\":{\"title\":\"...\",\"description\":\"...\"},\"plans\":[{\"title\":\"...\",\"description\":\"...\"}]}. objective and every plans item must each be an object; never pass them as strings. description is optional. The first Plan becomes active."
        }
        "UpdatePlanState" => {
            "Complete, cancel, or supersede a Plan. Closing the active Plan automatically advances the route."
        }
        "AddNote" => "Append one immutable typed Note to a Plan in Current.",
        "ChangePlan" => "Change the title or description of an open Plan.",
        "AddPlan" => {
            "Insert a new Plan after after_plan_id, or append it when after_plan_id is omitted."
        }
        "CloseObjective" => {
            "Cancel or supersede Current and atomically close every remaining open Plan."
        }
        "AddMemory" => {
            "Add one missing active Memory entry only when it is a globally applicable current Fact or explicit Agreement expected to remain valid and useful after the current Objective ends and across future Objectives. Current-Objective content is ineligible. Facts require basis; Agreements reject basis. Do not duplicate or substantially overlap active Memory; maintain an existing entry with InvalidateMemory instead."
        }
        "InvalidateMemory" => {
            "Remove an active entry from current Memory, or atomically supersede it with a replacement. Use it for clearly obsolete, renamed, changed, redundant, incorrectly classified, or clearly unusable entries. A replacement is allowed only when it independently satisfies the same global cross-Objective eligibility rule. Do not mutate an accurate entry for age, style, brevity, or demonstration."
        }
        _ => "",
    }
}

fn route(tool: &str) -> &'static str {
    match tool {
        "Read" => {
            "Use before substantial work, immediately after successful compaction, and for a final audit only when a successful UpdatePlanState result did not already establish the final Current state."
        }
        "ReadHistory" => {
            "Use only when earlier closed work is genuinely needed; never for routine final audits."
        }
        "Start" => {
            "Use when Read reports Current=null and substantial work begins. Pass objective as an object with title and optional description, and plans as a non-empty array of those objects; objective or Plan strings are invalid."
        }
        "UpdatePlanState" => "Use when a Plan reaches a truthful terminal boundary.",
        "AddNote" => {
            "Use throughout execution at each meaningful boundary, before proceeding to the next meaningful action; preserve material actions and results, findings, decisions, validation, adjustments, blockers, and exact continuation points."
        }
        "ChangePlan" => "Use when an open Plan's intended scope materially changes.",
        "AddPlan" => "Use when newly discovered future work belongs to Current's route.",
        "CloseObjective" => "Use only when the entire Objective is abandoned or replaced.",
        "AddMemory" => {
            "Use only when globally applicable current context is missing and is expected to remain valid and useful after the current Objective ends and across future Objectives; if uncertain, do not use it."
        }
        "InvalidateMemory" => {
            "Use when an active Memory entry genuinely must leave current state or be replaced because its meaning or subject changed; never use it for cosmetic cleanup. A replacement is allowed only for newer eligible global context, never for objective-specific content."
        }
        _ => "",
    }
}

fn examples(tool: &str) -> &'static str {
    match tool {
        "Read" => r#"{}"#,
        "ReadHistory" => r#"{"objective_id":"objective-1234abcd"}"#,
        "Start" => {
            r#"{"objective":{"title":"Deliver the requested verified outcome"},"plans":[{"title":"Establish the current constraints"},{"title":"Carry out the selected direction"},{"title":"Verify the resulting behavior"}]}"#
        }
        "UpdatePlanState" => {
            r#"{"plan_id":"plan-1234abcd","state":"completed","outcome":"The intended Plan result was achieved.","verification":"The result was directly checked."}"#
        }
        "AddNote" => {
            r#"{"plan_id":"plan-1234abcd","kind":"finding","content":"A confirmed constraint changes the viable direction."}"#
        }
        "ChangePlan" => {
            r#"{"plan_id":"plan-1234abcd","title":"Proceed with the revised direction","reason":"New evidence changed the intended scope."}"#
        }
        "AddPlan" => {
            r#"{"after_plan_id":"plan-1234abcd","plan":{"title":"Resolve the newly discovered requirement"}}"#
        }
        "CloseObjective" => {
            r#"{"state":"superseded","reason":"The user replaced the overall requested outcome."}"#
        }
        "AddMemory" => {
            r#"{"kind":"agreement","content":"Across all future Objectives, update the semantic specification whenever product behavior changes."}"#
        }
        "InvalidateMemory" => {
            r#"{"memory_id":"memory-1234abcd","reason":"The user changed this global agreement.","replacement":{"kind":"agreement","content":"Across all future Objectives, use the newly selected output format."}}"#
        }
        _ => "{}",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ApiState, EventBase, ToolResultState};

    fn call(edb: &mut EventDataBase, prompt: EventId, name: &str, arguments: &str) -> EventId {
        let api = edb.append_api_requesting(prompt).unwrap();
        let call = edb
            .append_tool_call(
                api,
                prompt,
                format!("provider-{call}", call = edb.next_event_id()),
                name,
                arguments,
            )
            .unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        call
    }

    fn execute_call(
        edb: &mut EventDataBase,
        prompt: EventId,
        name: &str,
        arguments: &str,
    ) -> Value {
        let call = call(edb, prompt, name, arguments);
        let output = execute(name, arguments, call, edb).unwrap();
        edb.append_tool_result(
            call,
            ToolResultState::Succeeded,
            None,
            serde_json::to_string(&output).unwrap(),
        )
        .unwrap();
        output
    }

    fn start_three(edb: &mut EventDataBase, prompt: EventId) -> Value {
        execute_call(
            edb,
            prompt,
            START,
            r#"{"objective":{"title":"Deliver result"},"plans":[{"title":"Inspect"},{"title":"Implement"},{"title":"Verify"}]}"#,
        )
    }

    #[test]
    fn catalog_exposes_the_new_workmap_surface_and_auditing_rules() {
        let (tools, brief) = catalog_parts();
        assert_eq!(tools.len(), 10);
        assert_eq!(brief.0, WORKMAP_TOOLBOX_NAME);
        for name in [
            READ,
            READ_HISTORY,
            START,
            UPDATE_PLAN_STATE,
            ADD_NOTE,
            CHANGE_PLAN,
            ADD_PLAN,
            CLOSE_OBJECTIVE,
            ADD_MEMORY,
            INVALIDATE_MEMORY,
        ] {
            assert!(tools.iter().any(|tool| tool.full_name == name));
        }
        assert!(
            brief
                .1
                .contains("never call Read after it merely to inspect or confirm state")
        );
        assert!(brief.1.contains("If `current` is non-null, continue"));
        assert!(brief.1.contains("If `current` is null"));
        assert!(brief.1.contains("Treat Current as a live execution record"));
        assert!(brief.1.contains("SetTitle first"));
        assert!(brief.1.contains("other than that mandatory SetTitle call"));
        assert!(
            brief
                .1
                .contains("never means the underlying deliverable was reviewed")
        );
        assert!(brief.1.contains("after each meaningful action"));
        assert!(brief.1.contains("Do not postpone Notes until the end"));
        assert!(
            brief
                .1
                .contains("Do not call ReadHistory for the final audit")
        );
        assert!(!brief.1.contains("WorkMap.Snapshot"));
        assert!(brief.1.contains("Facts require a basis"));
        assert!(
            brief
                .1
                .contains("never label an Agent assumption as an Agreement")
        );
        let read = tools.iter().find(|tool| tool.full_name == READ).unwrap();
        assert!(
            read.route
                .contains("only when a successful UpdatePlanState result did not already")
        );
        assert!(
            read.route
                .contains("immediately after successful compaction")
        );
        assert!(!read.route.contains("mandatory final audit"));
        let start = tools.iter().find(|tool| tool.full_name == START).unwrap();
        assert!(start.route.contains("objective as an object"));
        assert!(
            start
                .route
                .contains("objective or Plan strings are invalid")
        );
        assert!(
            start
                .instructions
                .contains("every plans item must each be an object")
        );
        let add_note = tools
            .iter()
            .find(|tool| tool.full_name == ADD_NOTE)
            .unwrap();
        assert!(add_note.route.contains("throughout execution"));
        assert!(add_note.route.contains("before proceeding to the next"));
    }

    #[test]
    fn catalog_states_strict_global_cross_objective_memory_guidance() {
        let (tools, brief) = catalog_parts();
        for required in [
            "after the current Objective ends and across future Objectives",
            "Relevance across multiple Plans within the current Objective is insufficient",
            "objective-specific requests or constraints",
            "single-turn discussion",
            "execution plans",
            "progress",
            "temporary decisions",
            "local trade-offs",
            "evidence",
            "validation results",
            "completed-task status",
            "Current's Objective, Plans, Notes, or the conversation",
            "If unsure whether information is global and cross-Objective, do not call AddMemory",
            "globally applicable beyond the current Objective",
            "A replacement must independently satisfy the same global cross-Objective eligibility rule",
        ] {
            assert!(
                brief.1.contains(required),
                "missing strict Memory guidance: {}",
                required
            );
        }
        assert!(
            !brief
                .1
                .contains("likely to matter across Plans or Objectives")
        );

        let add_memory = tools
            .iter()
            .find(|tool| tool.full_name == ADD_MEMORY)
            .unwrap();
        assert!(
            add_memory
                .route
                .contains("after the current Objective ends and across future Objectives")
        );
        assert!(add_memory.route.contains("if uncertain, do not use it"));
        assert!(
            add_memory
                .instructions
                .contains("Current-Objective content is ineligible")
        );
        assert!(add_memory.examples.contains("Across all future Objectives"));
        assert!(!add_memory.route.contains("across Plans or Objectives"));

        let invalidate_memory = tools
            .iter()
            .find(|tool| tool.full_name == INVALIDATE_MEMORY)
            .unwrap();
        assert!(
            invalidate_memory.instructions.contains(
                "independently satisfies the same global cross-Objective eligibility rule"
            )
        );
        assert!(
            invalidate_memory
                .route
                .contains("never for objective-specific content")
        );
        assert!(
            invalidate_memory
                .examples
                .contains("Across all future Objectives")
        );
    }

    #[test]
    fn catalog_defines_active_memory_lifecycle_without_forced_mutation() {
        let (tools, brief) = catalog_parts();
        for required in [
            "Memory is not an indefinitely growing history",
            "Active Memory is the authoritative current global state",
            "Keep an active entry unchanged when it is still accurate, clear, non-duplicated, and globally useful",
            "Age alone is never a reason to remove it",
            "clearly obsolete and will not be used again",
            "incorrectly classified because it is actually Objective-specific or temporary",
            "subject was renamed",
            "Fact, requirement, or Agreement changed",
            "Consolidate duplicate or substantially overlapping entries",
            "has lost so much essential context that it is clearly unusable",
            "If an unclear entry may still matter but cannot be interpreted safely",
            "do not guess its meaning, invent a replacement, or silently retract it",
            "Inspecting or maintaining Memory does not require a mutation",
            "Do not rewrite accurate entries merely to demonstrate maintenance",
        ] {
            assert!(
                brief.1.contains(required),
                "missing active Memory lifecycle guidance: {}",
                required
            );
        }

        for forbidden_environment_example in ["38200", "build.sh", "release.sh", "me-rust", "me-s"]
        {
            assert!(
                !brief.1.contains(forbidden_environment_example),
                "Memory teaching examples must stay generic: {}",
                forbidden_environment_example
            );
        }

        let add_memory = tools
            .iter()
            .find(|tool| tool.full_name == ADD_MEMORY)
            .unwrap();
        assert!(add_memory.route.contains("current context is missing"));
        assert!(
            add_memory
                .instructions
                .contains("Do not duplicate or substantially overlap active Memory")
        );

        let invalidate_memory = tools
            .iter()
            .find(|tool| tool.full_name == INVALIDATE_MEMORY)
            .unwrap();
        assert!(
            invalidate_memory
                .instructions
                .contains("clearly obsolete, renamed, changed, redundant, incorrectly classified")
        );
        assert!(
            invalidate_memory
                .route
                .contains("never use it for cosmetic cleanup")
        );
    }

    #[test]
    fn start_creates_one_objective_and_ordered_plans() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("start").unwrap();
        let output = start_three(&mut edb, prompt);
        let current = &output["current"];
        assert_eq!(current["objective"]["state"], "active");
        assert_eq!(current["plans"].as_array().unwrap().len(), 3);
        assert_eq!(current["plans"][0]["plan"]["state"], "active");
        assert_eq!(current["plans"][1]["plan"]["state"], "planned");
        assert!(
            current["objective"]["id"]
                .as_str()
                .unwrap()
                .starts_with("objective-")
        );
        assert!(
            current["plans"][0]["plan"]["id"]
                .as_str()
                .unwrap()
                .starts_with("plan-")
        );
    }

    #[test]
    fn completing_plans_advances_and_closes_the_objective() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("advance").unwrap();
        let started = start_three(&mut edb, prompt);
        let plans = started["current"]["plans"].as_array().unwrap();
        let ids = plans
            .iter()
            .map(|plan| plan["plan"]["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        for (index, id) in ids.iter().enumerate() {
            let output = execute_call(
                &mut edb,
                prompt,
                UPDATE_PLAN_STATE,
                &format!(
                    r#"{{"plan_id":"{id}","state":"completed","outcome":"done {index}","verification":"checked {index}"}}"#
                ),
            );
            assert_eq!(output.as_object().unwrap().len(), 1);
            assert!(output.get("memory").is_none());
            assert!(output.get("records").is_none());
            let tool_call_id = edb
                .events()
                .iter()
                .rev()
                .find_map(|event| match event {
                    Event::WorkMapMutation(event)
                        if event.mutation.operation == WorkMapOperation::PlanStateUpdated =>
                    {
                        Some(event.tool_call_id)
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                persisted_mutation_result(edb.events(), tool_call_id),
                Some(output.clone())
            );
            if index + 1 < ids.len() {
                assert_eq!(
                    output["current"]["plans"][index + 1]["plan"]["state"],
                    "active"
                );
            } else {
                assert!(output["current"].is_null());
            }
        }
        let read = execute_call(&mut edb, prompt, READ, "{}");
        assert!(read["current"].is_null());
        let history = execute_call(&mut edb, prompt, READ_HISTORY, "{}");
        assert_eq!(history["objectives"].as_array().unwrap().len(), 1);
        assert_eq!(history["objectives"][0]["objective"]["state"], "completed");
    }

    #[test]
    fn notes_changes_and_inserted_plans_preserve_structure() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("edit route").unwrap();
        let started = start_three(&mut edb, prompt);
        let first = started["current"]["plans"][0]["plan"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        execute_call(
            &mut edb,
            prompt,
            ADD_NOTE,
            &format!(r#"{{"plan_id":"{first}","kind":"finding","content":"constraint"}}"#),
        );
        execute_call(
            &mut edb,
            prompt,
            CHANGE_PLAN,
            &format!(
                r#"{{"plan_id":"{first}","title":"Inspect revised scope","reason":"scope changed"}}"#
            ),
        );
        let output = execute_call(
            &mut edb,
            prompt,
            ADD_PLAN,
            &format!(r#"{{"after_plan_id":"{first}","plan":{{"title":"New middle Plan"}}}}"#),
        );
        assert_eq!(output["current"]["plans"].as_array().unwrap().len(), 4);
        assert_eq!(output["current"]["plans"][0]["notes"][0]["kind"], "finding");
        assert_eq!(
            output["current"]["plans"][1]["plan"]["title"],
            "New middle Plan"
        );
        assert_eq!(output["current"]["plans"][1]["plan"]["order"], 2);
    }

    #[test]
    fn close_objective_closes_every_open_plan_atomically() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("replace").unwrap();
        let started = start_three(&mut edb, prompt);
        let objective_id = started["current"]["objective"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let output = execute_call(
            &mut edb,
            prompt,
            CLOSE_OBJECTIVE,
            r#"{"state":"superseded","reason":"goal replaced"}"#,
        );
        assert!(output["current"].is_null());
        let detail = execute_call(
            &mut edb,
            prompt,
            READ_HISTORY,
            &format!(r#"{{"objective_id":"{objective_id}"}}"#),
        );
        assert_eq!(detail["objective"]["objective"]["state"], "superseded");
        assert!(
            detail["objective"]["plans"]
                .as_array()
                .unwrap()
                .iter()
                .all(|plan| plan["plan"]["state"] == "superseded")
        );
    }

    #[test]
    fn memory_records_facts_agreements_supersession_and_retraction() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("remember").unwrap();
        let fact = execute_call(
            &mut edb,
            prompt,
            ADD_MEMORY,
            r#"{"kind":"fact","basis":"observed","content":"The workspace is empty."}"#,
        );
        let fact_id = fact["memory"]["facts"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let agreement = execute_call(
            &mut edb,
            prompt,
            ADD_MEMORY,
            r#"{"kind":"agreement","content":"Use concise Chinese responses."}"#,
        );
        let agreement_id = agreement["memory"]["agreements"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let superseded = execute_call(
            &mut edb,
            prompt,
            INVALIDATE_MEMORY,
            &format!(
                r#"{{"memory_id":"{fact_id}","reason":"The directory now contains files.","replacement":{{"kind":"fact","basis":"verified","content":"The workspace contains project files."}}}}"#
            ),
        );
        let active_facts = superseded["memory"]["facts"].as_array().unwrap();
        assert_eq!(active_facts.len(), 1);
        assert_eq!(active_facts[0]["state"], "active");
        let replacement_id = active_facts[0]["id"].as_str().unwrap();
        let complete_memory = WorkMapProjection::from_events(edb.events())
            .unwrap()
            .memory_snapshot();
        let old_fact = complete_memory
            .facts
            .iter()
            .find(|fact| fact.id == fact_id)
            .unwrap();
        assert_eq!(old_fact.state, MemoryState::Superseded);
        assert_eq!(old_fact.replacement_id.as_deref(), Some(replacement_id));

        let retracted = execute_call(
            &mut edb,
            prompt,
            INVALIDATE_MEMORY,
            &format!(
                r#"{{"memory_id":"{agreement_id}","reason":"The user withdrew this preference."}}"#
            ),
        );
        assert!(
            retracted["memory"]["agreements"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let read = execute_call(&mut edb, prompt, READ, "{}");
        assert_eq!(read["memory"]["facts"].as_array().unwrap().len(), 1);
        assert!(read["memory"]["agreements"].as_array().unwrap().is_empty());
        assert!(read["current"].is_null());
    }

    #[test]
    fn memory_validation_is_atomic_and_rewind_restores_the_previous_truth() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("memory validation").unwrap();
        let missing_basis_call = call(
            &mut edb,
            prompt,
            ADD_MEMORY,
            r#"{"kind":"fact","content":"unsupported"}"#,
        );
        assert!(
            execute(
                ADD_MEMORY,
                r#"{"kind":"fact","content":"unsupported"}"#,
                missing_basis_call,
                &mut edb,
            )
            .unwrap_err()
            .to_string()
            .contains("fact requires basis")
        );
        assert!(edb.events().iter().all(|event| {
            !matches!(event, Event::WorkMapMutation(event) if event.tool_call_id == missing_basis_call)
        }));

        let added = execute_call(
            &mut edb,
            prompt,
            ADD_MEMORY,
            r#"{"kind":"agreement","content":"Across all future Objectives, keep the original interface."}"#,
        );
        let id = added["memory"]["agreements"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let rewind_target = edb.append_user_prompt("global replacement").unwrap();
        execute_call(
            &mut edb,
            rewind_target,
            INVALIDATE_MEMORY,
            &format!(
                r#"{{"memory_id":"{id}","reason":"The user changed this global agreement.","replacement":{{"kind":"agreement","content":"Across all future Objectives, use the replacement interface."}}}}"#
            ),
        );
        assert_eq!(
            WorkMapProjection::from_events(edb.events())
                .unwrap()
                .memory_snapshot()
                .agreements
                .len(),
            2
        );
        edb.rewind_to_event(rewind_target).unwrap();
        let memory = WorkMapProjection::from_events(edb.events())
            .unwrap()
            .memory_snapshot();
        assert_eq!(memory.agreements.len(), 1);
        assert_eq!(memory.agreements[0].state, MemoryState::Active);
    }

    #[test]
    fn context_clear_resets_every_workmap_section_and_rewind_restores_it() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("build a complete map").unwrap();
        let started = start_three(&mut edb, prompt);
        let first_plan = started["current"]["plans"][0]["plan"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        execute_call(
            &mut edb,
            prompt,
            ADD_NOTE,
            &format!(
                r#"{{"plan_id":"{first_plan}","kind":"finding","content":"persistent detail"}}"#
            ),
        );
        execute_call(
            &mut edb,
            prompt,
            ADD_MEMORY,
            r#"{"kind":"agreement","content":"Preserve the public interface."}"#,
        );
        let objective_id = started["current"]["objective"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        execute_call(
            &mut edb,
            prompt,
            CLOSE_OBJECTIVE,
            r#"{"state":"cancelled","reason":"replace the objective"}"#,
        );
        let replacement = execute_call(
            &mut edb,
            prompt,
            START,
            r#"{"objective":{"title":"Replacement"},"plans":[{"title":"Continue"}]}"#,
        );
        assert_eq!(replacement["current"]["objective"]["title"], "Replacement");

        let before_clear = WorkMapProjection::from_events(edb.events())
            .unwrap()
            .snapshot();
        assert_eq!(before_clear.memory.agreements.len(), 1);
        assert_eq!(before_clear.history.len(), 1);
        assert_eq!(before_clear.history[0].objective.id, objective_id);
        assert!(before_clear.current.is_some());

        let clear_id = edb.append_context_cleared().unwrap();
        assert_eq!(
            WorkMapProjection::from_events(edb.events())
                .unwrap()
                .snapshot(),
            WorkMapSnapshot::default()
        );

        edb.rewind_to_event(clear_id).unwrap();
        assert_eq!(
            WorkMapProjection::from_events(edb.events())
                .unwrap()
                .snapshot(),
            before_clear
        );
    }

    #[test]
    fn invalid_completion_is_atomic() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("invalid").unwrap();
        let started = start_three(&mut edb, prompt);
        let future = started["current"]["plans"][1]["plan"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let call = call(
            &mut edb,
            prompt,
            UPDATE_PLAN_STATE,
            &format!(
                r#"{{"plan_id":"{future}","state":"completed","outcome":"fake","verification":"fake"}}"#
            ),
        );
        let error = execute(
            UPDATE_PLAN_STATE,
            &format!(r#"{{"plan_id":"{future}","state":"completed","outcome":"fake","verification":"fake"}}"#),
            call,
            &mut edb,
        )
        .unwrap_err();
        assert!(error.to_string().contains("active Plan"));
        assert!(edb.events().iter().all(|event| {
            !matches!(event, Event::WorkMapMutation(event) if event.tool_call_id == call)
        }));
    }

    #[test]
    fn workmap_persists_and_rewinds_with_edb() {
        let path = std::env::temp_dir().join(format!(
            "me-workmap-v2-{}-{}.edb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut edb = EventDataBase::open(&path).unwrap();
        let prompt = edb.append_user_prompt("persist").unwrap();
        let started = start_three(&mut edb, prompt);
        let first = started["current"]["plans"][0]["plan"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let remembered = execute_call(
            &mut edb,
            prompt,
            ADD_MEMORY,
            r#"{"kind":"agreement","content":"Across all future Objectives, preserve the public interface."}"#,
        );
        let memory_id = remembered["memory"]["agreements"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let branch = edb.append_user_prompt("temporary").unwrap();
        execute_call(
            &mut edb,
            branch,
            ADD_NOTE,
            &format!(r#"{{"plan_id":"{first}","kind":"note","content":"temporary"}}"#),
        );
        execute_call(
            &mut edb,
            branch,
            INVALIDATE_MEMORY,
            &format!(
                r#"{{"memory_id":"{memory_id}","reason":"The user changed this global agreement.","replacement":{{"kind":"agreement","content":"Across all future Objectives, use the replacement interface."}}}}"#
            ),
        );
        let mutation = edb
            .events()
            .iter()
            .find_map(|event| match event {
                Event::WorkMapMutation(event) => Some(event),
                _ => None,
            })
            .unwrap();
        assert_eq!(mutation.getEventKind().to_string(), "workmap-mutation");
        drop(edb);

        let mut reopened = EventDataBase::open(&path).unwrap();
        assert_eq!(
            WorkMapProjection::from_events(reopened.events())
                .unwrap()
                .current_snapshot()
                .unwrap()
                .plans[0]
                .notes
                .len(),
            1
        );
        let memory = WorkMapProjection::from_events(reopened.events())
            .unwrap()
            .memory_snapshot();
        assert_eq!(memory.agreements.len(), 2);
        assert_eq!(
            memory
                .agreements
                .iter()
                .find(|memory| memory.id == memory_id)
                .unwrap()
                .state,
            MemoryState::Superseded
        );
        reopened.rewind_to_event(branch).unwrap();
        assert!(
            WorkMapProjection::from_events(reopened.events())
                .unwrap()
                .current_snapshot()
                .unwrap()
                .plans[0]
                .notes
                .is_empty()
        );
        let memory = WorkMapProjection::from_events(reopened.events())
            .unwrap()
            .memory_snapshot();
        assert_eq!(memory.agreements.len(), 1);
        assert_eq!(memory.agreements[0].state, MemoryState::Active);
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn workmaps_are_isolated_by_agent_edb() {
        let mut left = EventDataBase::new();
        let left_prompt = left.append_user_prompt("left").unwrap();
        execute_call(
            &mut left,
            left_prompt,
            START,
            r#"{"objective":{"title":"Left"},"plans":[{"title":"Left Plan"}]}"#,
        );
        let mut right = EventDataBase::new();
        let right_prompt = right.append_user_prompt("right").unwrap();
        execute_call(
            &mut right,
            right_prompt,
            START,
            r#"{"objective":{"title":"Right"},"plans":[{"title":"Right Plan"}]}"#,
        );
        assert_eq!(
            WorkMapProjection::from_events(left.events())
                .unwrap()
                .current_snapshot()
                .unwrap()
                .objective
                .title,
            "Left"
        );
        assert_eq!(
            WorkMapProjection::from_events(right.events())
                .unwrap()
                .current_snapshot()
                .unwrap()
                .objective
                .title,
            "Right"
        );
    }
}
