use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    Result, agent_title,
    config::{
        ModelConfig, UNSET_EFFORT, WorkspaceConfig, create_private_directory, workspace_config_path,
    },
    event::{
        AgentKind, EdbMutation, Event, EventDataBase, EventId, agent_kind_definition,
        latest_agent_turn,
    },
    model::ModelRuntime,
    orchestrator::{
        self, AgentRuntime, ApiActivitySnapshot, InputDraft, apply_model_selection, latest_model,
    },
    terminal::{TerminalFrame, TerminalSessionPreview},
    toolbox::WORKSPACE_TEMP_DIRECTORY,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(format!("invalid AgentId {value:?}").into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub kind: AgentKind,
    pub orchestrator: String,
    pub parent_agent_id: Option<String>,
    pub system_prompt: Option<String>,
}

impl AgentDefinition {
    pub fn primary() -> Self {
        Self {
            kind: AgentKind::Primary,
            orchestrator: "main-agent".into(),
            parent_agent_id: None,
            system_prompt: None,
        }
    }

    pub fn interactive() -> Self {
        Self {
            kind: AgentKind::Interactive,
            orchestrator: "main-agent".into(),
            parent_agent_id: None,
            system_prompt: None,
        }
    }

    pub fn sub_agent(parent_agent_id: impl Into<String>, system_prompt: Option<String>) -> Self {
        Self {
            kind: AgentKind::SubAgent,
            orchestrator: "main-agent".into(),
            parent_agent_id: Some(parent_agent_id.into()),
            system_prompt: system_prompt.filter(|prompt| !prompt.is_empty()),
        }
    }

    pub fn with_orchestrator(mut self, orchestrator: impl Into<String>) -> Self {
        self.orchestrator = orchestrator.into();
        self
    }
}

struct AgentEntry {
    id: AgentId,
    runtime: Arc<Mutex<AgentRuntime>>,
}

struct WorkspaceShared {
    root: PathBuf,
    config: WorkspaceConfig,
    models: Vec<ModelConfig>,
    agent_creation: Mutex<()>,
    agents: Mutex<Vec<AgentEntry>>,
    revision: AtomicU64,
}

#[derive(Clone)]
pub struct WorkspaceHandle {
    shared: Arc<WorkspaceShared>,
}

pub struct Workspace {
    handle: WorkspaceHandle,
    agent_order: Vec<AgentId>,
    snapshots: BTreeMap<AgentId, WorkspaceAgentSnapshot>,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.handle.shutdown_all();
    }
}

#[derive(Clone)]
struct WorkspaceAgentSnapshot {
    title: Option<String>,
    events: Arc<[Event]>,
    edb_size_bytes: u64,
    mutation_revision: u64,
    last_mutation: Option<EdbMutation>,
    prompt_submission_revision: u64,
    input_draft: InputDraft,
    orchestrator_name: &'static str,
    agent_kind: AgentKind,
}

impl Workspace {
    pub fn open(
        root: impl Into<PathBuf>,
        config: WorkspaceConfig,
        models: Vec<ModelConfig>,
    ) -> Result<Self> {
        let default_model = config.model.clone();
        Self::open_with_default_model(root, config, models, &default_model)
    }

    pub fn open_with_default_model(
        root: impl Into<PathBuf>,
        mut config: WorkspaceConfig,
        models: Vec<ModelConfig>,
        default_model: &str,
    ) -> Result<Self> {
        if !orchestrator::AVAILABLE_ORCHESTRATORS.contains(&config.orchestrator.as_str()) {
            return Err(format!(
                "workspace default orchestrator {} is not available",
                config.orchestrator
            )
            .into());
        }
        let model_names = models
            .iter()
            .map(|model| model.name.as_str())
            .collect::<BTreeSet<_>>();
        if !model_names.contains(default_model) {
            return Err(format!("default model {default_model} does not exist").into());
        }
        let root = root.into();
        crate::toolbox::ensure_default_toolboxes(&root)?;
        let mut paths = edb_paths(&root)?;
        paths.sort_by_key(|path| {
            let name = path.file_stem().unwrap_or_default().to_string_lossy();
            (name != "main", name.into_owned())
        });
        let mut requires_model_fallback = !model_names.contains(config.model.as_str());
        for path in &paths {
            let edb = EventDataBase::open(path)?;
            if !edb.is_empty()
                && latest_model(&edb).is_some_and(|model| !model_names.contains(model))
            {
                requires_model_fallback = true;
            }
        }
        if requires_model_fallback
            && (config.model != default_model || config.effort != UNSET_EFFORT)
        {
            config.model = default_model.to_owned();
            config.effort = UNSET_EFFORT.to_owned();
            config.save(&workspace_config_path(&root))?;
        }
        let handle = WorkspaceHandle {
            shared: Arc::new(WorkspaceShared {
                root: root.clone(),
                config,
                models,
                agent_creation: Mutex::new(()),
                agents: Mutex::new(Vec::new()),
                revision: AtomicU64::new(0),
            }),
        };
        for path in paths {
            let id = agent_id_from_path(&path)?;
            let definition = if EventDataBase::open(&path)?.is_empty() {
                Some(if id.as_str() == "main" {
                    AgentDefinition::primary()
                        .with_orchestrator(handle.shared.config.orchestrator.clone())
                } else {
                    AgentDefinition::interactive()
                        .with_orchestrator(handle.shared.config.orchestrator.clone())
                })
            } else {
                None
            };
            let runtime = build_agent_runtime(&handle, &id, &path, definition, None, None)?;
            handle.insert_runtime(id, runtime)?;
        }
        if let Err(error) = handle.ensure_all_manager_workers() {
            handle.shutdown_all();
            return Err(error);
        }
        if let Err(error) = handle.validate_agent_graph() {
            handle.shutdown_all();
            return Err(error);
        }
        handle.shared.revision.store(0, Ordering::Release);
        let mut workspace = Self {
            handle,
            agent_order: Vec::new(),
            snapshots: BTreeMap::new(),
        };
        workspace.refresh_snapshots()?;
        Ok(workspace)
    }

    pub fn revision(&self) -> u64 {
        self.handle.revision()
    }

    pub(crate) fn handle(&self) -> WorkspaceHandle {
        self.handle.clone()
    }

    pub(crate) fn model_configs(&self) -> &[ModelConfig] {
        &self.handle.shared.models
    }

    pub(crate) fn workspace_path(&self) -> &Path {
        &self.handle.shared.root
    }

    pub(crate) fn default_orchestrator(&self) -> &str {
        &self.handle.shared.config.orchestrator
    }

    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.handle.agent_ids().unwrap_or_default()
    }

    pub(crate) fn visible_agent_ids(&self) -> Vec<AgentId> {
        self.agent_order.clone()
    }

    pub fn contains(&self, id: &AgentId) -> bool {
        self.handle.contains(id).unwrap_or(false)
    }

    pub fn poll(&mut self) -> Result<bool> {
        let runtime_changed = self.handle.poll()?;
        let snapshot_changed = self.refresh_snapshots()?;
        if snapshot_changed && !runtime_changed {
            self.handle.bump_revision();
        }
        Ok(snapshot_changed || runtime_changed)
    }

    pub fn create_agent(&mut self) -> Result<AgentId> {
        let id = self.handle.create_interactive_agent()?;
        self.refresh_snapshots()?;
        Ok(id)
    }

    pub fn create_agent_with_orchestrator(&mut self, orchestrator: &str) -> Result<AgentId> {
        let id = self
            .handle
            .create_interactive_agent_with_orchestrator(orchestrator)?;
        self.refresh_snapshots()?;
        Ok(id)
    }

    pub fn delete_agent(&mut self, id: &AgentId) -> Result<()> {
        self.handle.delete_agent(id, false)?;
        self.refresh_snapshots()?;
        Ok(())
    }

    pub fn deletion_blocker(&self, id: &AgentId) -> Result<Option<String>> {
        let children = self.handle.unmanaged_child_agent_ids(id)?;
        if !children.is_empty() {
            return Ok(Some(format!("仍有 {} 个子 Agent", children.len())));
        }
        self.handle
            .with_runtime(id, |runtime| runtime.deletion_blocker())
    }

    pub fn edb_path(&self, id: &AgentId) -> PathBuf {
        self.handle.edb_path(id)
    }

    pub fn orchestrator_name(&self, id: &AgentId) -> Result<&'static str> {
        Ok(self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .orchestrator_name)
    }

    pub fn agent_kind(&self, id: &AgentId) -> Result<AgentKind> {
        Ok(self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .agent_kind)
    }

    pub fn agent_title(&self, id: &AgentId) -> Result<Option<&str>> {
        Ok(self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .title
            .as_deref())
    }

    pub fn edb_events(&self, id: &AgentId) -> Result<&[Event]> {
        Ok(&self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .events)
    }

    pub(crate) fn edb_events_snapshot(&self, id: &AgentId) -> Result<Arc<[Event]>> {
        Ok(Arc::clone(
            &self
                .snapshots
                .get(id)
                .ok_or_else(|| format!("Agent {id} does not exist"))?
                .events,
        ))
    }

    pub fn edb_size_bytes(&self, id: &AgentId) -> Result<u64> {
        Ok(self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .edb_size_bytes)
    }

    pub fn edb_mutation_revision(&self, id: &AgentId) -> Result<u64> {
        Ok(self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .mutation_revision)
    }

    pub fn last_edb_mutation(&self, id: &AgentId) -> Result<Option<&EdbMutation>> {
        Ok(self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .last_mutation
            .as_ref())
    }

    pub fn prompt_submission_revision(&self, id: &AgentId) -> Result<u64> {
        Ok(self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .prompt_submission_revision)
    }

    pub fn input_draft(&self, id: &AgentId) -> Result<&InputDraft> {
        Ok(&self
            .snapshots
            .get(id)
            .ok_or_else(|| format!("Agent {id} does not exist"))?
            .input_draft)
    }

    pub fn active_terminal_sessions(&self, id: &AgentId) -> Result<Vec<TerminalSessionPreview>> {
        self.handle
            .with_runtime(id, AgentRuntime::preview_active_terminal_sessions)
    }

    pub fn terminal_frame(&self, id: &AgentId, session_id: &str) -> Result<Option<TerminalFrame>> {
        self.handle
            .with_runtime(id, |runtime| runtime.preview_terminal_frame(session_id))
    }

    pub fn terminal_backend(&self, id: &AgentId) -> Result<Option<String>> {
        self.handle
            .with_runtime(id, AgentRuntime::preview_terminal_backend)
    }

    pub fn submit_user_prompt(&self, id: &AgentId, content: String) -> Result<u64> {
        self.handle.submit_user_prompt(id, content)
    }

    pub fn submit_effort_change(&self, id: &AgentId, effort: String) -> Result<()> {
        self.handle.submit_effort_change(id, effort)
    }

    pub fn submit_model_change(&self, id: &AgentId, model: String) -> Result<()> {
        self.handle.submit_model_change(id, model)
    }

    pub fn submit_context_clear(&self, id: &AgentId) -> Result<()> {
        self.handle
            .with_runtime(id, AgentRuntime::submit_context_clear)
    }

    pub fn submit_context_rewind(&self, id: &AgentId, event_id: EventId) -> Result<()> {
        self.handle
            .with_runtime(id, |runtime| runtime.submit_context_rewind(event_id))
    }

    pub fn submit_turn_abort(&self, id: &AgentId) -> Result<bool> {
        self.handle
            .with_runtime(id, AgentRuntime::submit_turn_abort)
    }

    fn refresh_snapshots(&mut self) -> Result<bool> {
        let runtimes = self.handle.runtime_entries()?;
        let ids = runtimes
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut changed = self.snapshots.len() != runtimes.len() || self.agent_order != ids;
        self.snapshots.retain(|id, _| ids.contains(id));
        for (id, runtime) in runtimes {
            let runtime = runtime
                .lock()
                .map_err(|_| format!("Agent {id} runtime lock is poisoned"))?;
            let current = self.snapshots.get(&id);
            let input_draft = runtime.input_draft()?;
            let events_changed = current.is_none_or(|current| {
                current.events.len() != runtime.edb_events().len()
                    || current.events.last().map(Event::id)
                        != runtime.edb_events().last().map(Event::id)
                    || current.edb_size_bytes != runtime.edb_size_bytes()
                    || current.mutation_revision != runtime.edb_mutation_revision()
            });
            let needs_refresh = events_changed
                || current.is_some_and(|current| {
                    current.prompt_submission_revision != runtime.prompt_submission_revision()
                })
                || current
                    .is_none_or(|current| input_draft.revision != current.input_draft.revision);
            if !needs_refresh {
                continue;
            }
            let events: Arc<[Event]> = if events_changed {
                runtime.edb_events().to_vec().into()
            } else {
                Arc::clone(
                    &current
                        .expect("unchanged events require an existing snapshot")
                        .events,
                )
            };
            let next = WorkspaceAgentSnapshot {
                title: agent_title::current_title(&events).map(str::to_owned),
                agent_kind: agent_kind_definition(&events)?.kind,
                events,
                edb_size_bytes: runtime.edb_size_bytes(),
                mutation_revision: runtime.edb_mutation_revision(),
                last_mutation: runtime.last_edb_mutation().cloned(),
                prompt_submission_revision: runtime.prompt_submission_revision(),
                input_draft,
                orchestrator_name: runtime.orchestrator_name(),
            };
            self.snapshots.insert(id, next);
            changed = true;
        }
        self.agent_order = ids;
        Ok(changed)
    }
}

impl WorkspaceHandle {
    fn shutdown_all(&self) {
        let entries = self
            .shared
            .agents
            .lock()
            .map(|mut agents| std::mem::take(&mut *agents))
            .unwrap_or_default();
        drop(entries);
    }

    fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    fn bump_revision(&self) {
        self.shared.revision.fetch_add(1, Ordering::AcqRel);
    }

    fn agents(&self) -> Result<std::sync::MutexGuard<'_, Vec<AgentEntry>>> {
        self.shared
            .agents
            .lock()
            .map_err(|_| "Workspace Agent registry lock is poisoned".into())
    }

    fn runtime(&self, id: &AgentId) -> Result<Arc<Mutex<AgentRuntime>>> {
        self.agents()?
            .iter()
            .find(|entry| &entry.id == id)
            .map(|entry| Arc::clone(&entry.runtime))
            .ok_or_else(|| format!("Agent {id} does not exist").into())
    }

    fn runtime_entries(&self) -> Result<Vec<(AgentId, Arc<Mutex<AgentRuntime>>)>> {
        Ok(self
            .agents()?
            .iter()
            .map(|entry| (entry.id.clone(), Arc::clone(&entry.runtime)))
            .collect())
    }

    fn with_runtime<T>(
        &self,
        id: &AgentId,
        operation: impl FnOnce(&AgentRuntime) -> Result<T>,
    ) -> Result<T> {
        let runtime = self.runtime(id)?;
        let runtime = runtime
            .lock()
            .map_err(|_| format!("Agent {id} runtime lock is poisoned"))?;
        operation(&runtime)
    }

    pub(crate) fn with_runtime_for_agent_tool<T>(
        &self,
        id: &AgentId,
        operation: impl FnOnce(&AgentRuntime) -> Result<T>,
    ) -> Result<T> {
        self.with_runtime(id, operation)
    }

    pub(crate) fn api_activity(&self, id: &AgentId) -> Result<ApiActivitySnapshot> {
        self.with_runtime(id, |runtime| Ok(runtime.api_activity()))
    }

    fn with_runtime_mut<T>(
        &self,
        id: &AgentId,
        operation: impl FnOnce(&mut AgentRuntime) -> Result<T>,
    ) -> Result<T> {
        let runtime = self.runtime(id)?;
        let mut runtime = runtime
            .lock()
            .map_err(|_| format!("Agent {id} runtime lock is poisoned"))?;
        operation(&mut runtime)
    }

    pub fn agent_ids(&self) -> Result<Vec<AgentId>> {
        Ok(self
            .agents()?
            .iter()
            .map(|entry| entry.id.clone())
            .collect())
    }

    pub fn contains(&self, id: &AgentId) -> Result<bool> {
        Ok(self.agents()?.iter().any(|entry| &entry.id == id))
    }

    pub fn poll(&self) -> Result<bool> {
        let runtimes = self.runtime_entries()?;
        let mut changed = false;
        for (id, runtime) in runtimes {
            changed |= runtime
                .lock()
                .map_err(|_| format!("Agent {id} runtime lock is poisoned"))?
                .poll_edb()?;
        }
        if changed {
            self.bump_revision();
        }
        Ok(changed)
    }

    fn next_agent_id(&self) -> Result<AgentId> {
        loop {
            let mut random = [0_u8; 6];
            getrandom::fill(&mut random)?;
            let id = AgentId::new(format!(
                "agent-{}",
                random
                    .into_iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ))?;
            if !self.contains(&id)? && !self.edb_path(&id).exists() {
                return Ok(id);
            }
        }
    }

    pub(crate) fn edb_path(&self, id: &AgentId) -> PathBuf {
        self.shared.root.join(".me/edb").join(format!("{id}.edb"))
    }

    pub(crate) fn workspace_path(&self) -> &Path {
        &self.shared.root
    }

    pub(crate) fn temporary_directory(&self, id: &AgentId) -> PathBuf {
        self.shared
            .root
            .join(WORKSPACE_TEMP_DIRECTORY)
            .join(id.as_str())
    }

    pub(crate) fn deletion_blocker(&self, id: &AgentId) -> Result<Option<String>> {
        let children = self.unmanaged_child_agent_ids(id)?;
        if !children.is_empty() {
            return Ok(Some(format!("仍有 {} 个子 Agent", children.len())));
        }
        self.with_runtime(id, |runtime| runtime.deletion_blocker())
    }

    pub(crate) fn active_terminal_sessions(
        &self,
        id: &AgentId,
    ) -> Result<Vec<TerminalSessionPreview>> {
        self.with_runtime(id, AgentRuntime::preview_active_terminal_sessions)
    }

    pub(crate) fn terminal_frame(
        &self,
        id: &AgentId,
        session_id: &str,
    ) -> Result<Option<TerminalFrame>> {
        self.with_runtime(id, |runtime| runtime.preview_terminal_frame(session_id))
    }

    pub(crate) fn terminal_backend(&self, id: &AgentId) -> Result<Option<String>> {
        self.with_runtime(id, AgentRuntime::preview_terminal_backend)
    }

    fn insert_runtime(&self, id: AgentId, runtime: AgentRuntime) -> Result<()> {
        let mut agents = self.agents()?;
        if agents.iter().any(|entry| entry.id == id) {
            return Err(format!("Agent {id} already exists").into());
        }
        agents.push(AgentEntry {
            id,
            runtime: Arc::new(Mutex::new(runtime)),
        });
        self.bump_revision();
        Ok(())
    }

    fn insert_runtime_pair(
        &self,
        first: (AgentId, AgentRuntime),
        second: (AgentId, AgentRuntime),
    ) -> Result<()> {
        let mut agents = self.agents()?;
        if agents
            .iter()
            .any(|entry| entry.id == first.0 || entry.id == second.0)
            || first.0 == second.0
        {
            return Err("ManagerAgent or WorkerAgent identity already exists".into());
        }
        agents.push(AgentEntry {
            id: first.0,
            runtime: Arc::new(Mutex::new(first.1)),
        });
        agents.push(AgentEntry {
            id: second.0,
            runtime: Arc::new(Mutex::new(second.1)),
        });
        self.bump_revision();
        Ok(())
    }

    fn build_manager_pair(
        &self,
        manager: AgentId,
        manager_path: &Path,
        definition: Option<AgentDefinition>,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<(AgentRuntime, AgentId, AgentRuntime)> {
        let manager_runtime =
            match build_agent_runtime(self, &manager, manager_path, definition, model, effort) {
                Ok(runtime) => runtime,
                Err(error) => {
                    remove_incomplete_agent(self, &manager, manager_path, &error)?;
                    return Err(error);
                }
            };
        let worker_model =
            manager_runtime
                .edb_events()
                .iter()
                .rev()
                .find_map(|event| match event {
                    Event::ModelChanged(change) => Some(change.model.clone()),
                    _ => None,
                });
        let worker_effort =
            manager_runtime
                .edb_events()
                .iter()
                .rev()
                .find_map(|event| match event {
                    Event::ReasoningEffortChanged(change) => Some(change.effort.clone()),
                    _ => None,
                });
        let worker = match self.next_agent_id() {
            Ok(worker) => worker,
            Err(error) => {
                drop(manager_runtime);
                remove_incomplete_agent(self, &manager, manager_path, &error)?;
                return Err(error);
            }
        };
        let worker_path = self.edb_path(&worker);
        match build_agent_runtime(
            self,
            &worker,
            &worker_path,
            Some(
                AgentDefinition::sub_agent(manager.as_str(), None)
                    .with_orchestrator("worker-agent"),
            ),
            worker_model,
            worker_effort,
        ) {
            Ok(worker_runtime) => Ok((manager_runtime, worker, worker_runtime)),
            Err(error) => {
                drop(manager_runtime);
                let worker_cleanup = remove_incomplete_agent(self, &worker, &worker_path, &error);
                let manager_cleanup = remove_incomplete_agent(self, &manager, manager_path, &error);
                worker_cleanup.and(manager_cleanup)?;
                Err(error)
            }
        }
    }

    pub fn create_interactive_agent(&self) -> Result<AgentId> {
        let orchestrator = self.shared.config.orchestrator.clone();
        self.create_interactive_agent_with_orchestrator(&orchestrator)
    }

    pub fn create_interactive_agent_with_orchestrator(
        &self,
        orchestrator: &str,
    ) -> Result<AgentId> {
        if !orchestrator::AVAILABLE_ORCHESTRATORS.contains(&orchestrator) {
            return Err(
                format!("orchestrator {orchestrator} is not available for new sessions").into(),
            );
        }
        let _creation = self
            .shared
            .agent_creation
            .lock()
            .map_err(|_| "Workspace Agent creation lock is poisoned")?;
        let id = self.next_agent_id()?;
        if orchestrator == "manager-agent" {
            let path = self.edb_path(&id);
            let (manager_runtime, worker, worker_runtime) = self.build_manager_pair(
                id.clone(),
                &path,
                Some(AgentDefinition::interactive().with_orchestrator(orchestrator)),
                None,
                None,
            )?;
            if let Err(error) = self.insert_runtime_pair(
                (id.clone(), manager_runtime),
                (worker.clone(), worker_runtime),
            ) {
                let _ = remove_incomplete_agent(self, &id, &path, &error);
                let _ = remove_incomplete_agent(self, &worker, &self.edb_path(&worker), &error);
                return Err(error);
            }
        } else {
            self.add_agent_with_definition(
                id.clone(),
                AgentDefinition::interactive().with_orchestrator(orchestrator),
                None,
                None,
            )?;
        }
        Ok(id)
    }

    pub fn clone_agent_through_final_answer(
        &self,
        source: &AgentId,
        final_answer_event_id: EventId,
    ) -> Result<AgentId> {
        let _creation = self
            .shared
            .agent_creation
            .lock()
            .map_err(|_| "Workspace Agent creation lock is poisoned")?;
        let source_events = self.events(source)?;
        if agent_kind_definition(&source_events)?.kind == AgentKind::SubAgent {
            return Err(format!("sub-Agent {source} history is read-only").into());
        }
        let source_title = agent_title::current_title(&source_events).unwrap_or(source.as_str());
        let title = self.next_clone_title(source_title)?;
        let id = self.next_agent_id()?;
        let path = self.edb_path(&id);
        self.with_runtime_mut(source, |runtime| {
            runtime.clone_agent_through_final_answer(
                final_answer_event_id,
                path.clone(),
                title.clone(),
            )
        })?;
        let source_orchestrator = agent_kind_definition(&source_events)?.orchestrator.clone();
        if source_orchestrator == "manager-agent" {
            let (manager_runtime, worker, worker_runtime) =
                self.build_manager_pair(id.clone(), &path, None, None, None)?;
            if let Err(error) = self.insert_runtime_pair(
                (id.clone(), manager_runtime),
                (worker.clone(), worker_runtime),
            ) {
                let _ = remove_incomplete_agent(self, &id, &path, &error);
                let _ = remove_incomplete_agent(self, &worker, &self.edb_path(&worker), &error);
                return Err(error);
            }
            return Ok(id);
        }
        let runtime = match build_agent_runtime(self, &id, &path, None, None, None) {
            Ok(runtime) => runtime,
            Err(error) => {
                remove_incomplete_agent(self, &id, &path, &error)?;
                return Err(error);
            }
        };
        if let Err(error) = self.insert_runtime(id.clone(), runtime) {
            remove_incomplete_agent(self, &id, &path, &error)?;
            return Err(error);
        }
        Ok(id)
    }

    fn next_clone_title(&self, source_title: &str) -> Result<String> {
        let base = clone_title_base(source_title);
        let existing = self
            .agent_ids()?
            .into_iter()
            .map(|id| {
                let events = self.events(&id)?;
                Ok(agent_title::current_title(&events)
                    .unwrap_or(id.as_str())
                    .to_owned())
            })
            .collect::<Result<BTreeSet<_>>>()?;
        for number in 1_u64.. {
            let suffix = format!(" ({number})");
            let keep = agent_title::MAX_TITLE_CHARS.saturating_sub(suffix.chars().count());
            let mut candidate = base.chars().take(keep).collect::<String>();
            candidate.push_str(&suffix);
            if !existing.contains(&candidate) {
                return Ok(candidate);
            }
        }
        unreachable!("unbounded clone title sequence")
    }

    fn add_agent_with_definition(
        &self,
        id: AgentId,
        definition: AgentDefinition,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<()> {
        if self.contains(&id)? {
            return Err(format!("Agent {id} already exists").into());
        }
        let path = self.edb_path(&id);
        if path.exists() {
            return Err(format!("EDB {} already exists", path.display()).into());
        }
        let runtime = match build_agent_runtime(self, &id, &path, Some(definition), model, effort) {
            Ok(runtime) => runtime,
            Err(error) => {
                remove_incomplete_agent(self, &id, &path, &error)?;
                return Err(error);
            }
        };
        if let Err(error) = self.insert_runtime(id.clone(), runtime) {
            remove_incomplete_agent(self, &id, &path, &error)?;
            return Err(error);
        }
        Ok(())
    }

    pub fn create_sub_agent(
        &self,
        parent: &AgentId,
        system_prompt: Option<String>,
        prompt: String,
        model: String,
        effort: String,
    ) -> Result<AgentId> {
        if prompt.is_empty() {
            return Err("sub-Agent prompt cannot be empty".into());
        }
        if !self.contains(parent)? {
            return Err(format!("parent Agent {parent} does not exist").into());
        }
        let _creation = self
            .shared
            .agent_creation
            .lock()
            .map_err(|_| "Workspace Agent creation lock is poisoned")?;
        let id = self.next_agent_id()?;
        self.add_agent_with_definition(
            id.clone(),
            AgentDefinition::sub_agent(parent.as_str(), system_prompt)
                .with_orchestrator(self.orchestrator_name(parent)?),
            Some(model),
            Some(effort),
        )?;
        if let Err(error) = self.submit_parent_agent_prompt(&id, prompt) {
            let _ = self.delete_agent(&id, true);
            return Err(error);
        }
        Ok(id)
    }

    pub fn delete_agent(&self, id: &AgentId, force: bool) -> Result<()> {
        let children = self.child_agent_ids(id)?;
        let orchestrator = self.orchestrator_name(id)?;
        if !force && orchestrator == "worker-agent" {
            return Err("the dedicated Worker cannot be deleted separately".into());
        }
        let managed_children = orchestrator == "manager-agent"
            && children.iter().all(|child| {
                self.orchestrator_name(child)
                    .is_ok_and(|name| name == "worker-agent")
            });
        if !force && !children.is_empty() && !managed_children {
            return Err(format!(
                "Agent {id} cannot be deleted: 仍有 {} 个子 Agent",
                children.len()
            )
            .into());
        }
        if force || managed_children {
            for child in children {
                self.delete_agent(&child, true)?;
            }
        }
        let runtime = self.runtime(id)?;
        let deletion = {
            let runtime = runtime
                .lock()
                .map_err(|_| format!("Agent {id} runtime lock is poisoned"))?;
            runtime.request_edb_deletion(force)?
        };
        let deletion_result = match deletion.recv() {
            Ok(result) => result,
            Err(_) => {
                if let Ok(runtime) = runtime.lock() {
                    runtime.cancel_edb_deletion();
                }
                return Err("Agent worker stopped before deleting its EDB".into());
            }
        };
        if let Err(error) = deletion_result {
            if let Ok(runtime) = runtime.lock() {
                runtime.cancel_edb_deletion();
            }
            return Err(error.into());
        }
        let mut agents = self.agents()?;
        let index = agents
            .iter()
            .position(|entry| &entry.id == id)
            .ok_or_else(|| format!("Agent {id} disappeared while being deleted"))?;
        agents.remove(index);
        drop(agents);
        drop(runtime);
        let temporary_cleanup = remove_agent_temporary_directory(&self.temporary_directory(id));
        self.bump_revision();
        temporary_cleanup?;
        Ok(())
    }

    pub fn events(&self, id: &AgentId) -> Result<Vec<Event>> {
        self.with_runtime_mut(id, |runtime| {
            let _ = runtime.poll_edb()?;
            Ok(runtime.edb_events().to_vec())
        })
    }

    pub fn agent_kind(&self, id: &AgentId) -> Result<AgentKind> {
        let events = self.events(id)?;
        Ok(agent_kind_definition(&events)?.kind)
    }

    pub fn child_agent_ids(&self, parent: &AgentId) -> Result<Vec<AgentId>> {
        let ids = self.agent_ids()?;
        let mut children = Vec::new();
        for id in ids {
            let events = self.events(&id)?;
            if let Ok(definition) = agent_kind_definition(&events)
                && definition.kind == AgentKind::SubAgent
                && definition.parent_agent_id.as_deref() == Some(parent.as_str())
            {
                children.push(id);
            }
        }
        Ok(children)
    }

    fn orchestrator_name(&self, id: &AgentId) -> Result<&'static str> {
        self.with_runtime(id, |runtime| Ok(runtime.orchestrator_name()))
    }

    fn unmanaged_child_agent_ids(&self, parent: &AgentId) -> Result<Vec<AgentId>> {
        let children = self.child_agent_ids(parent)?;
        if self.orchestrator_name(parent)? != "manager-agent" {
            return Ok(children);
        }
        children
            .into_iter()
            .filter_map(|child| match self.orchestrator_name(&child) {
                Ok("worker-agent") => None,
                Ok(_) => Some(Ok(child)),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn ensure_all_manager_workers(&self) -> Result<()> {
        let _creation = self
            .shared
            .agent_creation
            .lock()
            .map_err(|_| "Workspace Agent creation lock is poisoned")?;
        let managers = self
            .agent_ids()?
            .into_iter()
            .filter_map(|id| match self.orchestrator_name(&id) {
                Ok("manager-agent") => Some(Ok(id)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>>>()?;
        for manager in managers {
            self.ensure_manager_worker(&manager)?;
        }
        Ok(())
    }

    fn ensure_manager_worker(&self, manager: &AgentId) -> Result<Option<AgentId>> {
        if self.orchestrator_name(manager)? != "manager-agent" {
            return Ok(None);
        }
        let children = self.child_agent_ids(manager)?;
        match children.as_slice() {
            [worker] if self.orchestrator_name(worker)? == "worker-agent" => {
                return Ok(Some(worker.clone()));
            }
            [] => {}
            _ => {
                return Err(format!(
                    "ManagerAgent {manager} must own exactly one dedicated Worker"
                )
                .into());
            }
        }
        let parent_events = self.events(manager)?;
        let model = parent_events.iter().rev().find_map(|event| match event {
            Event::ModelChanged(change) => Some(change.model.clone()),
            _ => None,
        });
        let effort = parent_events.iter().rev().find_map(|event| match event {
            Event::ReasoningEffortChanged(change) => Some(change.effort.clone()),
            _ => None,
        });
        let worker = self.next_agent_id()?;
        self.add_agent_with_definition(
            worker.clone(),
            AgentDefinition::sub_agent(manager.as_str(), None).with_orchestrator("worker-agent"),
            model,
            effort,
        )?;
        Ok(Some(worker))
    }

    fn validate_agent_graph(&self) -> Result<()> {
        let ids = self.agent_ids()?;
        let mut parents = BTreeMap::new();
        for id in &ids {
            let events = self.events(id)?;
            let definition = agent_kind_definition(&events)?;
            if definition.kind == AgentKind::SubAgent {
                let parent = AgentId::new(
                    definition
                        .parent_agent_id
                        .clone()
                        .ok_or_else(|| format!("sub-Agent {id} has no parent AgentId"))?,
                )?;
                if !ids.contains(&parent) {
                    return Err(format!("sub-Agent {id} references missing parent {parent}").into());
                }
                parents.insert(id.clone(), parent);
            }
        }
        for origin in parents.keys() {
            let mut visited = BTreeSet::new();
            let mut cursor = origin;
            while let Some(parent) = parents.get(cursor) {
                if !visited.insert(cursor.clone()) {
                    return Err(format!("Agent parent cycle contains {cursor}").into());
                }
                cursor = parent;
            }
        }
        Ok(())
    }

    pub fn submit_user_prompt(&self, id: &AgentId, content: String) -> Result<u64> {
        let (revision, _) = self.submit_user_prompt_with_draft_revision(id, content)?;
        Ok(revision)
    }

    pub(crate) fn submit_user_prompt_with_draft_revision(
        &self,
        id: &AgentId,
        content: String,
    ) -> Result<(u64, u64)> {
        self.with_runtime(id, |runtime| {
            runtime.submit_user_prompt_with_draft_revision(content)
        })
    }

    pub(crate) fn update_input_draft(
        &self,
        id: &AgentId,
        expected_revision: u64,
        content: String,
    ) -> Result<(u64, bool)> {
        self.with_runtime(id, |runtime| {
            runtime.update_input_draft(expected_revision, content)
        })
    }

    pub(crate) fn submit_manager_prompt(&self, id: &AgentId, content: String) -> Result<u64> {
        self.with_runtime(id, |runtime| runtime.submit_manager_prompt(content))
    }

    pub(crate) fn submit_parent_agent_prompt(&self, id: &AgentId, content: String) -> Result<u64> {
        self.with_runtime(id, |runtime| runtime.submit_parent_agent_prompt(content))
    }

    pub(crate) fn submit_effort_change(&self, id: &AgentId, effort: String) -> Result<()> {
        self.with_runtime(id, |runtime| runtime.submit_effort_change(effort))
    }

    pub(crate) fn submit_model_change(&self, id: &AgentId, model: String) -> Result<()> {
        self.with_runtime(id, |runtime| runtime.submit_model_change(model))
    }

    pub(crate) fn submit_context_clear(&self, id: &AgentId) -> Result<()> {
        self.with_runtime(id, AgentRuntime::submit_context_clear)
    }

    pub(crate) fn submit_context_rewind(&self, id: &AgentId, event_id: EventId) -> Result<()> {
        self.with_runtime(id, |runtime| runtime.submit_context_rewind(event_id))
    }

    pub(crate) fn delete_user_turn(&self, id: &AgentId, prompt_id: EventId) -> Result<()> {
        self.ensure_history_writable(id)?;
        self.with_runtime_mut(id, |runtime| runtime.delete_user_turn(prompt_id))
    }

    pub(crate) fn regenerate_final_answer(
        &self,
        id: &AgentId,
        final_answer_event_id: EventId,
    ) -> Result<(u64, u64)> {
        self.ensure_history_writable(id)?;
        self.with_runtime_mut(id, |runtime| {
            runtime.regenerate_final_answer(final_answer_event_id)
        })
    }

    fn ensure_history_writable(&self, id: &AgentId) -> Result<()> {
        if self.agent_kind(id)? == AgentKind::SubAgent {
            return Err(format!("sub-Agent {id} history is read-only").into());
        }
        Ok(())
    }

    pub(crate) fn submit_turn_abort(&self, id: &AgentId) -> Result<bool> {
        self.with_runtime(id, AgentRuntime::submit_turn_abort)
    }

    pub fn is_advancing(&self, id: &AgentId) -> Result<bool> {
        self.with_runtime(id, |runtime| Ok(runtime.is_advancing()))
    }

    pub fn latest_turn(&self, id: &AgentId) -> Result<Option<crate::event::AgentTurnProjection>> {
        latest_agent_turn(&self.events(id)?)
    }
}

fn clone_title_base(title: &str) -> &str {
    let Some(prefix) = title.strip_suffix(')') else {
        return title;
    };
    let Some((base, number)) = prefix.rsplit_once(" (") else {
        return title;
    };
    if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
        base
    } else {
        title
    }
}

fn remove_incomplete_edb(path: &Path, original: &dyn std::fmt::Display) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "{original}; failed to remove incomplete EDB {}: {error}",
            path.display()
        )
        .into()),
    }
}

fn remove_incomplete_agent(
    workspace: &WorkspaceHandle,
    id: &AgentId,
    edb_path: &Path,
    original: &dyn std::fmt::Display,
) -> Result<()> {
    let edb_cleanup = remove_incomplete_edb(edb_path, original);
    let temporary_cleanup = remove_agent_temporary_directory(&workspace.temporary_directory(id));
    edb_cleanup.and(temporary_cleanup)
}

fn remove_agent_temporary_directory(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed to inspect Agent temporary directory {}: {error}",
                path.display()
            )
            .into());
        }
    };
    let result = if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to remove Agent temporary directory {}: {error}",
            path.display()
        )
        .into()),
    }
}

fn build_agent_runtime(
    workspace: &WorkspaceHandle,
    id: &AgentId,
    path: &Path,
    requested_definition: Option<AgentDefinition>,
    requested_model: Option<String>,
    requested_effort: Option<String>,
) -> Result<AgentRuntime> {
    let temporary_directory = workspace.temporary_directory(id);
    let temporary_directory_existed = temporary_directory.exists();
    create_private_directory(&temporary_directory)?;
    let result = build_agent_runtime_inner(
        workspace,
        id,
        path,
        requested_definition,
        requested_model,
        requested_effort,
    );
    if result.is_err() && !temporary_directory_existed {
        let _ = remove_agent_temporary_directory(&temporary_directory);
    }
    result
}

fn build_agent_runtime_inner(
    workspace: &WorkspaceHandle,
    id: &AgentId,
    path: &Path,
    requested_definition: Option<AgentDefinition>,
    requested_model: Option<String>,
    requested_effort: Option<String>,
) -> Result<AgentRuntime> {
    let mut edb = EventDataBase::open(path)?;
    let definition = if edb.is_empty() {
        requested_definition.ok_or("new Agent requires an Agent definition")?
    } else {
        let definition = agent_kind_definition(edb.events())?;
        AgentDefinition {
            kind: definition.kind,
            orchestrator: definition.orchestrator.clone(),
            parent_agent_id: definition.parent_agent_id.clone(),
            system_prompt: definition.system_prompt.clone(),
        }
    };
    let persisted_model = latest_model(&edb).map(str::to_owned);
    let persisted_model_missing = requested_model.is_none()
        && persisted_model.as_deref().is_some_and(|model| {
            !workspace
                .shared
                .models
                .iter()
                .any(|candidate| candidate.name == model)
        });
    let bootstrap_model = requested_model
        .as_deref()
        .or_else(|| {
            persisted_model
                .as_deref()
                .filter(|_| !persisted_model_missing)
        })
        .unwrap_or(&workspace.shared.config.model);
    let mut models = ModelRuntime::new(workspace.shared.models.clone(), bootstrap_model)?;
    let effort = requested_effort.unwrap_or_else(|| workspace.shared.config.effort.clone());
    let orchestrator_name = definition.orchestrator.clone();
    let mut orchestrator = orchestrator::create(&orchestrator_name, Some(effort))?;
    orchestrator.configure_agent(definition)?;
    #[cfg(not(test))]
    orchestrator.configure_workspace(&workspace.shared.root)?;
    orchestrator.attach_workspace(workspace.clone(), id.clone())?;
    orchestrator
        .supports_edb(&edb)
        .map_err(|reason| format!("orchestrator {}: {reason}", orchestrator_name))?;
    if persisted_model_missing {
        apply_model_selection(&mut edb, &mut models, &workspace.shared.config.model, None)?;
    }
    orchestrator.restore(&edb, &mut models)?;
    let restored_len = edb.len();
    orchestrator.reconcile_startup(&mut edb, &mut models)?;
    if edb.len() != restored_len {
        orchestrator.restore(&edb, &mut models)?;
    }
    Ok(AgentRuntime::identified(
        id.as_str(),
        path,
        edb,
        orchestrator,
        models,
    ))
}

fn edb_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    let directory = workspace.join(".me/edb");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && path.extension().is_some_and(|value| value == "edb") {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn agent_id_from_path(path: &Path) -> Result<AgentId> {
    AgentId::new(
        path.file_stem()
            .ok_or_else(|| format!("EDB {} has no file name", path.display()))?
            .to_string_lossy(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelCapabilities, ProviderType};

    fn model() -> ModelConfig {
        ModelConfig {
            name: "test".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "http://127.0.0.1:1/v1".into(),
            endpoint: "chat/completions".into(),
            api_key: Some("test".into()),
            api_key_env: None,
            credential_file: None,
            model: "test".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: ModelCapabilities {
                context_window: 1024,
                reasoning_efforts: vec!["unset".into()],
                ..Default::default()
            },
            parameters: Default::default(),
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

    fn manager_config() -> WorkspaceConfig {
        WorkspaceConfig {
            orchestrator: "manager-agent".into(),
            ..config()
        }
    }

    #[test]
    fn missing_workspace_and_edb_models_fall_back_to_the_global_default_once() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-workspace-model-fallback-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        let stale = WorkspaceConfig {
            model: "removed".into(),
            effort: "high".into(),
            ..config()
        };
        stale.save(&workspace_config_path(&directory)).unwrap();
        let edb_path = directory.join(".me/edb/main.edb");
        let mut edb = EventDataBase::open(&edb_path).unwrap();
        edb.append_agent_kind_def(AgentKind::Primary, "chatbot", None, None)
            .unwrap();
        edb.append_initial_model("removed").unwrap();
        edb.append_initial_reasoning_effort("high").unwrap();
        drop(edb);
        let valid_edb_path = directory.join(".me/edb/agent-valid.edb");
        let mut valid_edb = EventDataBase::open(&valid_edb_path).unwrap();
        valid_edb
            .append_agent_kind_def(AgentKind::Interactive, "chatbot", None, None)
            .unwrap();
        valid_edb.append_initial_model("other").unwrap();
        valid_edb
            .append_initial_reasoning_effort(UNSET_EFFORT)
            .unwrap();
        drop(valid_edb);
        let mut other = model();
        other.name = "other".into();
        other.model = "other".into();

        let workspace = Workspace::open_with_default_model(
            &directory,
            stale,
            vec![model(), other.clone()],
            "test",
        )
        .unwrap();
        let main = AgentId::new("main").unwrap();
        assert_eq!(
            latest_model(&EventDataBase::open(&edb_path).unwrap()),
            Some("test")
        );
        assert_eq!(
            crate::orchestrator::latest_effort(&EventDataBase::open(&edb_path).unwrap()),
            Some(UNSET_EFFORT)
        );
        assert_eq!(
            WorkspaceConfig::load(&workspace_config_path(&directory))
                .unwrap()
                .model,
            "test"
        );
        assert_eq!(
            latest_model(&EventDataBase::open(&valid_edb_path).unwrap()),
            Some("other")
        );
        assert_eq!(EventDataBase::open(&valid_edb_path).unwrap().len(), 3);
        assert_eq!(workspace.edb_events(&main).unwrap().len(), 5);
        drop(workspace);

        let reopened = Workspace::open_with_default_model(
            &directory,
            WorkspaceConfig::load(&workspace_config_path(&directory)).unwrap(),
            vec![model(), other],
            "test",
        )
        .unwrap();
        assert_eq!(reopened.edb_events(&main).unwrap().len(), 5);
        assert_eq!(EventDataBase::open(&valid_edb_path).unwrap().len(), 3);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn workspace_discovers_adds_and_permanently_deletes_idle_agents() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-workspace-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/agent-existing.edb")).unwrap();

        let mut workspace =
            Workspace::open(&directory, config(), vec![model()]).expect("open workspace");
        let main_temporary = directory.join(".me/tmp/main");
        let existing_temporary = directory.join(".me/tmp/agent-existing");
        assert!(main_temporary.is_dir());
        assert!(existing_temporary.is_dir());
        assert_ne!(main_temporary, existing_temporary);
        assert_eq!(
            workspace.agent_ids(),
            vec![
                AgentId::new("main").unwrap(),
                AgentId::new("agent-existing").unwrap()
            ]
        );
        assert_eq!(
            workspace
                .agent_kind(&AgentId::new("main").unwrap())
                .unwrap(),
            AgentKind::Primary
        );
        assert_eq!(workspace.revision(), 0);
        let added = workspace.create_agent().unwrap();
        let added_temporary = directory.join(".me/tmp").join(added.as_str());
        assert!(added_temporary.is_dir());
        fs::write(added_temporary.join("scratch.txt"), "temporary").unwrap();
        assert!(matches!(
            workspace.edb_events(&added).unwrap().first(),
            Some(Event::AgentKindDef(_))
        ));
        let path = workspace.edb_path(&added);
        workspace.delete_agent(&added).unwrap();
        assert!(!workspace.contains(&added));
        assert!(!path.exists());
        assert!(!added_temporary.exists());
        assert!(main_temporary.is_dir());
        assert!(existing_temporary.is_dir());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sessions_persist_independent_orchestrators_and_default_only_affects_new_sessions() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-workspace-orchestrators-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();

        let (main_agent, chatbot, manager, worker) = {
            let mut workspace = Workspace::open(&directory, config(), vec![model()]).unwrap();
            let primary = AgentId::new("main").unwrap();
            assert_eq!(workspace.orchestrator_name(&primary).unwrap(), "chatbot");

            let main_agent = workspace
                .create_agent_with_orchestrator("main-agent")
                .unwrap();
            let chatbot = workspace.create_agent_with_orchestrator("chatbot").unwrap();
            let manager = workspace
                .create_agent_with_orchestrator("manager-agent")
                .unwrap();
            let worker = workspace.handle.child_agent_ids(&manager).unwrap()[0].clone();
            assert!(
                workspace
                    .create_agent_with_orchestrator("worker-agent")
                    .is_err()
            );

            for (id, expected) in [
                (&main_agent, "main-agent"),
                (&chatbot, "chatbot"),
                (&manager, "manager-agent"),
                (&worker, "worker-agent"),
            ] {
                assert_eq!(workspace.orchestrator_name(id).unwrap(), expected);
                assert_eq!(
                    agent_kind_definition(workspace.edb_events(id).unwrap())
                        .unwrap()
                        .orchestrator,
                    expected
                );
            }
            (main_agent, chatbot, manager, worker)
        };

        let changed_default = WorkspaceConfig {
            orchestrator: "main-agent".into(),
            ..config()
        };
        let mut workspace = Workspace::open(&directory, changed_default, vec![model()]).unwrap();
        assert_eq!(
            workspace
                .orchestrator_name(&AgentId::new("main").unwrap())
                .unwrap(),
            "chatbot"
        );
        assert_eq!(
            workspace.orchestrator_name(&main_agent).unwrap(),
            "main-agent"
        );
        assert_eq!(workspace.orchestrator_name(&chatbot).unwrap(), "chatbot");
        assert_eq!(
            workspace.orchestrator_name(&manager).unwrap(),
            "manager-agent"
        );
        assert_eq!(
            workspace.orchestrator_name(&worker).unwrap(),
            "worker-agent"
        );
        assert_eq!(
            workspace.handle.child_agent_ids(&manager).unwrap(),
            vec![worker]
        );

        let new_default_session = workspace.create_agent().unwrap();
        assert_eq!(
            workspace.orchestrator_name(&new_default_session).unwrap(),
            "main-agent"
        );

        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn manager_worker_is_created_once_restored_and_deleted_as_one_unit() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-manager-worker-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();

        let worker = {
            let workspace = Workspace::open(&directory, manager_config(), vec![model()]).unwrap();
            let main = AgentId::new("main").unwrap();
            assert_eq!(workspace.orchestrator_name(&main).unwrap(), "manager-agent");
            let children = workspace.handle.child_agent_ids(&main).unwrap();
            assert_eq!(children.len(), 1);
            let worker = children[0].clone();
            assert!(directory.join(".me/tmp/main").is_dir());
            assert!(directory.join(".me/tmp").join(worker.as_str()).is_dir());
            assert_eq!(
                workspace.orchestrator_name(&worker).unwrap(),
                "worker-agent"
            );
            assert!(
                !workspace
                    .edb_events(&worker)
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event, Event::UserPrompt(_)))
            );
            assert!(workspace.handle.delete_agent(&worker, false).is_err());
            worker
        };

        let mut workspace = Workspace::open(&directory, manager_config(), vec![model()]).unwrap();
        let main = AgentId::new("main").unwrap();
        assert_eq!(
            workspace.handle.child_agent_ids(&main).unwrap(),
            vec![worker.clone()]
        );
        assert_eq!(workspace.agent_ids().len(), 2);
        let added = workspace.create_agent().unwrap();
        let added_worker = workspace.handle.child_agent_ids(&added).unwrap()[0].clone();
        assert!(directory.join(".me/tmp").join(added.as_str()).is_dir());
        assert!(
            directory
                .join(".me/tmp")
                .join(added_worker.as_str())
                .is_dir()
        );
        assert_eq!(workspace.agent_ids().len(), 4);
        workspace.delete_agent(&added).unwrap();
        assert!(!directory.join(".me/tmp").join(added.as_str()).exists());
        assert!(
            !directory
                .join(".me/tmp")
                .join(added_worker.as_str())
                .exists()
        );
        workspace.delete_agent(&main).unwrap();
        assert!(workspace.agent_ids().is_empty());
        assert!(!directory.join(".me/edb/main.edb").exists());
        assert!(!directory.join(".me/tmp/main").exists());
        assert!(!directory.join(".me/tmp").join(worker.as_str()).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn manager_and_worker_model_and_effort_changes_are_independent() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-manager-model-sync-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();

        let mut first = model();
        first.name = "first".into();
        first.model = "first".into();
        first.capabilities.reasoning_efforts = vec!["unset".into(), "high".into()];
        let mut second = first.clone();
        second.name = "second".into();
        second.model = "second".into();
        let config = WorkspaceConfig {
            model: "first".into(),
            ..manager_config()
        };
        let mut workspace = Workspace::open(&directory, config, vec![first, second]).unwrap();
        let manager = AgentId::new("main").unwrap();
        let worker = workspace.handle.child_agent_ids(&manager).unwrap()[0].clone();

        workspace
            .submit_model_change(&manager, "second".into())
            .unwrap();
        workspace
            .submit_effort_change(&manager, "high".into())
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            workspace.poll().unwrap();
            let manager_events = workspace.edb_events(&manager).unwrap();
            let worker_events = workspace.edb_events(&worker).unwrap();
            let event_model_is = |events: &[Event], expected: &str| {
                events.iter().rev().find_map(|event| match event {
                    Event::ModelChanged(change) => Some(change.model == expected),
                    _ => None,
                }) == Some(true)
            };
            let event_effort_is = |events: &[Event], expected: &str| {
                events.iter().rev().find_map(|event| match event {
                    Event::ReasoningEffortChanged(change) => Some(change.effort == expected),
                    _ => None,
                }) == Some(true)
            };
            let independent = event_model_is(manager_events, "second")
                && event_effort_is(manager_events, "high")
                && event_model_is(worker_events, "first")
                && event_effort_is(worker_events, "unset");
            if independent {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Manager changes unexpectedly affected its Worker"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        workspace
            .submit_model_change(&worker, "second".into())
            .unwrap();
        workspace
            .submit_effort_change(&worker, "high".into())
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            workspace.poll().unwrap();
            let manager_events = workspace.edb_events(&manager).unwrap();
            let worker_events = workspace.edb_events(&worker).unwrap();
            let event_model_is = |events: &[Event], expected: &str| {
                events.iter().rev().find_map(|event| match event {
                    Event::ModelChanged(change) => Some(change.model == expected),
                    _ => None,
                }) == Some(true)
            };
            let event_effort_is = |events: &[Event], expected: &str| {
                events.iter().rev().find_map(|event| match event {
                    Event::ReasoningEffortChanged(change) => Some(change.effort == expected),
                    _ => None,
                }) == Some(true)
            };
            if event_model_is(manager_events, "second")
                && event_effort_is(manager_events, "high")
                && event_model_is(worker_events, "second")
                && event_effort_is(worker_events, "high")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Worker did not apply its own model/effort changes"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cloning_a_manager_creates_a_fresh_empty_worker() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-manager-clone-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        let final_answer = {
            let mut edb = EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
            edb.append_agent_kind_def(AgentKind::Primary, "manager-agent", None, None)
                .unwrap();
            for name in ["base", "policy", "manager", "tool"] {
                edb.append_system_prompt(name).unwrap();
            }
            edb.append_initial_model("test").unwrap();
            edb.append_initial_reasoning_effort("unset").unwrap();
            let prompt = edb.append_user_prompt("build it").unwrap();
            edb.append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Started, "")
                .unwrap();
            let api = edb.append_api_requesting(prompt).unwrap();
            edb.append_api_state(api, prompt, crate::event::ApiState::Streaming, "")
                .unwrap();
            edb.append_assist_response(prompt, "done", true).unwrap();
            edb.append_api_state(api, prompt, crate::event::ApiState::Completed, "")
                .unwrap();
            edb.append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Completed, "")
                .unwrap()
        };

        let workspace = Workspace::open(&directory, manager_config(), vec![model()]).unwrap();
        let main = AgentId::new("main").unwrap();
        let original_worker = workspace.handle.child_agent_ids(&main).unwrap()[0].clone();
        let cloned = workspace
            .handle
            .clone_agent_through_final_answer(&main, final_answer)
            .unwrap();
        let cloned_children = workspace.handle.child_agent_ids(&cloned).unwrap();
        assert_eq!(cloned_children.len(), 1);
        assert_ne!(cloned_children[0], original_worker);
        assert_eq!(
            workspace.handle.orchestrator_name(&cloned).unwrap(),
            "manager-agent"
        );
        assert_eq!(
            workspace
                .handle
                .orchestrator_name(&cloned_children[0])
                .unwrap(),
            "worker-agent"
        );
        assert!(
            !workspace
                .handle
                .events(&cloned_children[0])
                .unwrap()
                .iter()
                .any(|event| matches!(event, Event::UserPrompt(_)))
        );
        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn workspace_rejects_a_persisted_sub_agent_with_a_missing_parent() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-workspace-orphan-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
        let mut orphan = EventDataBase::open(&directory.join(".me/edb/agent-orphan.edb")).unwrap();
        orphan
            .append_agent_kind_def(
                AgentKind::SubAgent,
                "chatbot",
                Some("agent-missing".into()),
                None,
            )
            .unwrap();
        orphan.append_initial_model("test").unwrap();
        orphan.append_initial_reasoning_effort("unset").unwrap();
        drop(orphan);

        let error = Workspace::open(&directory, config(), vec![model()])
            .err()
            .expect("orphaned sub-Agent must be rejected");
        assert!(error.to_string().contains("missing parent agent-missing"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cloning_a_completed_turn_creates_independent_agents_with_numbered_titles() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-workspace-clone-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        let final_answer = {
            let mut edb = EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
            edb.append_agent_kind_def(AgentKind::Primary, "chatbot", None, None)
                .unwrap();
            edb.append_initial_model("test").unwrap();
            edb.append_initial_reasoning_effort("unset").unwrap();
            let prompt = edb.append_user_prompt("clone me").unwrap();
            edb.append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Started, "")
                .unwrap();
            let api = edb.append_api_requesting(prompt).unwrap();
            edb.append_api_state(api, prompt, crate::event::ApiState::Streaming, "")
                .unwrap();
            edb.append_assist_response(prompt, "done", true).unwrap();
            edb.append_api_state(api, prompt, crate::event::ApiState::Completed, "")
                .unwrap();
            let final_answer = edb
                .append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Completed, "")
                .unwrap();
            edb.append_user_prompt("later").unwrap();
            final_answer
        };
        let changed_default = WorkspaceConfig {
            orchestrator: "main-agent".into(),
            ..config()
        };
        let workspace = Workspace::open(&directory, changed_default, vec![model()]).unwrap();
        let first = workspace
            .handle
            .clone_agent_through_final_answer(&AgentId::new("main").unwrap(), final_answer)
            .unwrap();
        let second = workspace
            .handle
            .clone_agent_through_final_answer(&AgentId::new("main").unwrap(), final_answer)
            .unwrap();

        let first_events = workspace.handle.events(&first).unwrap();
        let second_events = workspace.handle.events(&second).unwrap();
        assert_eq!(
            agent_kind_definition(&first_events).unwrap().kind,
            AgentKind::Interactive
        );
        assert_eq!(
            agent_kind_definition(&first_events).unwrap().orchestrator,
            "chatbot"
        );
        assert_eq!(agent_title::current_title(&first_events), Some("main (1)"));
        assert_eq!(agent_title::current_title(&second_events), Some("main (2)"));
        assert!(!first_events.iter().any(|event| {
            matches!(event, Event::UserPrompt(prompt) if prompt.content == "later")
        }));
        assert!(first_events.iter().any(|event| event.id() == final_answer));

        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deleting_a_turn_through_the_runtime_updates_the_shared_edb_snapshot() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "me-workspace-delete-turn-{}-{}",
            std::process::id(),
            u64::from_le_bytes(suffix)
        ));
        fs::create_dir_all(directory.join(".me/edb")).unwrap();
        let prompt = {
            let mut edb = EventDataBase::open(&directory.join(".me/edb/main.edb")).unwrap();
            edb.append_agent_kind_def(AgentKind::Primary, "chatbot", None, None)
                .unwrap();
            edb.append_initial_model("test").unwrap();
            edb.append_initial_reasoning_effort("unset").unwrap();
            let prompt = edb.append_user_prompt("remove").unwrap();
            edb.append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Started, "")
                .unwrap();
            let api = edb.append_api_requesting(prompt).unwrap();
            edb.append_api_state(api, prompt, crate::event::ApiState::Streaming, "")
                .unwrap();
            edb.append_assist_response(prompt, "answer", true).unwrap();
            edb.append_api_state(api, prompt, crate::event::ApiState::Completed, "")
                .unwrap();
            edb.append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Completed, "")
                .unwrap();
            prompt
        };
        let workspace = Workspace::open(&directory, config(), vec![model()]).unwrap();
        let main = AgentId::new("main").unwrap();
        workspace.handle.delete_user_turn(&main, prompt).unwrap();
        let events = workspace.handle.events(&main).unwrap();
        assert!(!events.iter().any(|event| event.id() == prompt));

        drop(workspace);
        fs::remove_dir_all(directory).unwrap();
    }
}
