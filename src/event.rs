use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as FmtWrite,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Result, edb_migration,
    terminal::TerminalLineUpdate,
    workmap::{WorkMapMutation, WorkMapProjection, WorkMapRecord},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const FILE_MAGIC: &[u8; 8] = &edb_migration::CURRENT_FILE_MAGIC;
const FILE_HEADER_SIZE: usize = 16;
const RECORD_HEADER_SIZE: usize = 13;
const CODEC_RAW: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const ZSTD_LEVEL: i32 = 3;
const MAX_RECORD_SIZE: usize = 64 * 1024 * 1024;

pub type EventId = u64;
pub type EventOrder = usize;
pub const EDB_ID_BYTES: usize = 32;
pub const EDB_ID_HEX_LENGTH: usize = EDB_ID_BYTES * 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdbMutation {
    Rewind {
        target_event_id: EventId,
        restored_prompt_content: Option<String>,
    },
    DeleteTurn {
        prompt_id: EventId,
    },
    Regenerate {
        final_answer_event_id: EventId,
        prompt_id: EventId,
    },
}

pub const HOST_AGENT_TITLE_CHANGE: EventId = EventId::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    EdbIdGeneration,
    AgentKindDef,
    AgentTurn,
    SystemPrompt,
    UserPrompt,
    ManagerPrompt,
    ParentAgentPrompt,
    FollowUpPrompt,
    AssistResponse,
    ApiStateUpdate,
    ContextUsageEstimate,
    UserTurnAborted,
    ToolCall,
    ToolInfoUpdate,
    ToolCallResult,
    TerminalSessionCreated,
    TerminalSessionState,
    ModelContextItem,
    ModelChanged,
    ReasoningEffortChanged,
    ContextCleared,
    WorkMapMutation,
    WorkMapPendingReminder,
    CompactStateUpdate,
    SystemStaticPromptChange,
    AgentTitleChanged,
    CloneCompleted,
    ImageContent,
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EdbIdGeneration => "edb-id-generation",
            Self::AgentKindDef => "agent-kind-def",
            Self::AgentTurn => "agent-turn",
            Self::SystemPrompt => "system-prompt",
            Self::UserPrompt => "user-prompt",
            Self::ManagerPrompt => "manager-prompt",
            Self::ParentAgentPrompt => "parent-agent-prompt",
            Self::FollowUpPrompt => "follow-up-prompt",
            Self::AssistResponse => "assist-response",
            Self::ApiStateUpdate => "api-state-update",
            Self::ContextUsageEstimate => "context-usage-estimate",
            Self::UserTurnAborted => "user-turn-aborted",
            Self::ToolCall => "tool-call",
            Self::ToolInfoUpdate => "tool-info-update",
            Self::ToolCallResult => "tool-call-result",
            Self::TerminalSessionCreated => "terminal-session-created",
            Self::TerminalSessionState => "terminal-session-state",
            Self::ModelContextItem => "model-context-item",
            Self::ModelChanged => "model-changed",
            Self::ReasoningEffortChanged => "reasoning-effort-changed",
            Self::ContextCleared => "context-cleared",
            Self::WorkMapMutation => "workmap-mutation",
            Self::WorkMapPendingReminder => "workmap-pending-reminder",
            Self::CompactStateUpdate => "compact-state-update",
            Self::SystemStaticPromptChange => "system-static-prompt-change",
            Self::AgentTitleChanged => "agent-title-changed",
            Self::CloneCompleted => "clone-completed",
            Self::ImageContent => "image-content",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum AgentKind {
    Primary,
    Interactive,
    SubAgent,
}

impl AgentKind {
    fn code(self) -> u8 {
        match self {
            Self::Primary => 0,
            Self::Interactive => 1,
            Self::SubAgent => 2,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Primary),
            1 => Ok(Self::Interactive),
            2 => Ok(Self::SubAgent),
            _ => Err(format!("unsupported Agent kind {code}").into()),
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Primary => "primary",
            Self::Interactive => "interactive",
            Self::SubAgent => "sub-agent",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum AgentTurnState {
    Started,
    Completed,
    Interrupted,
    Failed,
}

impl AgentTurnState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Started)
    }

    fn code(self) -> u8 {
        match self {
            Self::Started => 0,
            Self::Completed => 1,
            Self::Interrupted => 2,
            Self::Failed => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Started),
            1 => Ok(Self::Completed),
            2 => Ok(Self::Interrupted),
            3 => Ok(Self::Failed),
            _ => Err(format!("unsupported Agent turn state {code}").into()),
        }
    }
}

impl std::fmt::Display for AgentTurnState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ApiState {
    Requesting,
    Streaming,
    Completed,
    Error,
    Retrying,
    Interrupted,
}

impl ApiState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Error | Self::Interrupted)
    }

    fn code(self) -> u8 {
        match self {
            Self::Requesting => 0,
            Self::Streaming => 1,
            Self::Completed => 2,
            Self::Error => 3,
            Self::Interrupted => 4,
            Self::Retrying => 5,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Requesting),
            1 => Ok(Self::Streaming),
            2 => Ok(Self::Completed),
            3 => Ok(Self::Error),
            4 => Ok(Self::Interrupted),
            5 => Ok(Self::Retrying),
            _ => Err(format!("unsupported API state {code}").into()),
        }
    }
}

impl std::fmt::Display for ApiState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Requesting => "requesting",
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Retrying => "retrying",
            Self::Interrupted => "interrupted",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum CompactState {
    Started,
    StageCompleted,
    Completed,
    Failed,
    Interrupted,
}

impl CompactState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }

    fn code(self) -> u8 {
        match self {
            Self::Started => 0,
            Self::Completed => 1,
            Self::Failed => 2,
            Self::Interrupted => 3,
            Self::StageCompleted => 4,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Started),
            1 => Ok(Self::Completed),
            2 => Ok(Self::Failed),
            3 => Ok(Self::Interrupted),
            4 => Ok(Self::StageCompleted),
            _ => Err(format!("unsupported Compact state {code}").into()),
        }
    }
}

impl std::fmt::Display for CompactState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Started => "started",
            Self::StageCompleted => "stage-completed",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum CompactKind {
    MainAgentMultiTurn,
    ManagerMultiTurn,
    WorkerSingleTurn,
    ChatbotSingleTurn,
}

impl CompactKind {
    fn code(self) -> u8 {
        match self {
            Self::MainAgentMultiTurn => 0,
            Self::ManagerMultiTurn => 1,
            Self::WorkerSingleTurn => 2,
            Self::ChatbotSingleTurn => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::MainAgentMultiTurn),
            1 => Ok(Self::ManagerMultiTurn),
            2 => Ok(Self::WorkerSingleTurn),
            3 => Ok(Self::ChatbotSingleTurn),
            _ => Err(format!("unsupported Compact kind {code}").into()),
        }
    }

    pub fn is_multi_turn(self) -> bool {
        matches!(self, Self::MainAgentMultiTurn | Self::ManagerMultiTurn)
    }

    fn base_stage_count(self) -> usize {
        if self.is_multi_turn() {
            CompactStage::MULTI_TURN.len()
        } else {
            1
        }
    }

    pub fn accepts_stage_count(self, stage_count: u8) -> bool {
        match self {
            Self::MainAgentMultiTurn | Self::ManagerMultiTurn => {
                usize::from(stage_count) == CompactStage::MULTI_TURN.len()
                    || usize::from(stage_count)
                        == CompactStage::MULTI_TURN_WITH_ACTIVE_SESSIONS.len()
            }
            Self::WorkerSingleTurn | Self::ChatbotSingleTurn => stage_count == 1,
        }
    }

    pub fn stages(self, stage_count: u8) -> Option<&'static [CompactStage]> {
        match self {
            Self::MainAgentMultiTurn | Self::ManagerMultiTurn
                if usize::from(stage_count) == CompactStage::MULTI_TURN.len() =>
            {
                Some(&CompactStage::MULTI_TURN)
            }
            Self::MainAgentMultiTurn | Self::ManagerMultiTurn
                if usize::from(stage_count)
                    == CompactStage::MULTI_TURN_WITH_ACTIVE_SESSIONS.len() =>
            {
                Some(&CompactStage::MULTI_TURN_WITH_ACTIVE_SESSIONS)
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for CompactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MainAgentMultiTurn => "main-agent-multi-turn",
            Self::ManagerMultiTurn => "manager-multi-turn",
            Self::WorkerSingleTurn => "worker-single-turn",
            Self::ChatbotSingleTurn => "chatbot-single-turn",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompactStage {
    Analysis,
    PrimaryRequestAndIntent,
    KeyTechnicalContextAndDecisions,
    FilesCodeAndArtifacts,
    ProblemsInvestigationsAndResolutions,
    CurrentStateAndContinuationPlan,
    ActiveToolSessions,
}

impl CompactStage {
    pub const MULTI_TURN: [Self; 6] = [
        Self::Analysis,
        Self::PrimaryRequestAndIntent,
        Self::KeyTechnicalContextAndDecisions,
        Self::FilesCodeAndArtifacts,
        Self::ProblemsInvestigationsAndResolutions,
        Self::CurrentStateAndContinuationPlan,
    ];

    pub const MULTI_TURN_WITH_ACTIVE_SESSIONS: [Self; 7] = [
        Self::Analysis,
        Self::PrimaryRequestAndIntent,
        Self::KeyTechnicalContextAndDecisions,
        Self::FilesCodeAndArtifacts,
        Self::ProblemsInvestigationsAndResolutions,
        Self::CurrentStateAndContinuationPlan,
        Self::ActiveToolSessions,
    ];

    fn code(self) -> u8 {
        match self {
            Self::Analysis => 0,
            Self::PrimaryRequestAndIntent => 1,
            Self::KeyTechnicalContextAndDecisions => 2,
            Self::FilesCodeAndArtifacts => 3,
            Self::ProblemsInvestigationsAndResolutions => 4,
            Self::CurrentStateAndContinuationPlan => 5,
            Self::ActiveToolSessions => 6,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        Self::MULTI_TURN_WITH_ACTIVE_SESSIONS
            .get(usize::from(code))
            .copied()
            .ok_or_else(|| format!("unsupported Compact stage {code}").into())
    }
}

impl std::fmt::Display for CompactStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Analysis => "analysis",
            Self::PrimaryRequestAndIntent => "primary-request-and-intent",
            Self::KeyTechnicalContextAndDecisions => "key-technical-context-and-decisions",
            Self::FilesCodeAndArtifacts => "files-code-and-artifacts",
            Self::ProblemsInvestigationsAndResolutions => "problems-investigations-and-resolutions",
            Self::CurrentStateAndContinuationPlan => "current-state-and-continuation-plan",
            Self::ActiveToolSessions => "active-tool-sessions",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
    Terminal,
}

impl ToolOutputStream {
    fn code(self) -> u8 {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
            Self::Terminal => 2,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Stdout),
            1 => Ok(Self::Stderr),
            2 => Ok(Self::Terminal),
            _ => Err(format!("unsupported tool output stream {code}").into()),
        }
    }
}

impl std::fmt::Display for ToolOutputStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Terminal => "terminal",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ToolResultState {
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl ToolResultState {
    fn code(self) -> u8 {
        match self {
            Self::Succeeded => 0,
            Self::Failed => 1,
            Self::Cancelled => 2,
            Self::Interrupted => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Succeeded),
            1 => Ok(Self::Failed),
            2 => Ok(Self::Cancelled),
            3 => Ok(Self::Interrupted),
            _ => Err(format!("unsupported tool result state {code}").into()),
        }
    }
}

impl std::fmt::Display for ToolResultState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum TerminalSessionState {
    Running,
    Exited,
    Killed,
    Lost,
}

impl TerminalSessionState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    fn code(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Exited => 1,
            Self::Killed => 2,
            Self::Lost => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Running),
            1 => Ok(Self::Exited),
            2 => Ok(Self::Killed),
            3 => Ok(Self::Lost),
            _ => Err(format!("unsupported terminal session state {code}").into()),
        }
    }
}

impl std::fmt::Display for TerminalSessionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::Lost => "lost",
        })
    }
}

#[allow(non_snake_case)]
pub trait EventBase {
    fn getID(&self) -> EventId;
    fn getTimestamp(&self) -> u64;
    fn getEventKind(&self) -> EventKind;
    fn getHash(&self) -> String;
    fn getBriefString(&self) -> String;
    fn getDetailString(&self) -> String;
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct EdbIdGenerationEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub edb_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentKindDefEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub kind: AgentKind,
    pub orchestrator: String,
    pub parent_agent_id: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentTurnEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub turn_id: EventId,
    pub prompt_id: EventId,
    pub state: AgentTurnState,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SystemPromptEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserPromptEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManagerPromptEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ParentAgentPromptEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FollowUpPromptEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub prompt_id: EventId,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AssistResponseEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub prompt_id: EventId,
    pub content: String,
    pub finished: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApiStateUpdateEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub api_call_id: EventId,
    pub prompt_id: EventId,
    pub state: ApiState,
    pub retry_count: u8,
    pub retry_limit: u8,
    pub usage: Option<ApiUsage>,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContextTokenUsage {
    pub system: u64,
    pub compact: u64,
    pub memory: u64,
    pub user: u64,
    pub model: u64,
    pub tool: u64,
}

impl ContextTokenUsage {
    pub fn sum(self) -> u64 {
        self.system
            .saturating_add(self.compact)
            .saturating_add(self.memory)
            .saturating_add(self.user)
            .saturating_add(self.model)
            .saturating_add(self.tool)
    }

    pub(crate) fn normalize_to(&mut self, target: u64) {
        let source = self.sum();
        if source == 0 {
            *self = Self {
                system: target,
                ..Self::default()
            };
            return;
        }
        let raw = [
            self.system,
            self.compact,
            self.memory,
            self.user,
            self.model,
            self.tool,
        ];
        let source = u128::from(source);
        let target_wide = u128::from(target);
        let mut scaled = [0_u64; 6];
        let mut remainders = [(0_u128, 0_usize); 6];
        for (index, value) in raw.into_iter().enumerate() {
            let product = u128::from(value).saturating_mul(target_wide);
            scaled[index] = u64::try_from(product / source).unwrap_or(u64::MAX);
            remainders[index] = (product % source, index);
        }
        let assigned = scaled.iter().copied().fold(0_u64, u64::saturating_add);
        let mut remainder = target.saturating_sub(assigned);
        remainders.sort_by(|left, right| right.cmp(left));
        for (_, index) in remainders {
            if remainder == 0 {
                break;
            }
            scaled[index] = scaled[index].saturating_add(1);
            remainder -= 1;
        }
        [
            self.system,
            self.compact,
            self.memory,
            self.user,
            self.model,
            self.tool,
        ] = scaled;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContextUsageEstimateEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub api_state_event_id: EventId,
    pub values: ContextTokenUsage,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserTurnAbortedEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub prompt_id: EventId,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolCallEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub api_call_id: EventId,
    pub prompt_id: EventId,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolInfoUpdateEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub tool_call_id: EventId,
    pub stream: ToolOutputStream,
    pub content: ToolInfoContent,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ToolInfoContent {
    Text(String),
    Terminal(TerminalLineUpdate),
}

impl ToolInfoContent {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(content) => Some(content),
            Self::Terminal(_) => None,
        }
    }

    pub fn terminal(&self) -> Option<&TerminalLineUpdate> {
        match self {
            Self::Text(_) => None,
            Self::Terminal(update) => Some(update),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(content) => content.trim().is_empty(),
            Self::Terminal(_) => false,
        }
    }

    fn stable_string(&self) -> String {
        serde_json::to_string(self).expect("ToolInfoContent serialization cannot fail")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolCallResultEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub tool_call_id: EventId,
    pub state: ToolResultState,
    pub exit_code: Option<i32>,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalSessionCreatedEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub tool_call_id: EventId,
    pub session_id: String,
    pub shell: String,
    pub cwd: String,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalSessionStateEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub session_id: String,
    pub state: TerminalSessionState,
    pub exit_code: Option<i32>,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelContextItemEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub api_call_id: EventId,
    pub prompt_id: EventId,
    pub provider: String,
    pub content: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ModelChangeCause {
    Initial,
    User,
}

impl ModelChangeCause {
    fn code(self) -> u8 {
        match self {
            Self::Initial => 0,
            Self::User => 1,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Initial),
            1 => Ok(Self::User),
            _ => Err(format!("invalid model change cause {code}").into()),
        }
    }
}

impl std::fmt::Display for ModelChangeCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Initial => "initial",
            Self::User => "user",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelChangedEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub model: String,
    pub cause: ModelChangeCause,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ReasoningEffortChangeCause {
    Initial,
    User,
    ModelUnsupported,
}

impl ReasoningEffortChangeCause {
    fn code(self) -> u8 {
        match self {
            Self::Initial => 0,
            Self::User => 1,
            Self::ModelUnsupported => 2,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Initial),
            1 => Ok(Self::User),
            2 => Ok(Self::ModelUnsupported),
            _ => Err(format!("invalid reasoning effort change cause {code}").into()),
        }
    }
}

impl std::fmt::Display for ReasoningEffortChangeCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Initial => "initial",
            Self::User => "user",
            Self::ModelUnsupported => "model-unsupported",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReasoningEffortChangedEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub effort: String,
    pub cause: ReasoningEffortChangeCause,
}

pub const MAX_SYSTEM_STATIC_PROMPT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum SystemStaticPromptMode {
    Default,
    Custom,
}

impl SystemStaticPromptMode {
    fn code(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Custom => 1,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Default),
            1 => Ok(Self::Custom),
            _ => Err(format!("invalid system static prompt mode {code}").into()),
        }
    }
}

impl std::fmt::Display for SystemStaticPromptMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Default => "default",
            Self::Custom => "custom",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SystemStaticPromptChangeEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub mode: SystemStaticPromptMode,
    pub content: Option<String>,
}

pub fn validate_system_static_prompt_change(
    mode: SystemStaticPromptMode,
    content: Option<&str>,
) -> Result<()> {
    match (mode, content) {
        (SystemStaticPromptMode::Default, None) => Ok(()),
        (SystemStaticPromptMode::Default, Some(_)) => {
            Err("default system static prompt change cannot contain custom content".into())
        }
        (SystemStaticPromptMode::Custom, None) => {
            Err("custom system static prompt change requires content".into())
        }
        (SystemStaticPromptMode::Custom, Some(content)) if content.trim().is_empty() => {
            Err("custom system static prompt cannot be empty".into())
        }
        (SystemStaticPromptMode::Custom, Some(content))
            if content.len() > MAX_SYSTEM_STATIC_PROMPT_BYTES =>
        {
            Err(format!(
                "custom system static prompt exceeds {MAX_SYSTEM_STATIC_PROMPT_BYTES} UTF-8 bytes"
            )
            .into())
        }
        (SystemStaticPromptMode::Custom, Some(_)) => Ok(()),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContextClearedEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkMapMutationEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub tool_call_id: EventId,
    pub mutation: WorkMapMutation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkMapPendingReminderEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub prompt_id: EventId,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CompactStateUpdateEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub compact_id: EventId,
    pub tool_call_id: EventId,
    pub prompt_id: EventId,
    pub kind: CompactKind,
    pub total_stages: u8,
    pub state: CompactState,
    pub stage: Option<CompactStage>,
    pub content: String,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentTitleChangedEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub tool_call_id: EventId,
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CloneCompletedEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageContentEvent {
    pub id: EventId,
    pub timestamp_ms: u64,
    pub tool_call_id: EventId,
    pub source: String,
    pub mime_type: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub content_sha256: String,
    pub data: Arc<[u8]>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Event {
    EdbIdGeneration(EdbIdGenerationEvent),
    AgentKindDef(AgentKindDefEvent),
    AgentTurn(AgentTurnEvent),
    SystemPrompt(SystemPromptEvent),
    UserPrompt(UserPromptEvent),
    ManagerPrompt(ManagerPromptEvent),
    ParentAgentPrompt(ParentAgentPromptEvent),
    FollowUpPrompt(FollowUpPromptEvent),
    AssistResponse(AssistResponseEvent),
    ApiStateUpdate(ApiStateUpdateEvent),
    ContextUsageEstimate(ContextUsageEstimateEvent),
    UserTurnAborted(UserTurnAbortedEvent),
    ToolCall(ToolCallEvent),
    ToolInfoUpdate(ToolInfoUpdateEvent),
    ToolCallResult(ToolCallResultEvent),
    TerminalSessionCreated(TerminalSessionCreatedEvent),
    TerminalSessionState(TerminalSessionStateEvent),
    ModelContextItem(ModelContextItemEvent),
    ModelChanged(ModelChangedEvent),
    ReasoningEffortChanged(ReasoningEffortChangedEvent),
    ContextCleared(ContextClearedEvent),
    WorkMapMutation(WorkMapMutationEvent),
    WorkMapPendingReminder(WorkMapPendingReminderEvent),
    CompactStateUpdate(CompactStateUpdateEvent),
    SystemStaticPromptChange(SystemStaticPromptChangeEvent),
    AgentTitleChanged(AgentTitleChangedEvent),
    CloneCompleted(CloneCompletedEvent),
    ImageContent(ImageContentEvent),
}

impl Event {
    pub fn root_prompt_content(&self) -> Option<&str> {
        match self {
            Self::UserPrompt(prompt) => Some(&prompt.content),
            Self::ManagerPrompt(prompt) => Some(&prompt.content),
            Self::ParentAgentPrompt(prompt) => Some(&prompt.content),
            _ => None,
        }
    }

    pub fn is_root_prompt(&self) -> bool {
        self.root_prompt_content().is_some()
    }

    pub fn id(&self) -> EventId {
        match self {
            Self::EdbIdGeneration(event) => event.id,
            Self::AgentKindDef(event) => event.id,
            Self::AgentTurn(event) => event.id,
            Self::SystemPrompt(event) => event.id,
            Self::UserPrompt(event) => event.id,
            Self::ManagerPrompt(event) => event.id,
            Self::ParentAgentPrompt(event) => event.id,
            Self::FollowUpPrompt(event) => event.id,
            Self::AssistResponse(event) => event.id,
            Self::ApiStateUpdate(event) => event.id,
            Self::ContextUsageEstimate(event) => event.id,
            Self::UserTurnAborted(event) => event.id,
            Self::ToolCall(event) => event.id,
            Self::ToolInfoUpdate(event) => event.id,
            Self::ToolCallResult(event) => event.id,
            Self::TerminalSessionCreated(event) => event.id,
            Self::TerminalSessionState(event) => event.id,
            Self::ModelContextItem(event) => event.id,
            Self::ModelChanged(event) => event.id,
            Self::ReasoningEffortChanged(event) => event.id,
            Self::ContextCleared(event) => event.id,
            Self::WorkMapMutation(event) => event.id,
            Self::WorkMapPendingReminder(event) => event.id,
            Self::CompactStateUpdate(event) => event.id,
            Self::SystemStaticPromptChange(event) => event.id,
            Self::AgentTitleChanged(event) => event.id,
            Self::CloneCompleted(event) => event.id,
            Self::ImageContent(event) => event.id,
        }
    }

    pub fn kind(&self) -> EventKind {
        match self {
            Self::EdbIdGeneration(_) => EventKind::EdbIdGeneration,
            Self::AgentKindDef(_) => EventKind::AgentKindDef,
            Self::AgentTurn(_) => EventKind::AgentTurn,
            Self::SystemPrompt(_) => EventKind::SystemPrompt,
            Self::UserPrompt(_) => EventKind::UserPrompt,
            Self::ManagerPrompt(_) => EventKind::ManagerPrompt,
            Self::ParentAgentPrompt(_) => EventKind::ParentAgentPrompt,
            Self::FollowUpPrompt(_) => EventKind::FollowUpPrompt,
            Self::AssistResponse(_) => EventKind::AssistResponse,
            Self::ApiStateUpdate(_) => EventKind::ApiStateUpdate,
            Self::ContextUsageEstimate(_) => EventKind::ContextUsageEstimate,
            Self::UserTurnAborted(_) => EventKind::UserTurnAborted,
            Self::ToolCall(_) => EventKind::ToolCall,
            Self::ToolInfoUpdate(_) => EventKind::ToolInfoUpdate,
            Self::ToolCallResult(_) => EventKind::ToolCallResult,
            Self::TerminalSessionCreated(_) => EventKind::TerminalSessionCreated,
            Self::TerminalSessionState(_) => EventKind::TerminalSessionState,
            Self::ModelContextItem(_) => EventKind::ModelContextItem,
            Self::ModelChanged(_) => EventKind::ModelChanged,
            Self::ReasoningEffortChanged(_) => EventKind::ReasoningEffortChanged,
            Self::ContextCleared(_) => EventKind::ContextCleared,
            Self::WorkMapMutation(_) => EventKind::WorkMapMutation,
            Self::WorkMapPendingReminder(_) => EventKind::WorkMapPendingReminder,
            Self::CompactStateUpdate(_) => EventKind::CompactStateUpdate,
            Self::SystemStaticPromptChange(_) => EventKind::SystemStaticPromptChange,
            Self::AgentTitleChanged(_) => EventKind::AgentTitleChanged,
            Self::CloneCompleted(_) => EventKind::CloneCompleted,
            Self::ImageContent(_) => EventKind::ImageContent,
        }
    }

    pub fn timestamp_ms(&self) -> u64 {
        match self {
            Self::EdbIdGeneration(event) => event.timestamp_ms,
            Self::AgentKindDef(event) => event.timestamp_ms,
            Self::AgentTurn(event) => event.timestamp_ms,
            Self::SystemPrompt(event) => event.timestamp_ms,
            Self::UserPrompt(event) => event.timestamp_ms,
            Self::ManagerPrompt(event) => event.timestamp_ms,
            Self::ParentAgentPrompt(event) => event.timestamp_ms,
            Self::FollowUpPrompt(event) => event.timestamp_ms,
            Self::AssistResponse(event) => event.timestamp_ms,
            Self::ApiStateUpdate(event) => event.timestamp_ms,
            Self::ContextUsageEstimate(event) => event.timestamp_ms,
            Self::UserTurnAborted(event) => event.timestamp_ms,
            Self::ToolCall(event) => event.timestamp_ms,
            Self::ToolInfoUpdate(event) => event.timestamp_ms,
            Self::ToolCallResult(event) => event.timestamp_ms,
            Self::TerminalSessionCreated(event) => event.timestamp_ms,
            Self::TerminalSessionState(event) => event.timestamp_ms,
            Self::ModelContextItem(event) => event.timestamp_ms,
            Self::ModelChanged(event) => event.timestamp_ms,
            Self::ReasoningEffortChanged(event) => event.timestamp_ms,
            Self::ContextCleared(event) => event.timestamp_ms,
            Self::WorkMapMutation(event) => event.timestamp_ms,
            Self::WorkMapPendingReminder(event) => event.timestamp_ms,
            Self::CompactStateUpdate(event) => event.timestamp_ms,
            Self::SystemStaticPromptChange(event) => event.timestamp_ms,
            Self::AgentTitleChanged(event) => event.timestamp_ms,
            Self::CloneCompleted(event) => event.timestamp_ms,
            Self::ImageContent(event) => event.timestamp_ms,
        }
    }
}

#[allow(non_snake_case)]
impl EventBase for EdbIdGenerationEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::EdbIdGeneration
    }

    fn getHash(&self) -> String {
        stable_hash(self.id, self.timestamp_ms, 28, &[], &[], &[&self.edb_id])
    }

    fn getBriefString(&self) -> String {
        format!(
            "EdbIdGenerationEvent(edb_id={})",
            abbreviated(&self.edb_id, 12)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "EdbIdGenerationEvent(id={}, timestamp_ms={}, edb_id={})",
            self.id,
            self.timestamp_ms,
            quoted(&self.edb_id)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for AgentKindDefEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::AgentKindDef
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            16,
            &[],
            &[self.kind.code()],
            &[
                &self.orchestrator,
                self.parent_agent_id.as_deref().unwrap_or(""),
                self.system_prompt.as_deref().unwrap_or(""),
            ],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "AgentKindDefEvent(kind={}, orchestrator={}, parent={})",
            self.kind,
            self.orchestrator,
            self.parent_agent_id.as_deref().unwrap_or("none")
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "AgentKindDefEvent(id={}, timestamp_ms={}, kind={}, orchestrator={}, parent_agent_id={}, system_prompt={})",
            self.id,
            self.timestamp_ms,
            self.kind,
            quoted(&self.orchestrator),
            self.parent_agent_id
                .as_deref()
                .map(quoted)
                .unwrap_or_else(|| "none".into()),
            self.system_prompt
                .as_deref()
                .map(quoted)
                .unwrap_or_else(|| "none".into())
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for AgentTurnEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::AgentTurn
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            17,
            &[self.turn_id, self.prompt_id],
            &[self.state.code()],
            &[&self.detail],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "AgentTurnEvent(turn_id={}, prompt_id={}, state={}, detail={})",
            self.turn_id,
            self.prompt_id,
            self.state,
            abbreviated(&self.detail, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "AgentTurnEvent(id={}, timestamp_ms={}, turn_id={}, prompt_id={}, state={}, detail={})",
            self.id,
            self.timestamp_ms,
            self.turn_id,
            self.prompt_id,
            self.state,
            quoted(&self.detail)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for SystemPromptEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::SystemPrompt
    }

    fn getHash(&self) -> String {
        stable_hash(self.id, self.timestamp_ms, 3, &[], &[], &[&self.name])
    }

    fn getBriefString(&self) -> String {
        format!("SystemPromptEvent(name={})", abbreviated(&self.name, 80))
    }

    fn getDetailString(&self) -> String {
        format!(
            "SystemPromptEvent(id={}, timestamp_ms={}, name={})",
            self.id,
            self.timestamp_ms,
            quoted(&self.name)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for UserPromptEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::UserPrompt
    }

    fn getHash(&self) -> String {
        stable_hash(self.id, self.timestamp_ms, 0, &[], &[], &[&self.content])
    }

    fn getBriefString(&self) -> String {
        format!(
            "UserPromptEvent(content={})",
            abbreviated(&self.content, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "UserPromptEvent(id={}, timestamp_ms={}, content={})",
            self.id,
            self.timestamp_ms,
            quoted(&self.content)
        )
    }
}

macro_rules! impl_agent_prompt_event_base {
    ($type:ty, $kind:expr, $hash_kind:expr, $name:literal) => {
        #[allow(non_snake_case)]
        impl EventBase for $type {
            fn getID(&self) -> EventId {
                self.id
            }

            fn getTimestamp(&self) -> u64 {
                self.timestamp_ms
            }

            fn getEventKind(&self) -> EventKind {
                $kind
            }

            fn getHash(&self) -> String {
                stable_hash(
                    self.id,
                    self.timestamp_ms,
                    $hash_kind,
                    &[],
                    &[],
                    &[&self.content],
                )
            }

            fn getBriefString(&self) -> String {
                format!(
                    concat!($name, "(content={})"),
                    abbreviated(&self.content, 80)
                )
            }

            fn getDetailString(&self) -> String {
                format!(
                    concat!($name, "(id={}, timestamp_ms={}, content={})"),
                    self.id,
                    self.timestamp_ms,
                    quoted(&self.content)
                )
            }
        }
    };
}

impl_agent_prompt_event_base!(
    ManagerPromptEvent,
    EventKind::ManagerPrompt,
    23,
    "ManagerPromptEvent"
);
impl_agent_prompt_event_base!(
    ParentAgentPromptEvent,
    EventKind::ParentAgentPrompt,
    24,
    "ParentAgentPromptEvent"
);

#[allow(non_snake_case)]
impl EventBase for FollowUpPromptEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::FollowUpPrompt
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            9,
            &[self.prompt_id],
            &[],
            &[&self.content],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "FollowUpPromptEvent(prompt_id={}, content={})",
            self.prompt_id,
            abbreviated(&self.content, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "FollowUpPromptEvent(id={}, timestamp_ms={}, prompt_id={}, content={})",
            self.id,
            self.timestamp_ms,
            self.prompt_id,
            quoted(&self.content)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for AssistResponseEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::AssistResponse
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            1,
            &[self.prompt_id],
            &[u8::from(self.finished)],
            &[&self.content],
        )
    }

    fn getBriefString(&self) -> String {
        assist_brief(self)
    }

    fn getDetailString(&self) -> String {
        format!(
            "AssistResponseEvent(id={}, timestamp_ms={}, prompt_id={}, finished={}, content={})",
            self.id,
            self.timestamp_ms,
            self.prompt_id,
            self.finished,
            quoted(&self.content)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ApiStateUpdateEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ApiStateUpdate
    }

    fn getHash(&self) -> String {
        let mut metadata = vec![
            self.state.code(),
            self.retry_count,
            self.retry_limit,
            u8::from(self.usage.is_some()),
        ];
        if let Some(usage) = self.usage {
            encode_varint(usage.input_tokens, &mut metadata);
            encode_varint(usage.output_tokens, &mut metadata);
            encode_varint(usage.total_tokens, &mut metadata);
        }
        stable_hash(
            self.id,
            self.timestamp_ms,
            2,
            &[self.api_call_id, self.prompt_id],
            &metadata,
            &[&self.detail],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ApiStateUpdateEvent(api_call_id={}, prompt_id={}, state={}, retry={}/{}, usage={}, detail={})",
            self.api_call_id,
            self.prompt_id,
            self.state,
            self.retry_count,
            self.retry_limit,
            api_usage_string(self.usage),
            abbreviated(&self.detail, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ApiStateUpdateEvent(id={}, timestamp_ms={}, api_call_id={}, prompt_id={}, state={}, retry_count={}, retry_limit={}, usage={}, detail={})",
            self.id,
            self.timestamp_ms,
            self.api_call_id,
            self.prompt_id,
            self.state,
            self.retry_count,
            self.retry_limit,
            api_usage_string(self.usage),
            quoted(&self.detail)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ContextUsageEstimateEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ContextUsageEstimate
    }

    fn getHash(&self) -> String {
        let mut metadata = Vec::new();
        for value in [
            self.values.system,
            self.values.compact,
            self.values.memory,
            self.values.user,
            self.values.model,
            self.values.tool,
        ] {
            encode_varint(value, &mut metadata);
        }
        stable_hash(
            self.id,
            self.timestamp_ms,
            26,
            &[self.api_state_event_id],
            &metadata,
            &[],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ContextUsageEstimateEvent(api_state_event_id={}, total={}, system={}, compact={}, memory={}, user={}, model={}, tool={})",
            self.api_state_event_id,
            self.values.sum(),
            self.values.system,
            self.values.compact,
            self.values.memory,
            self.values.user,
            self.values.model,
            self.values.tool,
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ContextUsageEstimateEvent(id={}, timestamp_ms={}, api_state_event_id={}, total={}, system={}, compact={}, memory={}, user={}, model={}, tool={})",
            self.id,
            self.timestamp_ms,
            self.api_state_event_id,
            self.values.sum(),
            self.values.system,
            self.values.compact,
            self.values.memory,
            self.values.user,
            self.values.model,
            self.values.tool,
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for UserTurnAbortedEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::UserTurnAborted
    }

    fn getHash(&self) -> String {
        stable_hash(self.id, self.timestamp_ms, 15, &[self.prompt_id], &[], &[])
    }

    fn getBriefString(&self) -> String {
        format!("UserTurnAbortedEvent(prompt_id={})", self.prompt_id)
    }

    fn getDetailString(&self) -> String {
        format!(
            "UserTurnAbortedEvent(id={}, timestamp_ms={}, prompt_id={})",
            self.id, self.timestamp_ms, self.prompt_id
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ToolCallEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ToolCall
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            4,
            &[self.api_call_id, self.prompt_id],
            &[],
            &[&self.provider_call_id, &self.name, &self.arguments],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ToolCallEvent(api_call_id={}, prompt_id={}, name={}, arguments={})",
            self.api_call_id,
            self.prompt_id,
            quoted(&self.name),
            abbreviated(&self.arguments, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ToolCallEvent(id={}, timestamp_ms={}, api_call_id={}, prompt_id={}, provider_call_id={}, name={}, arguments={})",
            self.id,
            self.timestamp_ms,
            self.api_call_id,
            self.prompt_id,
            quoted(&self.provider_call_id),
            quoted(&self.name),
            quoted(&self.arguments)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ToolInfoUpdateEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ToolInfoUpdate
    }

    fn getHash(&self) -> String {
        let content = self.content.stable_string();
        stable_hash(
            self.id,
            self.timestamp_ms,
            5,
            &[self.tool_call_id],
            &[self.stream.code()],
            &[&content],
        )
    }

    fn getBriefString(&self) -> String {
        let content = self.content.stable_string();
        format!(
            "ToolInfoUpdateEvent(tool_call_id={}, stream={}, content={})",
            self.tool_call_id,
            self.stream,
            abbreviated(&content, 80)
        )
    }

    fn getDetailString(&self) -> String {
        let content = self.content.stable_string();
        format!(
            "ToolInfoUpdateEvent(id={}, timestamp_ms={}, tool_call_id={}, stream={}, content={})",
            self.id,
            self.timestamp_ms,
            self.tool_call_id,
            self.stream,
            quoted(&content)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ToolCallResultEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ToolCallResult
    }

    fn getHash(&self) -> String {
        let mut metadata = vec![self.state.code(), u8::from(self.exit_code.is_some())];
        if let Some(exit_code) = self.exit_code {
            metadata.extend_from_slice(&exit_code.to_le_bytes());
        }
        stable_hash(
            self.id,
            self.timestamp_ms,
            6,
            &[self.tool_call_id],
            &metadata,
            &[&self.detail],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ToolCallResultEvent(tool_call_id={}, state={}, exit_code={:?}, detail={})",
            self.tool_call_id,
            self.state,
            self.exit_code,
            abbreviated(&self.detail, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ToolCallResultEvent(id={}, timestamp_ms={}, tool_call_id={}, state={}, exit_code={:?}, detail={})",
            self.id,
            self.timestamp_ms,
            self.tool_call_id,
            self.state,
            self.exit_code,
            quoted(&self.detail)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for TerminalSessionCreatedEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::TerminalSessionCreated
    }

    fn getHash(&self) -> String {
        let mut metadata = Vec::with_capacity(4);
        metadata.extend_from_slice(&self.width.to_le_bytes());
        metadata.extend_from_slice(&self.height.to_le_bytes());
        stable_hash(
            self.id,
            self.timestamp_ms,
            7,
            &[self.tool_call_id],
            &metadata,
            &[&self.session_id, &self.shell, &self.cwd],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "TerminalSessionCreatedEvent(tool_call_id={}, session_id={}, shell={}, size={}x{})",
            self.tool_call_id,
            quoted(&self.session_id),
            quoted(&self.shell),
            self.width,
            self.height
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "TerminalSessionCreatedEvent(id={}, timestamp_ms={}, tool_call_id={}, session_id={}, shell={}, cwd={}, width={}, height={})",
            self.id,
            self.timestamp_ms,
            self.tool_call_id,
            quoted(&self.session_id),
            quoted(&self.shell),
            quoted(&self.cwd),
            self.width,
            self.height
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for TerminalSessionStateEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::TerminalSessionState
    }

    fn getHash(&self) -> String {
        let mut metadata = vec![self.state.code(), u8::from(self.exit_code.is_some())];
        if let Some(exit_code) = self.exit_code {
            metadata.extend_from_slice(&exit_code.to_le_bytes());
        }
        stable_hash(
            self.id,
            self.timestamp_ms,
            8,
            &[],
            &metadata,
            &[&self.session_id, &self.detail],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "TerminalSessionStateEvent(session_id={}, state={}, exit_code={:?}, detail={})",
            quoted(&self.session_id),
            self.state,
            self.exit_code,
            abbreviated(&self.detail, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "TerminalSessionStateEvent(id={}, timestamp_ms={}, session_id={}, state={}, exit_code={:?}, detail={})",
            self.id,
            self.timestamp_ms,
            quoted(&self.session_id),
            self.state,
            self.exit_code,
            quoted(&self.detail)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ModelContextItemEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ModelContextItem
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            10,
            &[self.api_call_id, self.prompt_id],
            &[],
            &[&self.provider, &self.content],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ModelContextItemEvent(api_call_id={}, prompt_id={}, provider={}, content={})",
            self.api_call_id,
            self.prompt_id,
            quoted(&self.provider),
            abbreviated(&self.content, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ModelContextItemEvent(id={}, timestamp_ms={}, api_call_id={}, prompt_id={}, provider={}, content={})",
            self.id,
            self.timestamp_ms,
            self.api_call_id,
            self.prompt_id,
            quoted(&self.provider),
            quoted(&self.content)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ModelChangedEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ModelChanged
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            14,
            &[],
            &[self.cause.code()],
            &[&self.model],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ModelChangedEvent(model={}, cause={})",
            quoted(&self.model),
            self.cause
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ModelChangedEvent(id={}, timestamp_ms={}, model={}, cause={})",
            self.id,
            self.timestamp_ms,
            quoted(&self.model),
            self.cause
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ReasoningEffortChangedEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ReasoningEffortChanged
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            11,
            &[],
            &[self.cause.code()],
            &[&self.effort],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ReasoningEffortChangedEvent(effort={}, cause={})",
            quoted(&self.effort),
            self.cause
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ReasoningEffortChangedEvent(id={}, timestamp_ms={}, effort={}, cause={})",
            self.id,
            self.timestamp_ms,
            quoted(&self.effort),
            self.cause
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ContextClearedEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ContextCleared
    }

    fn getHash(&self) -> String {
        stable_hash(self.id, self.timestamp_ms, 12, &[], &[], &[])
    }

    fn getBriefString(&self) -> String {
        "ContextClearedEvent".to_owned()
    }

    fn getDetailString(&self) -> String {
        format!(
            "ContextClearedEvent(id={}, timestamp_ms={})",
            self.id, self.timestamp_ms
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for WorkMapMutationEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::WorkMapMutation
    }

    fn getHash(&self) -> String {
        let mutation = serde_json::to_string(&self.mutation)
            .expect("WorkMapMutation serialization cannot fail");
        stable_hash(
            self.id,
            self.timestamp_ms,
            18,
            &[self.tool_call_id],
            &[],
            &[&mutation],
        )
    }

    fn getBriefString(&self) -> String {
        let ids = self
            .mutation
            .records
            .iter()
            .map(WorkMapRecord::id)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "WorkMapMutationEvent(tool_call_id={}, operation={:?}, records={})",
            self.tool_call_id, self.mutation.operation, ids
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "WorkMapMutationEvent(id={}, timestamp_ms={}, tool_call_id={}, mutation={})",
            self.id,
            self.timestamp_ms,
            self.tool_call_id,
            serde_json::to_string(&self.mutation)
                .expect("WorkMapMutation serialization cannot fail")
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for WorkMapPendingReminderEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::WorkMapPendingReminder
    }

    fn getHash(&self) -> String {
        stable_hash(self.id, self.timestamp_ms, 20, &[self.prompt_id], &[], &[])
    }

    fn getBriefString(&self) -> String {
        format!("WorkMapPendingReminderEvent(prompt_id={})", self.prompt_id)
    }

    fn getDetailString(&self) -> String {
        format!(
            "WorkMapPendingReminderEvent(id={}, timestamp_ms={}, prompt_id={})",
            self.id, self.timestamp_ms, self.prompt_id
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for CompactStateUpdateEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::CompactStateUpdate
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            19,
            &[self.compact_id, self.tool_call_id, self.prompt_id],
            &[
                self.kind.code(),
                self.total_stages,
                self.state.code(),
                self.stage.map_or(u8::MAX, CompactStage::code),
            ],
            &[&self.content, &self.detail],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "CompactStateUpdateEvent(compact_id={}, kind={}, total_stages={}, state={}, stage={})",
            self.compact_id,
            self.kind,
            self.total_stages,
            self.state,
            self.stage
                .map_or_else(|| "none".to_owned(), |stage| stage.to_string())
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "CompactStateUpdateEvent(id={}, timestamp_ms={}, compact_id={}, tool_call_id={}, prompt_id={}, kind={}, total_stages={}, state={}, stage={}, content={}, detail={})",
            self.id,
            self.timestamp_ms,
            self.compact_id,
            self.tool_call_id,
            self.prompt_id,
            self.kind,
            self.total_stages,
            self.state,
            self.stage
                .map_or_else(|| "none".to_owned(), |stage| stage.to_string()),
            quoted(&self.content),
            quoted(&self.detail)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for SystemStaticPromptChangeEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::SystemStaticPromptChange
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            27,
            &[],
            &[self.mode.code()],
            &[self.content.as_deref().unwrap_or("")],
        )
    }

    fn getBriefString(&self) -> String {
        match &self.content {
            Some(content) => format!(
                "SystemStaticPromptChangeEvent(mode=custom, content={})",
                abbreviated(content, 80)
            ),
            None => "SystemStaticPromptChangeEvent(mode=default)".into(),
        }
    }

    fn getDetailString(&self) -> String {
        format!(
            "SystemStaticPromptChangeEvent(id={}, timestamp_ms={}, mode={}, content={})",
            self.id,
            self.timestamp_ms,
            self.mode,
            self.content
                .as_deref()
                .map_or_else(|| "none".to_owned(), quoted)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for AgentTitleChangedEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::AgentTitleChanged
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            21,
            &[self.tool_call_id],
            &[],
            &[&self.title],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "AgentTitleChangedEvent(tool_call_id={}, title={})",
            self.tool_call_id,
            abbreviated(&self.title, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "AgentTitleChangedEvent(id={}, timestamp_ms={}, tool_call_id={}, title={})",
            self.id,
            self.timestamp_ms,
            self.tool_call_id,
            quoted(&self.title)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for CloneCompletedEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::CloneCompleted
    }

    fn getHash(&self) -> String {
        stable_hash(self.id, self.timestamp_ms, 22, &[], &[], &[&self.title])
    }

    fn getBriefString(&self) -> String {
        format!(
            "CloneCompletedEvent(title={})",
            abbreviated(&self.title, 80)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "CloneCompletedEvent(id={}, timestamp_ms={}, title={})",
            self.id,
            self.timestamp_ms,
            quoted(&self.title)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for ImageContentEvent {
    fn getID(&self) -> EventId {
        self.id
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms
    }

    fn getEventKind(&self) -> EventKind {
        EventKind::ImageContent
    }

    fn getHash(&self) -> String {
        stable_hash(
            self.id,
            self.timestamp_ms,
            25,
            &[
                self.tool_call_id,
                u64::from(self.width),
                u64::from(self.height),
            ],
            &[],
            &[
                &self.source,
                &self.mime_type,
                &self.format,
                &self.content_sha256,
            ],
        )
    }

    fn getBriefString(&self) -> String {
        format!(
            "ImageContentEvent(tool_call_id={}, format={}, dimensions={}x{}, bytes={}, sha256={})",
            self.tool_call_id,
            self.format,
            self.width,
            self.height,
            self.data.len(),
            abbreviated(&self.content_sha256, 12)
        )
    }

    fn getDetailString(&self) -> String {
        format!(
            "ImageContentEvent(id={}, timestamp_ms={}, tool_call_id={}, source={}, mime_type={}, format={}, width={}, height={}, bytes={}, content_sha256={})",
            self.id,
            self.timestamp_ms,
            self.tool_call_id,
            quoted(&self.source),
            quoted(&self.mime_type),
            quoted(&self.format),
            self.width,
            self.height,
            self.data.len(),
            quoted(&self.content_sha256)
        )
    }
}

#[allow(non_snake_case)]
impl EventBase for Event {
    fn getID(&self) -> EventId {
        match self {
            Self::EdbIdGeneration(event) => event.getID(),
            Self::AgentKindDef(event) => event.getID(),
            Self::AgentTurn(event) => event.getID(),
            Self::SystemPrompt(event) => event.getID(),
            Self::UserPrompt(event) => event.getID(),
            Self::ManagerPrompt(event) => event.getID(),
            Self::ParentAgentPrompt(event) => event.getID(),
            Self::FollowUpPrompt(event) => event.getID(),
            Self::AssistResponse(event) => event.getID(),
            Self::ApiStateUpdate(event) => event.getID(),
            Self::ContextUsageEstimate(event) => event.getID(),
            Self::UserTurnAborted(event) => event.getID(),
            Self::ToolCall(event) => event.getID(),
            Self::ToolInfoUpdate(event) => event.getID(),
            Self::ToolCallResult(event) => event.getID(),
            Self::TerminalSessionCreated(event) => event.getID(),
            Self::TerminalSessionState(event) => event.getID(),
            Self::ModelContextItem(event) => event.getID(),
            Self::ModelChanged(event) => event.getID(),
            Self::ReasoningEffortChanged(event) => event.getID(),
            Self::ContextCleared(event) => event.getID(),
            Self::WorkMapMutation(event) => event.getID(),
            Self::WorkMapPendingReminder(event) => event.getID(),
            Self::CompactStateUpdate(event) => event.getID(),
            Self::SystemStaticPromptChange(event) => event.getID(),
            Self::AgentTitleChanged(event) => event.getID(),
            Self::CloneCompleted(event) => event.getID(),
            Self::ImageContent(event) => event.getID(),
        }
    }

    fn getTimestamp(&self) -> u64 {
        self.timestamp_ms()
    }

    fn getEventKind(&self) -> EventKind {
        match self {
            Self::EdbIdGeneration(event) => event.getEventKind(),
            Self::AgentKindDef(event) => event.getEventKind(),
            Self::AgentTurn(event) => event.getEventKind(),
            Self::SystemPrompt(event) => event.getEventKind(),
            Self::UserPrompt(event) => event.getEventKind(),
            Self::ManagerPrompt(event) => event.getEventKind(),
            Self::ParentAgentPrompt(event) => event.getEventKind(),
            Self::FollowUpPrompt(event) => event.getEventKind(),
            Self::AssistResponse(event) => event.getEventKind(),
            Self::ApiStateUpdate(event) => event.getEventKind(),
            Self::ContextUsageEstimate(event) => event.getEventKind(),
            Self::UserTurnAborted(event) => event.getEventKind(),
            Self::ToolCall(event) => event.getEventKind(),
            Self::ToolInfoUpdate(event) => event.getEventKind(),
            Self::ToolCallResult(event) => event.getEventKind(),
            Self::TerminalSessionCreated(event) => event.getEventKind(),
            Self::TerminalSessionState(event) => event.getEventKind(),
            Self::ModelContextItem(event) => event.getEventKind(),
            Self::ModelChanged(event) => event.getEventKind(),
            Self::ReasoningEffortChanged(event) => event.getEventKind(),
            Self::ContextCleared(event) => event.getEventKind(),
            Self::WorkMapMutation(event) => event.getEventKind(),
            Self::WorkMapPendingReminder(event) => event.getEventKind(),
            Self::CompactStateUpdate(event) => event.getEventKind(),
            Self::SystemStaticPromptChange(event) => event.getEventKind(),
            Self::AgentTitleChanged(event) => event.getEventKind(),
            Self::CloneCompleted(event) => event.getEventKind(),
            Self::ImageContent(event) => event.getEventKind(),
        }
    }

    fn getHash(&self) -> String {
        match self {
            Self::EdbIdGeneration(event) => event.getHash(),
            Self::AgentKindDef(event) => event.getHash(),
            Self::AgentTurn(event) => event.getHash(),
            Self::SystemPrompt(event) => event.getHash(),
            Self::UserPrompt(event) => event.getHash(),
            Self::ManagerPrompt(event) => event.getHash(),
            Self::ParentAgentPrompt(event) => event.getHash(),
            Self::FollowUpPrompt(event) => event.getHash(),
            Self::AssistResponse(event) => event.getHash(),
            Self::ApiStateUpdate(event) => event.getHash(),
            Self::ContextUsageEstimate(event) => event.getHash(),
            Self::UserTurnAborted(event) => event.getHash(),
            Self::ToolCall(event) => event.getHash(),
            Self::ToolInfoUpdate(event) => event.getHash(),
            Self::ToolCallResult(event) => event.getHash(),
            Self::TerminalSessionCreated(event) => event.getHash(),
            Self::TerminalSessionState(event) => event.getHash(),
            Self::ModelContextItem(event) => event.getHash(),
            Self::ModelChanged(event) => event.getHash(),
            Self::ReasoningEffortChanged(event) => event.getHash(),
            Self::ContextCleared(event) => event.getHash(),
            Self::WorkMapMutation(event) => event.getHash(),
            Self::WorkMapPendingReminder(event) => event.getHash(),
            Self::CompactStateUpdate(event) => event.getHash(),
            Self::SystemStaticPromptChange(event) => event.getHash(),
            Self::AgentTitleChanged(event) => event.getHash(),
            Self::CloneCompleted(event) => event.getHash(),
            Self::ImageContent(event) => event.getHash(),
        }
    }

    fn getBriefString(&self) -> String {
        match self {
            Self::EdbIdGeneration(event) => event.getBriefString(),
            Self::AgentKindDef(event) => event.getBriefString(),
            Self::AgentTurn(event) => event.getBriefString(),
            Self::SystemPrompt(event) => event.getBriefString(),
            Self::UserPrompt(event) => event.getBriefString(),
            Self::ManagerPrompt(event) => event.getBriefString(),
            Self::ParentAgentPrompt(event) => event.getBriefString(),
            Self::FollowUpPrompt(event) => event.getBriefString(),
            Self::AssistResponse(event) => event.getBriefString(),
            Self::ApiStateUpdate(event) => event.getBriefString(),
            Self::ContextUsageEstimate(event) => event.getBriefString(),
            Self::UserTurnAborted(event) => event.getBriefString(),
            Self::ToolCall(event) => event.getBriefString(),
            Self::ToolInfoUpdate(event) => event.getBriefString(),
            Self::ToolCallResult(event) => event.getBriefString(),
            Self::TerminalSessionCreated(event) => event.getBriefString(),
            Self::TerminalSessionState(event) => event.getBriefString(),
            Self::ModelContextItem(event) => event.getBriefString(),
            Self::ModelChanged(event) => event.getBriefString(),
            Self::ReasoningEffortChanged(event) => event.getBriefString(),
            Self::ContextCleared(event) => event.getBriefString(),
            Self::WorkMapMutation(event) => event.getBriefString(),
            Self::WorkMapPendingReminder(event) => event.getBriefString(),
            Self::CompactStateUpdate(event) => event.getBriefString(),
            Self::SystemStaticPromptChange(event) => event.getBriefString(),
            Self::AgentTitleChanged(event) => event.getBriefString(),
            Self::CloneCompleted(event) => event.getBriefString(),
            Self::ImageContent(event) => event.getBriefString(),
        }
    }

    fn getDetailString(&self) -> String {
        match self {
            Self::EdbIdGeneration(event) => event.getDetailString(),
            Self::AgentKindDef(event) => event.getDetailString(),
            Self::AgentTurn(event) => event.getDetailString(),
            Self::SystemPrompt(event) => event.getDetailString(),
            Self::UserPrompt(event) => event.getDetailString(),
            Self::ManagerPrompt(event) => event.getDetailString(),
            Self::ParentAgentPrompt(event) => event.getDetailString(),
            Self::FollowUpPrompt(event) => event.getDetailString(),
            Self::AssistResponse(event) => event.getDetailString(),
            Self::ApiStateUpdate(event) => event.getDetailString(),
            Self::ContextUsageEstimate(event) => event.getDetailString(),
            Self::UserTurnAborted(event) => event.getDetailString(),
            Self::ToolCall(event) => event.getDetailString(),
            Self::ToolInfoUpdate(event) => event.getDetailString(),
            Self::ToolCallResult(event) => event.getDetailString(),
            Self::TerminalSessionCreated(event) => event.getDetailString(),
            Self::TerminalSessionState(event) => event.getDetailString(),
            Self::ModelContextItem(event) => event.getDetailString(),
            Self::ModelChanged(event) => event.getDetailString(),
            Self::ReasoningEffortChanged(event) => event.getDetailString(),
            Self::ContextCleared(event) => event.getDetailString(),
            Self::WorkMapMutation(event) => event.getDetailString(),
            Self::WorkMapPendingReminder(event) => event.getDetailString(),
            Self::CompactStateUpdate(event) => event.getDetailString(),
            Self::SystemStaticPromptChange(event) => event.getDetailString(),
            Self::AgentTitleChanged(event) => event.getDetailString(),
            Self::CloneCompleted(event) => event.getDetailString(),
            Self::ImageContent(event) => event.getDetailString(),
        }
    }
}

pub struct EventDataBase {
    events: Vec<Event>,
    file: Option<File>,
    path: Option<PathBuf>,
    persisted_size_bytes: u64,
    next_event_id: EventId,
    mutation_revision: u64,
    last_mutation: Option<EdbMutation>,
}

impl Default for EventDataBase {
    fn default() -> Self {
        Self::new()
    }
}

impl EventDataBase {
    pub fn new() -> Self {
        let identity = new_edb_id_event(
            current_timestamp_ms().expect("system time must be available for an in-memory EDB"),
        )
        .expect("OS randomness must be available for an in-memory EDB");
        Self {
            events: vec![identity],
            file: None,
            path: None,
            persisted_size_bytes: 0,
            next_event_id: 1,
            mutation_revision: 0,
            last_mutation: None,
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            initialize_file(path)?;
        }
        let mut bytes = fs::read(path)?;
        if let Some(mut migration) = edb_migration::plan(&bytes)? {
            if migration.source_version >= migration.target_version
                || migration.target_version != edb_migration::CURRENT_FILE_VERSION
            {
                return Err("EDB migration plan did not target the current version".into());
            }
            let (events, valid_len, _) = decode_file(&migration.bytes)?;
            validate_event_ids(&events)?;
            validate_edb_identity(&events)?;
            migration.bytes.truncate(valid_len);
            edb_migration::commit(path, &migration.bytes)?;
            bytes = migration.bytes;
        }
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

        let (events, valid_len, persisted_next_event_id) = decode_file(&bytes)?;
        validate_event_ids(&events)?;
        validate_edb_identity(&events)?;
        if valid_len < bytes.len() {
            let recovery_file = OpenOptions::new().write(true).open(path)?;
            recovery_file.set_len(valid_len as u64)?;
            recovery_file.sync_data()?;
        }
        let file = OpenOptions::new().read(true).append(true).open(path)?;
        let next_after_events = events
            .last()
            .map(Event::id)
            .map_or(Ok(0), |id| id.checked_add(1).ok_or("EventId exhausted"))?;
        let next_event_id = persisted_next_event_id.max(next_after_events);
        let mut edb = Self {
            events,
            file: Some(file),
            path: Some(path.to_owned()),
            persisted_size_bytes: u64::try_from(valid_len)?,
            next_event_id,
            mutation_revision: 0,
            last_mutation: None,
        };
        edb.repair_invalid_tool_call_tail()?;
        Ok(edb)
    }

    fn repair_invalid_tool_call_tail(&mut self) -> Result<()> {
        let Some((invalid_order, api_call_id, prompt_id, tool_call_id)) = self
            .events
            .iter()
            .enumerate()
            .find_map(|(order, event)| match event {
                Event::ToolCall(call)
                    if serde_json::from_str::<serde_json::Value>(&call.arguments).is_err() =>
                {
                    Some((order, call.api_call_id, call.prompt_id, call.id))
                }
                _ => None,
            })
        else {
            return Ok(());
        };
        let prefix = &self.events[..invalid_order];
        if !prefix.iter().any(|event| {
            matches!(
                event,
                Event::ApiStateUpdate(update)
                    if update.id == api_call_id
                        && update.api_call_id == api_call_id
                        && update.prompt_id == prompt_id
                        && update.state == ApiState::Requesting
            )
        }) {
            return Err(format!(
                "cannot recover invalid JSON tool call {tool_call_id}: API call {api_call_id} is missing"
            )
            .into());
        }
        if prefix.iter().any(|event| {
            matches!(
                event,
                Event::ApiStateUpdate(update)
                    if update.api_call_id == api_call_id && update.state.is_terminal()
            )
        }) {
            return Err(format!(
                "cannot recover invalid JSON tool call {tool_call_id}: API call {api_call_id} already ended before the tool call"
            )
            .into());
        }

        let id = self.next_event_id;
        let next_event_id = id.checked_add(1).ok_or("EventId exhausted")?;
        let timestamp_ms = self.next_timestamp_ms()?;
        let mut events = prefix.to_vec();
        events.push(Event::ApiStateUpdate(ApiStateUpdateEvent {
            id,
            timestamp_ms,
            api_call_id,
            prompt_id,
            state: ApiState::Interrupted,
            retry_count: 0,
            retry_limit: 0,
            usage: None,
            detail: "model response was incomplete; discarded its invalid tool call and all later events during startup recovery".into(),
        }));
        validate_event_ids(&events)?;
        validate_edb_identity(&events)?;
        let persisted_size_bytes = if let Some(path) = &self.path {
            persist_replacement(path, &events, next_event_id, &mut self.file)?
        } else {
            0
        };
        self.events = events;
        self.persisted_size_bytes = persisted_size_bytes;
        self.next_event_id = next_event_id;
        Ok(())
    }

    pub(crate) fn close_storage(&mut self) {
        self.file.take();
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn reopen_storage(&mut self) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .ok_or("in-memory EDB has no storage to reopen")?;
        self.file = Some(OpenOptions::new().read(true).append(true).open(path)?);
        Ok(())
    }

    pub fn append_agent_kind_def(
        &mut self,
        kind: AgentKind,
        orchestrator: impl Into<String>,
        parent_agent_id: Option<String>,
        system_prompt: Option<String>,
    ) -> Result<EventId> {
        if !self.is_empty() {
            return Err("AgentKindDefEvent must immediately follow EdbIdGenerationEvent".into());
        }
        let orchestrator = orchestrator.into();
        if orchestrator.is_empty() {
            return Err("Agent definition requires an orchestrator".into());
        }
        match kind {
            AgentKind::SubAgent => {
                if parent_agent_id.as_deref().is_none_or(str::is_empty) {
                    return Err("sub-Agent definition requires parent_agent_id".into());
                }
            }
            AgentKind::Primary | AgentKind::Interactive => {
                if parent_agent_id.is_some() || system_prompt.is_some() {
                    return Err(
                        "only a sub-Agent definition may contain parent or system prompt data"
                            .into(),
                    );
                }
            }
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::AgentKindDef(AgentKindDefEvent {
            id,
            timestamp_ms,
            kind,
            orchestrator,
            parent_agent_id,
            system_prompt: system_prompt.filter(|value| !value.is_empty()),
        }))?;
        Ok(id)
    }

    pub fn append_agent_turn(
        &mut self,
        turn_id: EventId,
        prompt_id: EventId,
        state: AgentTurnState,
        detail: impl Into<String>,
    ) -> Result<EventId> {
        if self
            .get(prompt_id)
            .is_none_or(|event| !event.is_root_prompt())
        {
            return Err(format!("AgentTurnEvent references invalid prompt {prompt_id}").into());
        }
        let latest = latest_agent_turn(self.events())?;
        match state {
            AgentTurnState::Started => {
                if turn_id != prompt_id {
                    return Err("Agent turn ID must equal its root prompt EventId".into());
                }
                if latest
                    .as_ref()
                    .is_some_and(|turn| !turn.state.is_terminal())
                {
                    return Err("cannot start an Agent turn while another turn is open".into());
                }
            }
            terminal => match latest {
                Some(turn)
                    if turn.turn_id == turn_id
                        && turn.prompt_id == prompt_id
                        && !turn.state.is_terminal() => {}
                Some(turn) => {
                    return Err(format!(
                        "cannot close Agent turn {turn_id} as {terminal}; latest turn {} is {}",
                        turn.turn_id, turn.state
                    )
                    .into());
                }
                None => {
                    return Err(format!(
                        "cannot close Agent turn {turn_id} as {terminal} before it starts"
                    )
                    .into());
                }
            },
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::AgentTurn(AgentTurnEvent {
            id,
            timestamp_ms,
            turn_id,
            prompt_id,
            state,
            detail: detail.into(),
        }))?;
        Ok(id)
    }

    pub fn append_system_prompt(&mut self, name: impl Into<String>) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let name = name.into();
        if name.trim().is_empty() {
            return Err("system prompt name cannot be empty".into());
        }
        let event = Event::SystemPrompt(SystemPromptEvent {
            id,
            timestamp_ms,
            name,
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_user_prompt(&mut self, content: impl Into<String>) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::UserPrompt(UserPromptEvent {
            id,
            timestamp_ms,
            content: content.into(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_manager_prompt(&mut self, content: impl Into<String>) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::ManagerPrompt(ManagerPromptEvent {
            id,
            timestamp_ms,
            content: content.into(),
        }))?;
        Ok(id)
    }

    pub fn append_parent_agent_prompt(&mut self, content: impl Into<String>) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::ParentAgentPrompt(ParentAgentPromptEvent {
            id,
            timestamp_ms,
            content: content.into(),
        }))?;
        Ok(id)
    }

    pub fn append_follow_up_prompt(
        &mut self,
        prompt_id: EventId,
        content: impl Into<String>,
    ) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::FollowUpPrompt(FollowUpPromptEvent {
            id,
            timestamp_ms,
            prompt_id,
            content: content.into(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_workmap_pending_reminder(&mut self, prompt_id: EventId) -> Result<EventId> {
        if !matches!(self.events.last(), Some(Event::UserPrompt(prompt)) if prompt.id == prompt_id)
        {
            return Err(
                "WorkMapPendingReminderEvent must immediately follow its UserPromptEvent".into(),
            );
        }
        if WorkMapProjection::from_events(&self.events)?
            .current_snapshot()
            .is_none()
        {
            return Err("WorkMapPendingReminderEvent requires unfinished WorkMap work".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::WorkMapPendingReminder(WorkMapPendingReminderEvent {
            id,
            timestamp_ms,
            prompt_id,
        }))?;
        Ok(id)
    }

    pub fn append_assist_response(
        &mut self,
        prompt_id: EventId,
        content: impl Into<String>,
        finished: bool,
    ) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::AssistResponse(AssistResponseEvent {
            id,
            timestamp_ms,
            prompt_id,
            content: content.into(),
            finished,
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_api_requesting(&mut self, prompt_id: EventId) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ApiStateUpdate(ApiStateUpdateEvent {
            id,
            timestamp_ms,
            api_call_id: id,
            prompt_id,
            state: ApiState::Requesting,
            retry_count: 0,
            retry_limit: 0,
            usage: None,
            detail: String::new(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_api_state(
        &mut self,
        api_call_id: EventId,
        prompt_id: EventId,
        state: ApiState,
        detail: impl Into<String>,
    ) -> Result<EventId> {
        self.append_api_state_with_usage(api_call_id, prompt_id, state, None, detail)
    }

    pub fn append_api_state_with_usage(
        &mut self,
        api_call_id: EventId,
        prompt_id: EventId,
        state: ApiState,
        usage: Option<ApiUsage>,
        detail: impl Into<String>,
    ) -> Result<EventId> {
        if state == ApiState::Requesting {
            return Err("use append_api_requesting for the initial API state".into());
        }
        if state == ApiState::Retrying {
            return Err("use append_api_retrying for an API retry state".into());
        }
        if usage.is_some() && !state.is_terminal() {
            return Err("API usage is only valid on a terminal API state".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ApiStateUpdate(ApiStateUpdateEvent {
            id,
            timestamp_ms,
            api_call_id,
            prompt_id,
            state,
            retry_count: 0,
            retry_limit: 0,
            usage,
            detail: detail.into(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_context_usage_estimate(
        &mut self,
        api_state_event_id: EventId,
        values: ContextTokenUsage,
    ) -> Result<EventId> {
        let Some(Event::ApiStateUpdate(update)) = self.get(api_state_event_id) else {
            return Err(format!(
                "context usage estimate references non-API event {api_state_event_id}"
            )
            .into());
        };
        let Some(usage) = update.usage else {
            return Err(format!(
                "context usage estimate references API state {api_state_event_id} without usage"
            )
            .into());
        };
        if !matches!(update.state, ApiState::Completed | ApiState::Interrupted) {
            return Err(format!(
                "context usage estimate references non-committed API state {}",
                update.state
            )
            .into());
        }
        if values.sum() != usage.total_tokens {
            return Err(format!(
                "context usage estimate total {} does not match API usage {}",
                values.sum(),
                usage.total_tokens
            )
            .into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::ContextUsageEstimate(estimate)
                if estimate.api_state_event_id == api_state_event_id)
        }) {
            return Err(format!(
                "API state {api_state_event_id} already has a context usage estimate"
            )
            .into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::ContextUsageEstimate(ContextUsageEstimateEvent {
            id,
            timestamp_ms,
            api_state_event_id,
            values,
        }))?;
        Ok(id)
    }

    pub fn append_api_retrying(
        &mut self,
        api_call_id: EventId,
        prompt_id: EventId,
        retry_count: u8,
        retry_limit: u8,
        detail: impl Into<String>,
    ) -> Result<EventId> {
        if retry_count == 0 || retry_count > retry_limit {
            return Err("API retry count must be between 1 and its retry limit".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ApiStateUpdate(ApiStateUpdateEvent {
            id,
            timestamp_ms,
            api_call_id,
            prompt_id,
            state: ApiState::Retrying,
            retry_count,
            retry_limit,
            usage: None,
            detail: detail.into(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_user_turn_aborted(&mut self, prompt_id: EventId) -> Result<EventId> {
        if self
            .get(prompt_id)
            .is_none_or(|event| !event.is_root_prompt())
        {
            return Err(
                format!("UserTurnAbortedEvent references invalid prompt {prompt_id}").into(),
            );
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::UserTurnAborted(UserTurnAbortedEvent {
            id,
            timestamp_ms,
            prompt_id,
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_model_context_item(
        &mut self,
        api_call_id: EventId,
        prompt_id: EventId,
        provider: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<EventId> {
        let provider = provider.into();
        let content = content.into();
        if provider.is_empty() {
            return Err("ModelContextItemEvent provider is empty".into());
        }
        let value: serde_json::Value = serde_json::from_str(&content)?;
        if !value.is_object() {
            return Err("ModelContextItemEvent content must be a JSON object".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ModelContextItem(ModelContextItemEvent {
            id,
            timestamp_ms,
            api_call_id,
            prompt_id,
            provider,
            content,
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_tool_call(
        &mut self,
        api_call_id: EventId,
        prompt_id: EventId,
        provider_call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ToolCall(ToolCallEvent {
            id,
            timestamp_ms,
            api_call_id,
            prompt_id,
            provider_call_id: provider_call_id.into(),
            name: name.into(),
            arguments: arguments.into(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_tool_info(
        &mut self,
        tool_call_id: EventId,
        stream: ToolOutputStream,
        content: impl Into<String>,
    ) -> Result<EventId> {
        if stream == ToolOutputStream::Terminal {
            return Err("terminal tool info requires a structured TerminalLineUpdate".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ToolInfoUpdate(ToolInfoUpdateEvent {
            id,
            timestamp_ms,
            tool_call_id,
            stream,
            content: ToolInfoContent::Text(content.into()),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_terminal_update(
        &mut self,
        tool_call_id: EventId,
        update: TerminalLineUpdate,
    ) -> Result<EventId> {
        update
            .validate()
            .map_err(|error| format!("invalid terminal line update: {error}"))?;
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ToolInfoUpdate(ToolInfoUpdateEvent {
            id,
            timestamp_ms,
            tool_call_id,
            stream: ToolOutputStream::Terminal,
            content: ToolInfoContent::Terminal(update),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_tool_result(
        &mut self,
        tool_call_id: EventId,
        state: ToolResultState,
        exit_code: Option<i32>,
        detail: impl Into<String>,
    ) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::ToolCallResult(ToolCallResultEvent {
            id,
            timestamp_ms,
            tool_call_id,
            state,
            exit_code,
            detail: detail.into(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_system_static_prompt_change(
        &mut self,
        mode: SystemStaticPromptMode,
        content: Option<String>,
    ) -> Result<EventId> {
        validate_system_static_prompt_change(mode, content.as_deref())?;
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::SystemStaticPromptChange(
            SystemStaticPromptChangeEvent {
                id,
                timestamp_ms,
                mode,
                content,
            },
        ))?;
        Ok(id)
    }

    pub fn append_agent_title_changed(
        &mut self,
        tool_call_id: EventId,
        title: impl Into<String>,
    ) -> Result<EventId> {
        let Some(Event::ToolCall(call)) = self.get(tool_call_id) else {
            return Err(format!(
                "AgentTitleChangedEvent references invalid tool call {tool_call_id}"
            )
            .into());
        };
        if call.name != crate::agent_title::TOOL_NAME {
            return Err(format!(
                "AgentTitleChangedEvent references non-SetTitle tool {}",
                call.name
            )
            .into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::ToolCallResult(result) if result.tool_call_id == tool_call_id)
        }) {
            return Err(format!(
                "AgentTitleChangedEvent appears after result for tool call {tool_call_id}"
            )
            .into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::AgentTitleChanged(existing) if existing.tool_call_id == tool_call_id)
        }) {
            return Err(format!(
                "tool call {tool_call_id} already has an AgentTitleChangedEvent"
            )
            .into());
        }
        let title = crate::agent_title::normalize_title(&title.into())
            .map_err(|error| format!("invalid Agent title: {error}"))?;
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::AgentTitleChanged(AgentTitleChangedEvent {
            id,
            timestamp_ms,
            tool_call_id,
            title,
        }))?;
        Ok(id)
    }

    pub fn append_image_content(
        &mut self,
        tool_call_id: EventId,
        source: impl Into<String>,
        mime_type: impl Into<String>,
        format: impl Into<String>,
        width: u32,
        height: u32,
        data: Vec<u8>,
    ) -> Result<EventId> {
        let Some(Event::ToolCall(call)) = self.get(tool_call_id) else {
            return Err(
                format!("ImageContentEvent references invalid tool call {tool_call_id}").into(),
            );
        };
        if !crate::image_toolbox::stores_image_content(&call.name) {
            return Err(format!(
                "ImageContentEvent references non-image-producing tool {}",
                call.name
            )
            .into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::ToolCallResult(result) if result.tool_call_id == tool_call_id)
        }) {
            return Err(format!(
                "ImageContentEvent appears after result for tool call {tool_call_id}"
            )
            .into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::ImageContent(existing) if existing.tool_call_id == tool_call_id)
        }) {
            return Err(format!("tool call {tool_call_id} already has an ImageContentEvent").into());
        }
        let source = source.into();
        let mime_type = mime_type.into();
        let format = format.into();
        validate_image_content_fields(&source, &mime_type, &format, width, height, &data)?;
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let content_sha256 = image_content_sha256(&data);
        self.append(Event::ImageContent(ImageContentEvent {
            id,
            timestamp_ms,
            tool_call_id,
            source,
            mime_type,
            format,
            width,
            height,
            content_sha256,
            data: data.into(),
        }))?;
        Ok(id)
    }

    pub fn append_workmap_mutation(
        &mut self,
        tool_call_id: EventId,
        mut mutation: WorkMapMutation,
    ) -> Result<EventId> {
        let Some(Event::ToolCall(call)) = self.get(tool_call_id) else {
            return Err(format!(
                "WorkMapMutationEvent references invalid tool call {tool_call_id}"
            )
            .into());
        };
        if !crate::workmap::is_workmap_tool(&call.name) {
            return Err(format!(
                "WorkMapMutationEvent references non-WorkMap tool {}",
                call.name
            )
            .into());
        }
        if call.name != crate::workmap::operation_tool_name(mutation.operation) {
            return Err(format!(
                "WorkMap operation {:?} cannot be produced by {}",
                mutation.operation, call.name
            )
            .into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::ToolCallResult(result) if result.tool_call_id == tool_call_id)
        }) {
            return Err(format!(
                "WorkMapMutationEvent appears after result for tool call {tool_call_id}"
            )
                .into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::WorkMapMutation(existing) if existing.tool_call_id == tool_call_id)
        }) {
            return Err(format!(
                "tool call {tool_call_id} already has a WorkMapMutationEvent"
            )
            .into());
        }
        if mutation.records.is_empty() {
            return Err("WorkMapMutationEvent has no records".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        for record in &mut mutation.records {
            record.stamp(timestamp_ms);
        }
        let event = WorkMapMutationEvent {
            id,
            timestamp_ms,
            tool_call_id,
            mutation,
        };
        let mut projection = WorkMapProjection::from_events(&self.events)?;
        projection.apply(&event)?;
        self.append(Event::WorkMapMutation(event))?;
        Ok(id)
    }

    pub fn append_terminal_session_created(
        &mut self,
        tool_call_id: EventId,
        session_id: impl Into<String>,
        shell: impl Into<String>,
        cwd: impl Into<String>,
        width: u16,
        height: u16,
    ) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::TerminalSessionCreated(TerminalSessionCreatedEvent {
            id,
            timestamp_ms,
            tool_call_id,
            session_id: session_id.into(),
            shell: shell.into(),
            cwd: cwd.into(),
            width,
            height,
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_terminal_session_state(
        &mut self,
        session_id: impl Into<String>,
        state: TerminalSessionState,
        exit_code: Option<i32>,
        detail: impl Into<String>,
    ) -> Result<EventId> {
        if state == TerminalSessionState::Running {
            return Err("TerminalSessionCreatedEvent establishes the running state".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        let event = Event::TerminalSessionState(TerminalSessionStateEvent {
            id,
            timestamp_ms,
            session_id: session_id.into(),
            state,
            exit_code,
            detail: detail.into(),
        });
        self.append(event)?;
        Ok(id)
    }

    pub fn append_initial_model(&mut self, model: impl Into<String>) -> Result<EventId> {
        self.append_model_changed_with_cause(model, ModelChangeCause::Initial)
    }

    pub fn append_model_changed(&mut self, model: impl Into<String>) -> Result<EventId> {
        self.append_model_changed_with_cause(model, ModelChangeCause::User)
    }

    fn append_model_changed_with_cause(
        &mut self,
        model: impl Into<String>,
        cause: ModelChangeCause,
    ) -> Result<EventId> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err("ModelChangedEvent model is empty".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::ModelChanged(ModelChangedEvent {
            id,
            timestamp_ms,
            model,
            cause,
        }))?;
        Ok(id)
    }

    pub fn append_initial_reasoning_effort(
        &mut self,
        effort: impl Into<String>,
    ) -> Result<EventId> {
        self.append_reasoning_effort_changed_with_cause(effort, ReasoningEffortChangeCause::Initial)
    }

    pub fn append_reasoning_effort_changed(
        &mut self,
        effort: impl Into<String>,
    ) -> Result<EventId> {
        self.append_reasoning_effort_changed_with_cause(effort, ReasoningEffortChangeCause::User)
    }

    pub fn append_reasoning_effort_fallback(&mut self) -> Result<EventId> {
        self.append_reasoning_effort_changed_with_cause(
            crate::config::UNSET_EFFORT,
            ReasoningEffortChangeCause::ModelUnsupported,
        )
    }

    fn append_reasoning_effort_changed_with_cause(
        &mut self,
        effort: impl Into<String>,
        cause: ReasoningEffortChangeCause,
    ) -> Result<EventId> {
        let effort = effort.into();
        if effort.trim().is_empty() {
            return Err("ReasoningEffortChangedEvent effort is empty".into());
        }
        if cause == ReasoningEffortChangeCause::ModelUnsupported
            && effort != crate::config::UNSET_EFFORT
        {
            return Err("unsupported-model effort fallback must be unset".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::ReasoningEffortChanged(ReasoningEffortChangedEvent {
            id,
            timestamp_ms,
            effort,
            cause,
        }))?;
        Ok(id)
    }

    pub fn append_context_cleared(&mut self) -> Result<EventId> {
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::ContextCleared(ContextClearedEvent {
            id,
            timestamp_ms,
        }))?;
        Ok(id)
    }

    pub fn append_compact_started(
        &mut self,
        tool_call_id: EventId,
        prompt_id: EventId,
        kind: CompactKind,
    ) -> Result<EventId> {
        self.append_compact_started_with_stage_count(
            tool_call_id,
            prompt_id,
            kind,
            u8::try_from(kind.base_stage_count())?,
        )
    }

    pub fn append_compact_started_with_stage_count(
        &mut self,
        tool_call_id: EventId,
        prompt_id: EventId,
        kind: CompactKind,
        total_stages: u8,
    ) -> Result<EventId> {
        if !kind.accepts_stage_count(total_stages) {
            return Err(
                format!("Compact kind {kind} does not support {total_stages} stages").into(),
            );
        }
        let Some(Event::ToolCall(call)) = self.get(tool_call_id) else {
            return Err(format!("Compact references missing tool call {tool_call_id}").into());
        };
        if call.name != crate::compact::TOOL_NAME || call.prompt_id != prompt_id {
            return Err(format!("invalid Compact tool call {tool_call_id}").into());
        }
        if !self.events.iter().any(|event| {
            matches!(event, Event::ToolCallResult(result)
                if result.tool_call_id == tool_call_id
                    && result.state == ToolResultState::Succeeded)
        }) {
            return Err("Compact may start only after its tool result succeeded".into());
        }
        if self
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                Event::CompactStateUpdate(update) => Some(!update.state.is_terminal()),
                _ => None,
            })
            .unwrap_or(false)
        {
            return Err("another Compact lifecycle is already open".into());
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::CompactStateUpdate(CompactStateUpdateEvent {
            id,
            timestamp_ms,
            compact_id: id,
            tool_call_id,
            prompt_id,
            kind,
            total_stages,
            state: CompactState::Started,
            stage: None,
            content: String::new(),
            detail: String::new(),
        }))?;
        Ok(id)
    }

    pub fn append_compact_stage(
        &mut self,
        compact_id: EventId,
        stage: CompactStage,
        content: impl Into<String>,
    ) -> Result<EventId> {
        let content = content.into();
        if content.is_empty() {
            return Err("completed Compact stage content is empty".into());
        }
        let Some(Event::CompactStateUpdate(started)) = self.get(compact_id).cloned() else {
            return Err(format!("Compact {compact_id} did not start").into());
        };
        if started.state != CompactState::Started
            || started.compact_id != compact_id
            || !started.kind.is_multi_turn()
        {
            return Err(format!("event {compact_id} is not a multi-turn Compact start").into());
        }
        let mut completed = Vec::new();
        for event in &self.events {
            let Event::CompactStateUpdate(update) = event else {
                continue;
            };
            if update.compact_id != compact_id || update.id == compact_id {
                continue;
            }
            if update.state.is_terminal() {
                return Err(
                    format!("Compact {compact_id} already reached a terminal state").into(),
                );
            }
            if update.state == CompactState::StageCompleted {
                completed.push(update.stage.ok_or("Compact stage event has no stage")?);
            }
        }
        let expected = started
            .kind
            .stages(started.total_stages)
            .ok_or("multi-turn Compact has an invalid stage count")?
            .get(completed.len())
            .copied()
            .ok_or("all multi-turn Compact stages are already complete")?;
        if stage != expected {
            return Err(
                format!("Compact {compact_id} expected stage {expected}, found {stage}").into(),
            );
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::CompactStateUpdate(CompactStateUpdateEvent {
            id,
            timestamp_ms,
            compact_id,
            tool_call_id: started.tool_call_id,
            prompt_id: started.prompt_id,
            kind: started.kind,
            total_stages: started.total_stages,
            state: CompactState::StageCompleted,
            stage: Some(stage),
            content,
            detail: String::new(),
        }))?;
        Ok(id)
    }

    pub fn append_compact_terminal(
        &mut self,
        compact_id: EventId,
        state: CompactState,
        content: impl Into<String>,
        detail: impl Into<String>,
    ) -> Result<EventId> {
        if !state.is_terminal() {
            return Err("Compact terminal update cannot use started state".into());
        }
        let content = content.into();
        let detail = detail.into();
        let Some(Event::CompactStateUpdate(started)) = self.get(compact_id).cloned() else {
            return Err(format!("Compact {compact_id} did not start").into());
        };
        if started.state != CompactState::Started || started.compact_id != compact_id {
            return Err(format!("event {compact_id} is not a Compact start").into());
        }
        if self.events.iter().any(|event| {
            matches!(event, Event::CompactStateUpdate(update)
                if update.compact_id == compact_id && update.state.is_terminal())
        }) {
            return Err(format!("Compact {compact_id} already reached a terminal state").into());
        }
        match state {
            CompactState::Completed if content.trim().is_empty() => {
                return Err("completed Compact summary is empty".into());
            }
            CompactState::Completed if !detail.is_empty() => {
                return Err("completed Compact cannot contain failure detail".into());
            }
            CompactState::Failed | CompactState::Interrupted if detail.trim().is_empty() => {
                return Err("failed or interrupted Compact requires detail".into());
            }
            CompactState::Failed | CompactState::Interrupted if !content.is_empty() => {
                return Err("failed or interrupted Compact cannot contain summary content".into());
            }
            CompactState::StageCompleted => {
                return Err("Compact terminal update cannot use stage-completed state".into());
            }
            _ => {}
        }
        if state == CompactState::Completed && started.kind.is_multi_turn() {
            let stages = self
                .events
                .iter()
                .filter_map(|event| match event {
                    Event::CompactStateUpdate(update)
                        if update.compact_id == compact_id
                            && update.state == CompactState::StageCompleted =>
                    {
                        Some((update.stage, update.content.as_str()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let expected_stages = started
                .kind
                .stages(started.total_stages)
                .ok_or("multi-turn Compact has an invalid stage count")?;
            if stages.len() != expected_stages.len()
                || stages
                    .iter()
                    .zip(expected_stages.iter().copied())
                    .any(|((actual, _), expected)| *actual != Some(expected))
            {
                return Err("multi-turn Compact cannot complete before all stages".into());
            }
            let merged = crate::compact::merge_multi_turn_summary(
                stages.iter().skip(1).map(|(_, content)| *content),
            );
            if content != merged {
                return Err("multi-turn Compact final summary does not match its sections".into());
            }
        }
        let id = self.next_event_id;
        let timestamp_ms = self.next_timestamp_ms()?;
        self.append(Event::CompactStateUpdate(CompactStateUpdateEvent {
            id,
            timestamp_ms,
            compact_id,
            tool_call_id: started.tool_call_id,
            prompt_id: started.prompt_id,
            kind: started.kind,
            total_stages: started.total_stages,
            state,
            stage: None,
            content,
            detail,
        }))?;
        Ok(id)
    }

    pub fn rewind_to_event(&mut self, target_event_id: EventId) -> Result<EdbMutation> {
        let selected_order = self
            .order_of(target_event_id)
            .ok_or_else(|| format!("rewind target {target_event_id} does not exist"))?;
        let (target_order, restored_prompt_content) = match &self.events[selected_order] {
            Event::UserPrompt(prompt) => (selected_order, Some(prompt.content.clone())),
            Event::ContextCleared(_) | Event::SystemStaticPromptChange(_) => (selected_order, None),
            Event::CompactStateUpdate(update) if update.state == CompactState::Completed => {
                let Some(Event::ToolCall(call)) = self.get(update.tool_call_id) else {
                    return Err(format!(
                        "Compact rewind target {target_event_id} references missing tool call {}",
                        update.tool_call_id
                    )
                    .into());
                };
                let order = self.order_of(call.api_call_id).ok_or_else(|| {
                    format!("Compact rewind request {} is missing", call.api_call_id)
                })?;
                (order, None)
            }
            _ => {
                return Err(format!(
                    "rewind target {target_event_id} is not a user prompt, context clear, system static prompt change, or completed Compact"
                )
                .into());
            }
        };
        let mutation = EdbMutation::Rewind {
            target_event_id,
            restored_prompt_content,
        };
        self.replace_events(self.events[..target_order].to_vec(), mutation.clone())?;
        Ok(mutation)
    }

    pub fn delete_user_turn(&mut self, prompt_id: EventId) -> Result<EdbMutation> {
        let start = self
            .order_of(prompt_id)
            .filter(|order| matches!(self.events[*order], Event::UserPrompt(_)))
            .ok_or_else(|| format!("delete-turn target {prompt_id} is not a user prompt"))?;
        let next_prompt = self.events[start + 1..]
            .iter()
            .position(|event| matches!(event, Event::UserPrompt(_)))
            .map(|offset| start + 1 + offset);
        let search_end = next_prompt.unwrap_or(self.events.len());
        let completed = self.events[start + 1..search_end]
            .iter()
            .position(|event| {
                matches!(event, Event::AgentTurn(turn)
                    if turn.prompt_id == prompt_id && turn.state == AgentTurnState::Completed)
            })
            .map(|offset| start + 1 + offset);
        let end = completed.map_or(search_end, |order| order + 1);
        let mut events = Vec::with_capacity(self.events.len() - (end - start));
        events.extend_from_slice(&self.events[..start]);
        events.extend_from_slice(&self.events[end..]);
        let mutation = EdbMutation::DeleteTurn { prompt_id };
        self.replace_events(events, mutation.clone())?;
        Ok(mutation)
    }

    pub fn regenerate_from_final_answer(
        &mut self,
        final_answer_event_id: EventId,
    ) -> Result<(String, EdbMutation)> {
        let turn = self.completed_turn_event(final_answer_event_id)?;
        let prompt_id = turn.prompt_id;
        let content = match self.get(prompt_id) {
            Some(Event::UserPrompt(prompt)) => prompt.content.clone(),
            _ => {
                return Err(format!(
                    "completed Agent turn {final_answer_event_id} references missing user prompt {prompt_id}"
                )
                .into());
            }
        };
        let prompt_order = self
            .order_of(prompt_id)
            .ok_or_else(|| format!("user prompt {prompt_id} does not exist"))?;
        let mutation = EdbMutation::Regenerate {
            final_answer_event_id,
            prompt_id,
        };
        self.replace_events(self.events[..prompt_order].to_vec(), mutation.clone())?;
        Ok((content, mutation))
    }

    pub fn clone_through_final_answer(
        &self,
        final_answer_event_id: EventId,
        path: &Path,
        title: &str,
    ) -> Result<Self> {
        let _ = self.completed_turn_event(final_answer_event_id)?;
        let end = self
            .order_of(final_answer_event_id)
            .ok_or_else(|| format!("final answer event {final_answer_event_id} does not exist"))?;
        let mut events = self.events[..=end].to_vec();
        events[0] = new_edb_id_event(current_timestamp_ms()?)?;
        let Some(Event::AgentKindDef(definition)) = events.get_mut(1) else {
            return Err("cloned Agent EDB has no AgentKindDefEvent after its EDB ID".into());
        };
        if definition.kind == AgentKind::SubAgent {
            return Err("read-only sub-Agent history cannot be cloned from a UI operation".into());
        }
        definition.kind = AgentKind::Interactive;
        definition.parent_agent_id = None;
        definition.system_prompt = None;

        let title = crate::agent_title::normalize_title(title)
            .map_err(|error| format!("invalid cloned Agent title: {error}"))?;
        let previous = events.last().ok_or("cannot clone an empty EDB")?;
        let id = previous.id().checked_add(1).ok_or("EventId exhausted")?;
        let timestamp_ms = previous.timestamp_ms();
        events.push(Event::AgentTitleChanged(AgentTitleChangedEvent {
            id,
            timestamp_ms,
            tool_call_id: HOST_AGENT_TITLE_CHANGE,
            title: title.clone(),
        }));
        let id = id.checked_add(1).ok_or("EventId exhausted")?;
        events.push(Event::CloneCompleted(CloneCompletedEvent {
            id,
            timestamp_ms,
            title,
        }));
        Self::create_from_events(path, events)
    }

    fn completed_turn_event(&self, event_id: EventId) -> Result<&AgentTurnEvent> {
        match self.get(event_id) {
            Some(Event::AgentTurn(turn)) if turn.state == AgentTurnState::Completed => Ok(turn),
            Some(Event::AgentTurn(turn)) => Err(format!(
                "Agent turn event {event_id} is {}, not a completed final answer",
                turn.state
            )
            .into()),
            Some(event) => Err(format!(
                "event {event_id} is {}, not a completed final answer",
                event.kind()
            )
            .into()),
            None => Err(format!("final answer event {event_id} does not exist").into()),
        }
    }

    pub fn get(&self, id: EventId) -> Option<&Event> {
        self.events
            .binary_search_by_key(&id, Event::id)
            .ok()
            .map(|order| &self.events[order])
    }

    pub fn event_at_order(&self, order: EventOrder) -> Option<&Event> {
        self.events.get(order)
    }

    pub fn order_of(&self, id: EventId) -> Option<EventOrder> {
        self.events.binary_search_by_key(&id, Event::id).ok()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn edb_id(&self) -> Result<&str> {
        edb_id(self.events())
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.len() == 1 && matches!(self.events.first(), Some(Event::EdbIdGeneration(_)))
    }

    pub fn persisted_size_bytes(&self) -> u64 {
        self.persisted_size_bytes
    }

    pub fn next_event_id(&self) -> EventId {
        self.next_event_id
    }

    pub fn mutation_revision(&self) -> u64 {
        self.mutation_revision
    }

    pub fn last_mutation(&self) -> Option<&EdbMutation> {
        self.last_mutation.as_ref()
    }

    pub fn has_assist_response(&self, prompt_id: EventId) -> bool {
        self.events.iter().any(|event| {
            matches!(
                event,
                Event::AssistResponse(response) if response.prompt_id == prompt_id
            )
        })
    }

    fn next_timestamp_ms(&self) -> Result<u64> {
        let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        Ok(self
            .events
            .last()
            .map(Event::timestamp_ms)
            .unwrap_or(now)
            .max(now))
    }

    fn append(&mut self, event: Event) -> Result<()> {
        if event.id() != self.next_event_id {
            return Err(format!(
                "event ID {} does not match next EventId {}",
                event.id(),
                self.next_event_id
            )
            .into());
        }
        let next_event_id = self
            .next_event_id
            .checked_add(1)
            .ok_or("EventId exhausted")?;
        if self.file.is_none()
            && let Some(path) = &self.path
        {
            self.file = Some(OpenOptions::new().read(true).append(true).open(path)?);
        }
        if let Some(file) = &mut self.file {
            self.persisted_size_bytes = self
                .persisted_size_bytes
                .checked_add(persist_event(file, &event)?)
                .ok_or("EDB file size overflow")?;
        }
        self.events.push(event);
        self.next_event_id = next_event_id;
        Ok(())
    }

    fn replace_events(&mut self, events: Vec<Event>, mutation: EdbMutation) -> Result<()> {
        validate_event_ids(&events)?;
        validate_edb_identity(&events)?;

        let persisted_size_bytes = if let Some(path) = &self.path {
            persist_replacement(path, &events, self.next_event_id, &mut self.file)?
        } else {
            0
        };
        self.events = events;
        self.persisted_size_bytes = persisted_size_bytes;
        self.mutation_revision = self
            .mutation_revision
            .checked_add(1)
            .ok_or("EDB mutation revision exhausted")?;
        self.last_mutation = Some(mutation);
        Ok(())
    }

    fn create_from_events(path: &Path, events: Vec<Event>) -> Result<Self> {
        if path.exists() {
            return Err(format!("EDB {} already exists", path.display()).into());
        }
        validate_event_ids(&events)?;
        validate_edb_identity(&events)?;
        let next_event_id = events
            .last()
            .map(Event::id)
            .map_or(Ok(0), |id| id.checked_add(1).ok_or("EventId exhausted"))?;
        let mut edb = Self::open(path)?;
        match persist_replacement(path, &events, next_event_id, &mut edb.file) {
            Ok(size) => {
                edb.events = events;
                edb.persisted_size_bytes = size;
                edb.next_event_id = next_event_id;
                Ok(edb)
            }
            Err(error) => {
                edb.close_storage();
                let _ = fs::remove_file(path);
                Err(error)
            }
        }
    }
}

pub fn effective_conversation_events(events: &[Event]) -> Result<Vec<&Event>> {
    effective_branch_events(events, false, true)
}

pub(crate) fn effective_history_events(
    events: &[Event],
    end_exclusive: EventId,
) -> Result<Vec<&Event>> {
    let end = events
        .iter()
        .position(|event| event.id() == end_exclusive)
        .ok_or_else(|| format!("History boundary event {end_exclusive} does not exist"))?;
    effective_branch_events(&events[..end], false, false)
}

pub fn latest_system_static_prompt_change(
    events: &[Event],
) -> Option<&SystemStaticPromptChangeEvent> {
    events.iter().rev().find_map(|event| match event {
        Event::SystemStaticPromptChange(change) => Some(change),
        _ => None,
    })
}

pub fn latest_context_usage(events: &[Event]) -> Option<ApiUsage> {
    latest_context_usage_event(events).and_then(|event| event.usage)
}

pub fn latest_context_usage_event(events: &[Event]) -> Option<&ApiStateUpdateEvent> {
    let effective = effective_conversation_events(events).ok()?;
    let model_event_id = events.iter().rev().find_map(|event| match event {
        Event::ModelChanged(changed) => Some(changed.id),
        _ => None,
    })?;
    let mut errored = BTreeSet::new();
    let mut boundary = None;
    for event in effective {
        let Event::ApiStateUpdate(update) = event else {
            continue;
        };
        if update.id <= model_event_id {
            continue;
        }
        match update.state {
            ApiState::Completed => boundary = update.usage.map(|_| update),
            ApiState::Error => {
                errored.insert(update.api_call_id);
            }
            ApiState::Interrupted if !errored.contains(&update.api_call_id) => {
                boundary = update.usage.map(|_| update);
            }
            ApiState::Requesting
            | ApiState::Streaming
            | ApiState::Retrying
            | ApiState::Interrupted => {}
        }
    }
    boundary
}

pub fn completed_compact_count(events: &[Event]) -> u64 {
    u64::try_from(
        events
            .iter()
            .filter(|event| {
                matches!(event, Event::CompactStateUpdate(update) if update.state == CompactState::Completed)
            })
            .count(),
    )
    .unwrap_or(u64::MAX)
}

pub fn effective_ui_events(events: &[Event]) -> Result<Vec<&Event>> {
    effective_branch_events(events, true, false)
}

pub fn edb_id(events: &[Event]) -> Result<&str> {
    validate_edb_identity(events)?;
    match &events[0] {
        Event::EdbIdGeneration(identity) => Ok(&identity.edb_id),
        _ => unreachable!("validated EDB identity must be first"),
    }
}

pub fn agent_kind_definition(events: &[Event]) -> Result<&AgentKindDefEvent> {
    validate_edb_identity(events)?;
    match events.get(1) {
        Some(Event::AgentKindDef(definition)) => {
            if definition.orchestrator.is_empty() {
                return Err("Agent definition requires an orchestrator".into());
            }
            match definition.kind {
                AgentKind::SubAgent => {
                    if definition
                        .parent_agent_id
                        .as_deref()
                        .is_none_or(str::is_empty)
                    {
                        return Err("sub-Agent definition requires parent_agent_id".into());
                    }
                }
                AgentKind::Primary | AgentKind::Interactive => {
                    if definition.parent_agent_id.is_some() || definition.system_prompt.is_some() {
                        return Err(
                            "only a sub-Agent definition may contain parent or system prompt data"
                                .into(),
                        );
                    }
                }
            }
            Ok(definition)
        }
        Some(event) => Err(format!(
            "EDB ID must be immediately followed by AgentKindDefEvent, found {}",
            event.kind()
        )
        .into()),
        None => Err("EDB has no AgentKindDefEvent after its EDB ID".into()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTurnProjection {
    pub turn_id: EventId,
    pub prompt_id: EventId,
    pub state: AgentTurnState,
    pub started_event_id: EventId,
    pub terminal_event_id: Option<EventId>,
    pub detail: String,
}

pub fn latest_agent_turn(events: &[Event]) -> Result<Option<AgentTurnProjection>> {
    let mut turns: BTreeMap<EventId, AgentTurnProjection> = BTreeMap::new();
    for event in events {
        let Event::AgentTurn(update) = event else {
            continue;
        };
        match update.state {
            AgentTurnState::Started => {
                if turns
                    .values()
                    .next_back()
                    .is_some_and(|turn| !turn.state.is_terminal())
                {
                    return Err("a new Agent turn started before the previous turn ended".into());
                }
                if update.turn_id != update.prompt_id {
                    return Err(format!(
                        "Agent turn {} must use its root prompt ID as turn_id",
                        update.turn_id
                    )
                    .into());
                }
                if events
                    .iter()
                    .find(|event| event.id() == update.prompt_id)
                    .is_none_or(|event| !event.is_root_prompt())
                {
                    return Err(format!(
                        "Agent turn {} references missing prompt {}",
                        update.turn_id, update.prompt_id
                    )
                    .into());
                }
                if turns
                    .insert(
                        update.turn_id,
                        AgentTurnProjection {
                            turn_id: update.turn_id,
                            prompt_id: update.prompt_id,
                            state: update.state,
                            started_event_id: update.id,
                            terminal_event_id: None,
                            detail: update.detail.clone(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!("duplicate Agent turn {}", update.turn_id).into());
                }
            }
            terminal => {
                let turn = turns.get_mut(&update.turn_id).ok_or_else(|| {
                    format!("Agent turn {} ended before it started", update.turn_id)
                })?;
                if turn.prompt_id != update.prompt_id || turn.state.is_terminal() {
                    return Err(format!(
                        "invalid terminal update for Agent turn {}",
                        update.turn_id
                    )
                    .into());
                }
                turn.state = terminal;
                turn.terminal_event_id = Some(update.id);
                turn.detail = update.detail.clone();
            }
        }
    }
    Ok(turns.into_values().next_back())
}

fn effective_branch_events(
    events: &[Event],
    include_state_events: bool,
    apply_compact_boundary: bool,
) -> Result<Vec<&Event>> {
    let mut active: Vec<&Event> = Vec::new();
    for event in events {
        match event {
            Event::EdbIdGeneration(_) | Event::AgentKindDef(_) | Event::SystemPrompt(_) => {}
            Event::ModelChanged(_) | Event::ReasoningEffortChanged(_) if !include_state_events => {}
            Event::ContextCleared(_) => {
                active.clear();
                if include_state_events {
                    active.push(event);
                }
            }
            Event::CompactStateUpdate(update)
                if apply_compact_boundary && update.state == CompactState::Completed =>
            {
                active.clear();
                active.push(event);
            }
            _ => active.push(event),
        }
    }
    Ok(without_errored_api_content(active))
}

fn without_errored_api_content(events: Vec<&Event>) -> Vec<&Event> {
    let mut active_calls = BTreeMap::new();
    let mut assist_calls = BTreeMap::new();
    let mut errored_calls = BTreeSet::new();

    for event in &events {
        match event {
            Event::ApiStateUpdate(update) => match update.state {
                ApiState::Requesting => {
                    active_calls.insert(update.prompt_id, update.api_call_id);
                }
                ApiState::Completed | ApiState::Interrupted => {
                    if active_calls.get(&update.prompt_id) == Some(&update.api_call_id) {
                        active_calls.remove(&update.prompt_id);
                    }
                }
                ApiState::Error => {
                    errored_calls.insert(update.api_call_id);
                    if active_calls.get(&update.prompt_id) == Some(&update.api_call_id) {
                        active_calls.remove(&update.prompt_id);
                    }
                }
                ApiState::Streaming | ApiState::Retrying => {}
            },
            Event::AssistResponse(response) => {
                if let Some(api_call_id) = active_calls.get(&response.prompt_id) {
                    assist_calls.insert(response.id, *api_call_id);
                }
            }
            _ => {}
        }
    }

    events
        .into_iter()
        .filter(|event| match event {
            Event::AssistResponse(response) => assist_calls
                .get(&response.id)
                .is_none_or(|api_call_id| !errored_calls.contains(api_call_id)),
            Event::ModelContextItem(item) => !errored_calls.contains(&item.api_call_id),
            _ => true,
        })
        .collect()
}

fn current_timestamp_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn generate_edb_id() -> Result<String> {
    let mut bytes = [0_u8; EDB_ID_BYTES];
    getrandom::fill(&mut bytes)?;
    let mut edb_id = String::with_capacity(EDB_ID_HEX_LENGTH);
    for byte in bytes {
        write!(&mut edb_id, "{byte:02x}").expect("writing hexadecimal into String cannot fail");
    }
    Ok(edb_id)
}

fn new_edb_id_event(timestamp_ms: u64) -> Result<Event> {
    Ok(Event::EdbIdGeneration(EdbIdGenerationEvent {
        id: 0,
        timestamp_ms,
        edb_id: generate_edb_id()?,
    }))
}

fn initialize_file(path: &Path) -> Result<()> {
    let identity = new_edb_id_event(current_timestamp_ms()?)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(&encode_file_header(1))?;
    file.write_all(&encode_record(&identity)?)?;
    file.sync_data()?;
    Ok(())
}

fn persist_event(file: &mut File, event: &Event) -> Result<u64> {
    let record = encode_record(event)?;
    file.write_all(&record)?;
    file.sync_data()?;
    Ok(u64::try_from(record.len())?)
}

fn encode_record(event: &Event) -> Result<Vec<u8>> {
    encode_raw_record(&encode_event(event))
}

fn encode_legacy_agent_definition_record(event: &Event) -> Result<Vec<u8>> {
    let raw = match event {
        Event::AgentKindDef(event) => {
            let mut raw = Vec::new();
            encode_varint(event.id, &mut raw);
            raw.push(16);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.push(event.kind.code());
            raw.push(u8::from(event.parent_agent_id.is_some()));
            if let Some(parent) = &event.parent_agent_id {
                encode_sized_string(parent, &mut raw);
            }
            raw.push(u8::from(event.system_prompt.is_some()));
            if let Some(prompt) = &event.system_prompt {
                raw.extend_from_slice(prompt.as_bytes());
            }
            raw
        }
        _ => encode_event_v35(event),
    };
    encode_raw_record(&raw)
}

fn encode_v35_record(event: &Event) -> Result<Vec<u8>> {
    encode_raw_record(&encode_event_v35(event))
}

fn encode_v36_record(event: &Event) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    encode_varint(event.id(), &mut raw);
    if let Event::CompactStateUpdate(event) = event {
        raw.push(19);
        encode_varint(event.timestamp_ms, &mut raw);
        encode_varint(event.compact_id, &mut raw);
        encode_varint(event.tool_call_id, &mut raw);
        encode_varint(event.prompt_id, &mut raw);
        raw.push(event.state.code());
        raw.push(if event.kind == CompactKind::WorkerSingleTurn {
            0
        } else {
            1
        });
        raw.push(event.stage.map_or(u8::MAX, CompactStage::code));
        encode_sized_string(&event.content, &mut raw);
        raw.extend_from_slice(event.detail.as_bytes());
    } else {
        raw.extend(encode_event_body(event));
    }
    encode_raw_record(&raw)
}

fn encode_v37_record(event: &Event) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    encode_varint(event.id(), &mut raw);
    if let Event::CompactStateUpdate(event) = event {
        raw.push(19);
        encode_varint(event.timestamp_ms, &mut raw);
        encode_varint(event.compact_id, &mut raw);
        encode_varint(event.tool_call_id, &mut raw);
        encode_varint(event.prompt_id, &mut raw);
        raw.push(event.state.code());
        raw.push(event.kind.code());
        raw.push(event.stage.map_or(u8::MAX, CompactStage::code));
        encode_sized_string(&event.content, &mut raw);
        raw.extend_from_slice(event.detail.as_bytes());
    } else {
        raw.extend(encode_event_body(event));
    }
    encode_raw_record(&raw)
}

fn encode_event_v35(event: &Event) -> Vec<u8> {
    let mut raw = Vec::new();
    encode_varint(event.id(), &mut raw);
    if let Event::CompactStateUpdate(event) = event {
        raw.push(19);
        encode_varint(event.timestamp_ms, &mut raw);
        encode_varint(event.compact_id, &mut raw);
        encode_varint(event.tool_call_id, &mut raw);
        encode_varint(event.prompt_id, &mut raw);
        raw.push(event.state.code());
        encode_sized_string(&event.content, &mut raw);
        raw.extend_from_slice(event.detail.as_bytes());
    } else {
        raw.extend(encode_event_body(event));
    }
    raw
}

fn encode_raw_record(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() > MAX_RECORD_SIZE {
        return Err(format!("event record exceeds {MAX_RECORD_SIZE} bytes").into());
    }
    let compressed = zstd::bulk::compress(raw, ZSTD_LEVEL)?;
    let (codec, payload) = if compressed.len() < raw.len() {
        (CODEC_ZSTD, compressed.as_slice())
    } else {
        (CODEC_RAW, raw)
    };
    let payload_len = u32::try_from(payload.len())?;
    let raw_len = u32::try_from(raw.len())?;
    let checksum = crc32fast::hash(&raw);

    let mut record = Vec::with_capacity(RECORD_HEADER_SIZE + payload.len());
    record.push(codec);
    record.extend_from_slice(&payload_len.to_le_bytes());
    record.extend_from_slice(&raw_len.to_le_bytes());
    record.extend_from_slice(&checksum.to_le_bytes());
    record.extend_from_slice(payload);
    Ok(record)
}

fn persist_replacement(
    path: &Path,
    events: &[Event],
    next_event_id: EventId,
    append_file: &mut Option<File>,
) -> Result<u64> {
    let parent = path.parent().ok_or("EDB path has no parent")?;
    let file_name = path
        .file_name()
        .ok_or("EDB path has no file name")?
        .to_string_lossy();
    let temp_path = parent.join(format!(
        ".{file_name}.rewrite-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));

    let write_result = (|| -> Result<u64> {
        let mut replacement = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        #[cfg(unix)]
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))?;
        replacement.write_all(&encode_file_header(next_event_id))?;
        let mut size = u64::try_from(FILE_HEADER_SIZE)?;
        for event in events {
            let record = encode_record(event)?;
            replacement.write_all(&record)?;
            size = size
                .checked_add(u64::try_from(record.len())?)
                .ok_or("EDB file size overflow")?;
        }
        replacement.sync_all()?;
        drop(replacement);

        let previous_file = append_file.take();
        drop(previous_file);
        if let Err(error) = replace_file(&temp_path, path) {
            *append_file = OpenOptions::new().read(true).append(true).open(path).ok();
            return Err(error);
        }
        *append_file = OpenOptions::new().read(true).append(true).open(path).ok();
        let _ = sync_parent_directory(parent);
        Ok(size)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
}

fn validate_edb_id(edb_id: &str) -> Result<()> {
    if edb_id.len() != EDB_ID_HEX_LENGTH
        || !edb_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(format!(
            "EDB ID must contain exactly {EDB_ID_HEX_LENGTH} lowercase hexadecimal characters"
        )
        .into());
    }
    Ok(())
}

fn validate_edb_identity(events: &[Event]) -> Result<()> {
    let Some(Event::EdbIdGeneration(identity)) = events.first() else {
        return Err("EDB must begin with EdbIdGenerationEvent".into());
    };
    if identity.id != 0 {
        return Err("EdbIdGenerationEvent must use EventId 0".into());
    }
    validate_edb_id(&identity.edb_id)?;
    if events
        .iter()
        .skip(1)
        .any(|event| matches!(event, Event::EdbIdGeneration(_)))
    {
        return Err("EDB must contain exactly one EdbIdGenerationEvent".into());
    }
    Ok(())
}

fn validate_event_ids(events: &[Event]) -> Result<()> {
    if events.windows(2).any(|pair| pair[0].id() >= pair[1].id()) {
        return Err("EDB EventIds must be unique and strictly increase by EventOrder".into());
    }
    Ok(())
}

fn encode_file_header(next_event_id: EventId) -> [u8; FILE_HEADER_SIZE] {
    let mut header = [0_u8; FILE_HEADER_SIZE];
    header[..FILE_MAGIC.len()].copy_from_slice(FILE_MAGIC);
    header[FILE_MAGIC.len()..].copy_from_slice(&next_event_id.to_le_bytes());
    header
}

fn encode_event(event: &Event) -> Vec<u8> {
    let mut raw = Vec::new();
    encode_varint(event.id(), &mut raw);
    raw.extend(encode_event_body(event));
    raw
}

fn encode_event_body(event: &Event) -> Vec<u8> {
    match event {
        Event::EdbIdGeneration(event) => {
            let mut raw = Vec::with_capacity(10 + event.edb_id.len());
            raw.push(28);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.extend_from_slice(event.edb_id.as_bytes());
            raw
        }
        Event::AgentKindDef(event) => {
            let mut raw = Vec::new();
            raw.push(16);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.push(event.kind.code());
            raw.push(u8::from(event.parent_agent_id.is_some()));
            if let Some(parent) = &event.parent_agent_id {
                encode_sized_string(parent, &mut raw);
            }
            encode_sized_string(&event.orchestrator, &mut raw);
            raw.push(u8::from(event.system_prompt.is_some()));
            if let Some(prompt) = &event.system_prompt {
                raw.extend_from_slice(prompt.as_bytes());
            }
            raw
        }
        Event::AgentTurn(event) => {
            let mut raw = Vec::with_capacity(24 + event.detail.len());
            raw.push(17);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.turn_id, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            raw.push(event.state.code());
            raw.extend_from_slice(event.detail.as_bytes());
            raw
        }
        Event::SystemPrompt(event) => {
            let mut raw = Vec::with_capacity(1 + event.name.len());
            raw.push(3);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.extend_from_slice(event.name.as_bytes());
            raw
        }
        Event::UserPrompt(event) => {
            let mut raw = Vec::with_capacity(1 + event.content.len());
            raw.push(0);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.extend_from_slice(event.content.as_bytes());
            raw
        }
        Event::ManagerPrompt(event) => {
            let mut raw = Vec::with_capacity(1 + event.content.len());
            raw.push(23);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.extend_from_slice(event.content.as_bytes());
            raw
        }
        Event::ParentAgentPrompt(event) => {
            let mut raw = Vec::with_capacity(1 + event.content.len());
            raw.push(24);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.extend_from_slice(event.content.as_bytes());
            raw
        }
        Event::FollowUpPrompt(event) => {
            let mut raw = Vec::with_capacity(12 + event.content.len());
            raw.push(9);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            raw.extend_from_slice(event.content.as_bytes());
            raw
        }
        Event::AssistResponse(event) => {
            let mut raw = Vec::with_capacity(12 + event.content.len());
            raw.push(1);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            raw.push(u8::from(event.finished));
            raw.extend_from_slice(event.content.as_bytes());
            raw
        }
        Event::ApiStateUpdate(event) => {
            let mut raw = Vec::with_capacity(25 + event.detail.len());
            raw.push(2);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.api_call_id, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            raw.push(event.state.code());
            raw.push(event.retry_count);
            raw.push(event.retry_limit);
            raw.push(u8::from(event.usage.is_some()));
            if let Some(usage) = event.usage {
                encode_varint(usage.input_tokens, &mut raw);
                encode_varint(usage.output_tokens, &mut raw);
                encode_varint(usage.total_tokens, &mut raw);
            }
            raw.extend_from_slice(event.detail.as_bytes());
            raw
        }
        Event::ContextUsageEstimate(event) => {
            let mut raw = Vec::with_capacity(64);
            raw.push(26);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.api_state_event_id, &mut raw);
            for value in [
                event.values.system,
                event.values.compact,
                event.values.memory,
                event.values.user,
                event.values.model,
                event.values.tool,
            ] {
                encode_varint(value, &mut raw);
            }
            raw
        }
        Event::UserTurnAborted(event) => {
            let mut raw = Vec::with_capacity(20);
            raw.push(15);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            raw
        }
        Event::ToolCall(event) => {
            let mut raw = Vec::with_capacity(
                24 + event.provider_call_id.len() + event.name.len() + event.arguments.len(),
            );
            raw.push(4);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.api_call_id, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            encode_sized_string(&event.provider_call_id, &mut raw);
            encode_sized_string(&event.name, &mut raw);
            raw.extend_from_slice(event.arguments.as_bytes());
            raw
        }
        Event::ToolInfoUpdate(event) => {
            let content = serde_json::to_vec(&event.content)
                .expect("ToolInfoContent serialization cannot fail");
            let mut raw = Vec::with_capacity(12 + content.len());
            raw.push(5);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.tool_call_id, &mut raw);
            raw.push(event.stream.code());
            raw.extend_from_slice(&content);
            raw
        }
        Event::ToolCallResult(event) => {
            let mut raw = Vec::with_capacity(17 + event.detail.len());
            raw.push(6);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.tool_call_id, &mut raw);
            raw.push(event.state.code());
            raw.push(u8::from(event.exit_code.is_some()));
            if let Some(exit_code) = event.exit_code {
                raw.extend_from_slice(&exit_code.to_le_bytes());
            }
            raw.extend_from_slice(event.detail.as_bytes());
            raw
        }
        Event::WorkMapMutation(event) => {
            let mutation = serde_json::to_vec(&event.mutation)
                .expect("WorkMapMutation serialization cannot fail");
            let mut raw = Vec::with_capacity(16 + mutation.len());
            raw.push(18);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.tool_call_id, &mut raw);
            raw.extend_from_slice(&mutation);
            raw
        }
        Event::WorkMapPendingReminder(event) => {
            let mut raw = Vec::with_capacity(20);
            raw.push(20);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            raw
        }
        Event::TerminalSessionCreated(event) => {
            let mut raw = Vec::with_capacity(
                20 + event.session_id.len() + event.shell.len() + event.cwd.len(),
            );
            raw.push(7);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.tool_call_id, &mut raw);
            encode_sized_string(&event.session_id, &mut raw);
            encode_sized_string(&event.shell, &mut raw);
            encode_sized_string(&event.cwd, &mut raw);
            raw.extend_from_slice(&event.width.to_le_bytes());
            raw.extend_from_slice(&event.height.to_le_bytes());
            raw
        }
        Event::TerminalSessionState(event) => {
            let mut raw = Vec::with_capacity(12 + event.session_id.len() + event.detail.len());
            raw.push(8);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_sized_string(&event.session_id, &mut raw);
            raw.push(event.state.code());
            raw.push(u8::from(event.exit_code.is_some()));
            if let Some(exit_code) = event.exit_code {
                raw.extend_from_slice(&exit_code.to_le_bytes());
            }
            raw.extend_from_slice(event.detail.as_bytes());
            raw
        }
        Event::ModelContextItem(event) => {
            let mut raw = Vec::with_capacity(24 + event.provider.len() + event.content.len());
            raw.push(10);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.api_call_id, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            encode_sized_string(&event.provider, &mut raw);
            raw.extend_from_slice(event.content.as_bytes());
            raw
        }
        Event::ModelChanged(event) => {
            let mut raw = Vec::with_capacity(12 + event.model.len());
            raw.push(14);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.push(event.cause.code());
            raw.extend_from_slice(event.model.as_bytes());
            raw
        }
        Event::ReasoningEffortChanged(event) => {
            let mut raw = Vec::with_capacity(13 + event.effort.len());
            raw.push(11);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.push(event.cause.code());
            raw.extend_from_slice(event.effort.as_bytes());
            raw
        }
        Event::ContextCleared(event) => {
            let mut raw = Vec::with_capacity(11);
            raw.push(12);
            encode_varint(event.timestamp_ms, &mut raw);
            raw
        }
        Event::CompactStateUpdate(event) => {
            let mut raw = Vec::with_capacity(40 + event.content.len() + event.detail.len());
            raw.push(19);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.compact_id, &mut raw);
            encode_varint(event.tool_call_id, &mut raw);
            encode_varint(event.prompt_id, &mut raw);
            raw.push(event.state.code());
            raw.push(event.kind.code());
            raw.push(event.stage.map_or(u8::MAX, CompactStage::code));
            raw.push(event.total_stages);
            encode_sized_string(&event.content, &mut raw);
            raw.extend_from_slice(event.detail.as_bytes());
            raw
        }
        Event::SystemStaticPromptChange(event) => {
            let mut raw = Vec::with_capacity(12 + event.content.as_deref().map_or(0, str::len));
            raw.push(27);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.push(event.mode.code());
            if let Some(content) = &event.content {
                raw.extend_from_slice(content.as_bytes());
            }
            raw
        }
        Event::AgentTitleChanged(event) => {
            let mut raw = Vec::with_capacity(16 + event.title.len());
            raw.push(21);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.tool_call_id, &mut raw);
            raw.extend_from_slice(event.title.as_bytes());
            raw
        }
        Event::CloneCompleted(event) => {
            let mut raw = Vec::with_capacity(12 + event.title.len());
            raw.push(22);
            encode_varint(event.timestamp_ms, &mut raw);
            raw.extend_from_slice(event.title.as_bytes());
            raw
        }
        Event::ImageContent(event) => {
            let mut raw = Vec::with_capacity(
                64 + event.source.len()
                    + event.mime_type.len()
                    + event.format.len()
                    + event.content_sha256.len()
                    + event.data.len(),
            );
            raw.push(25);
            encode_varint(event.timestamp_ms, &mut raw);
            encode_varint(event.tool_call_id, &mut raw);
            encode_varint(u64::from(event.width), &mut raw);
            encode_varint(u64::from(event.height), &mut raw);
            encode_sized_string(&event.source, &mut raw);
            encode_sized_string(&event.mime_type, &mut raw);
            encode_sized_string(&event.format, &mut raw);
            encode_sized_string(&event.content_sha256, &mut raw);
            raw.extend_from_slice(&event.data);
            raw
        }
    }
}

fn stable_hash(
    id: EventId,
    timestamp_ms: u64,
    kind: u8,
    related_ids: &[EventId],
    metadata: &[u8],
    contents: &[&str],
) -> String {
    let content_size: usize = contents.iter().map(|content| content.len()).sum();
    let mut canonical = Vec::with_capacity(16 + content_size);
    canonical.extend_from_slice(b"me:event:v19\0");
    encode_varint(id, &mut canonical);
    encode_varint(timestamp_ms, &mut canonical);
    canonical.push(kind);
    for related_id in related_ids {
        encode_varint(*related_id, &mut canonical);
    }
    canonical.extend_from_slice(metadata);
    for content in contents {
        encode_varint(content.len() as u64, &mut canonical);
        canonical.extend_from_slice(content.as_bytes());
    }
    blake3::hash(&canonical).to_hex().to_string()
}

pub fn image_content_sha256(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn validate_image_content_fields(
    source: &str,
    mime_type: &str,
    format: &str,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<()> {
    if source.trim().is_empty() {
        return Err("image source is empty".into());
    }
    if !mime_type.starts_with("image/") {
        return Err(format!("invalid image MIME type {mime_type:?}").into());
    }
    if format.trim().is_empty() {
        return Err("image format is empty".into());
    }
    if width == 0 || height == 0 {
        return Err("image dimensions must be non-zero".into());
    }
    if data.is_empty() {
        return Err("image binary is empty".into());
    }
    Ok(())
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn abbreviated(value: &str, limit: usize) -> String {
    let mut content: String = value.chars().take(limit).collect();
    if value.chars().count() > limit {
        content.push('…');
    }
    quoted(&content)
}

fn assist_brief(event: &AssistResponseEvent) -> String {
    format!(
        "AssistResponseEvent(prompt_id={}, finished={}, content={})",
        event.prompt_id,
        event.finished,
        abbreviated(&event.content, 80)
    )
}

fn api_usage_string(usage: Option<ApiUsage>) -> String {
    match usage {
        Some(usage) => format!(
            "input_tokens={}, output_tokens={}, total_tokens={}",
            usage.input_tokens, usage.output_tokens, usage.total_tokens
        ),
        None => "none".to_owned(),
    }
}

fn decode_file(bytes: &[u8]) -> Result<(Vec<Event>, usize, EventId)> {
    if bytes.get(..FILE_MAGIC.len()) != Some(FILE_MAGIC) || bytes.len() < FILE_HEADER_SIZE {
        return Err("unsupported or corrupt EDB header".into());
    }
    decode_file_records(bytes, false, CompactRecordFormat::Current)
}

#[derive(Clone, Copy)]
enum CompactRecordFormat {
    LegacySingleTurn,
    GenericV36,
    StrategyV37,
    Current,
}

fn decode_legacy_agent_definition_file(
    bytes: &[u8],
    version: u8,
) -> Result<(Vec<Event>, usize, EventId)> {
    if bytes.len() < FILE_HEADER_SIZE
        || bytes.get(..4) != Some(b"MEDB")
        || bytes[4] != version
        || bytes[5..8] != [0, 0, 0]
    {
        return Err(format!("invalid EDB v{version} file").into());
    }
    decode_file_records(bytes, true, CompactRecordFormat::LegacySingleTurn)
}

fn decode_v35_file(bytes: &[u8]) -> Result<(Vec<Event>, usize, EventId)> {
    if bytes.len() < FILE_HEADER_SIZE
        || bytes.get(..4) != Some(b"MEDB")
        || bytes[4] != 35
        || bytes[5..8] != [0, 0, 0]
    {
        return Err("invalid EDB v35 file".into());
    }
    decode_file_records(bytes, false, CompactRecordFormat::LegacySingleTurn)
}

fn decode_v36_file(bytes: &[u8]) -> Result<(Vec<Event>, usize, EventId)> {
    if bytes.len() < FILE_HEADER_SIZE
        || bytes.get(..4) != Some(b"MEDB")
        || bytes[4] != 36
        || bytes[5..8] != [0, 0, 0]
    {
        return Err("invalid EDB v36 file".into());
    }
    decode_file_records(bytes, false, CompactRecordFormat::GenericV36)
}

fn decode_v37_file(bytes: &[u8]) -> Result<(Vec<Event>, usize, EventId)> {
    if bytes.len() < FILE_HEADER_SIZE
        || bytes.get(..4) != Some(b"MEDB")
        || bytes[4] != 37
        || bytes[5..8] != [0, 0, 0]
    {
        return Err("invalid EDB v37 file".into());
    }
    decode_file_records(bytes, false, CompactRecordFormat::StrategyV37)
}

fn decode_v40_file(bytes: &[u8]) -> Result<(Vec<Event>, usize, EventId)> {
    if bytes.len() < FILE_HEADER_SIZE
        || bytes.get(..4) != Some(b"MEDB")
        || bytes[4] != 40
        || bytes[5..8] != [0, 0, 0]
    {
        return Err("invalid EDB v40 file".into());
    }
    decode_file_records(bytes, false, CompactRecordFormat::Current)
}

fn decode_file_records(
    bytes: &[u8],
    legacy_agent_definition: bool,
    compact_format: CompactRecordFormat,
) -> Result<(Vec<Event>, usize, EventId)> {
    let persisted_next_event_id =
        EventId::from_le_bytes(bytes[FILE_MAGIC.len()..FILE_HEADER_SIZE].try_into()?);

    let mut events = Vec::new();
    let mut offset = FILE_HEADER_SIZE;
    while offset < bytes.len() {
        if bytes.len() - offset < RECORD_HEADER_SIZE {
            break;
        }
        let header = &bytes[offset..offset + RECORD_HEADER_SIZE];
        let codec = header[0];
        let payload_len = u32::from_le_bytes(header[1..5].try_into()?) as usize;
        let raw_len = u32::from_le_bytes(header[5..9].try_into()?) as usize;
        let checksum = u32::from_le_bytes(header[9..13].try_into()?);
        if payload_len > MAX_RECORD_SIZE || raw_len > MAX_RECORD_SIZE {
            return Err("EDB record exceeds the supported size".into());
        }
        let record_end = offset
            .checked_add(RECORD_HEADER_SIZE)
            .and_then(|value| value.checked_add(payload_len))
            .ok_or("EDB record length overflow")?;
        if record_end > bytes.len() {
            break;
        }

        let payload = &bytes[offset + RECORD_HEADER_SIZE..record_end];
        let raw = match codec {
            CODEC_RAW if payload_len == raw_len => payload.to_vec(),
            CODEC_RAW => return Err("invalid raw EDB record length".into()),
            CODEC_ZSTD => zstd::bulk::decompress(payload, raw_len)?,
            _ => return Err(format!("unsupported EDB record codec {codec}").into()),
        };
        if raw.len() != raw_len || crc32fast::hash(&raw) != checksum {
            return Err("EDB record checksum mismatch".into());
        }
        events.push(decode_event_with_format(
            &raw,
            legacy_agent_definition,
            compact_format,
        )?);
        offset = record_end;
    }
    Ok((events, offset, persisted_next_event_id))
}

pub(crate) fn migrate_prompt_sources_v32_to_v33(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() < FILE_HEADER_SIZE || bytes.get(..4) != Some(b"MEDB") || bytes[4] != 32 {
        return Err("prompt-source migration requires an EDB v32 file".into());
    }
    let (events, _, next_event_id) = decode_legacy_agent_definition_file(&bytes, 32)?;
    let is_sub_agent = matches!(
        events.first(),
        Some(Event::AgentKindDef(definition)) if definition.kind == AgentKind::SubAgent
    );
    let is_worker = is_sub_agent
        && events
            .iter()
            .any(|event| matches!(event, Event::SystemPrompt(prompt) if prompt.name == "worker"));

    let events = events
        .into_iter()
        .map(|event| match event {
            Event::UserPrompt(prompt) if is_worker => Event::ManagerPrompt(ManagerPromptEvent {
                id: prompt.id,
                timestamp_ms: prompt.timestamp_ms,
                content: prompt.content,
            }),
            Event::UserPrompt(prompt) if is_sub_agent => {
                Event::ParentAgentPrompt(ParentAgentPromptEvent {
                    id: prompt.id,
                    timestamp_ms: prompt.timestamp_ms,
                    content: prompt.content,
                })
            }
            event => event,
        })
        .collect::<Vec<_>>();

    let mut migrated = encode_file_header(next_event_id).to_vec();
    migrated[4] = 33;
    for event in &events {
        migrated.extend(encode_legacy_agent_definition_record(event)?);
    }
    Ok(migrated)
}

pub(crate) fn migrate_agent_orchestrator_v34_to_v35(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() < FILE_HEADER_SIZE || bytes.get(..4) != Some(b"MEDB") || bytes[4] != 34 {
        return Err("Agent orchestrator migration requires an EDB v34 file".into());
    }
    let (mut events, _, next_event_id) = decode_legacy_agent_definition_file(&bytes, 34)?;
    let orchestrator = if events
        .iter()
        .any(|event| matches!(event, Event::SystemPrompt(prompt) if prompt.name == "worker"))
    {
        "worker-agent"
    } else if events
        .iter()
        .any(|event| matches!(event, Event::SystemPrompt(prompt) if prompt.name == "manager"))
    {
        "manager-agent"
    } else if events
        .iter()
        .any(|event| matches!(event, Event::SystemPrompt(_)))
    {
        "main-agent"
    } else {
        "chatbot"
    };
    if let Some(Event::AgentKindDef(definition)) = events.first_mut() {
        definition.orchestrator = orchestrator.to_owned();
    }

    let mut migrated = encode_file_header(next_event_id).to_vec();
    migrated[4] = 35;
    for event in &events {
        migrated.extend(encode_v35_record(event)?);
    }
    Ok(migrated)
}

pub(crate) fn migrate_compact_kind_v35_to_v36(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() < FILE_HEADER_SIZE || bytes.get(..4) != Some(b"MEDB") || bytes[4] != 35 {
        return Err("Compact kind migration requires an EDB v35 file".into());
    }
    let (events, _, next_event_id) = decode_v35_file(&bytes)?;
    let mut migrated = encode_file_header(next_event_id).to_vec();
    migrated[4] = 36;
    for event in &events {
        migrated.extend(encode_v36_record(event)?);
    }
    Ok(migrated)
}

pub(crate) fn migrate_compact_strategy_v36_to_v37(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() < FILE_HEADER_SIZE || bytes.get(..4) != Some(b"MEDB") || bytes[4] != 36 {
        return Err("Compact strategy migration requires an EDB v36 file".into());
    }
    let (mut events, _, next_event_id) = decode_v36_file(&bytes)?;
    let manager_owned = events.iter().any(|event| {
        matches!(
            event,
            Event::AgentKindDef(definition) if definition.orchestrator == "manager-agent"
        )
    });
    for event in &mut events {
        let Event::CompactStateUpdate(update) = event else {
            continue;
        };
        update.kind = match update.kind {
            // Every pre-v36 Compact was a single-turn lifecycle. Keep that
            // historical execution fact even when it belongs to a Main or
            // Manager EDB.
            CompactKind::WorkerSingleTurn => CompactKind::WorkerSingleTurn,
            // Generic v36 multi-turn lifecycles used the owning orchestrator's
            // shared strategy. Make that strategy explicit in v37.
            CompactKind::MainAgentMultiTurn if manager_owned => CompactKind::ManagerMultiTurn,
            CompactKind::MainAgentMultiTurn => CompactKind::MainAgentMultiTurn,
            CompactKind::ManagerMultiTurn | CompactKind::ChatbotSingleTurn => {
                return Err("EDB v36 contains an impossible Compact kind".into());
            }
        };
    }

    let mut migrated = encode_file_header(next_event_id).to_vec();
    migrated[4] = 37;
    for event in &events {
        migrated.extend(encode_v37_record(event)?);
    }
    Ok(migrated)
}

pub(crate) fn migrate_compact_stage_count_v37_to_v38(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() < FILE_HEADER_SIZE || bytes.get(..4) != Some(b"MEDB") || bytes[4] != 37 {
        return Err("Compact stage-count migration requires an EDB v37 file".into());
    }
    let (events, _, next_event_id) = decode_v37_file(&bytes)?;
    let mut migrated = encode_file_header(next_event_id).to_vec();
    migrated[4] = 38;
    for event in &events {
        migrated.extend(encode_record(event)?);
    }
    Ok(migrated)
}

fn shift_event_id(event_id: &mut EventId) -> Result<()> {
    if *event_id != HOST_AGENT_TITLE_CHANGE {
        *event_id = event_id
            .checked_add(1)
            .ok_or("EventId exhausted during EDB ID migration")?;
    }
    Ok(())
}

fn shift_terminal_session_id(session_id: &mut String) -> Result<()> {
    let Some(suffix) = session_id.strip_prefix("pty-") else {
        return Ok(());
    };
    let event_id = suffix
        .parse::<EventId>()
        .map_err(|_| format!("invalid terminal session ID {session_id}"))?;
    *session_id = format!(
        "pty-{}",
        event_id
            .checked_add(1)
            .ok_or("EventId exhausted during terminal session migration")?
    );
    Ok(())
}

fn shift_event_ids(event: &mut Event) -> Result<()> {
    match event {
        Event::EdbIdGeneration(_) => {
            return Err("EDB v40 cannot contain EdbIdGenerationEvent".into());
        }
        Event::AgentKindDef(event) => shift_event_id(&mut event.id)?,
        Event::AgentTurn(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.turn_id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::SystemPrompt(event) => shift_event_id(&mut event.id)?,
        Event::UserPrompt(event) => shift_event_id(&mut event.id)?,
        Event::ManagerPrompt(event) => shift_event_id(&mut event.id)?,
        Event::ParentAgentPrompt(event) => shift_event_id(&mut event.id)?,
        Event::FollowUpPrompt(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::AssistResponse(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::ApiStateUpdate(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.api_call_id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::ContextUsageEstimate(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.api_state_event_id)?;
        }
        Event::UserTurnAborted(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::ToolCall(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.api_call_id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::ToolInfoUpdate(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.tool_call_id)?;
            if let ToolInfoContent::Terminal(update) = &mut event.content {
                shift_terminal_session_id(&mut update.session_id)?;
            }
        }
        Event::ToolCallResult(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.tool_call_id)?;
        }
        Event::TerminalSessionCreated(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.tool_call_id)?;
            shift_terminal_session_id(&mut event.session_id)?;
        }
        Event::TerminalSessionState(event) => {
            shift_event_id(&mut event.id)?;
            shift_terminal_session_id(&mut event.session_id)?;
        }
        Event::ModelContextItem(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.api_call_id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::ModelChanged(event) => shift_event_id(&mut event.id)?,
        Event::ReasoningEffortChanged(event) => shift_event_id(&mut event.id)?,
        Event::ContextCleared(event) => shift_event_id(&mut event.id)?,
        Event::WorkMapMutation(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.tool_call_id)?;
        }
        Event::WorkMapPendingReminder(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::CompactStateUpdate(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.compact_id)?;
            shift_event_id(&mut event.tool_call_id)?;
            shift_event_id(&mut event.prompt_id)?;
        }
        Event::SystemStaticPromptChange(event) => shift_event_id(&mut event.id)?,
        Event::AgentTitleChanged(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.tool_call_id)?;
        }
        Event::CloneCompleted(event) => shift_event_id(&mut event.id)?,
        Event::ImageContent(event) => {
            shift_event_id(&mut event.id)?;
            shift_event_id(&mut event.tool_call_id)?;
        }
    }
    Ok(())
}

pub(crate) fn migrate_edb_id_v40_to_v41(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() < FILE_HEADER_SIZE || bytes.get(..4) != Some(b"MEDB") || bytes[4] != 40 {
        return Err("EDB ID migration requires an EDB v40 file".into());
    }
    let (mut events, _, persisted_next_event_id) = decode_v40_file(&bytes)?;
    validate_event_ids(&events)?;
    let next_after_events = events
        .last()
        .map(Event::id)
        .map_or(Ok(0), |id| id.checked_add(1).ok_or("EventId exhausted"))?;
    let next_event_id = persisted_next_event_id
        .max(next_after_events)
        .checked_add(1)
        .ok_or("EventId exhausted during EDB ID migration")?;
    let timestamp_ms = events
        .first()
        .map(Event::timestamp_ms)
        .map_or_else(current_timestamp_ms, Ok)?;
    for event in &mut events {
        shift_event_ids(event)?;
    }
    events.insert(0, new_edb_id_event(timestamp_ms)?);
    validate_event_ids(&events)?;
    validate_edb_identity(&events)?;

    let mut migrated = encode_file_header(next_event_id).to_vec();
    migrated[4] = 41;
    for event in &events {
        migrated.extend(encode_record(event)?);
    }
    Ok(migrated)
}

#[cfg(test)]
fn decode_event(raw: &[u8]) -> Result<Event> {
    decode_event_with_format(raw, false, CompactRecordFormat::Current)
}

fn decode_event_with_format(
    raw: &[u8],
    legacy_agent_definition: bool,
    compact_format: CompactRecordFormat,
) -> Result<Event> {
    let (id, consumed) = decode_varint(raw)?;
    let raw = &raw[consumed..];
    let (&kind, body) = raw.split_first().ok_or("empty EDB event record")?;
    let (timestamp_ms, consumed) = decode_varint(body)?;
    let body = &body[consumed..];
    match kind {
        28 => {
            let edb_id = String::from_utf8(body.to_vec())?;
            validate_edb_id(&edb_id)?;
            Ok(Event::EdbIdGeneration(EdbIdGenerationEvent {
                id,
                timestamp_ms,
                edb_id,
            }))
        }
        16 => {
            let (&kind, body) = body.split_first().ok_or("missing AgentKindDefEvent kind")?;
            let kind = AgentKind::from_code(kind)?;
            let (&has_parent, body) = body
                .split_first()
                .ok_or("missing AgentKindDefEvent parent flag")?;
            let (parent_agent_id, body) = match has_parent {
                0 => (None, body),
                1 => {
                    let (parent, body) = decode_sized_string(body)?;
                    (Some(parent), body)
                }
                _ => return Err("invalid AgentKindDefEvent parent flag".into()),
            };
            let (orchestrator, body) = if legacy_agent_definition {
                (String::new(), body)
            } else {
                let (orchestrator, body) = decode_sized_string(body)?;
                if orchestrator.is_empty() {
                    return Err("Agent definition requires an orchestrator".into());
                }
                (orchestrator, body)
            };
            let (&has_prompt, body) = body
                .split_first()
                .ok_or("missing AgentKindDefEvent system prompt flag")?;
            let system_prompt = match has_prompt {
                0 if body.is_empty() => None,
                1 => Some(String::from_utf8(body.to_vec())?),
                0 => return Err("AgentKindDefEvent has trailing payload".into()),
                _ => return Err("invalid AgentKindDefEvent system prompt flag".into()),
            };
            if kind == AgentKind::SubAgent {
                if parent_agent_id.as_deref().is_none_or(str::is_empty) {
                    return Err("sub-Agent definition requires parent_agent_id".into());
                }
            } else if parent_agent_id.is_some() || system_prompt.is_some() {
                return Err(
                    "only a sub-Agent definition may contain parent or system prompt data".into(),
                );
            }
            Ok(Event::AgentKindDef(AgentKindDefEvent {
                id,
                timestamp_ms,
                kind,
                orchestrator,
                parent_agent_id,
                system_prompt,
            }))
        }
        17 => {
            let (turn_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (prompt_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (&state, detail) = body.split_first().ok_or("missing AgentTurnEvent state")?;
            Ok(Event::AgentTurn(AgentTurnEvent {
                id,
                timestamp_ms,
                turn_id,
                prompt_id,
                state: AgentTurnState::from_code(state)?,
                detail: String::from_utf8(detail.to_vec())?,
            }))
        }
        3 => {
            let name = String::from_utf8(body.to_vec())?;
            if name.trim().is_empty() {
                return Err("SystemPromptEvent name cannot be empty".into());
            }
            Ok(Event::SystemPrompt(SystemPromptEvent {
                id,
                timestamp_ms,
                name,
            }))
        }
        0 => Ok(Event::UserPrompt(UserPromptEvent {
            id,
            timestamp_ms,
            content: String::from_utf8(body.to_vec())?,
        })),
        23 => Ok(Event::ManagerPrompt(ManagerPromptEvent {
            id,
            timestamp_ms,
            content: String::from_utf8(body.to_vec())?,
        })),
        24 => Ok(Event::ParentAgentPrompt(ParentAgentPromptEvent {
            id,
            timestamp_ms,
            content: String::from_utf8(body.to_vec())?,
        })),
        9 => {
            let (prompt_id, consumed) = decode_varint(body)?;
            Ok(Event::FollowUpPrompt(FollowUpPromptEvent {
                id,
                timestamp_ms,
                prompt_id,
                content: String::from_utf8(body[consumed..].to_vec())?,
            }))
        }
        1 => {
            let (prompt_id, consumed) = decode_varint(body)?;
            let (&finished, content) = body[consumed..]
                .split_first()
                .ok_or("missing AssistResponseEvent finished flag")?;
            let finished = match finished {
                0 => false,
                1 => true,
                _ => return Err("invalid AssistResponseEvent finished flag".into()),
            };
            Ok(Event::AssistResponse(AssistResponseEvent {
                id,
                timestamp_ms,
                prompt_id,
                content: String::from_utf8(content.to_vec())?,
                finished,
            }))
        }
        2 => {
            let (api_call_id, api_call_consumed) = decode_varint(body)?;
            let (prompt_id, prompt_consumed) = decode_varint(&body[api_call_consumed..])?;
            let state_offset = api_call_consumed + prompt_consumed;
            let (&state, body) = body[state_offset..]
                .split_first()
                .ok_or("missing ApiStateUpdateEvent state")?;
            let state = ApiState::from_code(state)?;
            let (&retry_count, body) = body
                .split_first()
                .ok_or("missing ApiStateUpdateEvent retry count")?;
            let (&retry_limit, body) = body
                .split_first()
                .ok_or("missing ApiStateUpdateEvent retry limit")?;
            if state == ApiState::Retrying {
                if retry_count == 0 || retry_count > retry_limit {
                    return Err("invalid ApiStateUpdateEvent retry metadata".into());
                }
            } else if retry_count != 0 || retry_limit != 0 {
                return Err("non-retry API state contains retry metadata".into());
            }
            let (&has_usage, body) = body
                .split_first()
                .ok_or("missing ApiStateUpdateEvent usage flag")?;
            let (usage, detail) = match has_usage {
                0 => (None, body),
                1 => {
                    let (input_tokens, consumed) = decode_varint(body)?;
                    let body = &body[consumed..];
                    let (output_tokens, consumed) = decode_varint(body)?;
                    let body = &body[consumed..];
                    let (total_tokens, consumed) = decode_varint(body)?;
                    (
                        Some(ApiUsage {
                            input_tokens,
                            output_tokens,
                            total_tokens,
                        }),
                        &body[consumed..],
                    )
                }
                _ => return Err("invalid ApiStateUpdateEvent usage flag".into()),
            };
            if usage.is_some() && !state.is_terminal() {
                return Err("API usage is only valid on a terminal API state".into());
            }
            Ok(Event::ApiStateUpdate(ApiStateUpdateEvent {
                id,
                timestamp_ms,
                api_call_id,
                prompt_id,
                state,
                retry_count,
                retry_limit,
                usage,
                detail: String::from_utf8(detail.to_vec())?,
            }))
        }
        26 => {
            let (api_state_event_id, consumed) = decode_varint(body)?;
            let mut body = &body[consumed..];
            let mut values = [0_u64; 6];
            for value in &mut values {
                let (decoded, consumed) = decode_varint(body)?;
                *value = decoded;
                body = &body[consumed..];
            }
            if !body.is_empty() {
                return Err("invalid ContextUsageEstimateEvent payload".into());
            }
            Ok(Event::ContextUsageEstimate(ContextUsageEstimateEvent {
                id,
                timestamp_ms,
                api_state_event_id,
                values: ContextTokenUsage {
                    system: values[0],
                    compact: values[1],
                    memory: values[2],
                    user: values[3],
                    model: values[4],
                    tool: values[5],
                },
            }))
        }
        15 => {
            let (prompt_id, consumed) = decode_varint(body)?;
            if consumed != body.len() {
                return Err("invalid UserTurnAbortedEvent payload".into());
            }
            Ok(Event::UserTurnAborted(UserTurnAbortedEvent {
                id,
                timestamp_ms,
                prompt_id,
            }))
        }
        4 => {
            let (api_call_id, api_call_consumed) = decode_varint(body)?;
            let body = &body[api_call_consumed..];
            let (prompt_id, prompt_consumed) = decode_varint(body)?;
            let body = &body[prompt_consumed..];
            let (provider_call_id, body) = decode_sized_string(body)?;
            let (name, arguments) = decode_sized_string(body)?;
            Ok(Event::ToolCall(ToolCallEvent {
                id,
                timestamp_ms,
                api_call_id,
                prompt_id,
                provider_call_id,
                name,
                arguments: String::from_utf8(arguments.to_vec())?,
            }))
        }
        5 => {
            let (tool_call_id, consumed) = decode_varint(body)?;
            let (&stream, content) = body[consumed..]
                .split_first()
                .ok_or("missing ToolInfoUpdateEvent stream")?;
            let stream = ToolOutputStream::from_code(stream)?;
            let content: ToolInfoContent = serde_json::from_slice(content)?;
            if matches!(content, ToolInfoContent::Terminal(_))
                != (stream == ToolOutputStream::Terminal)
            {
                return Err("ToolInfoUpdateEvent stream/content mismatch".into());
            }
            if let ToolInfoContent::Terminal(update) = &content {
                update
                    .validate()
                    .map_err(|error| format!("invalid terminal line update: {error}"))?;
            }
            Ok(Event::ToolInfoUpdate(ToolInfoUpdateEvent {
                id,
                timestamp_ms,
                tool_call_id,
                stream,
                content,
            }))
        }
        6 => {
            let (tool_call_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (&state, body) = body
                .split_first()
                .ok_or("missing ToolCallResultEvent state")?;
            let (&has_exit_code, body) = body
                .split_first()
                .ok_or("missing ToolCallResultEvent exit code flag")?;
            let (exit_code, detail) = match has_exit_code {
                0 => (None, body),
                1 if body.len() >= 4 => {
                    (Some(i32::from_le_bytes(body[..4].try_into()?)), &body[4..])
                }
                1 => return Err("missing ToolCallResultEvent exit code".into()),
                _ => return Err("invalid ToolCallResultEvent exit code flag".into()),
            };
            Ok(Event::ToolCallResult(ToolCallResultEvent {
                id,
                timestamp_ms,
                tool_call_id,
                state: ToolResultState::from_code(state)?,
                exit_code,
                detail: String::from_utf8(detail.to_vec())?,
            }))
        }
        18 => {
            let (tool_call_id, consumed) = decode_varint(body)?;
            let mutation: WorkMapMutation = serde_json::from_slice(&body[consumed..])?;
            if mutation.records.is_empty() {
                return Err("WorkMapMutationEvent has no records".into());
            }
            Ok(Event::WorkMapMutation(WorkMapMutationEvent {
                id,
                timestamp_ms,
                tool_call_id,
                mutation,
            }))
        }
        20 => {
            let (prompt_id, consumed) = decode_varint(body)?;
            if consumed != body.len() {
                return Err("invalid WorkMapPendingReminderEvent payload".into());
            }
            Ok(Event::WorkMapPendingReminder(WorkMapPendingReminderEvent {
                id,
                timestamp_ms,
                prompt_id,
            }))
        }
        7 => {
            let (tool_call_id, consumed) = decode_varint(body)?;
            let (session_id, body) = decode_sized_string(&body[consumed..])?;
            let (shell, body) = decode_sized_string(body)?;
            let (cwd, body) = decode_sized_string(body)?;
            if body.len() != 4 {
                return Err("invalid TerminalSessionCreatedEvent size".into());
            }
            Ok(Event::TerminalSessionCreated(TerminalSessionCreatedEvent {
                id,
                timestamp_ms,
                tool_call_id,
                session_id,
                shell,
                cwd,
                width: u16::from_le_bytes(body[..2].try_into()?),
                height: u16::from_le_bytes(body[2..].try_into()?),
            }))
        }
        8 => {
            let (session_id, body) = decode_sized_string(body)?;
            let (&state, body) = body
                .split_first()
                .ok_or("missing TerminalSessionStateEvent state")?;
            let (&has_exit_code, body) = body
                .split_first()
                .ok_or("missing TerminalSessionStateEvent exit code flag")?;
            let (exit_code, detail) = match has_exit_code {
                0 => (None, body),
                1 if body.len() >= 4 => {
                    (Some(i32::from_le_bytes(body[..4].try_into()?)), &body[4..])
                }
                1 => return Err("missing TerminalSessionStateEvent exit code".into()),
                _ => return Err("invalid TerminalSessionStateEvent exit code flag".into()),
            };
            Ok(Event::TerminalSessionState(TerminalSessionStateEvent {
                id,
                timestamp_ms,
                session_id,
                state: TerminalSessionState::from_code(state)?,
                exit_code,
                detail: String::from_utf8(detail.to_vec())?,
            }))
        }
        10 => {
            let (api_call_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (prompt_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (provider, content) = decode_sized_string(body)?;
            let content = String::from_utf8(content.to_vec())?;
            let value: serde_json::Value = serde_json::from_str(&content)?;
            if !value.is_object() {
                return Err("ModelContextItemEvent content must be a JSON object".into());
            }
            Ok(Event::ModelContextItem(ModelContextItemEvent {
                id,
                timestamp_ms,
                api_call_id,
                prompt_id,
                provider,
                content,
            }))
        }
        14 => {
            let (&cause, model) = body
                .split_first()
                .ok_or("missing ModelChangedEvent cause")?;
            let model = String::from_utf8(model.to_vec())?;
            if model.trim().is_empty() {
                return Err("ModelChangedEvent model is empty".into());
            }
            Ok(Event::ModelChanged(ModelChangedEvent {
                id,
                timestamp_ms,
                model,
                cause: ModelChangeCause::from_code(cause)?,
            }))
        }
        11 => {
            let (&cause, effort) = body
                .split_first()
                .ok_or("missing ReasoningEffortChangedEvent cause")?;
            let cause = ReasoningEffortChangeCause::from_code(cause)?;
            let effort = String::from_utf8(effort.to_vec())?;
            if effort.trim().is_empty() {
                return Err("ReasoningEffortChangedEvent effort is empty".into());
            }
            if cause == ReasoningEffortChangeCause::ModelUnsupported
                && effort != crate::config::UNSET_EFFORT
            {
                return Err("unsupported-model effort fallback must be unset".into());
            }
            Ok(Event::ReasoningEffortChanged(ReasoningEffortChangedEvent {
                id,
                timestamp_ms,
                effort,
                cause,
            }))
        }
        12 if body.is_empty() => Ok(Event::ContextCleared(ContextClearedEvent {
            id,
            timestamp_ms,
        })),
        12 => Err("invalid ContextClearedEvent payload".into()),
        19 => {
            let (compact_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (tool_call_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (prompt_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (&state, body) = body
                .split_first()
                .ok_or("missing CompactStateUpdateEvent state")?;
            let state = CompactState::from_code(state)?;
            let (kind, stage, total_stages, body) =
                if matches!(compact_format, CompactRecordFormat::LegacySingleTurn) {
                    (CompactKind::WorkerSingleTurn, None, 1, body)
                } else {
                    let (&kind, body) = body
                        .split_first()
                        .ok_or("missing CompactStateUpdateEvent kind")?;
                    let kind = match compact_format {
                        CompactRecordFormat::GenericV36 => match kind {
                            0 => CompactKind::WorkerSingleTurn,
                            1 => CompactKind::MainAgentMultiTurn,
                            _ => {
                                return Err(
                                    format!("unsupported EDB v36 Compact kind {kind}").into()
                                );
                            }
                        },
                        CompactRecordFormat::StrategyV37 | CompactRecordFormat::Current => {
                            CompactKind::from_code(kind)?
                        }
                        CompactRecordFormat::LegacySingleTurn => unreachable!(),
                    };
                    let (&stage, body) = body
                        .split_first()
                        .ok_or("missing CompactStateUpdateEvent stage")?;
                    let stage = (stage != u8::MAX)
                        .then(|| CompactStage::from_code(stage))
                        .transpose()?;
                    let (total_stages, body) = match compact_format {
                        CompactRecordFormat::Current => {
                            let (&total_stages, body) = body
                                .split_first()
                                .ok_or("missing CompactStateUpdateEvent total stages")?;
                            (total_stages, body)
                        }
                        CompactRecordFormat::GenericV36 | CompactRecordFormat::StrategyV37 => {
                            (u8::try_from(kind.base_stage_count())?, body)
                        }
                        CompactRecordFormat::LegacySingleTurn => unreachable!(),
                    };
                    (kind, stage, total_stages, body)
                };
            let (content, detail) = decode_sized_string(body)?;
            let detail = String::from_utf8(detail.to_vec())?;
            if !kind.accepts_stage_count(total_stages) {
                return Err(
                    format!("Compact kind {kind} does not support {total_stages} stages").into(),
                );
            }
            if let Some(stage) = stage
                && kind
                    .stages(total_stages)
                    .is_none_or(|stages| !stages.contains(&stage))
            {
                return Err(format!(
                    "Compact stage {stage} is not part of the {kind} {total_stages}-stage plan"
                )
                .into());
            }
            match state {
                CompactState::Started
                    if compact_id != id
                        || stage.is_some()
                        || !content.is_empty()
                        || !detail.is_empty() =>
                {
                    return Err("invalid Compact started payload".into());
                }
                CompactState::StageCompleted
                    if !kind.is_multi_turn()
                        || stage.is_none()
                        || content.is_empty()
                        || !detail.is_empty() =>
                {
                    return Err("invalid Compact stage payload".into());
                }
                CompactState::Completed
                    if stage.is_some() || content.trim().is_empty() || !detail.is_empty() =>
                {
                    return Err("invalid Compact completed payload".into());
                }
                CompactState::Failed | CompactState::Interrupted
                    if stage.is_some() || !content.is_empty() || detail.trim().is_empty() =>
                {
                    return Err("invalid Compact unsuccessful payload".into());
                }
                _ => {}
            }
            Ok(Event::CompactStateUpdate(CompactStateUpdateEvent {
                id,
                timestamp_ms,
                compact_id,
                tool_call_id,
                prompt_id,
                kind,
                total_stages,
                state,
                stage,
                content,
                detail,
            }))
        }
        27 => {
            let (&mode, content) = body
                .split_first()
                .ok_or("missing SystemStaticPromptChangeEvent mode")?;
            let mode = SystemStaticPromptMode::from_code(mode)?;
            let content = match mode {
                SystemStaticPromptMode::Default if content.is_empty() => None,
                SystemStaticPromptMode::Default => {
                    return Err("default SystemStaticPromptChangeEvent has trailing content".into());
                }
                SystemStaticPromptMode::Custom => Some(String::from_utf8(content.to_vec())?),
            };
            validate_system_static_prompt_change(mode, content.as_deref())?;
            Ok(Event::SystemStaticPromptChange(
                SystemStaticPromptChangeEvent {
                    id,
                    timestamp_ms,
                    mode,
                    content,
                },
            ))
        }
        21 => {
            let (tool_call_id, consumed) = decode_varint(body)?;
            let title = String::from_utf8(body[consumed..].to_vec())?;
            let title = crate::agent_title::normalize_title(&title)
                .map_err(|error| format!("invalid AgentTitleChangedEvent title: {error}"))?;
            Ok(Event::AgentTitleChanged(AgentTitleChangedEvent {
                id,
                timestamp_ms,
                tool_call_id,
                title,
            }))
        }
        22 => {
            let title = String::from_utf8(body.to_vec())?;
            let title = crate::agent_title::normalize_title(&title)
                .map_err(|error| format!("invalid CloneCompletedEvent title: {error}"))?;
            Ok(Event::CloneCompleted(CloneCompletedEvent {
                id,
                timestamp_ms,
                title,
            }))
        }
        25 => {
            let (tool_call_id, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (width, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (height, consumed) = decode_varint(body)?;
            let body = &body[consumed..];
            let (source, body) = decode_sized_string(body)?;
            let (mime_type, body) = decode_sized_string(body)?;
            let (format, body) = decode_sized_string(body)?;
            let (content_sha256, data) = decode_sized_string(body)?;
            let width = u32::try_from(width)?;
            let height = u32::try_from(height)?;
            validate_image_content_fields(&source, &mime_type, &format, width, height, data)?;
            let actual_sha256 = image_content_sha256(data);
            if content_sha256 != actual_sha256 {
                return Err(format!(
                    "ImageContentEvent SHA-256 mismatch: expected {content_sha256}, found {actual_sha256}"
                )
                .into());
            }
            Ok(Event::ImageContent(ImageContentEvent {
                id,
                timestamp_ms,
                tool_call_id,
                source,
                mime_type,
                format,
                width,
                height,
                content_sha256,
                data: data.to_vec().into(),
            }))
        }
        _ => Err(format!("unsupported EDB event kind {kind}").into()),
    }
}

fn encode_sized_string(value: &str, output: &mut Vec<u8>) {
    encode_varint(value.len() as u64, output);
    output.extend_from_slice(value.as_bytes());
}

fn decode_sized_string(bytes: &[u8]) -> Result<(String, &[u8])> {
    let (length, consumed) = decode_varint(bytes)?;
    let length = usize::try_from(length)?;
    let end = consumed
        .checked_add(length)
        .ok_or("EDB string length overflow")?;
    if end > bytes.len() {
        return Err("incomplete EDB string".into());
    }
    Ok((
        String::from_utf8(bytes[consumed..end].to_vec())?,
        &bytes[end..],
    ))
}

fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push(value as u8 | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn decode_varint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().copied().take(10).enumerate() {
        if index == 9 && byte > 1 {
            return Err("invalid EDB varint".into());
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err("invalid EDB varint".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("me-edb-{name}-{}", std::process::id()))
    }

    fn rewrite_as_current_record_version(path: &Path, version: u8) {
        let edb = EventDataBase::open(path).unwrap();
        let events = edb.events().iter().skip(1).cloned().collect::<Vec<_>>();
        let next_event_id = edb.next_event_id();
        drop(edb);
        let mut bytes = encode_file_header(next_event_id).to_vec();
        bytes[4] = version;
        for event in &events {
            bytes.extend(encode_record(event).unwrap());
        }
        fs::write(path, bytes).unwrap();
    }

    fn rewrite_as_legacy_agent_definition(path: &Path, version: u8) {
        let edb = EventDataBase::open(path).unwrap();
        let events = edb.events().iter().skip(1).cloned().collect::<Vec<_>>();
        let next_event_id = edb.next_event_id();
        drop(edb);
        let mut bytes = encode_file_header(next_event_id).to_vec();
        bytes[4] = version;
        for event in &events {
            bytes.extend(encode_legacy_agent_definition_record(event).unwrap());
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn reserves_event_zero_for_edb_id_then_assigns_continuous_ids() {
        let mut edb = EventDataBase::new();
        assert!(matches!(edb.get(0), Some(Event::EdbIdGeneration(_))));
        assert_eq!(edb.edb_id().unwrap().len(), EDB_ID_HEX_LENGTH);
        let prompt = edb.append_user_prompt("hello").unwrap();
        assert_eq!(prompt, 1);
        let api = edb.append_api_requesting(prompt).unwrap();
        assert_eq!(api, 2);
        assert_eq!(
            edb.append_api_state(api, prompt, ApiState::Streaming, "")
                .unwrap(),
            3
        );
        assert_eq!(
            edb.append_assist_response(prompt, "world", true).unwrap(),
            4
        );
        assert_eq!(
            edb.append_api_state(api, prompt, ApiState::Completed, "")
                .unwrap(),
            5
        );
        assert_eq!(edb.get(prompt).unwrap().kind(), EventKind::UserPrompt);
        assert_eq!(edb.get(api).unwrap().kind(), EventKind::ApiStateUpdate);
        assert!(edb.has_assist_response(prompt));
        assert_eq!(edb.get(0).unwrap().getHash().len(), 64);
        assert!(edb.get(0).unwrap().getTimestamp() > 0);
        assert!(
            edb.events()
                .windows(2)
                .all(|events| events[0].id() < events[1].id())
        );
        assert!(edb.get(prompt).unwrap().getBriefString().contains("hello"));
        assert!(edb.get(5).unwrap().getBriefString().contains("completed"));
    }

    #[test]
    fn edb_ids_are_canonical_random_and_preserved_by_reopen_and_copy() {
        let first = EventDataBase::new();
        let first_id = first.edb_id().unwrap();
        assert_eq!(first_id.len(), EDB_ID_HEX_LENGTH);
        assert!(
            first_id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(first_id, EventDataBase::new().edb_id().unwrap());

        let directory = temporary_path("edb-id-copy");
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let source_path = directory.join("source.edb");
        let copy_path = directory.join("copy.edb");
        let persisted_id = {
            let mut edb = EventDataBase::open(&source_path).unwrap();
            let id = edb.edb_id().unwrap().to_owned();
            edb.append_user_prompt("persist identity").unwrap();
            id
        };
        fs::copy(&source_path, &copy_path).unwrap();
        assert_eq!(
            fs::read(&source_path).unwrap(),
            fs::read(&copy_path).unwrap()
        );

        let source = EventDataBase::open(&source_path).unwrap();
        let copied = EventDataBase::open(&copy_path).unwrap();
        assert_eq!(source.edb_id().unwrap(), persisted_id);
        assert_eq!(copied.edb_id().unwrap(), persisted_id);
        assert!(matches!(
            source.events().first(),
            Some(Event::EdbIdGeneration(event))
                if event.id == 0 && event.edb_id == persisted_id
        ));
        drop(source);
        drop(copied);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn edb_identity_validation_rejects_missing_invalid_and_duplicate_events() {
        let identity = EventDataBase::new().events()[0].clone();

        let missing = vec![Event::UserPrompt(UserPromptEvent {
            id: 0,
            timestamp_ms: 1,
            content: "missing identity".to_owned(),
        })];
        assert!(
            edb_id(&missing)
                .unwrap_err()
                .to_string()
                .contains("must begin")
        );

        let mut invalid_id = vec![identity.clone()];
        let Event::EdbIdGeneration(event) = &mut invalid_id[0] else {
            unreachable!()
        };
        event.id = 1;
        assert!(
            edb_id(&invalid_id)
                .unwrap_err()
                .to_string()
                .contains("EventId 0")
        );

        let mut invalid_format = vec![identity.clone()];
        let Event::EdbIdGeneration(event) = &mut invalid_format[0] else {
            unreachable!()
        };
        event.edb_id = "A".repeat(EDB_ID_HEX_LENGTH);
        assert!(
            edb_id(&invalid_format)
                .unwrap_err()
                .to_string()
                .contains("lowercase hexadecimal")
        );

        let mut duplicate = identity.clone();
        let Event::EdbIdGeneration(event) = &mut duplicate else {
            unreachable!()
        };
        event.id = 1;
        assert!(
            edb_id(&[identity, duplicate])
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[test]
    fn timestamp_is_part_of_the_stable_event_hash() {
        let first = UserPromptEvent {
            id: 0,
            timestamp_ms: 1_000,
            content: "hello".to_owned(),
        };
        let second = UserPromptEvent {
            timestamp_ms: 1_001,
            ..first.clone()
        };
        assert_ne!(first.getHash(), second.getHash());
        assert_eq!(first.getTimestamp(), 1_000);
    }

    #[test]
    fn internal_agent_prompt_events_have_distinct_semantics_and_descriptions() {
        let mut edb = EventDataBase::new();
        let manager = edb.append_manager_prompt("manager instruction").unwrap();
        let parent = edb.append_parent_agent_prompt("parent assignment").unwrap();

        assert_eq!(edb.get(manager).unwrap().kind(), EventKind::ManagerPrompt);
        assert_eq!(
            edb.get(parent).unwrap().kind(),
            EventKind::ParentAgentPrompt
        );
        assert!(
            edb.get(manager)
                .unwrap()
                .getBriefString()
                .contains("ManagerPromptEvent")
        );
        assert!(
            edb.get(parent)
                .unwrap()
                .getDetailString()
                .contains("parent assignment")
        );
        assert_ne!(
            edb.get(manager).unwrap().getHash(),
            edb.get(parent).unwrap().getHash()
        );
    }

    #[test]
    fn system_prompt_event_contains_only_a_nonempty_name() {
        let mut edb = EventDataBase::new();
        assert!(edb.append_system_prompt(" \n").is_err());

        let id = edb.append_system_prompt("tool").unwrap();
        let event = edb.get(id).unwrap();
        assert_eq!(event.kind(), EventKind::SystemPrompt);
        assert!(matches!(
            event,
            Event::SystemPrompt(prompt) if prompt.name == "tool"
        ));
        assert_eq!(event.getBriefString(), "SystemPromptEvent(name=\"tool\")");
        assert!(event.getDetailString().contains("name=\"tool\""));
        assert!(!event.getDetailString().contains("content="));
    }

    #[test]
    fn agent_title_change_round_trips_and_rewind_removes_it() {
        let directory = temporary_path("agent-title");
        let path = directory.join("main.edb");
        let prompt_id;
        let call_id;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            prompt_id = edb.append_user_prompt("investigate input latency").unwrap();
            let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            call_id = edb
                .append_tool_call(
                    api_call_id,
                    prompt_id,
                    "provider-title",
                    crate::agent_title::TOOL_NAME,
                    r#"{"title":"调查输入延迟"}"#,
                )
                .unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
                .unwrap();
            let changed_id = edb
                .append_agent_title_changed(call_id, "调查输入延迟")
                .unwrap();
            edb.append_tool_result(
                call_id,
                ToolResultState::Succeeded,
                None,
                r#"{"title":"调查输入延迟"}"#,
            )
            .unwrap();
            let changed = edb.get(changed_id).unwrap();
            assert_eq!(changed.kind(), EventKind::AgentTitleChanged);
            assert!(changed.getBriefString().contains("调查输入延迟"));
        }

        let mut edb = EventDataBase::open(&path).unwrap();
        assert_eq!(
            crate::agent_title::current_title(edb.events()),
            Some("调查输入延迟")
        );
        assert!(matches!(
            edb.events().iter().find(|event| event.kind() == EventKind::AgentTitleChanged),
            Some(Event::AgentTitleChanged(changed)) if changed.tool_call_id == call_id
        ));
        edb.rewind_to_event(prompt_id).unwrap();
        assert_eq!(crate::agent_title::current_title(edb.events()), None);
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn system_static_prompt_changes_validate_project_round_trip_and_rewind() {
        let directory = temporary_path("system-static-prompt-change");
        let path = directory.join("chatbot.edb");
        let custom_one;
        let restored_default;
        let custom_two;
        let hashes;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            assert!(
                edb.append_system_static_prompt_change(
                    SystemStaticPromptMode::Custom,
                    Some(" \n".into()),
                )
                .is_err()
            );
            assert!(
                edb.append_system_static_prompt_change(
                    SystemStaticPromptMode::Default,
                    Some("unexpected".into()),
                )
                .is_err()
            );
            assert!(
                edb.append_system_static_prompt_change(
                    SystemStaticPromptMode::Custom,
                    Some("x".repeat(MAX_SYSTEM_STATIC_PROMPT_BYTES + 1)),
                )
                .is_err()
            );

            custom_one = edb
                .append_system_static_prompt_change(
                    SystemStaticPromptMode::Custom,
                    Some("You are calm.\n准确回答。".into()),
                )
                .unwrap();
            restored_default = edb
                .append_system_static_prompt_change(SystemStaticPromptMode::Default, None)
                .unwrap();
            custom_two = edb
                .append_system_static_prompt_change(
                    SystemStaticPromptMode::Custom,
                    Some("Keep answers concise.".into()),
                )
                .unwrap();

            let custom_one_end = edb.order_of(custom_one).unwrap() + 1;
            let restored_default_end = edb.order_of(restored_default).unwrap() + 1;
            assert!(
                latest_system_static_prompt_change(&edb.events()[..custom_one_end - 1]).is_none()
            );
            assert!(matches!(
                latest_system_static_prompt_change(&edb.events()[..custom_one_end]),
                Some(change)
                    if change.id == custom_one
                        && change.mode == SystemStaticPromptMode::Custom
                        && change.content.as_deref() == Some("You are calm.\n准确回答。")
            ));
            assert!(matches!(
                latest_system_static_prompt_change(&edb.events()[..restored_default_end]),
                Some(change)
                    if change.id == restored_default
                        && change.mode == SystemStaticPromptMode::Default
                        && change.content.is_none()
            ));
            let latest = latest_system_static_prompt_change(edb.events()).unwrap();
            assert_eq!(latest.id, custom_two);
            assert!(latest.getBriefString().contains("Keep answers concise"));
            assert!(latest.getDetailString().contains("mode=custom"));
            hashes = edb
                .events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>();
        }

        let mut edb = EventDataBase::open(&path).unwrap();
        assert_eq!(
            edb.events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>(),
            hashes
        );
        assert!(matches!(
            latest_system_static_prompt_change(edb.events()),
            Some(change) if change.id == custom_two
        ));

        let mutation = edb.rewind_to_event(custom_two).unwrap();
        assert_eq!(
            mutation,
            EdbMutation::Rewind {
                target_event_id: custom_two,
                restored_prompt_content: None,
            }
        );
        assert!(matches!(
            latest_system_static_prompt_change(edb.events()),
            Some(change) if change.id == restored_default
                && change.mode == SystemStaticPromptMode::Default
        ));
        edb.rewind_to_event(restored_default).unwrap();
        assert!(matches!(
            latest_system_static_prompt_change(edb.events()),
            Some(change) if change.id == custom_one
                && change.content.as_deref() == Some("You are calm.\n准确回答。")
        ));
        assert_eq!(edb.next_event_id(), custom_two + 1);
        drop(edb);

        let reopened = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            latest_system_static_prompt_change(reopened.events()),
            Some(change) if change.id == custom_one
        ));
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn chatbot_single_turn_compact_kind_round_trips_with_wire_code_three() {
        assert_eq!(CompactKind::ChatbotSingleTurn.code(), 3);
        assert_eq!(
            CompactKind::from_code(3).unwrap(),
            CompactKind::ChatbotSingleTurn
        );
        assert!(CompactKind::ChatbotSingleTurn.accepts_stage_count(1));
        assert!(!CompactKind::ChatbotSingleTurn.accepts_stage_count(2));

        let directory = temporary_path("chatbot-compact-kind");
        let path = directory.join("chatbot.edb");
        let compact;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt = edb.append_user_prompt("compact chat").unwrap();
            let api = edb.append_api_requesting(prompt).unwrap();
            let tool = edb
                .append_tool_call(api, prompt, "compact", crate::compact::TOOL_NAME, "{}")
                .unwrap();
            edb.append_api_state(api, prompt, ApiState::Completed, "")
                .unwrap();
            edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
                .unwrap();
            compact = edb
                .append_compact_started(tool, prompt, CompactKind::ChatbotSingleTurn)
                .unwrap();
            edb.append_compact_terminal(
                compact,
                CompactState::Completed,
                "Summary:\nConversation continuity",
                "",
            )
            .unwrap();
        }

        let reopened = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            reopened.get(compact),
            Some(Event::CompactStateUpdate(update))
                if update.kind == CompactKind::ChatbotSingleTurn
                    && update.total_stages == 1
        ));
        assert!(matches!(
            reopened.events().last(),
            Some(Event::CompactStateUpdate(update))
                if update.kind == CompactKind::ChatbotSingleTurn
                    && update.state == CompactState::Completed
        ));
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opening_v39_inserts_a_new_edb_id_and_preserves_content() {
        let directory = temporary_path("migrate-v39");
        let path = directory.join("main.edb");
        let old_edb_id;
        let expected_next;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            old_edb_id = edb.edb_id().unwrap().to_owned();
            edb.append_user_prompt("preserve v39").unwrap();
            expected_next = edb.next_event_id() + 1;
        }
        rewrite_as_current_record_version(&path, 39);

        let migrated = EventDataBase::open(&path).unwrap();
        assert_ne!(migrated.edb_id().unwrap(), old_edb_id);
        assert_eq!(migrated.next_event_id(), expected_next);
        assert!(migrated.events().iter().any(|event| {
            matches!(event, Event::UserPrompt(prompt) if prompt.content == "preserve v39")
        }));
        assert_eq!(fs::read(&path).unwrap()[4], 41);
        drop(migrated);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opening_v40_shifts_event_references_terminal_ids_and_next_id_once() {
        let directory = temporary_path("migrate-v40-references");
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.edb");
        let events = vec![
            Event::UserPrompt(UserPromptEvent {
                id: 0,
                timestamp_ms: 1,
                content: "run".to_owned(),
            }),
            Event::ApiStateUpdate(ApiStateUpdateEvent {
                id: 1,
                timestamp_ms: 2,
                api_call_id: 1,
                prompt_id: 0,
                state: ApiState::Requesting,
                retry_count: 0,
                retry_limit: 0,
                usage: None,
                detail: String::new(),
            }),
            Event::ToolCall(ToolCallEvent {
                id: 2,
                timestamp_ms: 3,
                api_call_id: 1,
                prompt_id: 0,
                provider_call_id: "provider-call".to_owned(),
                name: "Terminal.Create".to_owned(),
                arguments: "{}".to_owned(),
            }),
            Event::TerminalSessionCreated(TerminalSessionCreatedEvent {
                id: 3,
                timestamp_ms: 4,
                tool_call_id: 2,
                session_id: "pty-2".to_owned(),
                shell: "/bin/zsh".to_owned(),
                cwd: "/workspace".to_owned(),
                width: 80,
                height: 24,
            }),
            Event::ToolInfoUpdate(ToolInfoUpdateEvent {
                id: 4,
                timestamp_ms: 5,
                tool_call_id: 2,
                stream: ToolOutputStream::Terminal,
                content: ToolInfoContent::Terminal({
                    let mut update = crate::terminal::test_update("ready");
                    update.session_id = "pty-2".to_owned();
                    update
                }),
            }),
            Event::TerminalSessionState(TerminalSessionStateEvent {
                id: 5,
                timestamp_ms: 6,
                session_id: "pty-2".to_owned(),
                state: TerminalSessionState::Lost,
                exit_code: None,
                detail: "lost".to_owned(),
            }),
        ];
        let mut bytes = encode_file_header(9).to_vec();
        bytes[4] = 40;
        for event in &events {
            bytes.extend(encode_record(event).unwrap());
        }
        fs::write(&path, bytes).unwrap();

        let migrated = EventDataBase::open(&path).unwrap();
        assert_eq!(
            migrated.events().iter().map(Event::id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5, 6]
        );
        assert_eq!(migrated.next_event_id(), 10);
        assert!(matches!(
            migrated.get(2),
            Some(Event::ApiStateUpdate(event))
                if event.api_call_id == 2 && event.prompt_id == 1
        ));
        assert!(matches!(
            migrated.get(3),
            Some(Event::ToolCall(event))
                if event.api_call_id == 2 && event.prompt_id == 1
        ));
        assert!(matches!(
            migrated.get(4),
            Some(Event::TerminalSessionCreated(event))
                if event.tool_call_id == 3 && event.session_id == "pty-3"
        ));
        assert!(matches!(
            migrated.get(5),
            Some(Event::ToolInfoUpdate(event))
                if event.tool_call_id == 3
                    && event.content.terminal().is_some_and(|update| update.session_id == "pty-3")
        ));
        assert!(matches!(
            migrated.get(6),
            Some(Event::TerminalSessionState(event)) if event.session_id == "pty-3"
        ));
        assert_eq!(fs::read(&path).unwrap()[4], 41);
        let migrated_id = migrated.edb_id().unwrap().to_owned();
        let migrated_bytes = fs::read(&path).unwrap();
        drop(migrated);

        let reopened = EventDataBase::open(&path).unwrap();
        assert_eq!(reopened.edb_id().unwrap(), migrated_id);
        assert_eq!(reopened.next_event_id(), 10);
        drop(reopened);
        assert_eq!(fs::read(&path).unwrap(), migrated_bytes);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn image_content_round_trips_as_self_contained_binary_and_validates_sha256() {
        let directory = temporary_path("image-content");
        let path = directory.join("main.edb");
        let bytes = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10, 1, 2, 3, 4];
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt_id = edb.append_user_prompt("view it").unwrap();
            let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            let call_id = edb
                .append_tool_call(
                    api_call_id,
                    prompt_id,
                    "provider-image",
                    crate::image_toolbox::VIEW_TOOL_NAME,
                    r#"{"url":"sample.png"}"#,
                )
                .unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
                .unwrap();
            let image_id = edb
                .append_image_content(
                    call_id,
                    "sample.png",
                    "image/png",
                    "png",
                    3,
                    2,
                    bytes.clone(),
                )
                .unwrap();
            edb.append_tool_result(
                call_id,
                ToolResultState::Succeeded,
                None,
                r#"{"image_event_id":4}"#,
            )
            .unwrap();
            let Event::ImageContent(image) = edb.get(image_id).unwrap() else {
                panic!("missing image event");
            };
            assert_eq!(image.data.as_ref(), bytes.as_slice());
            assert_eq!(image.content_sha256, image_content_sha256(&image.data));
            assert!(image.getDetailString().contains("bytes=12"));
            assert!(!image.getDetailString().contains("iVBOR"));
        }
        let reopened = EventDataBase::open(&path).unwrap();
        let image = reopened
            .events()
            .iter()
            .find_map(|event| match event {
                Event::ImageContent(image) => Some(image),
                _ => None,
            })
            .unwrap();
        assert_eq!(image.data.as_ref(), bytes.as_slice());
        assert_eq!(image.content_sha256, image_content_sha256(&bytes));

        let mut corrupted = Event::ImageContent(image.clone());
        let Event::ImageContent(corrupted_image) = &mut corrupted else {
            unreachable!()
        };
        corrupted_image.content_sha256 = "0".repeat(64);
        assert!(
            decode_event(&encode_event(&corrupted))
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_compressed_records_and_recovers_partial_tail() {
        let directory = temporary_path("persistence");
        let path = directory.join("main.edb");
        let long_content = "repeat ".repeat(1024);
        let valid_len;
        let response_id;
        let prompt_id;
        let expected_hash;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            assert!(edb.persisted_size_bytes() > FILE_HEADER_SIZE as u64);
            prompt_id = edb.append_user_prompt("hello").unwrap();
            response_id = edb
                .append_assist_response(prompt_id, &long_content, true)
                .unwrap();
            valid_len = path.metadata().unwrap().len();
            assert_eq!(edb.persisted_size_bytes(), valid_len);
            expected_hash = edb.get(response_id).unwrap().getHash();
            assert!(valid_len < long_content.len() as u64);
        }

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[1, 2, 3]).unwrap();
        drop(file);

        let edb = EventDataBase::open(&path).unwrap();
        assert_eq!(edb.len(), 3);
        assert_eq!(path.metadata().unwrap().len(), valid_len);
        assert_eq!(edb.persisted_size_bytes(), valid_len);
        assert_eq!(edb.get(response_id).unwrap().getHash(), expected_hash);
        assert!(matches!(
            edb.get(response_id),
            Some(Event::AssistResponse(event))
                if event.prompt_id == prompt_id
                    && event.content == long_content
                    && event.finished
                    && event.timestamp_ms > 0
        ));
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opening_repairs_invalid_json_tool_call_by_truncating_and_interrupting_its_api() {
        let directory = temporary_path("invalid-tool-json-recovery");
        let path = directory.join("main.edb");
        let (bad_tool, later_prompt, damaged_next_event_id, api_call_id, prompt_id);
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            edb.append_agent_kind_def(AgentKind::Interactive, "chatbot", None, None)
                .unwrap();
            edb.append_initial_model("test").unwrap();
            edb.append_initial_reasoning_effort("unset").unwrap();
            prompt_id = edb.append_user_prompt("edit it").unwrap();
            edb.append_agent_turn(prompt_id, prompt_id, AgentTurnState::Started, "")
                .unwrap();
            api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
                .unwrap();
            bad_tool = edb
                .append_tool_call(
                    api_call_id,
                    prompt_id,
                    "broken-edit",
                    "File.Edit",
                    r#"{"path":"renderer.js","edits":[{"operation":"replace""#,
                )
                .unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
                .unwrap();
            edb.append_agent_turn(
                prompt_id,
                prompt_id,
                AgentTurnState::Failed,
                "EOF while parsing a string",
            )
            .unwrap();
            later_prompt = edb.append_user_prompt("continue").unwrap();
            edb.append_agent_turn(later_prompt, later_prompt, AgentTurnState::Started, "")
                .unwrap();
            edb.append_api_requesting(later_prompt).unwrap();
            damaged_next_event_id = edb.next_event_id();
        }

        let repaired = EventDataBase::open(&path).unwrap();
        assert!(repaired.get(bad_tool).is_none());
        assert!(repaired.get(later_prompt).is_none());
        assert_eq!(repaired.next_event_id(), damaged_next_event_id + 1);
        assert!(matches!(
            repaired.events().last(),
            Some(Event::ApiStateUpdate(update))
                if update.id == damaged_next_event_id
                    && update.api_call_id == api_call_id
                    && update.prompt_id == prompt_id
                    && update.state == ApiState::Interrupted
                    && update.detail.contains("startup recovery")
        ));
        let repaired_events = repaired.events().to_vec();
        drop(repaired);

        let reopened = EventDataBase::open(&path).unwrap();
        assert_eq!(reopened.events(), repaired_events.as_slice());
        assert_eq!(reopened.next_event_id(), damaged_next_event_id + 1);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opening_refuses_to_guess_invalid_tool_recovery_without_an_active_api_prefix() {
        for (label, terminal_before_tool, expected) in [
            ("missing-api", false, "API call 999 is missing"),
            ("completed-api", true, "already ended before the tool call"),
        ] {
            let directory = temporary_path(label);
            let path = directory.join("main.edb");
            {
                let mut edb = EventDataBase::open(&path).unwrap();
                let prompt = edb.append_user_prompt("bad history").unwrap();
                let api_call_id = if terminal_before_tool {
                    let api = edb.append_api_requesting(prompt).unwrap();
                    edb.append_api_state(api, prompt, ApiState::Completed, "")
                        .unwrap();
                    api
                } else {
                    999
                };
                edb.append_tool_call(
                    api_call_id,
                    prompt,
                    "broken",
                    "File.Edit",
                    r#"{"path":"file","edits":["#,
                )
                .unwrap();
            }
            let original = fs::read(&path).unwrap();

            let error = EventDataBase::open(&path)
                .err()
                .expect("unconfirmed recovery must fail");
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(fs::read(&path).unwrap(), original);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn opening_v29_atomically_inserts_edb_id_and_preserves_events() {
        let directory = temporary_path("migrate-v29");
        let path = directory.join("main.edb");
        let expected_next;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt = edb.append_user_prompt("preserve me").unwrap();
            edb.append_assist_response(prompt, "preserved", true)
                .unwrap();
            expected_next = edb.next_event_id() + 1;
        }
        rewrite_as_current_record_version(&path, 29);

        let migrated = EventDataBase::open(&path).unwrap();
        assert_eq!(migrated.next_event_id(), expected_next);
        assert_eq!(
            migrated.events().first().unwrap().kind(),
            EventKind::EdbIdGeneration
        );
        let prompt = migrated
            .events()
            .iter()
            .find_map(|event| match event {
                Event::UserPrompt(prompt) if prompt.content == "preserve me" => Some(prompt.id),
                _ => None,
            })
            .unwrap();
        assert!(migrated.events().iter().any(|event| {
            matches!(event, Event::AssistResponse(response) if response.prompt_id == prompt && response.content == "preserved")
        }));
        assert_eq!(&fs::read(&path).unwrap()[..FILE_MAGIC.len()], FILE_MAGIC);
        drop(migrated);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opening_v30_atomically_inserts_edb_id_and_preserves_events() {
        let directory = temporary_path("migrate-v30");
        let path = directory.join("main.edb");
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            edb.append_user_prompt("preserve v30").unwrap();
        }
        rewrite_as_current_record_version(&path, 30);

        let migrated = EventDataBase::open(&path).unwrap();
        assert_eq!(
            migrated.events().first().unwrap().kind(),
            EventKind::EdbIdGeneration
        );
        assert!(migrated.events().iter().any(|event| {
            matches!(event, Event::UserPrompt(prompt) if prompt.content == "preserve v30")
        }));
        assert_eq!(&fs::read(&path).unwrap()[..FILE_MAGIC.len()], FILE_MAGIC);
        drop(migrated);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opening_v31_atomically_inserts_edb_id_and_preserves_events() {
        let directory = temporary_path("migrate-v31");
        let path = directory.join("main.edb");
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            edb.append_user_prompt("preserve v31").unwrap();
        }
        rewrite_as_current_record_version(&path, 31);

        let migrated = EventDataBase::open(&path).unwrap();
        assert_eq!(
            migrated.events().first().unwrap().kind(),
            EventKind::EdbIdGeneration
        );
        assert!(migrated.events().iter().any(|event| {
            matches!(event, Event::UserPrompt(prompt) if prompt.content == "preserve v31")
        }));
        assert_eq!(&fs::read(&path).unwrap()[..FILE_MAGIC.len()], FILE_MAGIC);
        drop(migrated);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn v32_migration_assigns_internal_prompt_sources_from_agent_identity() {
        for (name, system_prompt, expected_kind) in [
            ("worker", "worker", EventKind::ManagerPrompt),
            ("sub-agent", "parent-agent", EventKind::ParentAgentPrompt),
        ] {
            let directory = temporary_path(&format!("migrate-v32-{name}"));
            let path = directory.join("agent.edb");
            {
                let mut edb = EventDataBase::open(&path).unwrap();
                edb.append_agent_kind_def(
                    AgentKind::SubAgent,
                    if name == "worker" {
                        "worker-agent"
                    } else {
                        "main-agent"
                    },
                    Some("main".into()),
                    None,
                )
                .unwrap();
                edb.append_system_prompt(system_prompt).unwrap();
                edb.append_user_prompt("internal instruction").unwrap();
            }
            rewrite_as_legacy_agent_definition(&path, 32);

            let migrated = EventDataBase::open(&path).unwrap();
            let migrated_prompt = migrated.events().iter().find(|event| {
                event.kind() == expected_kind
                    && event.root_prompt_content() == Some("internal instruction")
            });
            assert!(migrated_prompt.is_some());
            assert_eq!(&fs::read(&path).unwrap()[..FILE_MAGIC.len()], FILE_MAGIC);
            drop(migrated);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn v34_migration_persists_the_original_orchestrator_per_agent() {
        for (name, kind, orchestrator, system_prompt) in [
            ("chatbot", AgentKind::Interactive, "chatbot", None),
            ("main", AgentKind::Interactive, "main-agent", Some("base")),
            (
                "manager",
                AgentKind::Interactive,
                "manager-agent",
                Some("manager"),
            ),
            (
                "worker",
                AgentKind::SubAgent,
                "worker-agent",
                Some("worker"),
            ),
        ] {
            let directory = temporary_path(&format!("migrate-v34-{name}"));
            let path = directory.join("agent.edb");
            let expected_kind;
            {
                let mut edb = EventDataBase::open(&path).unwrap();
                edb.append_agent_kind_def(
                    kind,
                    orchestrator,
                    (kind == AgentKind::SubAgent).then(|| "manager".into()),
                    None,
                )
                .unwrap();
                if let Some(system_prompt) = system_prompt {
                    edb.append_system_prompt(system_prompt).unwrap();
                }
                expected_kind = kind;
            }
            rewrite_as_legacy_agent_definition(&path, 34);

            let migrated = EventDataBase::open(&path).unwrap();
            let definition = agent_kind_definition(migrated.events()).unwrap();
            assert_eq!(definition.kind, expected_kind);
            assert_eq!(definition.orchestrator, orchestrator);
            assert_eq!(&fs::read(&path).unwrap()[..FILE_MAGIC.len()], FILE_MAGIC);
            drop(migrated);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn failed_or_unsupported_migration_preserves_the_source_file() {
        let directory = temporary_path("migration-preserves-source");
        let path = directory.join("main.edb");
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            edb.append_user_prompt("must survive").unwrap();
        }

        rewrite_as_current_record_version(&path, 29);
        let mut corrupt_v29 = fs::read(&path).unwrap();
        *corrupt_v29.last_mut().unwrap() ^= 0xff;
        fs::write(&path, &corrupt_v29).unwrap();
        assert!(EventDataBase::open(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt_v29);

        let mut unsupported_v28 = corrupt_v29;
        unsupported_v28[4] = 28;
        fs::write(&path, &unsupported_v28).unwrap();
        assert!(EventDataBase::open(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), unsupported_v28);

        let incomplete = b"MEDB";
        fs::write(&path, incomplete).unwrap();
        assert!(EventDataBase::open(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), incomplete);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn v29_migration_recovers_an_uncommitted_partial_tail() {
        let directory = temporary_path("migration-partial-tail");
        let path = directory.join("main.edb");
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            edb.append_user_prompt("committed").unwrap();
        }
        rewrite_as_current_record_version(&path, 29);
        let mut v29 = fs::read(&path).unwrap();
        v29.extend_from_slice(&[1, 2, 3]);
        fs::write(&path, &v29).unwrap();

        let migrated = EventDataBase::open(&path).unwrap();
        assert_eq!(migrated.len(), 2);
        assert_eq!(
            path.metadata().unwrap().len(),
            migrated.persisted_size_bytes()
        );
        assert!(migrated.events().iter().any(|event| {
            matches!(event, Event::UserPrompt(prompt) if prompt.content == "committed")
        }));
        assert_eq!(&fs::read(&path).unwrap()[..FILE_MAGIC.len()], FILE_MAGIC);
        drop(migrated);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_api_state_updates() {
        let directory = temporary_path("api-state");
        let path = directory.join("main.edb");
        let error_id;
        let retry_id;
        let prompt_id;
        let api_call_id;
        let expected_hashes;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            prompt_id = edb.append_user_prompt("hello").unwrap();
            api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
                .unwrap();
            error_id = edb
                .append_api_state_with_usage(
                    api_call_id,
                    prompt_id,
                    ApiState::Error,
                    Some(ApiUsage {
                        input_tokens: 8,
                        output_tokens: 2,
                        total_tokens: 10,
                    }),
                    "connection lost",
                )
                .unwrap();
            retry_id = edb
                .append_api_retrying(api_call_id, prompt_id, 1, 10, "connection lost")
                .unwrap();
            expected_hashes = [
                edb.get(error_id).unwrap().getHash(),
                edb.get(retry_id).unwrap().getHash(),
            ];
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert_eq!(edb.len(), 6);
        assert_eq!(edb.get(error_id).unwrap().getHash(), expected_hashes[0]);
        assert_eq!(edb.get(retry_id).unwrap().getHash(), expected_hashes[1]);
        assert!(matches!(
            edb.get(error_id),
            Some(Event::ApiStateUpdate(event))
                if event.api_call_id == api_call_id
                    && event.prompt_id == prompt_id
                    && event.state == ApiState::Error
                    && event.usage == Some(ApiUsage {
                        input_tokens: 8,
                        output_tokens: 2,
                        total_tokens: 10,
                    })
                    && event.detail == "connection lost"
                    && event.timestamp_ms > 0
        ));
        assert!(
            edb.get(error_id)
                .unwrap()
                .getDetailString()
                .contains("total_tokens=10")
        );
        assert!(matches!(
            edb.get(retry_id),
            Some(Event::ApiStateUpdate(event))
                if event.api_call_id == api_call_id
                    && event.state == ApiState::Retrying
                    && event.retry_count == 1
                    && event.retry_limit == 10
        ));
        assert!(
            edb.get(retry_id)
                .unwrap()
                .getBriefString()
                .contains("retry=1/10")
        );
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn context_usage_estimate_is_validated_and_round_trips() {
        let directory = temporary_path("context-usage-estimate");
        let path = directory.join("main.edb");
        let estimate_id;
        let expected = ContextTokenUsage {
            system: 30,
            compact: 10,
            memory: 5,
            user: 20,
            model: 25,
            tool: 10,
        };
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt = edb.append_user_prompt("hello").unwrap();
            let api = edb.append_api_requesting(prompt).unwrap();
            let completed = edb
                .append_api_state_with_usage(
                    api,
                    prompt,
                    ApiState::Completed,
                    Some(ApiUsage {
                        input_tokens: 80,
                        output_tokens: 20,
                        total_tokens: 100,
                    }),
                    "",
                )
                .unwrap();
            assert!(
                edb.append_context_usage_estimate(
                    completed,
                    ContextTokenUsage {
                        system: 99,
                        ..ContextTokenUsage::default()
                    },
                )
                .unwrap_err()
                .to_string()
                .contains("does not match API usage")
            );
            estimate_id = edb
                .append_context_usage_estimate(completed, expected)
                .unwrap();
            assert!(
                edb.append_context_usage_estimate(completed, expected)
                    .is_err()
            );
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            edb.get(estimate_id),
            Some(Event::ContextUsageEstimate(event))
                if event.api_state_event_id + 1 == estimate_id && event.values == expected
        ));
        assert!(
            edb.get(estimate_id)
                .unwrap()
                .getDetailString()
                .contains("system=30")
        );
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn api_usage_is_rejected_on_non_terminal_states_and_affects_hash() {
        let usage = ApiUsage {
            input_tokens: 8,
            output_tokens: 2,
            total_tokens: 10,
        };
        let mut edb = EventDataBase::new();
        let prompt_id = edb.append_user_prompt("hello").unwrap();
        let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
        assert!(
            edb.append_api_state_with_usage(
                api_call_id,
                prompt_id,
                ApiState::Streaming,
                Some(usage),
                "",
            )
            .is_err()
        );
        let without_usage = edb
            .append_api_state(api_call_id, prompt_id, ApiState::Error, "")
            .unwrap();

        let plain = ApiStateUpdateEvent {
            id: without_usage,
            timestamp_ms: edb.get(without_usage).unwrap().timestamp_ms(),
            api_call_id,
            prompt_id,
            state: ApiState::Error,
            retry_count: 0,
            retry_limit: 0,
            usage: None,
            detail: String::new(),
        };
        let with_usage = ApiStateUpdateEvent {
            usage: Some(usage),
            ..plain.clone()
        };
        assert_ne!(plain.getHash(), with_usage.getHash());
    }

    #[test]
    fn persists_user_turn_abort_with_stable_relation_and_descriptions() {
        let directory = temporary_path("turn-abort");
        let path = directory.join("main.edb");
        let prompt_id;
        let abort_id;
        let expected_hash;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            prompt_id = edb.append_user_prompt("stop this turn").unwrap();
            abort_id = edb.append_user_turn_aborted(prompt_id).unwrap();
            expected_hash = edb.get(abort_id).unwrap().getHash();
            assert!(
                edb.get(abort_id)
                    .unwrap()
                    .getBriefString()
                    .contains(&format!("prompt_id={prompt_id}"))
            );
            assert!(
                edb.get(abort_id)
                    .unwrap()
                    .getDetailString()
                    .contains("timestamp_ms=")
            );
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            edb.get(abort_id),
            Some(Event::UserTurnAborted(event))
                if event.prompt_id == prompt_id && event.timestamp_ms > 0
        ));
        assert_eq!(edb.get(abort_id).unwrap().getHash(), expected_hash);
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_prompt_and_tool_events() {
        let directory = temporary_path("tool-events");
        let path = directory.join("main.edb");
        let system_id;
        let tool_call_id;
        let created_id;
        let info_id;
        let result_id;
        let state_id;
        let session_id;
        let hashes;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            system_id = edb.append_system_prompt("tool").unwrap();
            let prompt_id = edb.append_user_prompt("run").unwrap();
            let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
                .unwrap();
            edb.append_assist_response(prompt_id, "", true).unwrap();
            tool_call_id = edb
                .append_tool_call(
                    api_call_id,
                    prompt_id,
                    "provider-1",
                    "Terminal.Create",
                    "{}",
                )
                .unwrap();
            session_id = format!("pty-{tool_call_id}");
            edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
                .unwrap();
            created_id = edb
                .append_terminal_session_created(
                    tool_call_id,
                    session_id.clone(),
                    "/bin/zsh",
                    "/workspace",
                    120,
                    40,
                )
                .unwrap();
            info_id = edb
                .append_tool_info(tool_call_id, ToolOutputStream::Stdout, "ready")
                .unwrap();
            result_id = edb
                .append_tool_result(
                    tool_call_id,
                    ToolResultState::Succeeded,
                    None,
                    format!(r#"{{"session_id":"{session_id}","state":"running"}}"#),
                )
                .unwrap();
            state_id = edb
                .append_terminal_session_state(
                    session_id.clone(),
                    TerminalSessionState::Lost,
                    None,
                    "me exited",
                )
                .unwrap();
            hashes = edb
                .events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>();
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert_eq!(edb.len(), 12);
        assert_eq!(
            hashes,
            edb.events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            edb.get(system_id),
            Some(Event::SystemPrompt(event)) if event.name == "tool"
        ));
        assert!(matches!(
            edb.get(created_id),
            Some(Event::TerminalSessionCreated(event))
                if event.tool_call_id == tool_call_id
                    && event.session_id == session_id
                    && event.shell == "/bin/zsh"
        ));
        assert!(matches!(
            edb.get(info_id),
            Some(Event::ToolInfoUpdate(event))
                if event.tool_call_id == tool_call_id
                    && event.stream == ToolOutputStream::Stdout
                    && event.content.text() == Some("ready")
        ));
        assert!(matches!(
            edb.get(result_id),
            Some(Event::ToolCallResult(event))
                if event.tool_call_id == tool_call_id
                    && event.state == ToolResultState::Succeeded
                    && event.exit_code.is_none()
        ));
        assert!(matches!(
            edb.get(state_id),
            Some(Event::TerminalSessionState(event))
                if event.session_id == session_id
                    && event.state == TerminalSessionState::Lost
        ));
        assert!(
            edb.get(tool_call_id)
                .unwrap()
                .getBriefString()
                .contains("Terminal.Create")
        );
        assert!(
            edb.get(state_id)
                .unwrap()
                .getDetailString()
                .contains("me exited")
        );
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_structured_terminal_updates_with_styles_and_cursor() {
        let directory = temporary_path("styled-terminal-update");
        let path = directory.join("main.edb");
        let expected_hash;
        let terminal_info_id;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt_id = edb.append_user_prompt("open terminal").unwrap();
            let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
                .unwrap();
            let tool_call_id = edb
                .append_tool_call(
                    api_call_id,
                    prompt_id,
                    "provider-1",
                    "Terminal.Interact",
                    "{}",
                )
                .unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
                .unwrap();
            let mut update = crate::terminal::test_update("command");
            update.style_count = 2;
            update
                .style_defs
                .push(crate::terminal::TerminalStyleDefinition {
                    id: 1,
                    style: crate::terminal::TerminalStyle {
                        inverse: true,
                        ..crate::terminal::TerminalStyle::default()
                    },
                });
            update.rows[0].runs[0].style = 1;
            update.cursor.col = 0;
            update.cursor.underlying = "c".into();
            let event_id = edb
                .append_terminal_update(tool_call_id, update.clone())
                .unwrap();
            terminal_info_id = event_id;
            expected_hash = edb.get(event_id).unwrap().getHash();
            assert!(
                edb.get(event_id)
                    .unwrap()
                    .getDetailString()
                    .contains("inverse")
            );
        }

        let edb = EventDataBase::open(&path).unwrap();
        let Event::ToolInfoUpdate(info) = edb.get(terminal_info_id).unwrap() else {
            panic!("expected terminal tool info");
        };
        let update = info.content.terminal().unwrap();
        assert_eq!(update.plain_text(), "000000: command");
        let model = update.model_value();
        assert_eq!(
            model["styles"],
            serde_json::json!([{"id": 1, "attributes": ["inverse"]}])
        );
        assert_eq!(model["rows"][0]["terminal_row"], 0);
        assert_eq!(model["rows"][0]["text"], "command");
        assert_eq!(
            model["rows"][0]["style_spans"],
            serde_json::json!([{"start_column": 0, "width": 7, "style": 1}])
        );
        assert_eq!(model["cursor"]["terminal_row"], 0);
        assert_eq!(model["cursor"]["column"], 0);
        assert_eq!(model["cursor"]["underlying"], "c");
        assert!(model.get("base_event_id").is_none());
        assert_eq!(edb.get(terminal_info_id).unwrap().getHash(), expected_hash);
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_interrupted_tool_result_with_empty_detail() {
        let directory = temporary_path("interrupted-tool-result");
        let path = directory.join("main.edb");
        let result_id;
        let expected_hash;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt_id = edb.append_user_prompt("run tool").unwrap();
            let api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Streaming, "")
                .unwrap();
            edb.append_assist_response(prompt_id, "", true).unwrap();
            let tool_call_id = edb
                .append_tool_call(api_call_id, prompt_id, "call-1", "Probe.Wait", "{}")
                .unwrap();
            edb.append_api_state(api_call_id, prompt_id, ApiState::Completed, "")
                .unwrap();
            result_id = edb
                .append_tool_result(tool_call_id, ToolResultState::Interrupted, None, "")
                .unwrap();
            expected_hash = edb.get(result_id).unwrap().getHash();
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            edb.get(result_id),
            Some(Event::ToolCallResult(event))
                if event.state == ToolResultState::Interrupted
                    && event.exit_code.is_none()
                    && event.detail.is_empty()
        ));
        assert_eq!(edb.get(result_id).unwrap().getHash(), expected_hash);
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_follow_up_prompt_with_stable_metadata() {
        let directory =
            std::env::temp_dir().join(format!("me-edb-follow-up-{}", std::process::id()));
        let path = directory.join("main.edb");
        let expected_hash;
        let prompt_id;
        let follow_up_id;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            prompt_id = edb.append_user_prompt("start").unwrap();
            follow_up_id = edb
                .append_follow_up_prompt(prompt_id, "also check <xml>")
                .unwrap();
            let event = edb.get(follow_up_id).unwrap();
            expected_hash = event.getHash();
            assert!(event.getBriefString().contains("FollowUpPromptEvent"));
            assert!(event.getDetailString().contains("also check <xml>"));
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            edb.get(follow_up_id),
            Some(Event::FollowUpPrompt(event))
                if event.prompt_id == prompt_id && event.content == "also check <xml>"
        ));
        assert_eq!(edb.get(follow_up_id).unwrap().getHash(), expected_hash);
        drop(edb);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_provider_model_context_items_as_opaque_json() {
        let directory = temporary_path("model-context-item");
        let path = directory.join("main.edb");
        let content = r#"{"type":"reasoning","encrypted_content":"opaque","summary":[]}"#;
        let expected_hash;
        let prompt_id;
        let api_call_id;
        let item_id;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            prompt_id = edb.append_user_prompt("hello").unwrap();
            api_call_id = edb.append_api_requesting(prompt_id).unwrap();
            item_id = edb
                .append_model_context_item(api_call_id, prompt_id, "codex-oauth", content)
                .unwrap();
            expected_hash = edb.get(item_id).unwrap().getHash();
            assert!(
                edb.get(item_id)
                    .unwrap()
                    .getBriefString()
                    .contains("reasoning")
            );
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            edb.get(item_id),
            Some(Event::ModelContextItem(event))
                if event.api_call_id == api_call_id
                    && event.prompt_id == prompt_id
                    && event.provider == "codex-oauth"
                    && event.content == content
        ));
        assert_eq!(edb.get(item_id).unwrap().getHash(), expected_hash);
        assert!(
            edb.get(item_id)
                .unwrap()
                .getDetailString()
                .contains("encrypted_content")
        );
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_context_control_events_with_stable_hashes() {
        let directory = temporary_path("context-controls");
        let path = directory.join("main.edb");
        let hashes;
        let discarded;
        let discarded_response;
        let effort;
        let clear;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            discarded = edb.append_user_prompt("discard me").unwrap();
            discarded_response = edb
                .append_assist_response(discarded, "old answer", true)
                .unwrap();
            effort = edb.append_reasoning_effort_changed("high").unwrap();
            clear = edb.append_context_cleared().unwrap();
            let target = edb.append_user_prompt("edit me").unwrap();
            edb.append_assist_response(target, "draft answer", true)
                .unwrap();
            edb.rewind_to_event(target).unwrap();
            hashes = edb
                .events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>();
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert_eq!(edb.len(), 5);
        assert_eq!(
            hashes,
            edb.events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            edb.get(effort),
            Some(Event::ReasoningEffortChanged(event)) if event.effort == "high"
        ));
        assert!(matches!(edb.get(clear), Some(Event::ContextCleared(_))));
        assert_eq!(
            edb.events().iter().map(Event::id).collect::<Vec<_>>(),
            vec![0, discarded, discarded_response, effort, clear]
        );
        for event in edb.events().iter().skip(3) {
            assert!(!event.getBriefString().is_empty());
            assert!(event.getDetailString().contains("timestamp_ms="));
        }
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_initial_and_changed_model_state_with_causes() {
        let directory = temporary_path("model-state");
        let path = directory.join("main.edb");
        let hashes;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            edb.append_initial_model("first").unwrap();
            edb.append_initial_reasoning_effort(crate::config::UNSET_EFFORT)
                .unwrap();
            edb.append_model_changed("second").unwrap();
            edb.append_reasoning_effort_fallback().unwrap();
            hashes = edb
                .events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>();
        }

        let edb = EventDataBase::open(&path).unwrap();
        assert_eq!(
            hashes,
            edb.events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            edb.get(1),
            Some(Event::ModelChanged(event))
                if event.model == "first" && event.cause == ModelChangeCause::Initial
        ));
        assert!(matches!(
            edb.get(2),
            Some(Event::ReasoningEffortChanged(event))
                if event.effort == crate::config::UNSET_EFFORT
                    && event.cause == ReasoningEffortChangeCause::Initial
        ));
        assert!(matches!(
            edb.get(3),
            Some(Event::ModelChanged(event))
                if event.model == "second" && event.cause == ModelChangeCause::User
        ));
        assert!(matches!(
            edb.get(4),
            Some(Event::ReasoningEffortChanged(event))
                if event.effort == crate::config::UNSET_EFFORT
                    && event.cause == ReasoningEffortChangeCause::ModelUnsupported
        ));
        for event in edb.events().iter().skip(1) {
            assert!(!event.getBriefString().is_empty());
            assert!(event.getDetailString().contains("cause="));
        }
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn effective_projection_supports_nested_rewind_and_clear() {
        let mut edb = EventDataBase::new();
        edb.append_system_prompt("base").unwrap();
        let first = edb.append_user_prompt("first").unwrap();
        edb.append_assist_response(first, "first answer", true)
            .unwrap();
        let second = edb.append_user_prompt("second").unwrap();
        edb.append_assist_response(second, "second answer", true)
            .unwrap();

        edb.rewind_to_event(second).unwrap();
        let branch = edb.append_user_prompt("branch").unwrap();
        edb.append_assist_response(branch, "branch answer", true)
            .unwrap();
        let active = effective_conversation_events(edb.events()).unwrap();
        assert_eq!(
            active.iter().map(|event| event.id()).collect::<Vec<_>>(),
            vec![first, first + 1, branch, branch + 1]
        );

        edb.rewind_to_event(first).unwrap();
        assert!(
            effective_conversation_events(edb.events())
                .unwrap()
                .is_empty()
        );
        assert!(edb.rewind_to_event(second).is_err());

        let after_rewind = edb.append_user_prompt("after rewind").unwrap();
        edb.append_reasoning_effort_changed("max").unwrap();
        edb.append_context_cleared().unwrap();
        assert!(
            effective_conversation_events(edb.events())
                .unwrap()
                .is_empty()
        );
        edb.rewind_to_event(after_rewind).unwrap();
        assert!(
            effective_conversation_events(edb.events())
                .unwrap()
                .is_empty()
        );
        assert!(
            edb.events()
                .iter()
                .all(|event| !matches!(event, Event::ContextCleared(_)))
        );

        let after_clear = edb.append_user_prompt("after clear").unwrap();
        let active = effective_conversation_events(edb.events()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id(), after_clear);
    }

    #[test]
    fn rewind_rewrites_order_but_never_reuses_event_ids_after_reopen() {
        let directory = temporary_path("rewind-event-id");
        let path = directory.join("main.edb");
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let first = edb.append_user_prompt("keep").unwrap();
            edb.append_assist_response(first, "kept answer", true)
                .unwrap();
            let target = edb.append_user_prompt("restore me").unwrap();
            edb.append_assist_response(target, "discarded answer", true)
                .unwrap();
            edb.append_reasoning_effort_changed("high").unwrap();

            assert_eq!(edb.next_event_id(), 6);
            assert_eq!(edb.mutation_revision(), 0);
            let mutation = edb.rewind_to_event(target).unwrap();
            assert_eq!(
                mutation,
                EdbMutation::Rewind {
                    target_event_id: target,
                    restored_prompt_content: Some("restore me".into()),
                }
            );
            assert_eq!(edb.mutation_revision(), 1);
            assert_eq!(
                edb.events().iter().map(Event::id).collect::<Vec<_>>(),
                vec![0, first, first + 1]
            );
            assert_eq!(edb.next_event_id(), 6);
            assert!(edb.get(target).is_none());
            assert!(edb.get(4).is_none());
            assert!(edb.get(5).is_none());
        }

        let mut edb = EventDataBase::open(&path).unwrap();
        assert_eq!(edb.next_event_id(), 6);
        assert_eq!(
            edb.events().iter().map(Event::id).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let replacement = edb.append_user_prompt("replacement").unwrap();
        assert_eq!(replacement, 6);
        assert_eq!(
            edb.events().iter().map(Event::id).collect::<Vec<_>>(),
            vec![0, 1, 2, replacement]
        );
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn clear_is_append_only_and_can_be_rewound_directly() {
        let mut edb = EventDataBase::new();
        let before_clear = edb.append_user_prompt("before clear").unwrap();
        let answer = edb
            .append_assist_response(before_clear, "answer", true)
            .unwrap();
        let clear_id = edb.append_context_cleared().unwrap();
        let after_clear = edb.append_user_prompt("after clear").unwrap();

        assert_eq!(edb.mutation_revision(), 0);
        assert!(matches!(edb.get(clear_id), Some(Event::ContextCleared(_))));
        assert_eq!(
            effective_conversation_events(edb.events())
                .unwrap()
                .iter()
                .map(|event| event.id())
                .collect::<Vec<_>>(),
            vec![after_clear]
        );
        assert!(edb.rewind_to_event(answer).is_err());
        assert_eq!(edb.mutation_revision(), 0);

        let mutation = edb.rewind_to_event(clear_id).unwrap();
        assert_eq!(edb.mutation_revision(), 1);
        assert_eq!(
            mutation,
            EdbMutation::Rewind {
                target_event_id: clear_id,
                restored_prompt_content: None,
            }
        );
        assert!(edb.get(before_clear).is_some());
        assert!(edb.get(answer).is_some());
        assert!(edb.get(clear_id).is_none());
        assert!(edb.get(after_clear).is_none());
        assert_eq!(
            edb.events().iter().map(Event::id).collect::<Vec<_>>(),
            vec![0, before_clear, answer]
        );
        assert_eq!(edb.next_event_id(), 5);
        assert_eq!(
            effective_conversation_events(edb.events())
                .unwrap()
                .iter()
                .map(|event| event.id())
                .collect::<Vec<_>>(),
            vec![before_clear, answer]
        );
    }

    #[test]
    fn completed_compact_is_a_rewindable_context_boundary() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("before compact").unwrap();
        let trigger_api = edb.append_api_requesting(prompt).unwrap();
        let tool = edb
            .append_tool_call(trigger_api, prompt, "provider-compact", "Compact", "{}")
            .unwrap();
        edb.append_api_state(trigger_api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_tool_result(
            tool,
            ToolResultState::Succeeded,
            None,
            r#"{"status":"accepted"}"#,
        )
        .unwrap();
        let compact = edb
            .append_compact_started(tool, prompt, CompactKind::WorkerSingleTurn)
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

        let effective = effective_conversation_events(edb.events()).unwrap();
        assert_eq!(effective.len(), 1);
        assert!(
            matches!(effective[0], Event::CompactStateUpdate(update) if update.id == completed)
        );
        assert!(
            effective_ui_events(edb.events())
                .unwrap()
                .iter()
                .any(|event| event.id() == prompt)
        );
        assert_eq!(completed_compact_count(edb.events()), 1);

        let mutation = edb.rewind_to_event(completed).unwrap();
        assert_eq!(
            mutation,
            EdbMutation::Rewind {
                target_event_id: completed,
                restored_prompt_content: None,
            }
        );
        assert_eq!(edb.events().len(), 2);
        assert_eq!(completed_compact_count(edb.events()), 0);
        assert!(matches!(edb.get(prompt), Some(Event::UserPrompt(_))));
        assert!(edb.get(trigger_api).is_none());
    }

    #[test]
    fn failed_compact_keeps_the_existing_context() {
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("keep me").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let tool = edb
            .append_tool_call(api, prompt, "provider-compact", "Compact", "{}")
            .unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
            .unwrap();
        let compact = edb
            .append_compact_started(tool, prompt, CompactKind::WorkerSingleTurn)
            .unwrap();
        edb.append_compact_terminal(compact, CompactState::Failed, "", "network error")
            .unwrap();
        assert_eq!(completed_compact_count(edb.events()), 0);

        let effective = effective_conversation_events(edb.events()).unwrap();
        assert!(effective.iter().any(|event| event.id() == prompt));
        assert!(edb.rewind_to_event(compact).is_err());
    }

    #[test]
    fn compact_lifecycle_round_trips_through_disk_storage() {
        let directory = temporary_path("compact-round-trip");
        let path = directory.join("main.edb");
        let expected;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt = edb.append_user_prompt("persist compact").unwrap();
            let trigger_api = edb.append_api_requesting(prompt).unwrap();
            let tool = edb
                .append_tool_call(
                    trigger_api,
                    prompt,
                    "compact-call",
                    crate::compact::TOOL_NAME,
                    "{}",
                )
                .unwrap();
            edb.append_api_state(trigger_api, prompt, ApiState::Completed, "")
                .unwrap();
            edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
                .unwrap();
            let compact = edb
                .append_compact_started(tool, prompt, CompactKind::WorkerSingleTurn)
                .unwrap();
            let summary_api = edb.append_api_requesting(prompt).unwrap();
            edb.append_api_state(summary_api, prompt, ApiState::Streaming, "")
                .unwrap();
            edb.append_api_state(summary_api, prompt, ApiState::Completed, "")
                .unwrap();
            edb.append_compact_terminal(
                compact,
                CompactState::Completed,
                "Summary:\n持久化内容",
                "",
            )
            .unwrap();
            expected = edb.events().to_vec();
        }

        let reopened = EventDataBase::open(&path).unwrap();
        assert_eq!(reopened.events(), expected);
        assert_eq!(
            reopened
                .events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>(),
            expected.iter().map(EventBase::getHash).collect::<Vec<_>>()
        );
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn multi_turn_compact_stages_are_ordered_persisted_and_rewind_as_one_lifecycle() {
        let directory = temporary_path("segmented-compact-round-trip");
        let path = directory.join("main.edb");
        let completed;
        let trigger_api;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt = edb.append_user_prompt("persist segmented compact").unwrap();
            trigger_api = edb.append_api_requesting(prompt).unwrap();
            let tool = edb
                .append_tool_call(
                    trigger_api,
                    prompt,
                    "compact-call",
                    crate::compact::TOOL_NAME,
                    "{}",
                )
                .unwrap();
            edb.append_api_state(trigger_api, prompt, ApiState::Completed, "")
                .unwrap();
            edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
                .unwrap();
            let compact = edb
                .append_compact_started(tool, prompt, CompactKind::MainAgentMultiTurn)
                .unwrap();
            let contents = [
                "analysis",
                "1. Primary Request and Intent\nintent",
                "2. Key Technical Context and Decisions\ndecisions",
                "3. Files, Code, and Artifacts\nfiles",
                "4. Problems, Investigations, and Resolutions\nproblems",
                "5. Current State and Continuation Plan\nnext",
            ];
            for (stage, content) in CompactStage::MULTI_TURN.into_iter().zip(contents) {
                let api = edb.append_api_requesting(prompt).unwrap();
                edb.append_api_state(api, prompt, ApiState::Streaming, "")
                    .unwrap();
                edb.append_api_state(api, prompt, ApiState::Completed, "")
                    .unwrap();
                edb.append_compact_stage(compact, stage, content).unwrap();
            }
            let summary = crate::compact::merge_multi_turn_summary(contents.into_iter().skip(1));
            completed = edb
                .append_compact_terminal(compact, CompactState::Completed, summary, "")
                .unwrap();
        }

        let mut reopened = EventDataBase::open(&path).unwrap();
        let stages = reopened
            .events()
            .iter()
            .filter_map(|event| match event {
                Event::CompactStateUpdate(update)
                    if update.state == CompactState::StageCompleted =>
                {
                    Some((update.kind, update.stage, update.content.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(stages.len(), CompactStage::MULTI_TURN.len());
        assert!(
            stages
                .iter()
                .all(|(kind, _, _)| *kind == CompactKind::MainAgentMultiTurn)
        );
        assert_eq!(
            stages
                .iter()
                .filter_map(|(_, stage, _)| *stage)
                .collect::<Vec<_>>(),
            CompactStage::MULTI_TURN
        );
        reopened.rewind_to_event(completed).unwrap();
        assert!(reopened.get(trigger_api).is_none());
        assert!(
            reopened
                .events()
                .iter()
                .all(|event| !matches!(event, Event::CompactStateUpdate(_)))
        );
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn multi_turn_compact_with_active_sessions_requires_and_merges_the_seventh_stage() {
        let directory = temporary_path("active-session-compact-round-trip");
        let path = directory.join("main.edb");
        let mut edb = EventDataBase::open(&path).unwrap();
        let prompt = edb.append_user_prompt("compact live sessions").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let tool = edb
            .append_tool_call(api, prompt, "compact-live", crate::compact::TOOL_NAME, "{}")
            .unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
            .unwrap();
        let compact = edb
            .append_compact_started_with_stage_count(tool, prompt, CompactKind::ManagerMultiTurn, 7)
            .unwrap();
        let contents = [
            "analysis",
            "1. Primary Request and Intent\nintent",
            "2. Key Technical Context and Decisions\ndecisions",
            "3. Files, Code, and Artifacts\nfiles",
            "4. Problems, Investigations, and Resolutions\nproblems",
            "5. Current State and Continuation Plan\nnext",
            "6. Active Tool Sessions\nTerminal pty-9 is running the build.",
        ];
        for (stage, content) in CompactStage::MULTI_TURN_WITH_ACTIVE_SESSIONS
            .into_iter()
            .zip(contents)
        {
            let stage_api = edb.append_api_requesting(prompt).unwrap();
            edb.append_api_state(stage_api, prompt, ApiState::Streaming, "")
                .unwrap();
            edb.append_api_state(stage_api, prompt, ApiState::Completed, "")
                .unwrap();
            edb.append_compact_stage(compact, stage, content).unwrap();
        }
        let summary = crate::compact::merge_multi_turn_summary(contents.into_iter().skip(1));
        edb.append_compact_terminal(compact, CompactState::Completed, summary.clone(), "")
            .unwrap();
        drop(edb);

        let edb = EventDataBase::open(&path).unwrap();

        assert!(summary.contains("6. Active Tool Sessions"));
        assert!(matches!(
            edb.get(compact),
            Some(Event::CompactStateUpdate(update)) if update.total_stages == 7
        ));
        assert!(matches!(
            edb.events().last(),
            Some(Event::CompactStateUpdate(update))
                if update.state == CompactState::Completed
                    && update.content.contains("Terminal pty-9")
        ));
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn v35_compact_events_migrate_as_worker_single_turn_lifecycles() {
        let directory = temporary_path("compact-v35-migration");
        let path = directory.join("main.edb");
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("legacy compact").unwrap();
        let trigger_api = edb.append_api_requesting(prompt).unwrap();
        let tool = edb
            .append_tool_call(
                trigger_api,
                prompt,
                "compact",
                crate::compact::TOOL_NAME,
                "{}",
            )
            .unwrap();
        edb.append_api_state(trigger_api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
            .unwrap();
        let compact = edb
            .append_compact_started(tool, prompt, CompactKind::WorkerSingleTurn)
            .unwrap();
        edb.append_compact_terminal(
            compact,
            CompactState::Interrupted,
            "",
            "legacy interruption",
        )
        .unwrap();
        let next_event_id = edb.next_event_id();
        let mut bytes = encode_file_header(next_event_id).to_vec();
        bytes[4] = 35;
        for event in edb.events().iter().skip(1) {
            bytes.extend(encode_v35_record(event).unwrap());
        }
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, bytes).unwrap();

        let migrated = EventDataBase::open(&path).unwrap();
        assert_eq!(migrated.next_event_id(), next_event_id + 1);
        assert!(
            migrated
                .events()
                .iter()
                .filter_map(|event| match event {
                    Event::CompactStateUpdate(update) => Some(update),
                    _ => None,
                })
                .all(
                    |update| update.kind == CompactKind::WorkerSingleTurn && update.stage.is_none()
                )
        );
        assert_eq!(fs::read(&path).unwrap()[4], 41);
        drop(migrated);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn v36_multi_turn_compacts_migrate_by_owning_orchestrator() {
        for (orchestrator, expected) in [
            ("main-agent", CompactKind::MainAgentMultiTurn),
            ("manager-agent", CompactKind::ManagerMultiTurn),
        ] {
            let directory = temporary_path(&format!("compact-v36-{orchestrator}"));
            let path = directory.join("main.edb");
            let mut edb = EventDataBase::new();
            edb.append_agent_kind_def(AgentKind::Interactive, orchestrator, None, None)
                .unwrap();
            let prompt = edb.append_user_prompt("legacy segmented compact").unwrap();
            let trigger_api = edb.append_api_requesting(prompt).unwrap();
            let tool = edb
                .append_tool_call(
                    trigger_api,
                    prompt,
                    "compact",
                    crate::compact::TOOL_NAME,
                    "{}",
                )
                .unwrap();
            edb.append_api_state(trigger_api, prompt, ApiState::Completed, "")
                .unwrap();
            edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
                .unwrap();
            // Code 1 represented the generic Segmented kind in v36. The
            // current Manager kind deliberately serializes as code 1 so this
            // fixture can write the exact old wire value.
            let compact = edb
                .append_compact_started(tool, prompt, CompactKind::ManagerMultiTurn)
                .unwrap();
            let next_event_id = edb.next_event_id();
            let event_ids = std::iter::once(0)
                .chain(edb.events().iter().skip(1).map(|event| event.id() + 1))
                .collect::<Vec<_>>();
            let mut bytes = encode_file_header(next_event_id).to_vec();
            bytes[4] = 36;
            for event in edb.events().iter().skip(1) {
                bytes.extend(encode_v36_record(event).unwrap());
            }
            fs::create_dir_all(&directory).unwrap();
            fs::write(&path, bytes).unwrap();

            let migrated = EventDataBase::open(&path).unwrap();
            assert_eq!(migrated.next_event_id(), next_event_id + 1);
            assert_eq!(
                migrated.events().iter().map(Event::id).collect::<Vec<_>>(),
                event_ids
            );
            assert!(matches!(
                migrated.get(compact + 1),
                Some(Event::CompactStateUpdate(update))
                    if update.kind == expected && update.total_stages == 6
            ));
            assert_eq!(fs::read(&path).unwrap()[4], 41);
            drop(migrated);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn v37_compacts_migrate_with_their_original_stage_count() {
        let directory = temporary_path("compact-v37-stage-count");
        let path = directory.join("main.edb");
        let mut edb = EventDataBase::new();
        let prompt = edb.append_user_prompt("legacy compact").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let tool = edb
            .append_tool_call(api, prompt, "compact", crate::compact::TOOL_NAME, "{}")
            .unwrap();
        edb.append_api_state(api, prompt, ApiState::Completed, "")
            .unwrap();
        edb.append_tool_result(tool, ToolResultState::Succeeded, None, "{}")
            .unwrap();
        let compact = edb
            .append_compact_started(tool, prompt, CompactKind::MainAgentMultiTurn)
            .unwrap();
        let next_event_id = edb.next_event_id();
        let mut bytes = encode_file_header(next_event_id).to_vec();
        bytes[4] = 37;
        for event in edb.events().iter().skip(1) {
            bytes.extend(encode_v37_record(event).unwrap());
        }
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, bytes).unwrap();

        let migrated = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            migrated.get(compact + 1),
            Some(Event::CompactStateUpdate(update)) if update.total_stages == 6
        ));
        assert_eq!(fs::read(&path).unwrap()[4], 41);
        drop(migrated);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn workmap_pending_reminder_requires_open_work_and_round_trips() {
        let mut empty = EventDataBase::new();
        let empty_prompt = empty.append_user_prompt("ordinary question").unwrap();
        assert!(
            empty
                .append_workmap_pending_reminder(empty_prompt)
                .unwrap_err()
                .to_string()
                .contains("requires unfinished WorkMap work")
        );

        let directory = temporary_path("workmap-pending-reminder");
        let path = directory.join("main.edb");
        let expected_hash;
        let reminder_id;
        let next_prompt;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            let prompt = edb.append_user_prompt("plan work").unwrap();
            let api = edb.append_api_requesting(prompt).unwrap();
            let tool = edb
                .append_tool_call(
                    api,
                    prompt,
                    "workmap-start",
                    crate::workmap::START,
                    r#"{"objective":{"title":"Continue later"},"plans":[{"title":"Inspect"}]}"#,
                )
                .unwrap();
            edb.append_api_state(api, prompt, ApiState::Completed, "")
                .unwrap();
            let output = crate::workmap::execute(
                crate::workmap::START,
                r#"{"objective":{"title":"Continue later"},"plans":[{"title":"Inspect"}]}"#,
                tool,
                &mut edb,
            )
            .unwrap();
            edb.append_tool_result(tool, ToolResultState::Succeeded, None, output.to_string())
                .unwrap();

            next_prompt = edb.append_user_prompt("resume or redirect").unwrap();
            reminder_id = edb.append_workmap_pending_reminder(next_prompt).unwrap();
            let reminder = edb.get(reminder_id).unwrap();
            expected_hash = reminder.getHash();
            assert_eq!(reminder.kind(), EventKind::WorkMapPendingReminder);
            assert_eq!(
                reminder.getBriefString(),
                format!("WorkMapPendingReminderEvent(prompt_id={next_prompt})")
            );
            assert!(reminder.getDetailString().contains("timestamp_ms="));

            edb.append_agent_turn(next_prompt, next_prompt, AgentTurnState::Started, "")
                .unwrap();
        }

        let reopened = EventDataBase::open(&path).unwrap();
        assert!(matches!(
            reopened.get(reminder_id),
            Some(Event::WorkMapPendingReminder(reminder))
                if reminder.prompt_id == next_prompt
        ));
        assert_eq!(reopened.get(reminder_id).unwrap().getHash(), expected_hash);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn agent_definition_and_turn_lifecycle_round_trip_compact_storage() {
        let directory = temporary_path("agent-turn-round-trip");
        let path = directory.join("child.edb");
        let turn_id;
        let hashes;
        {
            let mut edb = EventDataBase::open(&path).unwrap();
            edb.append_agent_kind_def(
                AgentKind::SubAgent,
                "main-agent",
                Some("main".into()),
                Some("Return a concise result.".into()),
            )
            .unwrap();
            turn_id = edb.append_user_prompt("inspect").unwrap();
            edb.append_agent_turn(turn_id, turn_id, AgentTurnState::Started, "")
                .unwrap();
            edb.append_agent_turn(
                turn_id,
                turn_id,
                AgentTurnState::Completed,
                "final answer completed",
            )
            .unwrap();
            hashes = edb
                .events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>();
        }

        let edb = EventDataBase::open(&path).unwrap();
        let definition = agent_kind_definition(edb.events()).unwrap();
        assert_eq!(definition.kind, AgentKind::SubAgent);
        assert_eq!(definition.parent_agent_id.as_deref(), Some("main"));
        assert_eq!(
            definition.system_prompt.as_deref(),
            Some("Return a concise result.")
        );
        let turn = latest_agent_turn(edb.events()).unwrap().unwrap();
        assert_eq!(turn.turn_id, turn_id);
        assert_eq!(turn.prompt_id, turn_id);
        assert_eq!(turn.state, AgentTurnState::Completed);
        assert_eq!(turn.detail, "final answer completed");
        assert_eq!(
            hashes,
            edb.events()
                .iter()
                .map(EventBase::getHash)
                .collect::<Vec<_>>()
        );
        for event in edb.events() {
            assert!(!event.getBriefString().is_empty());
            assert!(event.getDetailString().contains("timestamp_ms="));
        }
        drop(edb);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn agent_turn_append_rejects_overlapping_and_mismatched_lifecycles() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Primary, "main-agent", None, None)
            .unwrap();
        let first = edb.append_user_prompt("first").unwrap();
        assert!(
            edb.append_agent_turn(first, first, AgentTurnState::Completed, "")
                .is_err()
        );
        edb.append_agent_turn(first, first, AgentTurnState::Started, "")
            .unwrap();
        let second = edb.append_user_prompt("second").unwrap();
        assert!(
            edb.append_agent_turn(second, second, AgentTurnState::Started, "")
                .is_err()
        );
        assert!(
            edb.append_agent_turn(second, second, AgentTurnState::Failed, "wrong turn")
                .is_err()
        );
        edb.append_agent_turn(first, first, AgentTurnState::Interrupted, "stopped")
            .unwrap();
        edb.append_agent_turn(second, second, AgentTurnState::Started, "")
            .unwrap();
        let turn = latest_agent_turn(edb.events()).unwrap().unwrap();
        assert_eq!(turn.turn_id, second);
        assert_eq!(turn.state, AgentTurnState::Started);
    }

    #[test]
    fn delete_turn_removes_only_a_completed_turn_and_preserves_later_history() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "main-agent", None, None)
            .unwrap();
        let first = edb.append_user_prompt("first").unwrap();
        edb.append_agent_turn(first, first, AgentTurnState::Started, "")
            .unwrap();
        edb.append_assist_response(first, "answer", true).unwrap();
        let final_answer = edb
            .append_agent_turn(first, first, AgentTurnState::Completed, "")
            .unwrap();
        let retained_state = edb.append_reasoning_effort_changed("high").unwrap();
        let second = edb.append_user_prompt("second").unwrap();
        edb.append_agent_turn(second, second, AgentTurnState::Started, "")
            .unwrap();
        edb.append_agent_turn(second, second, AgentTurnState::Completed, "")
            .unwrap();

        assert_eq!(
            edb.delete_user_turn(first).unwrap(),
            EdbMutation::DeleteTurn { prompt_id: first }
        );
        assert!(edb.get(first).is_none());
        assert!(edb.get(final_answer).is_none());
        assert!(edb.get(retained_state).is_some());
        assert!(edb.get(second).is_some());
    }

    #[test]
    fn delete_interrupted_turn_stops_immediately_before_the_next_user_prompt() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "main-agent", None, None)
            .unwrap();
        let first = edb.append_user_prompt("first").unwrap();
        edb.append_agent_turn(first, first, AgentTurnState::Started, "")
            .unwrap();
        let interrupted = edb
            .append_agent_turn(first, first, AgentTurnState::Interrupted, "stopped")
            .unwrap();
        let second = edb.append_user_prompt("second").unwrap();

        edb.delete_user_turn(first).unwrap();
        assert!(edb.get(first).is_none());
        assert!(edb.get(interrupted).is_none());
        assert!(edb.get(second).is_some());
    }

    #[test]
    fn regenerate_removes_the_target_prompt_and_every_later_event() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "main-agent", None, None)
            .unwrap();
        let prompt = edb.append_user_prompt("try this again").unwrap();
        edb.append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
            .unwrap();
        edb.append_assist_response(prompt, "old answer", true)
            .unwrap();
        let final_answer = edb
            .append_agent_turn(prompt, prompt, AgentTurnState::Completed, "")
            .unwrap();
        edb.append_reasoning_effort_changed("high").unwrap();

        let (content, mutation) = edb.regenerate_from_final_answer(final_answer).unwrap();
        assert_eq!(content, "try this again");
        assert_eq!(
            mutation,
            EdbMutation::Regenerate {
                final_answer_event_id: final_answer,
                prompt_id: prompt,
            }
        );
        assert_eq!(edb.len(), 2);
        assert!(matches!(edb.events()[0], Event::EdbIdGeneration(_)));
        assert!(matches!(edb.events()[1], Event::AgentKindDef(_)));
        assert!(edb.next_event_id() > final_answer);
    }

    #[test]
    fn clone_persists_an_exact_completed_prefix_as_an_independent_agent() {
        let directory = temporary_path("clone-final-answer");
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let source_path = directory.join("source.edb");
        let clone_path = directory.join("clone.edb");
        let final_answer;
        let source_edb_id;
        let cloned_edb_id;
        {
            let mut source = EventDataBase::open(&source_path).unwrap();
            source_edb_id = source.edb_id().unwrap().to_owned();
            source
                .append_agent_kind_def(AgentKind::Primary, "main-agent", None, None)
                .unwrap();
            source.append_initial_model("test").unwrap();
            source.append_initial_reasoning_effort("unset").unwrap();
            let prompt = source.append_user_prompt("hello").unwrap();
            source
                .append_agent_turn(prompt, prompt, AgentTurnState::Started, "")
                .unwrap();
            source
                .append_assist_response(prompt, "world", true)
                .unwrap();
            final_answer = source
                .append_agent_turn(prompt, prompt, AgentTurnState::Completed, "")
                .unwrap();
            source.append_user_prompt("must not be cloned").unwrap();
            let cloned = source
                .clone_through_final_answer(final_answer, &clone_path, "Greeting (1)")
                .unwrap();
            cloned_edb_id = cloned.edb_id().unwrap().to_owned();
            assert_ne!(source_edb_id, cloned_edb_id);
            assert_eq!(source.edb_id().unwrap(), source_edb_id);
            assert_eq!(
                agent_kind_definition(cloned.events()).unwrap().kind,
                AgentKind::Interactive
            );
            assert_eq!(
                crate::agent_title::current_title(cloned.events()),
                Some("Greeting (1)")
            );
            assert!(
                !source
                    .events()
                    .iter()
                    .any(|event| matches!(event, Event::CloneCompleted(_)))
            );
            let completed = cloned.events().last().unwrap();
            assert_eq!(completed.kind(), EventKind::CloneCompleted);
            assert!(matches!(
                completed,
                Event::CloneCompleted(event)
                    if event.title == "Greeting (1)"
                        && event.id == final_answer + 2
                        && event.getHash().len() == 64
                        && event.getBriefString().contains("Greeting (1)")
                        && event.getDetailString().contains("title=\"Greeting (1)\"")
            ));
            assert!(!cloned.events().iter().any(|event| {
                matches!(event, Event::UserPrompt(prompt) if prompt.content == "must not be cloned")
            }));
        }
        let reopened_source = EventDataBase::open(&source_path).unwrap();
        assert_eq!(reopened_source.edb_id().unwrap(), source_edb_id);
        drop(reopened_source);
        let reopened = EventDataBase::open(&clone_path).unwrap();
        assert_eq!(reopened.edb_id().unwrap(), cloned_edb_id);
        assert_eq!(
            crate::agent_title::current_title(reopened.events()),
            Some("Greeting (1)")
        );
        assert!(matches!(
            reopened.get(final_answer),
            Some(Event::AgentTurn(turn)) if turn.state == AgentTurnState::Completed
        ));
        assert!(matches!(
            reopened.events().last(),
            Some(Event::CloneCompleted(event)) if event.title == "Greeting (1)"
        ));
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }
}
