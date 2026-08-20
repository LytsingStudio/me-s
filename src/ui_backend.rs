use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{
    Result,
    config::UNSET_EFFORT,
    event::{AgentKind, EdbMutation, Event, EventId},
    terminal::{TerminalFrame, TerminalSessionPreview},
    workspace::{AgentId, Workspace, WorkspaceHandle},
};

pub const CHAT_HIDDEN_TOOL_NAMES: &[&str] = &[crate::agent_title::TOOL_NAME];
pub const CHAT_HIDDEN_TOOL_PREFIXES: &[&str] = &["WorkMap.", "Worker."];
pub const CHAT_ACTIVITY_TOOL_NAMES: &[&str] = &[crate::agent_toolbox::WORKER_WAIT];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatToolPresentation {
    Standard,
    Hidden,
    WorkerActivity,
}

pub fn tool_chat_presentation(name: &str) -> ChatToolPresentation {
    if CHAT_ACTIVITY_TOOL_NAMES.contains(&name) {
        return ChatToolPresentation::WorkerActivity;
    }
    if CHAT_HIDDEN_TOOL_NAMES.contains(&name)
        || CHAT_HIDDEN_TOOL_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    {
        return ChatToolPresentation::Hidden;
    }
    ChatToolPresentation::Standard
}

pub fn tool_is_chat_visible(name: &str) -> bool {
    tool_chat_presentation(name) == ChatToolPresentation::Standard
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiModelOption {
    pub name: String,
    pub context_window: u64,
    pub reasoning_efforts: Vec<String>,
    pub output_token_reservations: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
pub struct UiAgentSnapshot {
    pub id: AgentId,
    pub title: Option<String>,
    pub kind: AgentKind,
    pub parent_agent_id: Option<AgentId>,
    pub orchestrator_name: String,
    pub edb_path: PathBuf,
    pub edb_size_bytes: u64,
    pub mutation_revision: u64,
    pub last_mutation: Option<EdbMutation>,
    pub prompt_submission_revision: u64,
    pub input_draft: String,
    pub input_draft_revision: u64,
    pub events: Arc<[Event]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiAgentRevision {
    pub event_count: usize,
    pub last_event_id: Option<EventId>,
    pub edb_size_bytes: u64,
    pub mutation_revision: u64,
    pub prompt_submission_revision: u64,
    pub input_draft_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiEnvironment {
    pub workspace: PathBuf,
    pub os: String,
    pub arch: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiApiActivity {
    pub active: bool,
    pub received_sse_events: u64,
}

impl UiAgentSnapshot {
    pub fn revision(&self) -> UiAgentRevision {
        UiAgentRevision {
            event_count: self.events.len(),
            last_event_id: self.events.last().map(Event::id),
            edb_size_bytes: self.edb_size_bytes,
            mutation_revision: self.mutation_revision,
            prompt_submission_revision: self.prompt_submission_revision,
            input_draft_revision: self.input_draft_revision,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UiSnapshot {
    pub revision: u64,
    pub environment: Arc<UiEnvironment>,
    pub agents: Vec<UiAgentSnapshot>,
    pub models: Arc<[UiModelOption]>,
    pub orchestrators: Arc<[String]>,
    pub default_orchestrator: String,
}

impl UiSnapshot {
    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.iter().map(|agent| agent.id.clone()).collect()
    }

    pub fn agent(&self, id: &AgentId) -> Option<&UiAgentSnapshot> {
        self.agents.iter().find(|agent| &agent.id == id)
    }

    pub fn contains(&self, id: &AgentId) -> bool {
        self.agent(id).is_some()
    }
}

pub trait UiBackend: Send + Sync {
    fn snapshot(&self) -> Result<UiSnapshot>;

    fn api_activity(&self, agent_id: &AgentId) -> Result<UiApiActivity>;

    fn terminal_sessions(&self, agent_id: &AgentId) -> Result<Vec<TerminalSessionPreview>>;

    fn terminal_frame(&self, agent_id: &AgentId, session_id: &str)
    -> Result<Option<TerminalFrame>>;

    fn terminal_backend(&self, agent_id: &AgentId) -> Result<Option<String>>;

    fn deletion_blocker(&self, agent_id: &AgentId) -> Result<Option<String>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiAgentDraft {
    pub id: AgentId,
    pub edb_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    UpdateInputDraft {
        agent_id: AgentId,
        expected_revision: u64,
        content: String,
    },
    SubmitUserPrompt {
        agent_id: AgentId,
        content: String,
    },
    ChangeEffort {
        agent_id: AgentId,
        effort: String,
    },
    ChangeModel {
        agent_id: AgentId,
        model: String,
    },
    ClearContext {
        agent_id: AgentId,
    },
    RewindContext {
        agent_id: AgentId,
        event_id: EventId,
    },
    CloneAgent {
        agent_id: AgentId,
        final_answer_event_id: EventId,
    },
    DeleteTurn {
        agent_id: AgentId,
        prompt_id: EventId,
    },
    Regenerate {
        agent_id: AgentId,
        final_answer_event_id: EventId,
    },
    AbortTurn {
        agent_id: AgentId,
    },
    AddAgent {
        orchestrator: String,
    },
    DeleteAgent {
        agent_id: AgentId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommandReceipt {
    Accepted,
    InputDraftUpdated {
        accepted: bool,
        revision: u64,
    },
    UserPromptSubmitted {
        prompt_revision: u64,
        input_draft_revision: u64,
    },
    AbortRequested(bool),
    AgentCreated(UiAgentDraft),
}

pub trait UiCommandGateway: Send + Sync {
    fn submit(&self, command: UiCommand) -> Result<UiCommandReceipt>;
}

#[derive(Clone)]
pub struct WorkspaceUiBackend {
    workspace: Arc<Mutex<Workspace>>,
    handle: WorkspaceHandle,
    environment: Arc<UiEnvironment>,
    models: Arc<[UiModelOption]>,
    orchestrators: Arc<[String]>,
    default_orchestrator: String,
}

#[derive(Clone)]
pub struct WorkspaceUiCommandGateway {
    handle: WorkspaceHandle,
}

pub fn workspace_ui_ports(workspace: Workspace) -> (WorkspaceUiBackend, WorkspaceUiCommandGateway) {
    let handle = workspace.handle();
    let environment = Arc::new(UiEnvironment {
        workspace: workspace.workspace_path().to_owned(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    });
    let models =
        workspace
            .model_configs()
            .iter()
            .filter(|model| !crate::codex_oauth::is_legacy_model_name(&model.name))
            .map(|model| {
                let mut output_token_reservations = BTreeMap::from([(
                    UNSET_EFFORT.to_owned(),
                    model.output_token_reservation(Some(UNSET_EFFORT)),
                )]);
                output_token_reservations.extend(
                    model.capabilities.reasoning_efforts.iter().map(|effort| {
                        (effort.clone(), model.output_token_reservation(Some(effort)))
                    }),
                );
                UiModelOption {
                    name: model.name.clone(),
                    context_window: model.capabilities.context_window,
                    reasoning_efforts: model.capabilities.reasoning_efforts.clone(),
                    output_token_reservations,
                }
            })
            .collect::<Vec<_>>()
            .into();
    let orchestrators = crate::orchestrator::AVAILABLE_ORCHESTRATORS
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>()
        .into();
    let default_orchestrator = workspace.default_orchestrator().to_owned();
    (
        WorkspaceUiBackend {
            workspace: Arc::new(Mutex::new(workspace)),
            handle: handle.clone(),
            environment,
            models,
            orchestrators,
            default_orchestrator,
        },
        WorkspaceUiCommandGateway { handle },
    )
}

impl UiBackend for WorkspaceUiBackend {
    fn snapshot(&self) -> Result<UiSnapshot> {
        let mut workspace = self
            .workspace
            .lock()
            .map_err(|_| "UI Workspace snapshot lock is poisoned")?;
        workspace.poll()?;
        let agents = workspace
            .visible_agent_ids()
            .into_iter()
            .map(|id| {
                let events = workspace.edb_events_snapshot(&id)?;
                let definition = crate::event::agent_kind_definition(&events)?;
                Ok(UiAgentSnapshot {
                    title: workspace.agent_title(&id)?.map(str::to_owned),
                    kind: definition.kind,
                    parent_agent_id: definition
                        .parent_agent_id
                        .as_deref()
                        .map(AgentId::new)
                        .transpose()?,
                    orchestrator_name: workspace.orchestrator_name(&id)?.to_owned(),
                    edb_path: workspace.edb_path(&id),
                    edb_size_bytes: workspace.edb_size_bytes(&id)?,
                    mutation_revision: workspace.edb_mutation_revision(&id)?,
                    last_mutation: workspace.last_edb_mutation(&id)?.cloned(),
                    prompt_submission_revision: workspace.prompt_submission_revision(&id)?,
                    input_draft: workspace.input_draft(&id)?.content.clone(),
                    input_draft_revision: workspace.input_draft(&id)?.revision,
                    events,
                    id,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(UiSnapshot {
            revision: workspace.revision(),
            environment: Arc::clone(&self.environment),
            agents,
            models: Arc::clone(&self.models),
            orchestrators: Arc::clone(&self.orchestrators),
            default_orchestrator: self.default_orchestrator.clone(),
        })
    }

    fn api_activity(&self, agent_id: &AgentId) -> Result<UiApiActivity> {
        let activity = self.handle.api_activity(agent_id)?;
        Ok(UiApiActivity {
            active: activity.active,
            received_sse_events: activity.received_sse_events,
        })
    }

    fn terminal_sessions(&self, agent_id: &AgentId) -> Result<Vec<TerminalSessionPreview>> {
        self.handle.active_terminal_sessions(agent_id)
    }

    fn terminal_frame(
        &self,
        agent_id: &AgentId,
        session_id: &str,
    ) -> Result<Option<TerminalFrame>> {
        self.handle.terminal_frame(agent_id, session_id)
    }

    fn terminal_backend(&self, agent_id: &AgentId) -> Result<Option<String>> {
        self.handle.terminal_backend(agent_id)
    }

    fn deletion_blocker(&self, agent_id: &AgentId) -> Result<Option<String>> {
        self.handle.deletion_blocker(agent_id)
    }
}

impl UiCommandGateway for WorkspaceUiCommandGateway {
    fn submit(&self, command: UiCommand) -> Result<UiCommandReceipt> {
        match command {
            UiCommand::UpdateInputDraft {
                agent_id,
                expected_revision,
                content,
            } => {
                let (revision, accepted) =
                    self.handle
                        .update_input_draft(&agent_id, expected_revision, content)?;
                Ok(UiCommandReceipt::InputDraftUpdated { accepted, revision })
            }
            UiCommand::SubmitUserPrompt { agent_id, content } => {
                let (prompt_revision, input_draft_revision) = self
                    .handle
                    .submit_user_prompt_with_draft_revision(&agent_id, content)?;
                Ok(UiCommandReceipt::UserPromptSubmitted {
                    prompt_revision,
                    input_draft_revision,
                })
            }
            UiCommand::ChangeEffort { agent_id, effort } => {
                self.handle.submit_effort_change(&agent_id, effort)?;
                Ok(UiCommandReceipt::Accepted)
            }
            UiCommand::ChangeModel { agent_id, model } => {
                self.handle.submit_model_change(&agent_id, model)?;
                Ok(UiCommandReceipt::Accepted)
            }
            UiCommand::ClearContext { agent_id } => {
                self.handle.submit_context_clear(&agent_id)?;
                Ok(UiCommandReceipt::Accepted)
            }
            UiCommand::RewindContext { agent_id, event_id } => {
                self.handle.submit_context_rewind(&agent_id, event_id)?;
                Ok(UiCommandReceipt::Accepted)
            }
            UiCommand::CloneAgent {
                agent_id,
                final_answer_event_id,
            } => {
                let id = self
                    .handle
                    .clone_agent_through_final_answer(&agent_id, final_answer_event_id)?;
                Ok(UiCommandReceipt::AgentCreated(UiAgentDraft {
                    edb_path: self.handle.edb_path(&id),
                    id,
                }))
            }
            UiCommand::DeleteTurn {
                agent_id,
                prompt_id,
            } => {
                self.handle.delete_user_turn(&agent_id, prompt_id)?;
                Ok(UiCommandReceipt::Accepted)
            }
            UiCommand::Regenerate {
                agent_id,
                final_answer_event_id,
            } => {
                let (prompt_revision, input_draft_revision) = self
                    .handle
                    .regenerate_final_answer(&agent_id, final_answer_event_id)?;
                Ok(UiCommandReceipt::UserPromptSubmitted {
                    prompt_revision,
                    input_draft_revision,
                })
            }
            UiCommand::AbortTurn { agent_id } => Ok(UiCommandReceipt::AbortRequested(
                self.handle.submit_turn_abort(&agent_id)?,
            )),
            UiCommand::AddAgent { orchestrator } => {
                let id = self
                    .handle
                    .create_interactive_agent_with_orchestrator(&orchestrator)?;
                Ok(UiCommandReceipt::AgentCreated(UiAgentDraft {
                    edb_path: self.handle.edb_path(&id),
                    id,
                }))
            }
            UiCommand::DeleteAgent { agent_id } => {
                self.handle.delete_agent(&agent_id, false)?;
                Ok(UiCommandReceipt::Accepted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use super::*;
    use crate::config::{ModelCapabilities, ModelConfig, ProviderType, WorkspaceConfig};

    fn model() -> ModelConfig {
        ModelConfig {
            name: "test".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "http://127.0.0.1:1/v1".into(),
            endpoint: "chat/completions".into(),
            api_key: Some("must-not-reach-ui".into()),
            api_key_env: None,
            credential_file: None,
            model: "test".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: ModelCapabilities {
                context_window: 4096,
                reasoning_efforts: vec!["unset".into(), "high".into()],
                ..Default::default()
            },
            parameters: toml::from_str("max_tokens = 512").unwrap(),
            effort_parameters: Default::default(),
        }
    }

    fn config() -> WorkspaceConfig {
        WorkspaceConfig {
            version: 2,
            model: "test".into(),
            effort: "unset".into(),
            orchestrator: "chatbot".into(),
        }
    }

    fn workspace() -> PathBuf {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-ui-backend-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        directory
    }

    #[test]
    fn chat_tool_visibility_policy_hides_control_plane_tools() {
        assert!(!tool_is_chat_visible(crate::agent_title::TOOL_NAME));
        assert!(!tool_is_chat_visible("WorkMap.Start"));
        assert!(!tool_is_chat_visible("WorkMap.UpdatePlanState"));
        assert!(!tool_is_chat_visible("Worker.Ask"));
        assert!(!tool_is_chat_visible("Worker.Wait"));
        assert_eq!(
            tool_chat_presentation("Worker.Wait"),
            ChatToolPresentation::WorkerActivity
        );
        assert!(tool_is_chat_visible("File.Read"));
        assert!(tool_is_chat_visible("Terminal.Interact"));
        assert!(tool_is_chat_visible("Agent.Wait"));
    }

    #[test]
    fn ui_snapshot_exposes_the_manager_worker_parent_relation() {
        let directory = workspace();
        let mut manager_config = config();
        manager_config.orchestrator = "manager-agent".into();
        let workspace = Workspace::open(&directory, manager_config, vec![model()]).unwrap();
        let (backend, commands) = workspace_ui_ports(workspace);
        let manager = match commands
            .submit(UiCommand::AddAgent {
                orchestrator: "manager-agent".into(),
            })
            .unwrap()
        {
            UiCommandReceipt::AgentCreated(agent) => agent.id,
            receipt => panic!("unexpected receipt {receipt:?}"),
        };
        let snapshot = backend.snapshot().unwrap();
        let worker = snapshot
            .agents
            .iter()
            .find(|agent| agent.orchestrator_name == "worker-agent")
            .unwrap();
        assert_eq!(worker.kind, AgentKind::SubAgent);
        assert_eq!(worker.parent_agent_id.as_ref(), Some(&manager));
        assert!(snapshot.agent(&manager).unwrap().parent_agent_id.is_none());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn multiple_ui_readers_share_authoritative_state_without_consuming_it() {
        let directory = workspace();
        let workspace = Workspace::open(&directory, config(), vec![model()]).unwrap();
        let (backend, commands) = workspace_ui_ports(workspace);
        let reader_a = backend.clone();
        let reader_b = backend.clone();

        let before_a = reader_a.snapshot().unwrap();
        let before_b = reader_b.snapshot().unwrap();
        assert!(before_a.agents.is_empty());
        assert!(before_b.agents.is_empty());
        assert_eq!(before_a.revision, before_b.revision);
        assert_eq!(before_a.default_orchestrator, "chatbot");
        assert_eq!(
            before_a.orchestrators.as_ref(),
            &["main-agent", "manager-agent", "chatbot"]
        );
        assert_eq!(
            before_a.models.as_ref(),
            &[UiModelOption {
                name: "test".into(),
                context_window: 4096,
                reasoning_efforts: vec!["unset".into(), "high".into()],
                output_token_reservations: BTreeMap::from([
                    ("unset".into(), 512),
                    ("high".into(), 512),
                ]),
            }]
        );

        let UiCommandReceipt::AgentCreated(draft) = commands
            .submit(UiCommand::AddAgent {
                orchestrator: "main-agent".into(),
            })
            .unwrap()
        else {
            panic!("AddAgent did not return its atomic creation result");
        };

        let after_a = reader_a.snapshot().unwrap();
        let after_b = reader_b.snapshot().unwrap();
        assert!(after_a.contains(&draft.id));
        assert!(after_b.contains(&draft.id));
        assert_eq!(after_a.revision, after_b.revision);
        assert!(after_a.revision > before_a.revision);
        assert!(!before_a.contains(&draft.id));
        assert!(!before_b.contains(&draft.id));
        assert!(Arc::ptr_eq(
            &after_a.agent(&draft.id).unwrap().events,
            &after_b.agent(&draft.id).unwrap().events,
        ));

        let UiCommandReceipt::InputDraftUpdated {
            accepted,
            revision: draft_revision,
        } = commands
            .submit(UiCommand::UpdateInputDraft {
                agent_id: draft.id.clone(),
                expected_revision: 0,
                content: "shared draft\nfrom reader A".into(),
            })
            .unwrap()
        else {
            panic!("UpdateInputDraft did not return its revision");
        };
        assert!(accepted);
        let draft_a = reader_a.snapshot().unwrap();
        let draft_b = reader_b.snapshot().unwrap();
        for snapshot in [&draft_a, &draft_b] {
            let agent = snapshot.agent(&draft.id).unwrap();
            assert_eq!(agent.input_draft, "shared draft\nfrom reader A");
            assert_eq!(agent.input_draft_revision, draft_revision);
        }
        let UiCommandReceipt::InputDraftUpdated { accepted, revision } = commands
            .submit(UiCommand::UpdateInputDraft {
                agent_id: draft.id.clone(),
                expected_revision: 0,
                content: String::new(),
            })
            .unwrap()
        else {
            panic!("UpdateInputDraft did not return the authoritative draft");
        };
        assert!(!accepted);
        assert_eq!(revision, draft_revision);
        assert_eq!(
            reader_a
                .snapshot()
                .unwrap()
                .agent(&draft.id)
                .unwrap()
                .input_draft,
            "shared draft\nfrom reader A"
        );
        assert_eq!(after_a.agent(&draft.id).unwrap().input_draft, "");
        assert!(Arc::ptr_eq(
            &draft_a.agent(&draft.id).unwrap().events,
            &after_a.agent(&draft.id).unwrap().events,
        ));

        commands
            .submit(UiCommand::ClearContext {
                agent_id: draft.id.clone(),
            })
            .unwrap();
        let initial_count = after_a.agent(&draft.id).unwrap().events.len();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let changed_a = loop {
            let snapshot = reader_a.snapshot().unwrap();
            if snapshot.agent(&draft.id).unwrap().events.len() > initial_count {
                break snapshot;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        };
        let changed_b = reader_b.snapshot().unwrap();
        assert_eq!(
            changed_a.agent(&draft.id).unwrap().revision(),
            changed_b.agent(&draft.id).unwrap().revision()
        );

        let (prompt_revision, cleared_revision) = commands
            .handle
            .with_runtime_for_agent_tool(&draft.id, |runtime| {
                runtime.submit_user_prompt_with_draft_revision("shared draft\nfrom reader A".into())
            })
            .unwrap();
        assert_eq!(prompt_revision, 1);
        assert!(cleared_revision > draft_revision);
        let cleared = reader_b.snapshot().unwrap();
        let cleared_agent = cleared.agent(&draft.id).unwrap();
        assert_eq!(cleared_agent.input_draft, "");
        assert_eq!(cleared_agent.input_draft_revision, cleared_revision);

        commands.handle.delete_agent(&draft.id, true).unwrap();
        assert!(!reader_a.snapshot().unwrap().contains(&draft.id));
        assert!(!reader_b.snapshot().unwrap().contains(&draft.id));
        assert!(changed_a.contains(&draft.id));

        drop(changed_b);
        drop(changed_a);
        drop(after_b);
        drop(after_a);
        drop(before_b);
        drop(before_a);
        drop(reader_b);
        drop(reader_a);
        drop(commands);
        drop(backend);
        fs::remove_dir_all(directory).unwrap();
    }
}
