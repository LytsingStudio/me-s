"use strict";

const HTTP_SYNC_ACTIVE_MS = 250;
const HTTP_SYNC_IDLE_MS = 1000;
const HTTP_SYNC_TIMEOUT_MS = 15000;
const RECONNECT_MAX_MS = 5000;
const INPUT_ANIMATION_QUIET_MS = 250;
const TRANSCRIPT_BOTTOM_THRESHOLD_PX = 24;
const SEND_SHORTCUT_COOKIE = "me_send_shortcut";
const SEND_SHORTCUT_ENTER = "enter";
const SEND_SHORTCUT_MODIFIED_ENTER = "modified-enter";
const API_ACTIVE = new Set(["Requesting", "Streaming", "Retrying"]);
const API_SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const COMMANDS = [
  ["/agent-add", "新建会话"],
  ["/agent-delete", "删除当前会话"],
  ["/model", "切换当前模型"],
  ["/effort", "选择推理强度"],
  ["/clear", "清空当前上下文"],
  ["/rewind", "撤回到较早的会话位置"],
  ["/exit", "关闭当前页面"],
];
const PORTRAIT_LAYOUT = matchMedia("(orientation: portrait)");

const state = {
  gateway: { workspaces: [], selected_workspace_id: "chat", selected_agent_id: null, notices: [] },
  workspaceId: null,
  workspaceStates: new Map(),
  gatewayRefreshInFlight: false,
  lastNoticeId: 0,
  workspaceMenu: null,
  snapshot: {
    revision: 0, environment: null, agents: [], models: [], orchestrators: [], default_orchestrator: null,
    tool_visibility: { hidden_names: [], hidden_prefixes: [], activity_names: [] },
  },
  stores: new Map(),
  drafts: new Map(),
  draftSync: new Map(),
  selectedAgent: null,
  apiActivity: { agentId: null, active: false, receivedSseEvents: 0 },
  pendingAgentSelection: null,
  view: { kind: "chat", sessionId: null },
  terminals: [],
  terminalRevisions: new Map(),
  terminalFollowBottom: true,
  expandedTools: new Set(),
  expandedHistoryObjectives: new Set(),
  objectiveDisclosure: emptyObjectiveDisclosure(),
  workerActivityIndexes: new Map(),
  pendingRender: emptyRenderRequest(),
  inputResizeFrame: null,
  apiAnimationTick: 0,
  lastInputAt: Number.NEGATIVE_INFINITY,
  composing: false,
  sendShortcut: readSendShortcutCookie(typeof document.cookie === "string" ? document.cookie : ""),
  slashIndex: 0,
  userMenu: null,
  agentMenu: null,
  modal: null,
  drawer: null,
  contextDrawerOpen: false,
  contextDrawerSignature: null,
  contextCompactContent: null,
  contextCompactAnalysis: null,
  contextMemoryContent: null,
  authRequired: false,
  authenticated: false,
  connected: false,
  connecting: false,
  snapshotInitialized: false,
  syncGeneration: 0,
  syncController: null,
  syncInFlight: false,
  syncTimer: null,
  reconnectTimer: null,
  reconnectAttempt: 0,
  terminalFrames: new Map(),
  terminalFramesUnavailable: new Set(),
  pageClosing: false,
};

const $ = (selector) => document.querySelector(selector);
const elements = {
  app: $("#app"),
  loginScreen: $("#login-screen"),
  loginForm: $("#login-form"),
  loginPassword: $("#login-password"),
  loginError: $("#login-error"),
  loginSubmit: $("#login-submit"),
  connectionOverlay: $("#connection-overlay"),
  connectionOverlayTitle: $("#connection-overlay-title"),
  connectionOverlayMessage: $("#connection-overlay-message"),
  connectionRetry: $("#connection-retry"),
  themeCycle: $("#theme-cycle"),
  themeMode: $("#theme-mode"),
  environment: $("#environment-footer"),
  workspaceList: $("#workspace-list"),
  createWorkspace: $("#create-workspace"),
  openWorkspace: $("#open-workspace"),
  openSettings: $("#open-settings"),
  agents: $("#agent-list"),
  addAgent: $("#add-agent"),
  mobileDeleteAgent: $("#mobile-delete-agent"),
  mobileSidebarToggle: $("#mobile-sidebar-toggle"),
  mobileSidebarBackdrop: $("#mobile-sidebar-backdrop"),
  tabs: $("#view-tabs"),
  terminalTabs: $("#terminal-tabs"),
  chatView: $("#chat-view"),
  workmapView: $("#workmap-view"),
  terminalView: $("#terminal-view"),
  transcript: $("#transcript"),
  transcriptContent: $("#transcript-content"),
  scrollToBottom: $("#scroll-to-bottom"),
  objective: $("#objective-summary"),
  composer: $("#composer-shell"),
  input: $("#prompt-input"),
  stop: $("#stop-generation"),
  send: $("#send-prompt"),
  sendSpinner: $("#send-prompt-spinner"),
  sendLabel: $("#send-prompt-label"),
  inputHint: $("#input-hint"),
  slashMenu: $("#slash-menu"),
  userMessageMenu: $("#user-message-menu"),
  copyUserMessage: $("#copy-user-message"),
  rewindUserMessage: $("#rewind-user-message"),
  deleteUserTurn: $("#delete-user-turn"),
  agentMenu: $("#agent-menu"),
  deleteAgentMenu: $("#delete-agent-menu"),
  workspaceMenu: $("#workspace-menu"),
  closeWorkspaceMenu: $("#close-workspace-menu"),
  workmap: $("#workmap-content"),
  workmapCount: $("#workmap-count"),
  terminalScreen: $("#terminal-screen"),
  terminalMessage: $("#terminal-message"),
  statusModelTrigger: $("#status-model-trigger"),
  statusEffortTrigger: $("#status-effort-trigger"),
  statusModel: $("#status-model"),
  statusEffort: $("#status-effort"),
  statusLiveTokens: $("#status-live-tokens"),
  statusContext: $("#status-context"),
  statusContextTrigger: $("#status-context-trigger"),
  apiSpinner: $("#api-spinner"),
  modalBackdrop: $("#modal-backdrop"),
  modalTitle: $("#modal-title"),
  modalDescription: $("#modal-description"),
  modalContent: $("#modal-content"),
  modalConfirm: $("#modal-confirm"),
  modalCancel: $("#modal-cancel"),
  modalClose: $("#modal-close"),
  drawerBackdrop: $("#choice-drawer-backdrop"),
  drawerTitle: $("#choice-drawer-title"),
  drawerDescription: $("#choice-drawer-description"),
  drawerContent: $("#choice-drawer-content"),
  drawerClose: $("#choice-drawer-close"),
  contextDrawerBackdrop: $("#context-drawer-backdrop"),
  contextDrawerClose: $("#context-drawer-close"),
  contextRing: $("#context-ring"),
  contextPercent: $("#context-percent"),
  contextUsageText: $("#context-usage-text"),
  contextBreakdown: $("#context-breakdown"),
  contextClear: $("#context-clear"),
  compactSummaryBackdrop: $("#compact-summary-backdrop"),
  compactSummaryTitle: $("#compact-summary-title"),
  compactSummaryClose: $("#compact-summary-close"),
  compactSummaryContent: $("#compact-summary-content"),
  toasts: $("#toast-region"),
};

function eventParts(event) {
  const entry = Object.entries(event)[0];
  return entry || ["Unknown", {}];
}

function replaceElementChildren(element, ...children) {
  while (element.firstChild) element.removeChild(element.firstChild);
  for (const child of children) element.appendChild(child);
}

function normalizeSendShortcut(value) {
  return value === SEND_SHORTCUT_ENTER ? SEND_SHORTCUT_ENTER : SEND_SHORTCUT_MODIFIED_ENTER;
}

function readSendShortcutCookie(cookieHeader) {
  const prefix = `${SEND_SHORTCUT_COOKIE}=`;
  for (const part of String(cookieHeader || "").split(";")) {
    const cookie = part.trim();
    if (!cookie.startsWith(prefix)) continue;
    try { return normalizeSendShortcut(decodeURIComponent(cookie.slice(prefix.length))); }
    catch (_) { return SEND_SHORTCUT_MODIFIED_ENTER; }
  }
  return SEND_SHORTCUT_MODIFIED_ENTER;
}

function setSendShortcut(value) {
  state.sendShortcut = normalizeSendShortcut(value);
  document.cookie = `${SEND_SHORTCUT_COOKIE}=${encodeURIComponent(state.sendShortcut)}; Max-Age=31536000; Path=/; SameSite=Lax`;
  renderComposer();
  elements.input.focus();
}

function sendShortcutHint() {
  return state.sendShortcut === SEND_SHORTCUT_ENTER
    ? "Enter 发送 · Shift/Alt+Enter 换行"
    : "Enter 换行 · Shift/Alt+Enter 发送";
}

function sendShortcutPressed(event, mode) {
  if (event.key !== "Enter" || event.ctrlKey || event.metaKey) return false;
  return normalizeSendShortcut(mode) === SEND_SHORTCUT_ENTER
    ? !event.shiftKey && !event.altKey
    : event.shiftKey || event.altKey;
}

function agentMeta() {
  return state.snapshot.agents.find((agent) => agent.id === state.selectedAgent) || null;
}

function isWorkerAgent(meta = agentMeta()) {
  return meta?.orchestrator === "worker-agent";
}

function canControlRuntime(meta = agentMeta()) {
  return !!meta && (meta.kind !== "sub-agent" || isWorkerAgent(meta));
}

function currentStore() {
  return state.selectedAgent ? state.stores.get(state.selectedAgent) || null : null;
}

function currentProjection() {
  return currentStore()?.projection || emptyProjection();
}

function emptyProjection() {
  return {
    messages: [], apiState: null, apiUsage: null, model: null, effort: null, turnState: null,
    _activeAssistant: null,
    _activeTools: new Map(),
    _turnStartedAt: new Map(),
    _turnContextBaseline: new Map(),
    _lastAssistantByPrompt: new Map(),
    _completedApiUsage: new Map(),
    _erroredApis: new Set(),
    _completedCompactTools: new Set(),
    _compactActivity: null,
    _hiddenTools: new Set(),
    _messageByKey: new Map(),
    _turn: null,
  };
}

function scopedApiPath(path, workspaceId = state.workspaceId) {
  const childPath = path === "/api/sync" || path === "/api/snapshot" || path === "/api/command"
    || path.startsWith("/api/deletion-blocker/");
  if (!childPath) return path;
  if (!workspaceId) throw new Error("尚未选择工作区");
  return `/api/workspaces/${encodeURIComponent(workspaceId)}/${path.slice(5)}`;
}

async function api(path, options = {}, workspaceId = state.workspaceId) {
  const response = await fetch(scopedApiPath(path, workspaceId), { cache: "no-store", ...options });
  const payload = await response.json().catch(() => ({ ok: false, error: `HTTP ${response.status}` }));
  if (!response.ok || payload.ok === false) {
    const error = new Error(payload.error || `HTTP ${response.status}`);
    error.status = response.status;
    throw error;
  }
  return payload;
}

let gatewaySelectionSave = Promise.resolve();

function persistGatewaySelection(workspaceId, agentId) {
  gatewaySelectionSave = gatewaySelectionSave
    .catch(() => {})
    .then(() => api("/api/gateway/selection", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ workspace_id: workspaceId, agent_id: agentId }),
    }))
    .catch((error) => {
      if (error.status === 401) showLogin("登录已失效，请重新登录");
    });
  return gatewaySelectionSave;
}

function emptyGatewayWorkspaceState() {
  return {
    snapshot: {
      revision: 0, environment: null, agents: [], models: [], orchestrators: [], default_orchestrator: null,
      tool_visibility: { hidden_names: [], hidden_prefixes: [], activity_names: [] },
    },
    stores: new Map(), drafts: new Map(), draftSync: new Map(), selectedAgent: null,
    apiActivity: { agentId: null, active: false, receivedSseEvents: 0 },
    pendingAgentSelection: null, view: { kind: "chat", sessionId: null }, terminals: [],
    terminalRevisions: new Map(), terminalFollowBottom: true, expandedTools: new Set(),
    expandedHistoryObjectives: new Set(), workerActivityIndexes: new Map(),
    terminalFrames: new Map(), terminalFramesUnavailable: new Set(), snapshotInitialized: false,
    scrollTop: 0, followBottom: true,
  };
}

function gatewayWorkspaceState(workspaceId) {
  let workspace = state.workspaceStates.get(workspaceId);
  if (!workspace) {
    workspace = emptyGatewayWorkspaceState();
    state.workspaceStates.set(workspaceId, workspace);
  }
  return workspace;
}

function captureActiveWorkspace() {
  if (!state.workspaceId) return;
  const workspace = gatewayWorkspaceState(state.workspaceId);
  Object.assign(workspace, {
    snapshot: state.snapshot, stores: state.stores, drafts: state.drafts, draftSync: state.draftSync,
    selectedAgent: state.selectedAgent, apiActivity: state.apiActivity,
    pendingAgentSelection: state.pendingAgentSelection, view: state.view, terminals: state.terminals,
    terminalRevisions: state.terminalRevisions, terminalFollowBottom: state.terminalFollowBottom,
    expandedTools: state.expandedTools, expandedHistoryObjectives: state.expandedHistoryObjectives,
    workerActivityIndexes: state.workerActivityIndexes, terminalFrames: state.terminalFrames,
    terminalFramesUnavailable: state.terminalFramesUnavailable, snapshotInitialized: state.snapshotInitialized,
    scrollTop: elements.transcript.scrollTop, followBottom: transcriptBottomFollower.isFollowing(),
  });
  for (const sync of workspace.draftSync.values()) sync.paused = true;
}

function activateWorkspace(workspaceId, preferredAgent = null, beginPolling = true) {
  if (!workspaceId) return;
  if (state.workspaceId === workspaceId) {
    if (preferredAgent) selectAgent(preferredAgent);
    return;
  }
  stopHttpPolling();
  saveDraft();
  captureActiveWorkspace();
  closeContextDrawer();
  closeChoiceDrawer();
  closeModal();
  closeUserMessageMenu();
  closeAgentMenu();
  closeWorkspaceMenu();
  const workspace = gatewayWorkspaceState(workspaceId);
  for (const sync of workspace.draftSync.values()) sync.paused = false;
  state.workspaceId = workspaceId;
  state.snapshot = workspace.snapshot;
  state.stores = workspace.stores;
  state.drafts = workspace.drafts;
  state.draftSync = workspace.draftSync;
  state.selectedAgent = preferredAgent || workspace.selectedAgent;
  state.apiActivity = workspace.apiActivity;
  state.pendingAgentSelection = workspace.pendingAgentSelection;
  state.view = workspace.view;
  state.terminals = workspace.terminals;
  state.terminalRevisions = workspace.terminalRevisions;
  state.terminalFollowBottom = workspace.terminalFollowBottom;
  state.expandedTools = workspace.expandedTools;
  state.expandedHistoryObjectives = workspace.expandedHistoryObjectives;
  state.workerActivityIndexes = workspace.workerActivityIndexes;
  state.terminalFrames = workspace.terminalFrames;
  state.terminalFramesUnavailable = workspace.terminalFramesUnavailable;
  state.snapshotInitialized = workspace.snapshotInitialized;
  state.connected = false;
  state.connecting = false;
  state.reconnectAttempt = 0;
  state.pendingRender = emptyRenderRequest();
  state.composing = false;
  delete elements.terminalScreen.dataset.terminalKey;
  delete elements.terminalScreen.dataset.revision;
  restoreDraft();
  renderAll();
  requestAnimationFrame(() => {
    elements.transcript.scrollTop = workspace.followBottom ? elements.transcript.scrollHeight : workspace.scrollTop;
    transcriptBottomFollower.restore(workspace.followBottom);
  });
  persistGatewaySelection(workspaceId, state.selectedAgent);
  if (beginPolling) startHttpPolling();
}

async function refreshWorkspaceSummaries() {
  await Promise.all((state.gateway.workspaces || []).map(async (workspace) => {
    if (workspace.id === state.workspaceId && state.snapshotInitialized) return;
    try {
      const payload = await api(`/api/workspaces/${encodeURIComponent(workspace.id)}/snapshot`);
      const summary = { ...payload };
      delete summary.ok;
      gatewayWorkspaceState(workspace.id).snapshot = summary;
    } catch (_) {}
  }));
}

function applyGatewaySnapshot(snapshot) {
  state.gateway = snapshot;
  const ids = new Set((snapshot.workspaces || []).map((workspace) => workspace.id));
  for (const notice of snapshot.notices || []) {
    if (Number(notice.id) > state.lastNoticeId) toast(notice.message, true);
    state.lastNoticeId = Math.max(state.lastNoticeId, Number(notice.id) || 0);
  }
  if (state.workspaceId && !ids.has(state.workspaceId)) activateWorkspace("chat");
  for (const id of state.workspaceStates.keys()) if (!ids.has(id)) state.workspaceStates.delete(id);
  renderAgents();
}

async function refreshGatewayState() {
  if (state.gatewayRefreshInFlight || !state.authenticated) return;
  state.gatewayRefreshInFlight = true;
  try {
    const snapshot = await api("/api/gateway/state");
    applyGatewaySnapshot(snapshot);
    await refreshWorkspaceSummaries();
    renderAgents();
  } catch (error) {
    if (error.status === 401) showLogin("登录已失效，请重新登录");
  } finally {
    state.gatewayRefreshInFlight = false;
  }
}

async function initializeGateway() {
  const snapshot = await api("/api/gateway/state");
  applyGatewaySnapshot(snapshot);
  await refreshWorkspaceSummaries();
  const ids = new Set((snapshot.workspaces || []).map((workspace) => workspace.id));
  const workspaceId = ids.has(snapshot.selected_workspace_id) ? snapshot.selected_workspace_id : "chat";
  activateWorkspace(workspaceId, snapshot.selected_agent_id, false);
}

function showLogin(message = "") {
  stopHttpPolling();
  state.authenticated = false;
  state.connected = false;
  hideConnectionOverlay();
  elements.app.classList.add("hidden");
  elements.loginScreen.classList.remove("hidden");
  elements.loginError.textContent = message;
  elements.loginPassword.focus();
}

function showApplication() {
  elements.loginScreen.classList.add("hidden");
  elements.app.classList.remove("hidden");
  elements.loginError.textContent = "";
}

async function initializeAuthentication() {
  try {
    const status = await api("/api/auth/status");
    state.authRequired = Boolean(status.required);
    state.authenticated = Boolean(status.authenticated);
    if (state.authRequired && !state.authenticated) {
      showLogin();
      return;
    }
    showApplication();
    await initializeGateway();
    restoreDraft();
    startHttpPolling();
  } catch (error) {
    showLogin(`无法读取登录状态：${error.message}`);
  }
}

async function submitLogin(event) {
  event.preventDefault();
  elements.loginSubmit.disabled = true;
  elements.loginError.textContent = "";
  try {
    await api("/api/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ password: elements.loginPassword.value }),
    });
    elements.loginPassword.value = "";
    state.authenticated = true;
    showApplication();
    await initializeGateway();
    restoreDraft();
    startHttpPolling();
  } catch (error) {
    showLogin(error.message);
    elements.loginPassword.select();
  } finally {
    elements.loginSubmit.disabled = false;
  }
}

function startHttpPolling() {
  if (state.pageClosing || (state.authRequired && !state.authenticated)) return;
  if (state.syncInFlight) return;
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = null;
  state.connected = false;
  state.connecting = true;
  renderConnection();
  showConnectionOverlay(
    state.reconnectAttempt ? "正在重新连接" : "正在连接",
    state.reconnectAttempt
      ? `连接已断开，正在进行第 ${state.reconnectAttempt + 1} 次重试。`
      : "正在同步当前界面，请稍候。",
  );
  state.syncGeneration += 1;
  void requestHttpSync();
}

function stopHttpPolling() {
  clearTimeout(state.syncTimer);
  clearTimeout(state.reconnectTimer);
  state.syncTimer = null;
  state.reconnectTimer = null;
  state.syncInFlight = false;
  state.syncGeneration += 1;
  state.syncController?.abort();
  state.syncController = null;
}

function handlePollingFailure(error) {
  clearTimeout(state.syncTimer);
  state.syncTimer = null;
  state.syncGeneration += 1;
  state.syncController?.abort();
  state.connected = false;
  state.connecting = false;
  state.syncInFlight = false;
  state.syncController = null;
  renderConnection();
  if (state.pageClosing || (state.authRequired && !state.authenticated)) return;
  state.reconnectAttempt += 1;
  const delay = Math.min(RECONNECT_MAX_MS, 250 * (2 ** Math.min(state.reconnectAttempt - 1, 5)));
  const detail = error instanceof Error ? error.message : String(error || "网络连接不可用");
  showConnectionOverlay(
    "正在重新连接",
    `${detail}，将在 ${Math.max(1, Math.ceil(delay / 1000))} 秒内重试。`,
  );
  const generation = state.syncGeneration;
  state.reconnectTimer = setTimeout(async () => {
    if (generation !== state.syncGeneration || state.pageClosing) return;
    if (state.authRequired) {
      try {
        const status = await api("/api/auth/status");
        if (generation !== state.syncGeneration || state.pageClosing) return;
        if (!status.authenticated) {
          showLogin("登录已失效，请重新登录");
          return;
        }
      } catch (_) {
        // The server can be temporarily unreachable. Keep the reconnect loop active.
      }
    }
    startHttpPolling();
  }, delay);
}

function failHttpSync(title, error) {
  const detail = error instanceof Error ? error.message : String(error || "未知错误");
  console.error(title, error);
  clearTimeout(state.syncTimer);
  clearTimeout(state.reconnectTimer);
  state.syncTimer = null;
  state.reconnectTimer = null;
  state.syncInFlight = false;
  state.connected = false;
  state.connecting = false;
  state.syncGeneration += 1;
  state.syncController?.abort();
  state.syncController = null;
  renderConnection();
  showConnectionOverlay(title, `${detail}。请点击“立即重试”。`);
}

function showConnectionOverlay(title, message) {
  elements.connectionOverlayTitle.textContent = title;
  elements.connectionOverlayMessage.textContent = message;
  elements.connectionOverlay.classList.remove("hidden");
  elements.app.inert = true;
  if (elements.app.contains(document.activeElement)) document.activeElement.blur();
}

function hideConnectionOverlay() {
  elements.connectionOverlay.classList.add("hidden");
  elements.app.inert = false;
}

function scheduleHttpSync(delay) {
  clearTimeout(state.syncTimer);
  if (!state.connected || state.pageClosing) return;
  state.syncTimer = setTimeout(requestHttpSync, delay);
}

async function requestHttpSync() {
  if (state.syncInFlight || state.pageClosing) return;
  const generation = state.syncGeneration;
  const terminalKey = state.view.kind === "terminal" && state.selectedAgent && state.view.sessionId
    ? `${state.selectedAgent}:${state.view.sessionId}` : null;
  state.syncInFlight = true;
  const controller = typeof AbortController === "function" ? new AbortController() : null;
  state.syncController = controller;
  const timeout = setTimeout(() => controller?.abort(), HTTP_SYNC_TIMEOUT_MS);
  try {
    const message = await api("/api/sync", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      signal: controller?.signal,
      body: JSON.stringify({
        snapshot_revision: state.snapshotInitialized ? state.snapshot.revision : null,
        agents: [...state.stores].map(([id, store]) => ({
          id,
          event_count: store.events.length,
          mutation_revision: store.mutationRevision,
        })),
        selected_agent: state.selectedAgent,
        terminal_session: state.view.kind === "terminal" ? state.view.sessionId : null,
        terminal_revision: terminalKey ? state.terminalRevisions.get(terminalKey) ?? null : null,
      }),
    });
    if (generation !== state.syncGeneration || state.pageClosing) return;
    state.syncInFlight = false;
    state.syncController = null;
    try {
      applySyncState(message);
    } catch (error) {
      return failHttpSync("无法更新界面", error);
    }
    const delay = message.more_events || state.apiActivity.active || state.view.kind === "terminal"
      ? HTTP_SYNC_ACTIVE_MS : HTTP_SYNC_IDLE_MS;
    scheduleHttpSync(message.more_events ? 0 : delay);
  } catch (error) {
    if (generation !== state.syncGeneration || state.pageClosing) return;
    state.syncInFlight = false;
    state.syncController = null;
    if (error.status === 401) return showLogin("登录已失效，请重新登录");
    if (error.status && ![502, 503, 504].includes(error.status)) {
      return failHttpSync("界面同步失败", error);
    }
    const failure = error.name === "AbortError" ? new Error("界面同步超时") : error;
    handlePollingFailure(failure);
  } finally {
    clearTimeout(timeout);
  }
}

function requestHttpSyncNow() {
  clearTimeout(state.syncTimer);
  state.syncTimer = null;
  if (!state.syncInFlight && (state.connected || state.connecting)) void requestHttpSync();
}

function applySyncState(payload) {
  const previousSnapshot = state.snapshot;
  const wasConnected = state.connected;
  if (payload.snapshot) {
    state.snapshot = payload.snapshot;
    state.snapshotInitialized = true;
  }
  if (!state.snapshotInitialized) throw new Error("同步响应未提供初始状态");
  const presentationChanged = snapshotPresentationSignature(previousSnapshot)
    !== snapshotPresentationSignature(state.snapshot);
  const selectionChanged = reconcileAgents();
  const updates = new Map((payload.event_updates || []).map((update) => [update.agent_id, update]));
  const eventChanges = state.snapshot.agents.map((meta) => syncAgentEvents(meta, updates.get(meta.id)));
  const selectedEventsChanged = eventChanges.some((change) =>
    change.changed && change.agentId === state.selectedAgent);
  const selectedWorkerChanged = eventChanges.some((change) =>
    change.changed && isWorkerForSelectedAgent(change.agentId));
  const agentSummaryChanged = eventChanges.some((change) => change.summaryChanged);
  const responseMatchesSelection = (payload.selected_agent ?? null) === state.selectedAgent;
  const apiActivityChanged = responseMatchesSelection
    ? syncApiActivity(payload.api_activity || {}) : false;
  const terminalChanged = responseMatchesSelection
    ? syncTerminals(payload.terminals || []) : false;
  if (responseMatchesSelection && payload.terminal_frame_updated) {
    syncTerminalFrame(payload.terminal_session, payload.terminal_frame ?? null);
  }
  const fullyRecovered = responseMatchesSelection || wasConnected;
  state.connected = fullyRecovered;
  state.connecting = !fullyRecovered;
  if (fullyRecovered) {
    state.reconnectAttempt = 0;
    hideConnectionOverlay();
  }
  requestRender({
    full: !wasConnected || selectionChanged,
    connection: !wasConnected,
    agents: presentationChanged || agentSummaryChanged,
    tabs: presentationChanged || terminalChanged,
    currentEvents: selectedEventsChanged,
    workerEvents: selectedWorkerChanged,
    apiActivity: apiActivityChanged,
    status: apiActivityChanged,
  });
  if (!inputHasPriority()) flushPendingRender();
  else if (state.view.kind === "terminal") renderTerminal();
  for (const [agentId, sync] of state.draftSync) {
    if (sync.sent !== sync.desired) void runDraftSync(agentId, sync);
  }
  if (!responseMatchesSelection) requestHttpSyncNow();
}

function inputHasPriority() {
  return state.composing || performance.now() - state.lastInputAt < INPUT_ANIMATION_QUIET_MS;
}

function emptyRenderRequest() {
  return {
    full: false,
    connection: false,
    agents: false,
    tabs: false,
    currentEvents: false,
    workerEvents: false,
    apiActivity: false,
    status: false,
  };
}

function syncApiActivity(next) {
  const agentId = state.selectedAgent;
  const normalized = {
    agentId,
    active: Boolean(next.active),
    receivedSseEvents: Math.max(0, Number(next.received_sse_events) || 0),
  };
  const changed = state.apiActivity.agentId !== normalized.agentId
    || state.apiActivity.active !== normalized.active
    || state.apiActivity.receivedSseEvents !== normalized.receivedSseEvents;
  state.apiActivity = normalized;
  return changed;
}

function requestRender(update) {
  for (const key of Object.keys(state.pendingRender)) {
    if (!state.pendingRender[key]) state.pendingRender[key] = Boolean(update[key]);
  }
}

function snapshotPresentationSignature(snapshot) {
  return JSON.stringify({
    agents: (snapshot.agents || []).map((agent) => [
      agent.id, agent.title, agent.kind, agent.parent_agent_id, agent.orchestrator,
    ]),
    models: (snapshot.models || []).map((model) => [
      model.name, model.context_window, model.reasoning_efforts, model.output_token_reservations,
    ]),
    orchestrators: snapshot.orchestrators || [],
    defaultOrchestrator: snapshot.default_orchestrator || null,
  });
}

function isWorkerForSelectedAgent(agentId) {
  const meta = state.snapshot.agents.find((agent) => agent.id === agentId);
  return meta?.kind === "sub-agent" && meta.orchestrator === "worker-agent"
    && meta.parent_agent_id === state.selectedAgent;
}

function reconcileAgents() {
  const previousAgent = state.selectedAgent;
  const previousView = `${state.view.kind}:${state.view.sessionId || ""}`;
  let changed = false;
  const ids = new Set(state.snapshot.agents.map((agent) => agent.id));
  for (const id of state.stores.keys()) {
    if (!ids.has(id)) {
      state.stores.delete(id);
      state.drafts.delete(id);
      state.draftSync.delete(id);
      state.workerActivityIndexes.delete(id);
      changed = true;
    }
  }
  if (state.pendingAgentSelection && ids.has(state.pendingAgentSelection)) {
    state.selectedAgent = state.pendingAgentSelection;
    state.pendingAgentSelection = null;
    state.view = { kind: "chat", sessionId: null };
  }
  if (!state.selectedAgent || !ids.has(state.selectedAgent)) {
    state.selectedAgent = state.snapshot.agents.find((agent) => agent.id === "main")?.id
      || state.snapshot.agents[0]?.id || null;
    state.view = { kind: "chat", sessionId: null };
  }
  return changed || previousAgent !== state.selectedAgent
    || previousView !== `${state.view.kind}:${state.view.sessionId || ""}`;
}

function syncAgentEvents(meta, payload) {
  let store = state.stores.get(meta.id);
  let changed = false;
  if (!store) {
    store = {
      events: [],
      mutationRevision: meta.mutation_revision,
      promptSubmissionRevision: Number(meta.prompt_submission_revision || 0),
      inputDraftRevision: Number(meta.input_draft_revision || 0),
      pendingPromptSubmission: null,
      projection: emptyProjection(),
      workmap: emptyWorkMap(),
      turnHistory: null,
      summary: { model: null, apiState: null },
      projectedOrder: 0,
      needsReplay: true,
    };
    state.stores.set(meta.id, store);
    const initialDraft = String(meta.input_draft || "");
    state.drafts.set(meta.id, initialDraft);
    if (state.selectedAgent === meta.id && elements.input.value !== initialDraft) {
      elements.input.value = initialDraft;
      autoSizeInput();
      renderSlashMenu();
    }
    changed = true;
  }
  observeInputDraft(meta, store);
  if (store.events.length === meta.event_count && store.mutationRevision === meta.mutation_revision) {
    observePromptSubmission(meta, store);
    return { agentId: meta.id, changed, summaryChanged: false };
  }
  // A large initial replay is transferred in bounded batches. Agents without a
  // batch in this response remain pending and are requested again immediately.
  if (!payload) return { agentId: meta.id, changed, summaryChanged: false };
  const previousSummary = JSON.stringify(store.summary);
  if (payload.reset) {
    store.events = payload.events;
    store.summary = projectAgentSummary(store.events);
    store.projectedOrder = 0;
    store.needsReplay = true;
    state.workerActivityIndexes.delete(meta.id);
  } else {
    store.events.push(...payload.events);
    updateAgentSummary(store.summary, payload.events);
  }
  store.mutationRevision = payload.mutation_revision;
  if (payload.turn_history_updated) store.turnHistory = payload.turn_history ?? null;
  observePromptSubmission(meta, store);
  return {
    agentId: meta.id,
    changed: true,
    summaryChanged: previousSummary !== JSON.stringify(store.summary),
  };
}

function observeInputDraft(meta, store) {
  if (store.pendingPromptSubmission) return false;
  const revision = Number(meta.input_draft_revision || 0);
  if (revision <= store.inputDraftRevision) return false;
  const sync = state.draftSync.get(meta.id);
  if (sync?.paused) return false;
  const content = String(meta.input_draft || "");

  // A state sync can observe this page's write before its command response arrives.
  // Advance the shared baseline without replacing newer text still being typed.
  const flight = sync?.inFlight;
  if (flight && revision === flight.expectedRevision + 1 && content === flight.content) {
    store.inputDraftRevision = revision;
    sync.sent = content;
    return false;
  }

  // IME composition is one logical edit. Remember concurrent remote state so the
  // completed local composition can be written against its latest revision.
  if (sync && state.composing && state.selectedAgent === meta.id) {
    if (!sync.pendingRemote || revision > sync.pendingRemote.revision) {
      sync.pendingRemote = { revision, content };
    }
    return false;
  }

  // Preserve a local edit that lost its acknowledgement during a disconnect. The
  // recovered server revision becomes the next compare-and-set base, then the draft
  // is safely submitted again instead of replacing text already visible to the user.
  if (sync && !sync.paused && !sync.inFlight && sync.desired !== sync.sent) {
    store.inputDraftRevision = revision;
    sync.sent = content;
    return false;
  }
  adoptInputDraft(meta.id, store, revision, content);
  return true;
}

function adoptInputDraft(agentId, store, revision, content) {
  if (revision < store.inputDraftRevision) return false;
  store.inputDraftRevision = revision;
  const sync = state.draftSync.get(agentId);
  if (sync) {
    sync.desired = content;
    sync.sent = content;
    if (sync.pendingRemote?.revision <= revision) sync.pendingRemote = null;
  }
  state.drafts.set(agentId, content);
  if (state.selectedAgent === agentId && elements.input.value !== content) {
    elements.input.value = content;
    state.slashIndex = 0;
    autoSizeInput();
    renderSlashMenu();
  }
  return true;
}

function projectAgentSummary(events) {
  const summary = { model: null, apiState: null };
  updateAgentSummary(summary, events);
  return summary;
}

function updateAgentSummary(summary, events) {
  for (const event of events) {
    const [kind, value] = eventParts(event);
    if (kind === "ModelChanged") summary.model = value.model;
    else if (kind === "ApiStateUpdate") summary.apiState = value.state;
  }
}

function observePromptSubmission(meta, store) {
  const revision = Number(meta.prompt_submission_revision || 0);
  if (revision === store.promptSubmissionRevision) return false;
  store.promptSubmissionRevision = revision;
  return true;
}

function syncTerminals(sessions) {
  let changed = terminalListSignature(state.terminals) !== terminalListSignature(sessions);
  state.terminals = sessions;
  if (state.view.kind === "terminal"
      && !state.terminals.some((session) => session.session_id === state.view.sessionId)) {
    state.view = { kind: "chat", sessionId: null };
    changed = true;
  }
  return changed;
}

function syncTerminalFrame(sessionId, frame) {
  if (!sessionId || !state.selectedAgent) return;
  const key = `${state.selectedAgent}:${sessionId}`;
  if (frame) {
    state.terminalFrames.set(key, frame);
    state.terminalRevisions.set(key, Number(frame.revision) || 0);
    state.terminalFramesUnavailable.delete(key);
  } else {
    state.terminalFrames.delete(key);
    state.terminalRevisions.delete(key);
    state.terminalFramesUnavailable.add(key);
  }
}

function terminalListSignature(sessions) {
  return JSON.stringify(sessions.map((session) => [
    session.session_id, session.creation_order, session.width, session.height,
  ]));
}

function effectiveUiEvents(events) {
  const active = [];
  for (const event of events) {
    const [kind] = eventParts(event);
    if (kind === "AgentKindDef" || kind === "SystemPrompt") continue;
    if (kind === "ContextCleared") {
      active.length = 0;
      active.push(event);
    } else {
      active.push(event);
    }
  }
  const activeCalls = new Map();
  const assistCalls = new Map();
  const errored = new Set();
  for (const event of active) {
    const [kind, value] = eventParts(event);
    if (kind === "ApiStateUpdate") {
      if (value.state === "Requesting") activeCalls.set(value.prompt_id, value.api_call_id);
      if (value.state === "Completed" || value.state === "Interrupted") {
        if (activeCalls.get(value.prompt_id) === value.api_call_id) activeCalls.delete(value.prompt_id);
      }
      if (value.state === "Error") {
        errored.add(value.api_call_id);
        if (activeCalls.get(value.prompt_id) === value.api_call_id) activeCalls.delete(value.prompt_id);
      }
    } else if (kind === "AssistResponse" && activeCalls.has(value.prompt_id)) {
      assistCalls.set(value.id, activeCalls.get(value.prompt_id));
    }
  }
  return active.filter((event) => {
    const [kind, value] = eventParts(event);
    if (kind === "AssistResponse") return !errored.has(assistCalls.get(value.id));
    if (kind === "ModelContextItem") return !errored.has(value.api_call_id);
    return true;
  });
}

function effectiveConversationEvents(events) {
  const active = [];
  for (const event of events) {
    const [kind, value] = eventParts(event);
    if (kind === "AgentKindDef" || kind === "SystemPrompt"
        || kind === "ModelChanged" || kind === "ReasoningEffortChanged") continue;
    if (kind === "ContextCleared") active.length = 0;
    else if (kind === "CompactStateUpdate" && value.state === "Completed") {
      active.length = 0;
      active.push(event);
    } else active.push(event);
  }
  return effectiveUiEvents(active);
}

function projectChat(events) {
  const effective = effectiveUiEvents(events);
  const projection = emptyProjection();
  projection._completedCompactTools = new Set(effective.flatMap((event) => {
    const [kind, value] = eventParts(event);
    return kind === "CompactStateUpdate" && value.state === "Completed" ? [value.tool_call_id] : [];
  }));
  projection._hiddenTools = new Set(effective.flatMap((event) => {
    const [kind, value] = eventParts(event);
    return kind === "ToolCall" && !toolIsChatVisible(value.name)
      && !toolIsWorkerActivity(value.name) ? [value.id] : [];
  }));
  consumeChatEvents(projection, effective);
  projection.model ||= [...events].reverse().map(eventParts)
    .find(([kind]) => kind === "ModelChanged")?.[1].model || null;
  projection.effort ||= [...events].reverse().map(eventParts)
    .find(([kind]) => kind === "ReasoningEffortChanged")?.[1].effort || null;
  return projection;
}

function chatAppendNeedsReplay(events) {
  return events.some((event) => {
    const [kind, value] = eventParts(event);
    return kind === "ContextCleared"
      || (kind === "CompactStateUpdate" && value.state === "Completed")
      || (kind === "ApiStateUpdate" && value.state === "Error");
  });
}

function emptyProjectionChanges() {
  return {
    transcript: false, transcriptFrom: null, status: false, turn: false, promptConfirmed: false,
  };
}

function pendingPromptReachedProjection(store) {
  const pending = store?.pendingPromptSubmission;
  if (!pending) return false;
  return store.projection.messages.some((message) =>
    message.kind === "user"
      && message.key?.startsWith("user:")
      && Number(message.eventId) > pending.afterEventId
      && message.content === pending.content);
}

function markPendingPromptConfirmation(store, changes) {
  changes.promptConfirmed = pendingPromptReachedProjection(store);
  return changes;
}

function beginConfirmedPromptRender(changes, bottomFollower = transcriptBottomFollower) {
  if (!changes.promptConfirmed) return false;
  bottomFollower.follow();
  return true;
}

function markTranscriptChanged(changes, index) {
  changes.transcript = true;
  changes.transcriptFrom = changes.transcriptFrom == null
    ? index : Math.min(changes.transcriptFrom, index);
}

function appendProjectedMessage(projection, changes, message) {
  message._projectionIndex = projection.messages.length;
  projection.messages.push(message);
  if (message.key) projection._messageByKey.set(message.key, message);
  markTranscriptChanged(changes, message._projectionIndex);
  return message;
}

function markProjectedMessageChanged(changes, message) {
  const index = message?._projectionIndex;
  if (index != null) markTranscriptChanged(changes, index);
}

function markProjectedToolChanged(projection, changes, tool) {
  if (tool?._messageIndex != null) markTranscriptChanged(changes, tool._messageIndex);
}

function consumeChatEvents(projection, events) {
  const changes = emptyProjectionChanges();

  for (const event of events) {
    const [kind, value] = eventParts(event);
    switch (kind) {
      case "ModelChanged":
        projection.model = value.model;
        projection.apiUsage = null;
        changes.status = true;
        if (value.cause !== "Initial") {
          addNotice(projection, changes, `模型已变更为 ${value.model}`, value);
        }
        break;
      case "ReasoningEffortChanged":
        projection.effort = value.effort;
        changes.status = true;
        if (value.cause !== "Initial") {
          addNotice(projection, changes,
            value.cause === "ModelUnsupported" ? "思考强度不支持，已退回 unset" : `effort 已变更为 ${value.effort}`,
            value);
        }
        break;
      case "UserPrompt":
        beginProjectedTurn(projection, value.id);
        projection._turnStartedAt.set(value.id, value.timestamp_ms);
        projection._turnContextBaseline.set(value.id, projection.apiUsage?.total_tokens ?? null);
        appendProjectedMessage(projection, changes, {
          key: `user:${value.id}`, revision: value.id, kind: "user",
          content: value.content, timestamp: value.timestamp_ms, eventId: value.id,
          rewindable: true,
        });
        projection._activeAssistant = null;
        changes.turn = true;
        break;
      case "ManagerPrompt":
      case "ParentAgentPrompt":
        beginProjectedTurn(projection, value.id);
        projection._turnStartedAt.set(value.id, value.timestamp_ms);
        projection._turnContextBaseline.set(value.id, projection.apiUsage?.total_tokens ?? null);
        appendProjectedMessage(projection, changes, {
          key: `agent-prompt:${value.id}`, revision: value.id, kind: "user",
          content: value.content, timestamp: value.timestamp_ms, eventId: value.id,
          rewindable: false,
        });
        projection._activeAssistant = null;
        changes.turn = true;
        break;
      case "FollowUpPrompt":
        appendProjectedMessage(projection, changes, {
          key: `user:${value.id}`, revision: value.id, kind: "user",
          content: value.content, timestamp: value.timestamp_ms, eventId: value.id,
          rewindable: false,
        });
        projection._activeAssistant = null;
        break;
      case "AssistResponse":
        if (value.content) {
          if (!projection._activeAssistant
              || projection._activeAssistant.promptId !== value.prompt_id) {
            const message = {
              key: `assistant:${value.prompt_id}:${value.id}`, revision: value.id,
              kind: "assistant", content: "", timestamp: value.timestamp_ms,
            };
            appendProjectedMessage(projection, changes, message);
            projection._activeAssistant = { promptId: value.prompt_id, message };
            projection._lastAssistantByPrompt.set(value.prompt_id, message);
          }
          projection._activeAssistant.message.content += value.content;
          projection._activeAssistant.message.revision = value.id;
          markProjectedMessageChanged(changes, projection._activeAssistant.message);
        }
        if (value.finished) {
          projection._activeAssistant = null;
          finishProjectedAssistant(projection, value.prompt_id);
          changes.turn = true;
        }
        break;
      case "AgentTurn": {
        const stateName = normalize(value.state);
        if (stateName === "completed") {
          const assistant = projection._lastAssistantByPrompt.get(value.prompt_id);
          const started = projection._turnStartedAt.get(value.prompt_id);
          if (assistant && started != null
              && projection.messages[projection.messages.length - 1] === assistant
              && assistant.content.trim()) {
            appendProjectedMessage(projection, changes, {
              key: `turn-toolbar:${value.turn_id}`, revision: value.id, kind: "turn-toolbar",
              timestamp: value.timestamp_ms,
              finalAnswerEventId: value.id,
              promptId: value.prompt_id,
              durationMs: Math.max(0, Number(value.timestamp_ms) - Number(started)),
              tokenCount: completedTurnContextGrowth(
                projection._completedApiUsage,
                value.prompt_id,
                projection._turnContextBaseline.get(value.prompt_id) ?? null,
              ),
            });
          }
        }
        finishProjectedTurn(projection, value.prompt_id, stateName);
        changes.turn = true;
        break;
      }
      case "ApiStateUpdate":
        projection.apiState = value.state;
        updateProjectedApiState(projection, value);
        changes.status = true;
        changes.turn = true;
        if (value.state === "Completed") {
          projection.apiUsage = value.usage;
          projection._completedApiUsage.set(value.api_call_id, {
            promptId: value.prompt_id,
            usage: value.usage ?? null,
          });
        }
        if (value.state === "Error") {
          projection._erroredApis.add(value.api_call_id);
          addNotice(projection, changes, `API 错误：${value.detail}`, value);
        }
        if (value.state === "Retrying") {
          addNotice(projection, changes, `API 正在重试 ${value.retry_count}/${value.retry_limit}`, value);
        }
        if (value.state === "Interrupted") {
          if (!projection._erroredApis.has(value.api_call_id)) projection.apiUsage = value.usage;
          else {
            addNotice(projection, changes, `API 已中断：${value.detail}`, value);
          }
        }
        break;
      case "ToolCall": {
        openProjectedTool(projection, value);
        changes.turn = true;
        const workerActivity = toolIsWorkerActivity(value.name);
        if (!toolIsChatVisible(value.name) && !workerActivity) {
          projection._hiddenTools.add(value.id);
          break;
        }
        if (projection._completedCompactTools.has(value.id)) break;
        const queued = [...projection._activeTools.values()]
          .some((tool) => tool.apiCallId === value.api_call_id);
        const args = safeJson(value.arguments);
        const tool = {
          id: value.id, apiCallId: value.api_call_id, name: value.name, arguments: value.arguments,
          args, started: value.timestamp_ms, queued, sessionId: args?.session_id || null,
          output: "", result: null, revision: value.id,
        };
        const message = appendProjectedMessage(projection, changes, {
          key: `tool:${value.id}`, revision: value.id,
          kind: workerActivity ? "worker-activity" : "tool",
          timestamp: value.timestamp_ms, tool,
        });
        tool._messageIndex = message._projectionIndex;
        projection._activeTools.set(value.id, tool);
        break;
      }
      case "ToolInfoUpdate": {
        if (projection._hiddenTools.has(value.tool_call_id)) break;
        if (projection._completedCompactTools.has(value.tool_call_id)) break;
        const tool = projection._activeTools.get(value.tool_call_id);
        if (tool) {
          tool.output += toolInfoText(value.content);
          tool.revision = value.id;
          markProjectedToolChanged(projection, changes, tool);
        }
        break;
      }
      case "ToolCallResult": {
        closeProjectedTool(projection, value.tool_call_id);
        changes.turn = true;
        if (projection._hiddenTools.has(value.tool_call_id)) break;
        if (projection._completedCompactTools.has(value.tool_call_id)) break;
        const tool = projection._activeTools.get(value.tool_call_id);
        if (!tool) break;
        tool.result = { state: value.state, exitCode: value.exit_code, detail: value.detail, finished: value.timestamp_ms };
        tool.revision = value.id;
        if (!tool.sessionId && tool.name === "Terminal.Create") tool.sessionId = safeJson(value.detail)?.session_id || null;
        projection._activeTools.delete(value.tool_call_id);
        const next = [...projection._activeTools.values()]
          .find((candidate) => candidate.apiCallId === tool.apiCallId && candidate.queued);
        if (next) {
          next.queued = false;
          next.started = value.timestamp_ms;
          next.revision = value.id;
          markProjectedToolChanged(projection, changes, next);
        }
        markProjectedToolChanged(projection, changes, tool);
        break;
      }
      case "TerminalSessionCreated": {
        const tool = projection._activeTools.get(value.tool_call_id);
        if (tool) {
          tool.sessionId = value.session_id;
          tool.revision = value.id;
          markProjectedToolChanged(projection, changes, tool);
        }
        break;
      }
      case "TerminalSessionState":
        appendProjectedMessage(projection, changes, {
          key: `session:${value.id}`, revision: value.id,
          kind: "session", timestamp: value.timestamp_ms,
          content: `Session ${value.session_id} ${normalize(value.state)} · exit_code=${value.exit_code ?? "None"} · ${value.detail}`,
        });
        break;
      case "UserTurnAborted":
        abortProjectedTurn(projection, value.prompt_id);
        changes.turn = true;
        break;
      case "ContextCleared":
        addNotice(projection, changes, "上下文已清空", value);
        break;
      case "CompactStateUpdate":
        if (value.state === "Started") {
          beginCompactActivity(projection, changes, value);
        } else if (value.state === "StageCompleted") {
          advanceCompactActivity(projection, changes, value);
        } else if (value.state === "Completed") {
          projection.apiUsage = null;
          projection._turn = null;
          projection.turnState = null;
          projection._turnContextBaseline.set(value.prompt_id, null);
          for (const [apiCallId, entry] of projection._completedApiUsage) {
            if (entry.promptId === value.prompt_id) projection._completedApiUsage.delete(apiCallId);
          }
          finishCompactActivity(projection, changes, value, "上下文已压缩");
          changes.status = true;
        } else if (value.state === "Failed") {
          finishCompactActivity(projection, changes, value, "压缩失败");
        } else if (value.state === "Interrupted") {
          finishCompactActivity(projection, changes, value, "压缩中断");
        }
        break;
      case "CloneCompleted":
        addNotice(projection, changes, `克隆完成。新会话：${value.title}`, value);
        break;
      default:
        break;
    }
  }
  refreshProjectedTurnState(projection);
  return changes;
}

function beginProjectedTurn(projection, promptId) {
  projection._turn = {
    promptId,
    aborted: false,
    terminal: false,
    apiStates: new Map(),
    openTools: new Set(),
    latestApiCallId: null,
    latestApiHasTool: false,
    latestApiHasFinal: false,
  };
  refreshProjectedTurnState(projection);
}

function updateProjectedApiState(projection, update) {
  const turn = projection._turn;
  if (!turn || turn.promptId !== update.prompt_id) return;
  turn.apiStates.set(update.api_call_id, update.state);
  if (update.state === "Requesting") {
    turn.latestApiCallId = update.api_call_id;
    turn.latestApiHasTool = false;
    turn.latestApiHasFinal = false;
    turn.terminal = false;
  } else if (["Streaming", "Retrying"].includes(update.state)) {
    turn.terminal = false;
  } else if (["Error", "Interrupted"].includes(update.state)) {
    turn.terminal = true;
  }
  refreshProjectedTurnState(projection);
}

function openProjectedTool(projection, call) {
  const turn = projection._turn;
  if (!turn || turn.promptId !== call.prompt_id) return;
  turn.openTools.add(call.id);
  if (turn.latestApiCallId === call.api_call_id) turn.latestApiHasTool = true;
  turn.terminal = false;
  refreshProjectedTurnState(projection);
}

function closeProjectedTool(projection, toolCallId) {
  const turn = projection._turn;
  if (!turn) return;
  turn.openTools.delete(toolCallId);
  refreshProjectedTurnState(projection);
}

function finishProjectedAssistant(projection, promptId) {
  const turn = projection._turn;
  if (!turn || turn.promptId !== promptId) return;
  turn.latestApiHasFinal = true;
  if (!turn.latestApiHasTool) turn.terminal = true;
  refreshProjectedTurnState(projection);
}

function finishProjectedTurn(projection, promptId, stateName) {
  const turn = projection._turn;
  if (!turn || turn.promptId !== promptId || stateName === "started") return;
  turn.terminal = true;
  refreshProjectedTurnState(projection);
}

function abortProjectedTurn(projection, promptId) {
  const turn = projection._turn;
  if (!turn || turn.promptId !== promptId) return;
  turn.aborted = true;
  refreshProjectedTurnState(projection);
}

function refreshProjectedTurnState(projection) {
  const turn = projection._turn;
  if (!turn) {
    projection.turnState = null;
    return;
  }
  if (turn.aborted) {
    const terminalStates = new Set(["Completed", "Error", "Interrupted"]);
    const settled = [...turn.apiStates.values()].every((stateName) => terminalStates.has(stateName))
      && turn.openTools.size === 0;
    projection.turnState = { state: settled ? "aborted" : "aborting", promptId: turn.promptId };
    return;
  }
  const apiState = turn.latestApiCallId == null ? null : turn.apiStates.get(turn.latestApiCallId);
  const active = ["Requesting", "Streaming", "Retrying"].includes(apiState)
    || turn.openTools.size > 0
    || !turn.terminal;
  projection.turnState = { state: active ? "active" : "completed", promptId: turn.promptId };
}

function addNotice(projection, changes, content, event) {
  return appendProjectedMessage(projection, changes, {
    key: `notice:${event.id}`, revision: event.id,
    kind: "notice", content, timestamp: event.timestamp_ms,
  });
}

function compactStageCount(kind, totalStages = null) {
  const persisted = Number(totalStages);
  if (Number.isInteger(persisted) && persisted > 0) return persisted;
  return kind === "MainAgentMultiTurn" || kind === "ManagerMultiTurn" ? 6 : 1;
}

function compactProgressText(kind, stage, receivedSseEvents = null, totalStages = null) {
  const total = compactStageCount(kind, totalStages);
  const current = Math.min(Math.max(1, Number(stage) || 1), total);
  const progress = `正在压缩 (${current}/${total}) ...`;
  return receivedSseEvents == null
    ? progress : `${progress} ↓ ${Math.max(0, Number(receivedSseEvents) || 0)}`;
}

function refreshCompactActivity(projection, changes, revision) {
  const activity = projection._compactActivity;
  if (!activity) return;
  activity.message.content = compactProgressText(
    activity.kind, activity.stage, null, activity.totalStages,
  );
  activity.message.revision = revision;
  markProjectedMessageChanged(changes, activity.message);
}

function beginCompactActivity(projection, changes, event) {
  const totalStages = compactStageCount(event.kind, event.total_stages);
  const message = appendProjectedMessage(projection, changes, {
    key: `compact:${event.compact_id}`, revision: event.id,
    kind: "notice", content: compactProgressText(event.kind, 1, null, totalStages),
    timestamp: event.timestamp_ms,
  });
  projection._compactActivity = {
    compactId: event.compact_id,
    kind: event.kind,
    totalStages,
    stage: 1,
    message,
  };
}

function advanceCompactActivity(projection, changes, event) {
  const activity = projection._compactActivity;
  if (!activity || activity.compactId !== event.compact_id) return;
  activity.stage = Math.min(activity.stage + 1, activity.totalStages);
  refreshCompactActivity(projection, changes, event.id);
}

function applyCompactApiActivity(projection, apiActivity) {
  const changes = emptyProjectionChanges();
  const activity = projection._compactActivity;
  if (!activity) return changes;
  const receivedSseEvents = apiActivity.active ? apiActivity.receivedSseEvents : null;
  const content = compactProgressText(
    activity.kind, activity.stage, receivedSseEvents, activity.totalStages,
  );
  if (activity.message.content === content) return changes;
  activity.message.content = content;
  activity.message.presentationRevision = (activity.message.presentationRevision || 0) + 1;
  markProjectedMessageChanged(changes, activity.message);
  return changes;
}

function finishCompactActivity(projection, changes, event, content) {
  const activity = projection._compactActivity;
  if (activity && activity.compactId === event.compact_id) {
    activity.message.content = content;
    activity.message.timestamp = event.timestamp_ms;
    activity.message.revision = event.id;
    delete activity.message.presentationRevision;
    markProjectedMessageChanged(changes, activity.message);
    projection._compactActivity = null;
  } else {
    addNotice(projection, changes, content, event);
  }
}

function toolInfoText(content) {
  if (!content) return "";
  if (content.kind === "text") return content.value || "";
  if (content.kind === "terminal") {
    return (content.value?.rows || []).map((row) => `${String(row.row).padStart(6, "0")}: ${terminalRowText(row)}`).join("\n");
  }
  return "";
}

function terminalRowText(row) {
  let output = "", column = 0;
  for (const run of row.runs || []) {
    if (run.col > column) output += " ".repeat(run.col - column);
    output += run.text;
    column = run.col + run.width;
  }
  return output.replace(/\s+$/, "");
}

function safeJson(value) {
  try { return JSON.parse(value); } catch { return null; }
}

function toolIsChatVisible(name) {
  if (toolIsWorkerActivity(name)) return false;
  const policy = state.snapshot.tool_visibility || {};
  return !(policy.hidden_names || []).includes(name)
    && !(policy.hidden_prefixes || []).some((prefix) => name.startsWith(prefix));
}

function toolIsWorkerActivity(name) {
  return (state.snapshot.tool_visibility?.activity_names || []).includes(name);
}

function emptyWorkMap() {
  return {
    memory: { facts: [], agreements: [] }, history: [], current: null, recordCount: 0,
    _records: new Map(),
  };
}

function projectWorkMap(events) {
  const workmap = emptyWorkMap();
  consumeWorkMapEvents(workmap, events);
  return workmap;
}

function consumeWorkMapEvents(workmap, events) {
  let changed = false;
  for (const event of events) {
    const [kind, value] = eventParts(event);
    if (kind === "ContextCleared") {
      workmap._records.clear();
      changed = true;
      continue;
    }
    if (kind !== "WorkMapMutation") continue;
    for (const record of value.mutation.records || []) {
      const recordType = record.kind;
      const data = record.record;
      if (data?.id) {
        workmap._records.set(data.id, { recordType, ...data });
        changed = true;
      }
    }
  }
  if (!changed) return false;
  materializeWorkMap(workmap);
  return true;
}

function materializeWorkMap(workmap) {
  const records = workmap._records;
  const objectives = [...records.values()].filter((record) => record.recordType === "objective")
    .sort((a, b) => a.created_at_ms - b.created_at_ms);
  const plans = [...records.values()].filter((record) => record.recordType === "plan");
  const notes = [...records.values()].filter((record) => record.recordType === "note");
  const memories = [...records.values()].filter((record) => record.recordType === "memory" && record.state === "active");
  const objectiveSnapshot = (objective) => ({
    objective,
    plans: plans.filter((plan) => plan.objective_id === objective.id)
      .sort((a, b) => a.order - b.order)
      .map((plan) => ({ plan, notes: notes.filter((note) => note.plan_id === plan.id).sort((a, b) => a.sequence - b.sequence) })),
  });
  const current = objectives.find((objective) => objective.state === "active");
  workmap.memory = {
    facts: memories.filter((memory) => memory.kind === "fact"),
    agreements: memories.filter((memory) => memory.kind === "agreement"),
  };
  workmap.history = objectives.filter((objective) => objective.state !== "active")
    .map(objectiveSnapshot);
  workmap.current = current ? objectiveSnapshot(current) : null;
  workmap.recordCount = objectives.length + plans.length + notes.length + memories.length;
}

function flushPendingRender() {
  const request = state.pendingRender;
  state.pendingRender = emptyRenderRequest();
  if (request.full) {
    renderAll();
    return;
  }
  renderIncremental(request);
}

function renderAll() {
  state.pendingRender = emptyRenderRequest();
  const changes = advanceCurrentProjection();
  const promptConfirmed = beginConfirmedPromptRender(changes);
  applyCompactApiActivity(currentProjection(), state.apiActivity);
  renderConnection();
  renderAgents();
  renderTabs();
  renderAgentControls();
  renderTranscript(true, 0);
  renderObjective();
  renderWorkMap();
  if (promptConfirmed) finishPendingPromptSubmission(state.selectedAgent);
  renderComposer();
  renderStatus();
  transcriptBottomFollower.layoutChanged();
  if (state.view.kind === "terminal") void renderTerminal();
}

function renderIncremental(request) {
  const changes = request.currentEvents ? advanceCurrentProjection() : emptyProjectionChanges();
  if (request.apiActivity || request.currentEvents) {
    const activityChanges = applyCompactApiActivity(currentProjection(), state.apiActivity);
    if (activityChanges.transcript) {
      changes.transcript = true;
      changes.transcriptFrom = changes.transcriptFrom == null
        ? activityChanges.transcriptFrom
        : Math.min(changes.transcriptFrom, activityChanges.transcriptFrom);
    }
  }
  const promptConfirmed = beginConfirmedPromptRender(changes);
  let transcriptChanged = false;
  if (request.connection) renderConnection();
  if (request.agents) renderAgents();
  if (request.tabs) renderTabs();
  if (changes.transcript) {
    renderTranscript(Boolean(changes.fullReplay), changes.transcriptFrom ?? 0);
    transcriptChanged = true;
  } else if (request.workerEvents && state.view.kind === "chat") {
    refreshWorkerActivityCards();
    transcriptChanged = true;
  }
  if (changes.workmap) {
    renderObjective();
    if (state.view.kind === "workmap") renderWorkMap();
  }
  if (promptConfirmed) finishPendingPromptSubmission(state.selectedAgent);
  if (changes.turn || promptConfirmed) renderComposer();
  if (request.status || changes.status) renderStatus();
  if (transcriptChanged || promptConfirmed) {
    transcriptBottomFollower.layoutChanged();
  }
  if (state.view.kind === "terminal") void renderTerminal();
}

function advanceCurrentProjection() {
  const store = currentStore();
  if (!store) return emptyProjectionChanges();
  if (store.needsReplay) {
    store.projection = projectChat(store.events);
    store.workmap = projectWorkMap(store.events);
    store.projectedOrder = store.events.length;
    store.needsReplay = false;
    return markPendingPromptConfirmation(store, {
      transcript: true, status: true, turn: true, workmap: true, fullReplay: true,
    });
  }
  const appended = store.events.slice(store.projectedOrder);
  if (!appended.length) return markPendingPromptConfirmation(store, emptyProjectionChanges());
  const fullReplay = chatAppendNeedsReplay(appended);
  const changes = fullReplay
    ? { transcript: true, transcriptFrom: 0, status: true, turn: true }
    : consumeChatEvents(store.projection, appended);
  if (fullReplay) store.projection = projectChat(store.events);
  changes.workmap = consumeWorkMapEvents(store.workmap, appended);
  changes.fullReplay = fullReplay;
  store.projectedOrder = store.events.length;
  return markPendingPromptConfirmation(store, changes);
}

function transcriptIsNearBottom() {
  return elements.transcript.scrollHeight
    - elements.transcript.scrollTop
    - elements.transcript.clientHeight <= TRANSCRIPT_BOTTOM_THRESHOLD_PX;
}

function createTranscriptBottomFollower(viewport, content, onPositionChange, runtime = {}) {
  return MeTranscript.createTranscriptBottomFollower(
    viewport,
    content,
    onPositionChange,
    { threshold: TRANSCRIPT_BOTTOM_THRESHOLD_PX, ...runtime },
  );
}

function updateScrollToBottomButton() {
  const overflow = elements.transcript.scrollHeight - elements.transcript.clientHeight
    > TRANSCRIPT_BOTTOM_THRESHOLD_PX;
  const visible = state.view.kind === "chat" && overflow && !transcriptIsNearBottom();
  elements.scrollToBottom.classList.toggle("hidden", !visible);
}

function scrollTranscriptToBottomAfterLayout() {
  if (state.view.kind !== "chat") return;
  transcriptBottomFollower.follow();
}

function suspendTranscriptAutoFollow() {
  transcriptBottomFollower.noteUserInteraction();
}

function beginTranscriptUserInteraction() {
  transcriptBottomFollower.beginUserInteraction();
}

function endTranscriptUserInteraction() {
  transcriptBottomFollower.endUserInteraction();
}

function finishTranscriptScrolling() {
  transcriptBottomFollower.noteScrollEnd();
}

function renderConnection() {
  const environment = state.snapshot.environment;
  elements.environment.textContent = environment
    ? `${environment.system} · ${window.location.host}`
    : window.location.host;
  elements.environment.title = environment?.workspace || "";
}

function renderAgents() {
  const workspaces = state.gateway.workspaces || [];
  const chat = workspaces.find((workspace) => workspace.builtin);
  const external = workspaces.filter((workspace) => !workspace.builtin);
  if (state.agentMenu && !state.snapshot.agents.some((agent) => agent.id === state.agentMenu.agentId)) closeAgentMenu();
  for (let index = 0; index < external.length; index += 1) {
    const workspace = external[index];
    let group = elements.workspaceList.children[index];
    if (!group || group.dataset.workspaceGroup !== workspace.id) {
      while (elements.workspaceList.children.length > index) elements.workspaceList.lastElementChild.remove();
      group = createWorkspaceGroup(workspace);
      elements.workspaceList.append(group);
    }
    updateWorkspaceGroup(group, workspace);
    renderWorkspaceAgentRows(group.querySelector("[data-workspace-agents]"), workspace.id);
  }
  while (elements.workspaceList.children.length > external.length) elements.workspaceList.lastElementChild.remove();
  renderWorkspaceAgentRows(elements.agents, chat?.id || "chat");
  if (state.workspaceMenu) {
    const trigger = [...elements.workspaceList.querySelectorAll("[data-workspace-menu]")]
      .find((button) => button.dataset.workspaceMenu === state.workspaceMenu.workspaceId);
    if (trigger) {
      state.workspaceMenu.trigger = trigger;
      trigger.setAttribute("aria-expanded", "true");
    } else closeWorkspaceMenu();
  }
  if (state.agentMenu) {
    const trigger = [...document.querySelectorAll("[data-workspace-id] [data-agent-menu]")]
      .find((button) => button.closest("[data-workspace-id]")?.dataset.workspaceId === state.workspaceId
        && button.dataset.agentMenu === state.agentMenu.agentId);
    if (trigger) {
      state.agentMenu.trigger = trigger;
      trigger.setAttribute("aria-expanded", "true");
    } else closeAgentMenu();
  }
}

function createWorkspaceGroup(workspace) {
  const template = document.createElement("template");
  template.innerHTML = `<section class="workspace-group" data-workspace-group="${escapeAttr(workspace.id)}">
    <header class="workspace-row">
      <button class="workspace-select" type="button" data-workspace-select="${escapeAttr(workspace.id)}"><strong></strong><span></span></button>
      <button class="workspace-add-agent" type="button" data-workspace-add="${escapeAttr(workspace.id)}">＋</button>
      <button class="workspace-actions" type="button" data-workspace-menu="${escapeAttr(workspace.id)}" aria-haspopup="menu" aria-expanded="false">···</button>
    </header>
    <div class="workspace-agent-list" data-workspace-agents="${escapeAttr(workspace.id)}"></div>
  </section>`;
  const group = template.content.firstElementChild;
  group.querySelector("[data-workspace-select]").addEventListener("click", () => activateWorkspace(workspace.id));
  group.querySelector("[data-workspace-add]").addEventListener("click", () => {
    activateWorkspace(workspace.id);
    openAddAgent();
  });
  group.querySelector("[data-workspace-menu]").addEventListener("click", (event) => {
    event.stopPropagation();
    openWorkspaceMenu(event.currentTarget, workspace.id);
  });
  return group;
}

function updateWorkspaceGroup(group, workspace) {
  group.classList.toggle("active", workspace.id === state.workspaceId);
  const select = group.querySelector("[data-workspace-select]");
  const add = group.querySelector("[data-workspace-add]");
  const actions = group.querySelector("[data-workspace-menu]");
  select.title = workspace.path;
  const title = select.querySelector("strong");
  const path = select.querySelector("span");
  if (title.textContent !== workspace.name) title.textContent = workspace.name;
  if (path.textContent !== workspace.path) path.textContent = workspace.path;
  add.setAttribute("aria-label", `在 ${workspace.name}中新建会话`);
  actions.setAttribute("aria-label", `打开 ${workspace.name} 的工作区选项`);
}

function renderWorkspaceAgentRows(container, workspaceId) {
  if (!container) return;
  const bucket = workspaceId === state.workspaceId ? state : gatewayWorkspaceState(workspaceId);
  const agents = bucket.snapshot?.agents || [];
  if (!agents.length) {
    if (!container.querySelector(":scope > .empty-state")) container.innerHTML = `<div class="empty-state">暂无会话</div>`;
    return;
  }
  if (container.querySelector(":scope > .empty-state")) replaceElementChildren(container);
  for (let index = 0; index < agents.length; index += 1) {
    const agent = agents[index];
    let row = container.children[index];
    if (!row || row.dataset.agentRow !== agent.id || row.dataset.workspaceId !== workspaceId) {
      while (container.children.length > index) container.lastElementChild.remove();
      row = createAgentRow(agent, workspaceId);
      container.append(row);
    }
    updateAgentRow(row, agent, workspaceId, bucket);
  }
  while (container.children.length > agents.length) container.lastElementChild.remove();
}

function updateAgentRow(row, agent, workspaceId, bucket) {
  const summary = bucket.stores.get(agent.id)?.summary;
  const active = API_ACTIVE.has(summary?.apiState);
  const label = agent.title || agent.id;
  const secondary = [agent.orchestrator === "worker-agent" ? "Worker" : agent.kind === "sub-agent" ? "子会话" : null, summary?.model]
    .filter(Boolean).join(" · ") || "新会话";
  row.classList.toggle("active", workspaceId === state.workspaceId && agent.id === state.selectedAgent);
  row.querySelector(".agent-dot").classList.toggle("active", active);
  const title = row.querySelector(".agent-label strong");
  const detail = row.querySelector(".agent-label span");
  if (title.textContent !== label) title.textContent = label;
  if (detail.textContent !== secondary) detail.textContent = secondary;
  row.querySelector(".agent-actions").setAttribute("aria-label", `打开 ${label} 的操作菜单`);
}

function createAgentRow(agent, workspaceId = state.workspaceId) {
  const template = document.createElement("template");
  template.innerHTML = `<div class="agent-row" data-agent-row="${escapeAttr(agent.id)}" data-workspace-id="${escapeAttr(workspaceId)}">
    <button class="agent-item" type="button" data-agent="${escapeAttr(agent.id)}">
      <span class="agent-dot"></span>
      <span class="agent-label"><strong></strong><span></span></span>
    </button>
    <button class="agent-actions" type="button" data-agent-menu="${escapeAttr(agent.id)}" aria-haspopup="menu" aria-expanded="false">···</button>
  </div>`;
  const row = template.content.firstElementChild;
  row.querySelector(".agent-item").addEventListener("click", () => selectWorkspaceAgent(workspaceId, agent.id));
  row.querySelector(".agent-actions").addEventListener("click", (event) => {
    event.stopPropagation();
    selectWorkspaceAgent(workspaceId, agent.id);
    requestAnimationFrame(() => {
      const trigger = [...document.querySelectorAll("[data-workspace-id] [data-agent-menu]")]
        .find((node) => node.closest("[data-workspace-id]")?.dataset.workspaceId === workspaceId
          && node.dataset.agentMenu === agent.id);
      if (trigger) openAgentMenu(trigger, agent.id);
    });
  });
  return row;
}

function selectWorkspaceAgent(workspaceId, agentId) {
  if (state.workspaceId !== workspaceId) activateWorkspace(workspaceId, agentId);
  else selectAgent(agentId);
}

function selectAgent(id) {
  closeContextDrawer();
  closeMobileSidebar();
  closeUserMessageMenu();
  closeAgentMenu();
  closeWorkspaceMenu();
  saveDraft();
  state.selectedAgent = id;
  transcriptBottomFollower.follow();
  state.view = { kind: "chat", sessionId: null };
  state.terminals = [];
  state.terminalRevisions.clear();
  state.terminalFollowBottom = true;
  delete elements.terminalScreen.dataset.terminalKey;
  delete elements.terminalScreen.dataset.revision;
  restoreDraft();
  renderAll();
  persistGatewaySelection(state.workspaceId, id);
  requestHttpSyncNow();
}

function renderTabs() {
  elements.tabs.querySelectorAll("button[data-view]").forEach((button) => button.classList.toggle("active", state.view.kind === button.dataset.view));
  elements.terminalTabs.innerHTML = state.terminals.map((session) =>
    `<button data-terminal="${escapeAttr(session.session_id)}" class="${state.view.kind === "terminal" && state.view.sessionId === session.session_id ? "active" : ""}">Terminal · ${escapeHtml(session.session_id)}</button>`
  ).join("");
  elements.terminalTabs.querySelectorAll("[data-terminal]").forEach((button) => button.addEventListener("click", () => {
    showView({ kind: "terminal", sessionId: button.dataset.terminal });
  }));
  elements.chatView.classList.toggle("active", state.view.kind === "chat");
  elements.workmapView.classList.toggle("active", state.view.kind === "workmap");
  elements.terminalView.classList.toggle("active", state.view.kind === "terminal");
}

function showView(view) {
  flushPendingRender();
  if (view.kind === "terminal"
      && (state.view.kind !== "terminal" || state.view.sessionId !== view.sessionId)) {
    state.terminalFollowBottom = true;
  }
  state.view = view;
  renderTabs();
  renderObjective();
  if (state.view.kind === "workmap") renderWorkMap();
  renderComposer();
  renderStatus();
  updateScrollToBottomButton();
  if (state.view.kind === "terminal") void renderTerminal();
}

function renderAgentControls() {
  const meta = agentMeta();
  elements.mobileDeleteAgent.disabled = !meta;
}

function openMobileSidebar() {
  document.body.classList.add("mobile-sidebar-open");
  elements.mobileSidebarToggle.setAttribute("aria-expanded", "true");
}

function closeMobileSidebar() {
  document.body.classList.remove("mobile-sidebar-open");
  elements.mobileSidebarToggle.setAttribute("aria-expanded", "false");
  closeAgentMenu();
}

function renderTranscript(forceFull = false, changedFrom = 0) {
  const projection = currentProjection();
  const messages = projection.messages;
  if (forceFull && state.userMenu && (state.userMenu.agentId !== state.selectedAgent
    || !messages.some((message) => message.kind === "user" && message.eventId === state.userMenu.eventId))) {
    closeUserMessageMenu();
  }
  if (!messages.length) {
    const environment = state.snapshot.environment;
    const rendered = `<div class="empty-state"><div><strong>ME-S</strong><p>从这里开始一段对话。</p>${environment ? `<small>${escapeHtml(environment.workspace)}<br>${escapeHtml(environment.system)}</small>` : ""}</div></div>`;
    MeTranscript.reconcileHtmlChildren(elements.transcriptContent, rendered);
    return;
  }
  reconcileTranscript(messages, forceFull ? 0 : changedFrom);
}

function reconcileTranscript(messages, changedFrom = 0) {
  const start = Math.max(0, Math.min(changedFrom, messages.length));
  const existing = new Map([...elements.transcriptContent.children].slice(start)
    .filter((node) => node.dataset.messageKey)
    .map((node) => [node.dataset.messageKey, node]));
  let previousKind = previousVisibleRenderedKind(start);
  for (let index = start; index < messages.length; index += 1) {
    const message = messages[index];
    const visible = messageIsVisible(message);
    const afterTool = visible && isToolLikeKind(previousKind) && message.kind === "assistant";
    const followsTool = visible && previousKind === "tool" && message.kind === "tool";
    const key = messageDomKey(message, index);
    const revision = messageRenderRevision(message, afterTool, followsTool);
    let node = elements.transcriptContent.children[index];
    if (!node || node.dataset.messageKey !== key) {
      node = existing.get(key) || createMessageNode(message, afterTool, followsTool, index);
      elements.transcriptContent.insertBefore(
        node,
        elements.transcriptContent.children[index] || null,
      );
    }
    if (node.meRenderRevision !== revision) updateMessageNode(node, message, afterTool, followsTool, index);
    if (visible) previousKind = message.kind;
  }
  while (elements.transcriptContent.children.length > messages.length) {
    elements.transcriptContent.lastElementChild.remove();
  }
}

function previousVisibleRenderedKind(index) {
  for (let previous = index - 1; previous >= 0; previous -= 1) {
    const node = elements.transcriptContent.children[previous];
    if (node?.dataset.messageVisible === "true") return node.dataset.messageKind || null;
  }
  return null;
}

function messageIsVisible(message) {
  if (message.kind === "assistant") return Boolean(message.content.trim());
  if (message.kind === "worker-activity") return workerWaitIsVisible(message.tool);
  return true;
}

function createMessageNode(message, afterTool, followsTool, index) {
  const template = document.createElement("template");
  template.innerHTML = renderMessageHtml(message, afterTool, followsTool).trim();
  const node = template.content.firstElementChild;
  initializeMessageNode(node, message, afterTool, followsTool, index);
  return node;
}

function updateMessageNode(node, message, afterTool, followsTool, index) {
  const visible = messageIsVisible(message);
  if (node.dataset.messageVisible !== String(visible)) {
    node.replaceWith(createMessageNode(message, afterTool, followsTool, index));
    return;
  }
  if (!visible) {
    node.meRenderRevision = messageRenderRevision(message, afterTool, followsTool);
    return;
  }
  if (message.kind === "assistant") {
    node.classList.toggle("after-tool", afterTool);
    const markdown = node.querySelector(":scope > .markdown");
    const rendered = renderMarkdown(message.content.trim());
    MeTranscript.reconcileHtmlChildren(markdown, rendered);
    node.meRenderRevision = messageRenderRevision(message, afterTool, followsTool);
    return;
  }
  if (message.kind === "worker-activity") {
    updateWorkerActivityNode(node, message.tool);
    node.meRenderRevision = messageRenderRevision(message, afterTool, followsTool);
    return;
  }
  if (message.kind === "tool") {
    updateToolCardNode(node, message.tool, followsTool);
    node.meRenderRevision = messageRenderRevision(message, afterTool, followsTool);
    return;
  }
  node.replaceWith(createMessageNode(message, afterTool, followsTool, index));
}

function initializeMessageNode(node, message, afterTool, followsTool, index) {
  node.dataset.messageKey = messageDomKey(message, index);
  node.dataset.messageVisible = String(messageIsVisible(message));
  node.dataset.messageKind = messageIsVisible(message) ? message.kind : "";
  node.meRenderRevision = messageRenderRevision(message, afterTool, followsTool);
  if (message.kind === "tool") bindToolCard(node);
  if (message.kind === "user") bindUserMessage(node, message);
  if (message.kind === "turn-toolbar") bindTurnToolbar(node, message);
}

function messageDomKey(message, index) {
  return `${state.selectedAgent}:${message.key || `${message.kind}:${message.timestamp}:${index}`}`;
}

function renderMessageHtml(message, afterTool, followsTool = false) {
  if (!messageIsVisible(message)) return `<div class="message-block projection-hidden hidden" aria-hidden="true"></div>`;
  if (message.kind === "user") return `<div class="message-block user"><div class="user-message-content"> ${escapeHtml(message.content)}</div><button class="user-message-actions" type="button" aria-label="消息操作" aria-haspopup="menu" aria-expanded="false">···</button></div>`;
  if (message.kind === "assistant") return `<div class="message-block assistant ${afterTool ? "after-tool" : ""}"><span class="block-marker">●</span><div class="markdown">${renderMarkdown(message.content.trim())}</div></div>`;
  if (message.kind === "turn-toolbar") return `<div class="message-block turn-toolbar" aria-label="本轮用时"><span>▶ 用时 ${formatTurnElapsed(message.durationMs)} · ${formatTurnTokens(message.tokenCount)} · ${formatTurnCompletedAt(message.timestamp)}</span><div class="turn-actions"><button class="clone-turn" type="button">克隆</button><button class="regenerate-turn" type="button">重新生成</button></div></div>`;
  if (message.kind === "tool") return renderToolCard(message.tool, followsTool);
  if (message.kind === "worker-activity") return renderWorkerActivity(message.tool);
  const className = message.kind === "session" ? "session" : "notice";
  return `<div class="message-block ${className}"><span class="block-marker">●</span><div class="${className}-content">${escapeHtml(message.content)}</div></div>`;
}

function bindUserMessage(node, message) {
  const trigger = node.querySelector(".user-message-actions");
  trigger.addEventListener("click", (event) => {
    event.stopPropagation();
    openUserMessageMenu(trigger, message);
  });
}

function bindTurnToolbar(node, message) {
  const agentId = state.selectedAgent;
  const readOnly = agentMeta()?.kind === "sub-agent";
  const cloneButton = node.querySelector(".clone-turn");
  const regenerateButton = node.querySelector(".regenerate-turn");
  cloneButton.disabled = readOnly;
  regenerateButton.disabled = readOnly;
  if (readOnly) {
    cloneButton.title = "只读会话";
    regenerateButton.title = "只读会话";
    return;
  }
  cloneButton.addEventListener("click", async () => {
    const button = cloneButton;
    button.disabled = true;
    try {
      const payload = await sendCommand({
        command: "clone_agent",
        agent_id: agentId,
        final_answer_event_id: message.finalAnswerEventId,
      });
      const id = payload?.receipt?.agent_id;
      if (id) state.pendingAgentSelection = id;
    } catch (error) { toast(error.message, true); }
    finally { button.disabled = false; }
  });
  regenerateButton.addEventListener("click", () => {
    openConfirm(
      "重新生成这条回复？",
      "这条回复及其后的内容将被永久移除，并从对应的用户消息重新生成。",
      "重新生成",
      () => sendCommand({
        command: "regenerate",
        agent_id: agentId,
        final_answer_event_id: message.finalAnswerEventId,
      }),
      true,
    );
  });
}

function openUserMessageMenu(trigger, message) {
  closeAgentMenu();
  closeUserMessageMenu();
  const rewindable = message.rewindable && agentMeta()?.kind !== "sub-agent";
  state.userMenu = {
    agentId: state.selectedAgent,
    eventId: message.eventId,
    content: message.content,
    trigger,
    rewindable,
    deletable: rewindable,
  };
  trigger.setAttribute("aria-expanded", "true");
  elements.rewindUserMessage.disabled = !rewindable;
  elements.rewindUserMessage.title = rewindable ? "" : "无法撤回到此消息";
  elements.deleteUserTurn.disabled = !rewindable;
  elements.deleteUserTurn.title = rewindable ? "" : "无法删除这一轮";
  elements.userMessageMenu.classList.remove("hidden");
  const triggerRect = trigger.getBoundingClientRect();
  const menuRect = elements.userMessageMenu.getBoundingClientRect();
  const margin = 8;
  const left = Math.max(margin, Math.min(triggerRect.right - menuRect.width, window.innerWidth - menuRect.width - margin));
  const below = triggerRect.bottom + 5;
  const top = below + menuRect.height <= window.innerHeight - margin
    ? below
    : Math.max(margin, triggerRect.top - menuRect.height - 5);
  elements.userMessageMenu.style.left = `${left}px`;
  elements.userMessageMenu.style.top = `${top}px`;
}

function closeUserMessageMenu() {
  state.userMenu?.trigger?.setAttribute("aria-expanded", "false");
  state.userMenu = null;
  elements.userMessageMenu.classList.add("hidden");
}

function openAgentMenu(trigger, agentId) {
  closeUserMessageMenu();
  closeAgentMenu();
  state.agentMenu = { agentId, trigger };
  trigger.setAttribute("aria-expanded", "true");
  elements.agentMenu.classList.remove("hidden");
  const triggerRect = trigger.getBoundingClientRect();
  const menuRect = elements.agentMenu.getBoundingClientRect();
  const margin = 8;
  const left = Math.max(margin, Math.min(triggerRect.right - menuRect.width, window.innerWidth - menuRect.width - margin));
  const below = triggerRect.bottom + 5;
  const top = below + menuRect.height <= window.innerHeight - margin
    ? below
    : Math.max(margin, triggerRect.top - menuRect.height - 5);
  elements.agentMenu.style.left = `${left}px`;
  elements.agentMenu.style.top = `${top}px`;
}

function closeAgentMenu() {
  state.agentMenu?.trigger?.setAttribute("aria-expanded", "false");
  state.agentMenu = null;
  elements.agentMenu.classList.add("hidden");
}

function openWorkspaceMenu(trigger, workspaceId) {
  closeUserMessageMenu();
  closeAgentMenu();
  closeWorkspaceMenu();
  state.workspaceMenu = { workspaceId, trigger };
  trigger.setAttribute("aria-expanded", "true");
  elements.workspaceMenu.classList.remove("hidden");
  const triggerRect = trigger.getBoundingClientRect();
  const menuRect = elements.workspaceMenu.getBoundingClientRect();
  const margin = 8;
  const left = Math.max(margin, Math.min(triggerRect.right - menuRect.width, window.innerWidth - menuRect.width - margin));
  const below = triggerRect.bottom + 5;
  const top = below + menuRect.height <= window.innerHeight - margin
    ? below : Math.max(margin, triggerRect.top - menuRect.height - 5);
  elements.workspaceMenu.style.left = `${left}px`;
  elements.workspaceMenu.style.top = `${top}px`;
}

function closeWorkspaceMenu() {
  state.workspaceMenu?.trigger?.setAttribute("aria-expanded", "false");
  state.workspaceMenu = null;
  elements.workspaceMenu.classList.add("hidden");
}

async function copyTextToClipboard(content) {
  if (navigator.clipboard?.writeText && window.isSecureContext) {
    await navigator.clipboard.writeText(content);
    return;
  }
  const scratch = document.createElement("textarea");
  scratch.value = content;
  scratch.setAttribute("readonly", "");
  scratch.style.position = "fixed";
  scratch.style.opacity = "0";
  document.body.append(scratch);
  scratch.select();
  const copied = document.execCommand("copy");
  scratch.remove();
  if (!copied) throw new Error("浏览器未允许复制到剪贴板");
}

function messageRenderRevision(message, afterTool, followsTool = false) {
  const expanded = message.kind === "tool"
    && state.expandedTools.has(`${state.selectedAgent}:${message.tool.id}`);
  const revision = message.kind === "tool" || message.kind === "worker-activity"
    ? message.tool.revision : message.revision;
  const workerRevision = message.kind === "worker-activity"
    ? workerActivityForWait(message.tool)?.revision || 0 : 0;
  return `${revision ?? message.timestamp}:${message.presentationRevision || 0}:${workerRevision}:${afterTool ? 1 : 0}:${followsTool ? 1 : 0}:${expanded ? 1 : 0}`;
}

function isToolLikeKind(kind) {
  return kind === "tool" || kind === "worker-activity";
}

function bindToolCard(card) {
  const toggle = () => {
    if (window.getSelection()?.toString()) return;
    const key = `${state.selectedAgent}:${card.dataset.toolCard}`;
    if (state.expandedTools.has(key)) state.expandedTools.delete(key); else state.expandedTools.add(key);
    const messages = currentProjection().messages;
    const index = messages.findIndex((message) =>
      message.kind === "tool" && String(message.tool.id) === card.dataset.toolCard);
    if (index >= 0) {
      updateToolCardNode(card, messages[index].tool);
      card.meRenderRevision = messageRenderRevision(messages[index], false, card.classList.contains("follows-tool"));
    }
  };
  card.addEventListener("click", toggle);
  card.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") { event.preventDefault(); toggle(); }
  });
}

function renderToolCard(tool, followsTool = false) {
  const view = toolCardView(tool);
  const marker = "●";
  return `<div class="tool-card ${view.status} ${view.expanded ? "expanded" : ""} ${followsTool ? "follows-tool" : ""}" data-tool-card="${escapeAttr(tool.id)}" role="button" tabindex="0" aria-expanded="${view.expanded}">
    <div class="tool-header"><span class="tool-marker">${marker}</span><span class="tool-name">${escapeHtml(tool.name)}</span><span class="tool-brief">${escapeHtml(view.brief)}</span></div>
    ${view.expanded ? renderToolDetails(view.rows) : ""}
  </div>`;
}

function toolCardView(tool) {
  const resultState = normalize(tool.result?.state || "");
  const expanded = state.expandedTools.has(`${state.selectedAgent}:${tool.id}`);
  return {
    expanded,
    status: !tool.result ? "running" : resultState === "succeeded" ? "succeeded" : "failed",
    brief: toolBrief(tool),
    rows: expanded ? toolRows(tool, true) : [],
  };
}

function renderToolDetails(rows) {
  return `<div class="tool-details">${rows.map((row, index) => `<div class="tool-row"><span class="tool-tree">${index + 1 === rows.length ? "└" : "├"}─</span><span class="tool-key">${escapeHtml(row.key)}</span><${row.pre ? "pre" : "span"} class="tool-value ${row.pre ? "tool-output" : ""}"${row.runningStarted == null ? "" : ` data-running-started="${row.runningStarted}"`}>${escapeHtml(row.value)}</${row.pre ? "pre" : "span"}></div>`).join("")}</div>`;
}

function updateToolCardNode(node, tool, followsTool = node.classList.contains("follows-tool")) {
  const view = toolCardView(tool);
  node.className = `tool-card ${view.status} ${view.expanded ? "expanded" : ""} ${followsTool ? "follows-tool" : ""}`;
  node.setAttribute("aria-expanded", String(view.expanded));
  const name = node.querySelector(":scope > .tool-header .tool-name");
  const brief = node.querySelector(":scope > .tool-header .tool-brief");
  if (name.textContent !== tool.name) name.textContent = tool.name;
  if (brief.textContent !== view.brief) brief.textContent = view.brief;
  const details = node.querySelector(":scope > .tool-details");
  if (!view.expanded) {
    details?.remove();
    return;
  }
  const template = document.createElement("template");
  template.innerHTML = renderToolDetails(view.rows);
  const replacement = template.content.firstElementChild;
  if (details) MeTranscript.reconcileNode(details, replacement); else node.append(replacement);
}

function renderWorkerActivity(wait) {
  const view = workerActivityView(wait);
  return `<div class="worker-activity ${view.status}" data-worker-wait="${escapeAttr(wait.id)}">
    <div class="worker-activity-header"><span class="worker-activity-marker">●</span><span class="worker-activity-title">${view.title}</span></div>
    <div class="worker-activity-tools">${view.tools.map(renderWorkerTool).join("")}</div>
  </div>`;
}

function refreshWorkerActivityCards() {
  const projection = currentProjection();
  elements.transcriptContent.querySelectorAll(":scope > [data-worker-wait]").forEach((node) => {
    const message = projection._messageByKey.get(`tool:${node.dataset.workerWait}`);
    if (!message) return;
    updateWorkerActivityNode(node, message.tool);
    node.meRenderRevision = messageRenderRevision(message, false);
  });
}

function workerActivityView(wait) {
  const activity = workerActivityForWait(wait);
  const stateName = activity?.state || workerWaitState(wait);
  return {
    status: stateName === "completed" ? "succeeded"
      : stateName === "running" ? "running" : "failed",
    title: stateName === "completed" ? "已完成"
      : stateName === "interrupted" ? "已中断"
        : stateName === "failed" ? "未完成" : "正在执行",
    tools: activity?.tools || [],
  };
}

function workerToolView(tool) {
  return {
    status: !tool.result ? "running"
      : normalize(tool.result.state) === "succeeded" ? "succeeded" : "failed",
    brief: toolBrief(tool),
  };
}

function renderWorkerTool(tool) {
  const view = workerToolView(tool);
  return `<div class="worker-activity-tool ${view.status}" data-worker-tool="${escapeAttr(tool.id)}"><span class="worker-tool-marker">●</span><span class="worker-tool-name">${escapeHtml(tool.name)}</span><span class="worker-tool-brief">${escapeHtml(view.brief)}</span></div>`;
}

function updateWorkerActivityNode(node, wait) {
  const view = workerActivityView(wait);
  node.className = `worker-activity ${view.status}`;
  const title = node.querySelector(":scope > .worker-activity-header .worker-activity-title");
  if (title.textContent !== view.title) title.textContent = view.title;
  const tools = node.querySelector(":scope > .worker-activity-tools");
  for (let index = 0; index < view.tools.length; index += 1) {
    const tool = view.tools[index];
    let current = tools.children[index];
    if (!current || current.dataset.workerTool !== String(tool.id)) {
      while (tools.children.length > index) tools.lastElementChild.remove();
      const template = document.createElement("template");
      template.innerHTML = renderWorkerTool(tool);
      tools.append(template.content.firstElementChild);
      current = tools.children[index];
    }
    const toolView = workerToolView(tool);
    current.className = `worker-activity-tool ${toolView.status}`;
    const name = current.querySelector(".worker-tool-name");
    const brief = current.querySelector(".worker-tool-brief");
    if (name.textContent !== tool.name) name.textContent = tool.name;
    if (brief.textContent !== toolView.brief) brief.textContent = toolView.brief;
  }
  while (tools.children.length > view.tools.length) tools.lastElementChild.remove();
}

function workerActivityForWait(wait) {
  const worker = state.snapshot.agents.find((agent) => agent.kind === "sub-agent"
    && agent.orchestrator === "worker-agent" && agent.parent_agent_id === state.selectedAgent);
  if (!worker) return null;
  const index = workerActivityIndex(worker);
  const targetTurnId = Number(safeJson(wait.result?.detail)?.turn_id);
  let turn = Number.isFinite(targetTurnId) ? index.byPromptId.get(targetTurnId) : null;
  if (!turn) {
    const cutoff = wait.result ? Number(wait.result.finished) : Number.POSITIVE_INFINITY;
    turn = [...index.turns].reverse().find((candidate) => candidate.timestamp <= cutoff) || null;
  }
  return turn ? {
    state: workerWaitState(wait),
    tools: turn.tools,
    revision: turn.revision,
  } : null;
}

function workerActivityIndex(worker) {
  const store = state.stores.get(worker.id);
  const events = store?.events || [];
  let cache = state.workerActivityIndexes.get(worker.id);
  const prefixChanged = !cache
    || cache.mutationRevision !== store?.mutationRevision
    || cache.nextOrder > events.length
    || (cache.nextOrder > 0
      && eventParts(events[cache.nextOrder - 1] || {})[1]?.id !== cache.lastEventId);
  if (prefixChanged) {
    cache = {
      mutationRevision: store?.mutationRevision || 0,
      nextOrder: 0,
      lastEventId: null,
      index: { turns: [], byPromptId: new Map() },
      activeTools: new Map(),
      turn: null,
    };
    state.workerActivityIndexes.set(worker.id, cache);
  }
  while (cache.nextOrder < events.length) {
    const event = events[cache.nextOrder];
    const [kind, value] = eventParts(event);
    if (kind === "ManagerPrompt") {
      cache.turn = {
        promptId: Number(value.id), timestamp: Number(value.timestamp_ms),
        tools: [], revision: value.id,
      };
      cache.index.turns.push(cache.turn);
      cache.index.byPromptId.set(cache.turn.promptId, cache.turn);
      cache.activeTools.clear();
    } else if (cache.turn && kind === "ToolCall") {
      if (toolIsChatVisible(value.name) && !toolIsWorkerActivity(value.name)) {
        const queued = [...cache.activeTools.values()]
          .some((tool) => tool.apiCallId === value.api_call_id);
        const args = safeJson(value.arguments);
        const tool = {
          id: value.id, apiCallId: value.api_call_id, name: value.name, arguments: value.arguments,
          args, started: value.timestamp_ms, queued, sessionId: args?.session_id || null,
          output: "", result: null, revision: value.id,
        };
        cache.turn.tools.push(tool);
        cache.turn.revision = value.id;
        cache.activeTools.set(value.id, tool);
      }
    } else if (cache.turn && kind === "ToolInfoUpdate") {
      const tool = cache.activeTools.get(value.tool_call_id);
      if (tool) {
        tool.output += toolInfoText(value.content);
        tool.revision = value.id;
        cache.turn.revision = value.id;
      }
    } else if (cache.turn && kind === "ToolCallResult") {
      const tool = cache.activeTools.get(value.tool_call_id);
      if (tool) {
        tool.result = { state: value.state, exitCode: value.exit_code, detail: value.detail, finished: value.timestamp_ms };
        tool.revision = value.id;
        cache.turn.revision = value.id;
        if (!tool.sessionId && tool.name === "Terminal.Create") {
          tool.sessionId = safeJson(value.detail)?.session_id || null;
        }
        cache.activeTools.delete(value.tool_call_id);
        const next = [...cache.activeTools.values()]
          .find((candidate) => candidate.apiCallId === tool.apiCallId && candidate.queued);
        if (next) {
          next.queued = false;
          next.started = value.timestamp_ms;
          next.revision = value.id;
        }
      }
    } else if (cache.turn && kind === "TerminalSessionCreated") {
      const tool = cache.activeTools.get(value.tool_call_id);
      if (tool) {
        tool.sessionId = value.session_id;
        tool.revision = value.id;
        cache.turn.revision = value.id;
      }
    }
    cache.nextOrder += 1;
    cache.lastEventId = value?.id ?? cache.lastEventId;
  }
  return cache.index;
}

function workerWaitState(wait) {
  if (!wait.result) return "running";
  if (normalize(wait.result.state) !== "succeeded") return "failed";
  const stateName = normalize(safeJson(wait.result.detail)?.state || "");
  if (stateName === "completed") return "completed";
  if (stateName === "interrupted" || stateName === "stopped") return "interrupted";
  if (stateName === "wait_interrupted") return "running";
  if (stateName === "failed" || stateName === "api_error") return "failed";
  return "running";
}

function workerWaitIsVisible(wait) {
  return !wait.result || workerWaitState(wait) !== "running";
}

function toolBrief(tool) {
  if (tool.result && normalize(tool.result.state) !== "succeeded") {
    const detail = String(tool.result.detail || "").trim().split(/\r?\n/, 1)[0];
    return detail ? `失败: ${detail}` : "失败";
  }
  const parts = [];
  if (tool.sessionId) parts.push(String(tool.sessionId));
  const terminalInput = tool.name.startsWith("Terminal.") ? toolInput(tool) : "";
  if (terminalInput) parts.push(toolPreview(terminalInput));
  if (tool.args) {
    for (const key of ["path", "url", "page_id", "element_id", "query", "command", "instruction", "name"]) {
      const value = tool.args[key];
      if (["string", "number", "boolean"].includes(typeof value) && String(value)
          && !parts.includes(String(value))) parts.push(String(value));
      if (parts.length >= 2) break;
    }
  }
  if (!parts.length) {
    const input = toolInput(tool);
    if (input) parts.push(toolPreview(input));
  }
  return parts.join(" ");
}

function toolRows(tool, expanded) {
  const rows = [];
  if (tool.sessionId) rows.push({ key: "Session", value: tool.sessionId });
  const input = toolInput(tool);
  if (input) rows.push({ key: "Input", value: expanded ? input : toolPreview(input), pre: expanded });
  const output = tool.output || (!tool.result ? "" : tool.result.detail) || (tool.result?.state === "Succeeded" ? "(no output)" : "");
  if (output) rows.push({ key: "Output", value: expanded ? output : toolPreview(output), pre: expanded });
  if (tool.queued) rows.push({ key: "State", value: "Queued" });
  else if (!tool.result) rows.push({ key: "State", value: `Running ... ${formatDuration(Date.now() - tool.started)}`, runningStarted: tool.started });
  else rows.push({ key: "Time use", value: formatDuration(Math.max(0, tool.result.finished - tool.started)) });
  return rows;
}

function toolPreview(value) {
  const preview = String(value).trim().replace(/\s+/g, " ");
  return preview.length > 240 ? `${preview.slice(0, 239)}…` : preview;
}

function toolInput(tool) {
  const args = tool.args;
  if (!args || Object.keys(args).length === 0) return "";
  if (typeof args.content === "string") return args.content;
  if (typeof args.input === "string") return args.input;
  if (tool.name.startsWith("Terminal.") && Array.isArray(args.input)) {
    return args.input.map(terminalInputActionLabel).join("");
  }
  if (Array.isArray(args.events)) return args.events.map((event) => event.text || event.key || JSON.stringify(event)).join("");
  const copy = { ...args };
  delete copy.session_id;
  return Object.keys(copy).length ? JSON.stringify(copy, null, 2) : "";
}

function terminalInputActionLabel(action) {
  if (action?.type === "text") {
    return Array.from(String(action.text || ""), (character) => {
      if (character === "\r" || character === "\n") return "↵";
      if (character === "\t") return "⇥";
      if (character === "\u001b") return "Esc";
      if (character === "\u007f") return "Del";
      const code = character.codePointAt(0);
      return code < 32 ? `\\u{${code.toString(16).padStart(4, "0")}}` : character;
    }).join("");
  }
  if (action?.type !== "key" || typeof action.key !== "string") return "";
  const modifiers = new Set(Array.isArray(action.modifiers) ? action.modifiers : []);
  const prefix = [["ctrl", "Ctrl"], ["alt", "Alt"], ["shift", "Shift"]]
    .filter(([modifier]) => modifiers.has(modifier))
    .map(([, label]) => `${label}+`).join("");
  const plain = modifiers.size === 0;
  const labels = plain ? {
    enter: "↵", escape: "Esc", tab: "⇥", backspace: "⌫", delete: "Del",
    up: "↑", down: "↓", left: "←", right: "→",
  } : {};
  let key = labels[action.key];
  if (!key) key = action.key.length === 1
    ? action.key.toUpperCase()
    : action.key.replace(/(^|_)(.)/g, (_match, _separator, letter) => letter.toUpperCase());
  const repeat = Number(action.repeat) > 1 ? `×${Number(action.repeat)}` : "";
  return `${prefix}${key}${repeat}`;
}

function emptyObjectiveDisclosure() {
  return { scopeId: null, objectiveId: null, expanded: false };
}

function syncObjectiveDisclosure(disclosure, scopeId, objectiveId) {
  const nextScopeId = scopeId ?? null;
  const nextObjectiveId = objectiveId ?? null;
  if (disclosure.scopeId !== nextScopeId || disclosure.objectiveId !== nextObjectiveId) {
    disclosure.scopeId = nextScopeId;
    disclosure.objectiveId = nextObjectiveId;
    disclosure.expanded = false;
  }
  return disclosure;
}

function objectiveSummaryHtml(current, expanded) {
  const active = current.plans.some(({ plan }) => plan.state === "active");
  const toggleLabel = expanded ? "折叠 Objective 详情" : "展开 Objective 详情";
  const details = expanded ? `${current.objective.description ? `<div class="objective-description">${escapeHtml(current.objective.description)}</div>` : ""}
    ${current.plans.map(({ plan, notes }) => `<div class="objective-plan ${plan.state === "active" ? "active" : ""}"><span>${planSymbol(plan.state)}</span><span>${escapeHtml(plan.title)}${notes.length ? ` (${notes.length} ${notes.length === 1 ? "note" : "notes"})` : ""}</span></div>`).join("")}` : "";
  return `<div class="objective-header"><div class="objective-title">${active ? "■" : "□"} ${escapeHtml(current.objective.title)}</div>
    <button class="objective-toggle" data-objective-toggle type="button" title="${toggleLabel}" aria-label="${toggleLabel}" aria-expanded="${expanded}" aria-controls="objective-details"><span class="objective-toggle-icon" aria-hidden="true">${expanded ? "▾" : "▸"}</span></button></div>
    <div id="objective-details" class="objective-details${expanded ? "" : " hidden"}">${details}</div>`;
}

function renderObjective() {
  const current = currentStore()?.workmap.current;
  if (!current) {
    syncObjectiveDisclosure(state.objectiveDisclosure, null, null);
    elements.objective.classList.add("hidden");
    MeTranscript.reconcileHtmlChildren(elements.objective, "");
    return;
  }
  const scopeId = JSON.stringify([state.workspaceId, state.selectedAgent]);
  const disclosure = syncObjectiveDisclosure(state.objectiveDisclosure, scopeId, current.objective.id);
  if (state.view.kind !== "chat") {
    elements.objective.classList.add("hidden");
    MeTranscript.reconcileHtmlChildren(elements.objective, "");
    return;
  }
  elements.objective.classList.remove("hidden");
  MeTranscript.reconcileHtmlChildren(elements.objective, objectiveSummaryHtml(current, disclosure.expanded));
}

function renderWorkMap() {
  const workmap = currentStore()?.workmap || emptyWorkMap();
  const objectives = workmap.history.length + (workmap.current ? 1 : 0);
  const plans = [...workmap.history, ...(workmap.current ? [workmap.current] : [])].reduce((sum, objective) => sum + objective.plans.length, 0);
  const notes = [...workmap.history, ...(workmap.current ? [workmap.current] : [])].reduce((sum, objective) => sum + objective.plans.reduce((value, plan) => value + plan.notes.length, 0), 0);
  elements.workmapCount.textContent = `${workmap.recordCount} records · ${objectives} objectives · ${plans} plans · ${notes} notes`;
  const historyIds = new Set(workmap.history.map(({ objective }) => objective.id));
  for (const id of state.expandedHistoryObjectives) if (!historyIds.has(id)) state.expandedHistoryObjectives.delete(id);
  elements.workmap.innerHTML = `${renderMemory(workmap.memory)}
    <section class="workmap-section"><h2>History (${workmap.history.length})</h2>${workmap.history.length ? workmap.history.map((objective) => renderObjectiveCard(objective, state.expandedHistoryObjectives.has(objective.objective.id), true)).join("") : `<div class="workmap-empty">—</div>`}</section>
    <section class="workmap-section"><h2>Current (${workmap.current ? 1 : 0})</h2>${workmap.current ? renderObjectiveCard(workmap.current, true) : `<div class="workmap-empty">—</div>`}</section>`;
  const toggleHistory = (card) => {
    if (window.getSelection()?.toString()) return;
    const id = card.dataset.historyObjective;
    if (state.expandedHistoryObjectives.has(id)) state.expandedHistoryObjectives.delete(id); else state.expandedHistoryObjectives.add(id);
    renderWorkMap();
  };
  elements.workmap.querySelectorAll("[data-history-objective]").forEach((card) => {
    card.addEventListener("click", () => toggleHistory(card));
    card.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") { event.preventDefault(); toggleHistory(card); }
    });
  });
}

function renderMemory(memory) {
  const count = memory.facts.length + memory.agreements.length;
  if (!count) return `<section class="workmap-section"><h2>Memory (0)</h2><div class="workmap-empty">—</div></section>`;
  const group = (title, items) => `<div class="memory-group"><h3>${title} (${items.length})</h3>${items.length ? items.map((item) => `<div class="memory-item"><span class="memory-label">${escapeHtml(memoryLabel(item))}</span>${escapeHtml(item.content)}</div>`).join("") : `<div class="workmap-empty">—</div>`}</div>`;
  return `<section class="workmap-section"><h2>Memory (${count})</h2>${group("Facts", memory.facts)}${group("Agreements", memory.agreements)}</section>`;
}

function renderObjectiveCard(snapshot, detailed, expandable = false) {
  const objective = snapshot.objective;
  const behavior = expandable ? ` data-history-objective="${escapeAttr(objective.id)}" role="button" tabindex="0" aria-expanded="${detailed}"` : "";
  const disclosure = expandable ? `<span class="disclosure">${detailed ? "▾" : "▸"}</span>` : "";
  return `<article class="objective-card${expandable ? " history-card" : ""}"${behavior}><h3>${disclosure}${objectiveSymbol(objective.state)} ${escapeHtml(objective.title)}</h3>
    ${detailed && objective.description ? `<div class="objective-meta">${escapeHtml(objective.description)}</div>` : ""}
    ${objective.status_reason ? `<div class="objective-meta">${escapeHtml(objective.status_reason)}</div>` : ""}
    ${detailed ? snapshot.plans.map(renderPlanCard).join("") : ""}</article>`;
}

function renderPlanCard({ plan, notes }) {
  return `<div class="plan-card"><div class="plan-title">${planSymbol(plan.state)} ${escapeHtml(plan.title)}</div>
    ${plan.description ? `<div class="plan-detail">${escapeHtml(plan.description)}</div>` : ""}
    ${plan.outcome ? `<div class="plan-detail"><strong>Outcome:</strong> ${escapeHtml(plan.outcome)}</div>` : ""}
    ${plan.verification ? `<div class="plan-detail"><strong>Verification:</strong> ${escapeHtml(plan.verification)}</div>` : ""}
    ${plan.status_reason ? `<div class="plan-detail"><strong>Reason:</strong> ${escapeHtml(plan.status_reason)}</div>` : ""}
    ${notes.map((note) => `<div class="plan-note"><span class="note-kind">${escapeHtml(String(note.kind).toUpperCase())}</span>${escapeHtml(note.content)}</div>`).join("")}</div>`;
}

function renderComposer() {
  const meta = agentMeta();
  const readOnly = meta?.kind === "sub-agent";
  const worker = isWorkerAgent(meta);
  const pending = currentStore()?.pendingPromptSubmission || null;
  const sending = Boolean(pending);
  const canStop = canControlRuntime(meta) && currentProjection().turnState?.state === "active";
  elements.composer.classList.toggle("hidden", !meta);
  elements.composer.classList.toggle("sending", sending);
  elements.input.disabled = readOnly || sending;
  elements.send.disabled = readOnly || sending;
  elements.send.setAttribute("aria-busy", String(sending));
  elements.sendSpinner.classList.toggle("hidden", !sending);
  elements.sendLabel.textContent = pending?.status === "confirming" ? "正在确认" : sending ? "正在发送" : "发送";
  elements.stop.disabled = !canStop;
  elements.input.placeholder = readOnly ? `${worker ? "Worker" : "子 Agent"} 对话只读 · ${childStateLabel(currentStore()?.events || [])}` : "发送消息，输入 / 查看命令";
  elements.inputHint.textContent = sending
    ? "消息进入列表后即可继续输入"
    : worker ? "可调整模型、推理强度或停止当前任务"
      : readOnly ? "子 Agent 仅允许查看" : `${sendShortcutHint()} · Esc 中止/撤回/清空`;
  renderSlashMenu();
}

function renderStatus() {
  const projection = currentProjection();
  const model = state.snapshot.models.find((model) => model.name === projection.model);
  const canChange = canControlRuntime();
  elements.statusModel.textContent = projection.model || "—";
  elements.statusEffort.textContent = projection.effort || "—";
  elements.statusLiveTokens.textContent = state.apiActivity.active
    ? `↓ ${state.apiActivity.receivedSseEvents}` : "";
  elements.statusModelTrigger.disabled = !canChange;
  elements.statusEffortTrigger.disabled = !canChange || !model?.reasoning_efforts?.length;
  elements.statusContextTrigger.disabled = !agentMeta();
  elements.statusContext.textContent = `${formatTokens(projection.apiUsage?.total_tokens)}/${formatLimit(model?.context_window)}`;
  const active = state.apiActivity.active || API_ACTIVE.has(projection.apiState);
  elements.apiSpinner.textContent = active ? API_SPINNER_FRAMES[state.apiAnimationTick % API_SPINNER_FRAMES.length] : "";
  if (state.contextDrawerOpen) renderContextDrawer();
}

const CONTEXT_CATEGORIES = [
  { key: "system", label: "系统提示词", color: "var(--context-system)" },
  { key: "compact", label: "上下文压缩", color: "var(--context-compact)" },
  { key: "memory", label: "记忆", color: "var(--context-memory)" },
  { key: "user", label: "用户消息", color: "var(--context-user)" },
  { key: "model", label: "模型输出", color: "var(--context-assistant)" },
  { key: "tool", label: "工具调用", color: "var(--context-tool)" },
  { key: "reserve", label: "输出预留", color: "var(--context-reserve)" },
];

function latestCommittedUsageBoundary(events, expectedUsage) {
  if (!expectedUsage) return null;
  const effective = effectiveConversationEvents(events);
  const errored = new Set();
  let boundary = null;
  for (const event of effective) {
    const [kind, value] = eventParts(event);
    if (kind !== "ApiStateUpdate") continue;
    if (value.state === "Error") errored.add(value.api_call_id);
    const committed = value.state === "Completed"
      || (value.state === "Interrupted" && !errored.has(value.api_call_id));
    if (committed && value.usage) boundary = value;
  }
  if (!boundary || boundary.usage.total_tokens !== expectedUsage.total_tokens) return null;
  return events.findIndex((event) => eventParts(event)[1].id === boundary.id);
}

function latestCompactPreview(events) {
  const completed = effectiveConversationEvents(events)
    .map(eventParts)
    .find(([kind, value]) => kind === "CompactStateUpdate" && value.state === "Completed")?.[1];
  if (!completed) return { content: null, analysis: null };
  const analysis = events.map(eventParts).find(([kind, value]) =>
    kind === "CompactStateUpdate"
      && value.compact_id === completed.compact_id
      && value.state === "StageCompleted"
      && value.stage === "Analysis")?.[1].content ?? null;
  return { content: completed.content, analysis };
}

function estimateContextBreakdown(events, usage, memoryContent) {
  const empty = { system: 0, compact: 0, memory: 0, user: 0, model: 0, tool: 0 };
  const currentCompact = latestCompactPreview(events);
  const currentCompactContent = currentCompact.content;
  const currentCompactAnalysis = currentCompact.analysis;
  const total = Number(usage?.total_tokens);
  if (!Number.isFinite(total) || total < 0) return { total: null, values: empty, compactContent: currentCompactContent, compactAnalysis: currentCompactAnalysis, memoryContent };
  const boundary = latestCommittedUsageBoundary(events, usage);
  if (boundary == null || boundary < 0) return { total, values: { ...empty, system: total }, compactContent: currentCompactContent, compactAnalysis: currentCompactAnalysis, memoryContent };
  const boundaryId = eventParts(events[boundary])[1].id;
  const persistedEstimate = events.map(eventParts).find(([kind, value]) =>
    kind === "ContextUsageEstimate" && value.api_state_event_id === boundaryId)?.[1];
  if (persistedEstimate) {
    const values = {
      system: Number(persistedEstimate.values?.system || 0),
      compact: Number(persistedEstimate.values?.compact || 0),
      memory: Number(persistedEstimate.values?.memory || 0),
      user: Number(persistedEstimate.values?.user || 0),
      model: Number(persistedEstimate.values?.model || 0),
      tool: Number(persistedEstimate.values?.tool || 0),
    };
    const estimatedTotal = Object.values(values).reduce((sum, value) => sum + value, 0);
    if (Object.values(values).every(Number.isFinite) && estimatedTotal === total) {
      return {
        total,
        values,
        compactContent: currentCompactContent,
        compactAnalysis: currentCompactAnalysis,
        memoryContent: currentCompactContent === null ? null : memoryContent,
      };
    }
  }
  return {
    total,
    values: { ...empty, system: total },
    compactContent: currentCompactContent,
    compactAnalysis: currentCompactAnalysis,
    memoryContent: currentCompactContent === null ? null : memoryContent,
  };
}

function renderContextDrawer() {
  const projection = currentProjection();
  const model = state.snapshot.models.find((candidate) => candidate.name === projection.model);
  const limit = Number(model?.context_window);
  const { total, values: usageValues, compactContent, compactAnalysis, memoryContent } = estimateContextBreakdown(currentStore()?.events || [], projection.apiUsage, currentStore()?.turnHistory ?? null);
  const configuredReserve = Number(model?.output_token_reservations?.[projection.effort] ?? 0);
  const outputReserve = Number.isFinite(configuredReserve) && configuredReserve > 0 ? configuredReserve : 0;
  const values = { ...usageValues, reserve: outputReserve };
  const hasCompact = compactContent !== null;
  const hasMemory = memoryContent !== null;
  state.contextCompactContent = compactContent;
  state.contextCompactAnalysis = compactAnalysis;
  state.contextMemoryContent = memoryContent;
  const validLimit = Number.isFinite(limit) && limit > 0 ? limit : null;
  const percent = total == null || !validLimit ? null : total / validLimit * 100;
  const chartTotal = total == null ? (validLimit || 1) : Math.max(validLimit || total || 1, total || 0);
  const signature = JSON.stringify([state.selectedAgent, projection.model, total, validLimit, values, hasCompact, hasMemory, agentMeta()?.kind]);
  if (state.contextDrawerSignature === signature) return;
  state.contextDrawerSignature = signature;
  const categories = CONTEXT_CATEGORIES.filter((category) => (category.key !== "compact" || hasCompact) && (category.key !== "memory" || hasMemory));
  let offset = 0;
  const segments = categories.map((category) => {
    const rawLength = category.key === "reserve" || total != null ? values[category.key] / chartTotal * 100 : 0;
    const length = Math.max(0, Math.min(rawLength, 100 - offset));
    const formattedValue = formatContextCategoryTokens(category.key, values[category.key]);
    const markup = length <= 0 ? "" : `<circle class="context-ring-segment" cx="90" cy="90" r="68" pathLength="100" transform="rotate(-90 90 90)" stroke="${category.color}" stroke-dasharray="${length} ${100 - length}" stroke-dashoffset="${-offset}"><title>${escapeHtml(category.label)} ${escapeHtml(formattedValue)}</title></circle>`;
    offset += length;
    return markup;
  }).join("");
  elements.contextRing.innerHTML = `<circle class="context-ring-track" cx="90" cy="90" r="68" pathLength="100"></circle>${segments}`;
  elements.contextPercent.textContent = percent == null ? "—" : `${Math.round(percent)}%`;
  elements.contextUsageText.textContent = `${formatTokens(total)} / ${formatLimit(validLimit)}`;
  elements.contextBreakdown.innerHTML = categories.map((category) => {
    const value = category.key === "reserve" ? values.reserve : (total == null ? null : values[category.key]);
    const share = category.key === "reserve"
      ? (validLimit ? value / validLimit * 100 : null)
      : (total > 0 ? value / total * 100 : null);
    const help = category.key === "compact"
      ? `<button class="context-detail-help" type="button" data-detail="compact" aria-label="查看原始压缩内容" title="查看原始压缩内容">?</button>`
      : category.key === "memory"
        ? `<button class="context-detail-help" type="button" data-detail="memory" aria-label="查看记忆内容" title="查看记忆内容">?</button>`
        : "";
    return `<div class="context-breakdown-row"><span class="context-swatch" style="background:${category.color}"></span><span class="context-breakdown-label">${escapeHtml(category.label)}${help}</span><strong class="context-breakdown-value">${escapeHtml(formatContextCategoryTokens(category.key, value))}</strong><span class="context-breakdown-percent">${share == null ? "—" : `${share.toFixed(1)}%`}</span></div>`;
  }).join("");
  elements.contextClear.disabled = !agentMeta() || agentMeta().kind === "sub-agent";
}

function openContextDrawer() {
  if (!agentMeta()) return;
  closeChoiceDrawer();
  closeModal();
  state.contextDrawerOpen = true;
  state.contextDrawerSignature = null;
  elements.statusContextTrigger.setAttribute("aria-expanded", "true");
  renderContextDrawer();
  elements.contextDrawerBackdrop.classList.remove("hidden");
}

function closeContextDrawer() {
  state.contextDrawerOpen = false;
  state.contextDrawerSignature = null;
  state.contextCompactContent = null;
  state.contextCompactAnalysis = null;
  state.contextMemoryContent = null;
  elements.statusContextTrigger.setAttribute("aria-expanded", "false");
  elements.contextDrawerBackdrop.classList.add("hidden");
}

function openContextDetail(title, content, markdown = false) {
  closeContextDrawer();
  elements.compactSummaryTitle.textContent = title;
  if (markdown) {
    elements.compactSummaryContent.innerHTML = renderMarkdown(content);
  } else {
    const pre = document.createElement("pre");
    pre.className = "context-detail-raw";
    pre.textContent = content;
    replaceElementChildren(elements.compactSummaryContent, pre);
  }
  elements.compactSummaryBackdrop.classList.remove("hidden");
  elements.compactSummaryClose.focus();
}

function compactPreviewMarkdown(analysis, summary) {
  const sections = [];
  if (analysis !== null) sections.push(`## Analysis\n\n${analysis}`);
  sections.push(`## 压缩摘要\n\n${summary}`);
  return sections.join("\n\n---\n\n");
}

function closeCompactSummary() {
  elements.compactSummaryBackdrop.classList.add("hidden");
  replaceElementChildren(elements.compactSummaryContent);
}

function confirmContextClear() {
  if (!state.selectedAgent || agentMeta()?.kind === "sub-agent") return;
  const agentId = state.selectedAgent;
  closeContextDrawer();
  openConfirm("清空上下文？", "", "清空上下文", () => sendCommand({ command: "clear_context", agent_id: agentId }), true);
}

function renderTerminal() {
  const sessionId = state.view.sessionId;
  if (!sessionId || !state.selectedAgent) return;
  const revisionKey = `${state.selectedAgent}:${sessionId}`;
  const frame = state.terminalFrames.get(revisionKey);
  if (!frame) {
    showTerminalMessage(state.terminalFramesUnavailable.has(revisionKey)
      ? `Terminal ${sessionId} 已不可用`
      : `正在同步 Terminal ${sessionId}…`);
    requestHttpSyncNow();
    return;
  }
  if (state.view.kind !== "terminal" || state.view.sessionId !== sessionId) return;
    const previousRevision = state.terminalRevisions.get(revisionKey) || 0;
    if (frame.revision < previousRevision) return;
    const sameRenderedFrame = elements.terminalScreen.dataset.terminalKey === revisionKey
      && Number(elements.terminalScreen.dataset.revision) === frame.revision;
    if (sameRenderedFrame) {
      if (state.terminalFollowBottom) scrollTerminalToBottom();
      return;
    }
    const switchingTerminal = elements.terminalScreen.dataset.terminalKey !== revisionKey;
    const scroll = captureTerminalScroll(
      elements.terminalView,
      state.terminalFollowBottom || switchingTerminal,
    );
    state.terminalRevisions.set(revisionKey, frame.revision);
    elements.terminalMessage.classList.add("hidden");
    elements.terminalScreen.classList.remove("hidden");
    elements.terminalScreen.style.width = `${frame.width}ch`;
    const styles = new Map((frame.style_defs || []).map((definition) => [definition.id, definition.style]));
    elements.terminalScreen.innerHTML = (frame.rows || []).map((row) => {
      const runs = (row.runs || []).map((run) => `<span style="left:${run.col}ch;${terminalStyle(styles.get(run.style))}">${escapeHtml(run.text)}</span>`).join("");
      const cursor = frame.cursor.visible && frame.cursor.row === row.row
        ? `<span class="terminal-cursor" style="left:${frame.cursor.col}ch;width:${frame.cursor.wide ? 2 : 1}ch"></span>` : "";
      return `<div class="terminal-row" style="width:${frame.width}ch;position:relative"><span style="position:absolute">${" ".repeat(frame.width)}</span>${runs}${cursor}</div>`;
    }).join("");
    elements.terminalScreen.dataset.terminalKey = revisionKey;
    elements.terminalScreen.dataset.revision = String(frame.revision);
    restoreTerminalScroll(elements.terminalView, scroll);
    state.terminalFollowBottom = scroll.followBottom;
}

function showTerminalMessage(message) {
  elements.terminalMessage.textContent = message;
  elements.terminalMessage.classList.remove("hidden");
  elements.terminalScreen.classList.add("hidden");
  delete elements.terminalScreen.dataset.terminalKey;
  delete elements.terminalScreen.dataset.revision;
}

function terminalIsNearBottom(view) {
  return view.scrollHeight - view.scrollTop - view.clientHeight <= 2;
}

function captureTerminalScroll(view, followBottom) {
  return {
    scrollTop: view.scrollTop,
    followBottom: followBottom || terminalIsNearBottom(view),
  };
}

function restoreTerminalScroll(view, snapshot) {
  view.scrollTop = snapshot.followBottom ? view.scrollHeight : snapshot.scrollTop;
}

function scrollTerminalToBottom() {
  elements.terminalView.scrollTop = elements.terminalView.scrollHeight;
}

function terminalStyle(style = {}) {
  let foreground = colorCss(style.foreground), background = colorCss(style.background);
  if (style.inverse) [foreground, background] = [background || "#0b0c10", foreground || "#f2f3f5"];
  return `position:absolute;${foreground ? `color:${foreground};` : ""}${background ? `background:${background};` : ""}${style.bold ? "font-weight:700;" : ""}${style.dim ? "opacity:.55;" : ""}${style.italic ? "font-style:italic;" : ""}${style.underline ? "text-decoration:underline;" : ""}`;
}

function colorCss(color) {
  if (!color) return null;
  if (color.kind === "rgb") return `rgb(${color.value.join(",")})`;
  if (color.kind !== "indexed") return null;
  const index = color.value;
  const base = ["#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5", "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff"];
  if (index < 16) return base[index];
  if (index >= 232) { const value = 8 + (index - 232) * 10; return `rgb(${value},${value},${value})`; }
  const n = index - 16, levels = [0, 95, 135, 175, 215, 255];
  return `rgb(${levels[Math.floor(n / 36)]},${levels[Math.floor(n / 6) % 6]},${levels[n % 6]})`;
}

function renderSlashMenu() {
  if (currentStore()?.pendingPromptSubmission) {
    elements.slashMenu.classList.add("hidden");
    return;
  }
  const value = elements.input.value;
  const matches = value.startsWith("/") && !value.includes(" ")
    ? COMMANDS.filter(([name]) => name.startsWith(value)) : [];
  if (!matches.length) { elements.slashMenu.classList.add("hidden"); return; }
  state.slashIndex = Math.min(state.slashIndex, matches.length - 1);
  elements.slashMenu.classList.remove("hidden");
  elements.slashMenu.innerHTML = matches.map(([name, description], index) =>
    `<button class="slash-item ${index === state.slashIndex ? "selected" : ""}" data-command="${name}"><strong>${name}</strong><span>${description}</span></button>`).join("");
  elements.slashMenu.querySelectorAll("[data-command]").forEach((button) => button.addEventListener("click", () => openSlashCommand(button.dataset.command)));
}

async function openSlashCommand(name) {
  elements.input.value = "";
  saveDraft();
  state.slashIndex = 0;
  autoSizeInput();
  renderSlashMenu();
  if (!agentMeta() && name !== "/agent-add") return;
  if (name === "/agent-add") return openAddAgent();
  if (name === "/agent-delete") return openDeleteAgent();
  if (name === "/model") return openModel();
  if (name === "/effort") return openEffort();
  if (name === "/clear") return openConfirm("清空上下文？", "", "清空上下文", () => sendCommand({ command: "clear_context", agent_id: state.selectedAgent }));
  if (name === "/rewind") return openRewind();
  if (name === "/exit") { window.close(); toast("浏览器不允许页面自行关闭时，请直接关闭标签页。"); }
}

function openModel() {
  const projection = currentProjection();
  openChoice("切换模型", "选择后将从下一次回复开始生效。", state.snapshot.models.map((model) => ({ value: model.name, label: model.name })), projection.model,
    (model) => sendCommand({ command: "change_model", agent_id: state.selectedAgent, model }));
}

function openEffort() {
  const projection = currentProjection();
  const model = state.snapshot.models.find((candidate) => candidate.name === projection.model);
  openChoice("切换推理强度", "选择后将从下一次回复开始生效。", (model?.reasoning_efforts || []).map((effort) => ({ value: effort, label: effort })), projection.effort,
    (effort) => sendCommand({ command: "change_effort", agent_id: state.selectedAgent, effort }));
}

function openModelDrawer() {
  const agentId = state.selectedAgent;
  if (!agentId || !canControlRuntime()) return;
  const projection = currentProjection();
  openChoiceDrawer("切换模型", "选择后将从下一次回复开始生效。", state.snapshot.models.map((model) => ({
    value: model.name,
    label: model.name,
    detail: `上下文 ${formatLimit(model.context_window)}`,
  })), projection.model, (model) => sendCommand({ command: "change_model", agent_id: agentId, model }));
}

function openEffortDrawer() {
  const agentId = state.selectedAgent;
  if (!agentId || !canControlRuntime()) return;
  const projection = currentProjection();
  const model = state.snapshot.models.find((candidate) => candidate.name === projection.model);
  openChoiceDrawer("切换推理强度", `当前模型：${projection.model || "—"}`, (model?.reasoning_efforts || []).map((effort) => ({
    value: effort,
    label: effort,
  })), projection.effort, (effort) => sendCommand({ command: "change_effort", agent_id: agentId, effort }));
}

function rewindChoices(events) {
  return [...events].reverse().flatMap((event) => {
    const [kind, value] = eventParts(event);
    if (kind === "UserPrompt") return [{ value: value.id, label: collapse(value.content), detail: "用户消息" }];
    if (kind === "ContextCleared") return [{ value: value.id, label: "上下文已清空", detail: "清理位置" }];
    if (kind === "CompactStateUpdate" && value.state === "Completed") return [{ value: value.id, label: "上下文已压缩", detail: "压缩位置" }];
    return [];
  });
}

function openRewind() {
  const choices = rewindChoices(currentStore()?.events || []);
  openChoice("撤回", "所选位置及其后的内容将从当前会话中永久移除。", choices, choices[0]?.value,
    (eventId) => sendCommand({ command: "rewind_context", agent_id: state.selectedAgent, event_id: Number(eventId) }));
}

function openAddAgent() {
  const presentation = {
    "main-agent": ["标准 (main-agent)", "单 Agent 模式，响应直接，Token 开销较低"],
    "manager-agent": ["协作 (manager-agent)", "双 Agent 协作，适合复杂任务，减少主模型上下文占用，但总 Token 开销更高"],
    chatbot: ["聊天 (chatbot)", "仅进行对话，不使用工作工具"],
  };
  const choices = (state.snapshot.orchestrators || []).map((orchestrator) => {
    const [label, detail] = presentation[orchestrator] || [orchestrator, "自定义 Agent"];
    return { value: orchestrator, label, detail };
  });
  openChoice("创建新的会话？", "选择 Agent 类型。创建后不可更改。", choices, state.snapshot.default_orchestrator,
    async (orchestrator) => {
    const payload = await sendCommand({ command: "add_agent", orchestrator });
    const id = payload?.receipt?.agent_id;
    if (id) state.pendingAgentSelection = id;
  });
}

async function openDeleteAgent(agentId = state.selectedAgent) {
  if (!agentId) return;
  try {
    const agent = state.snapshot.agents.find((candidate) => candidate.id === agentId);
    if (!agent) return toast("该会话已不存在", true);
    const label = agent.title || agent.id;
    const payload = await api(`/api/deletion-blocker/${encodeURIComponent(agentId)}`);
    if (payload.blocker) return openConfirm("无法删除会话", `“${label}”当前不可删除：${payload.blocker}`, "返回", async () => {});
    openConfirm("删除会话？", `将永久删除“${label}”及其全部记录。此操作不可恢复。`, "永久删除", async () => {
      await sendCommand({ command: "delete_agent", agent_id: agentId });
    }, true);
  } catch (error) { toast(error.message, true); }
}

function openChoice(title, description, choices, selectedValue, onConfirm) {
  if (!choices.length) return toast("没有可用选项", true);
  const selected = choices.some((choice) => String(choice.value) === String(selectedValue)) ? selectedValue : choices[0].value;
  openModal({ title, description, choices, selected, confirmLabel: "确认", onConfirm });
}

function openChoiceDrawer(title, description, choices, selectedValue, onSelect) {
  if (!choices.length) return toast("没有可用选项", true);
  closeModal();
  closeContextDrawer();
  const selected = choices.some((choice) => String(choice.value) === String(selectedValue)) ? selectedValue : null;
  state.drawer = { title, description, choices, selected, onSelect, busy: false };
  elements.drawerTitle.textContent = title;
  elements.drawerDescription.textContent = description;
  elements.drawerDescription.classList.toggle("hidden", !description);
  elements.drawerContent.innerHTML = choices.map((choice) => {
    const current = String(choice.value) === String(selected);
    return `<button class="drawer-choice ${current ? "current" : ""}" type="button" data-value="${escapeAttr(choice.value)}">
      <span class="drawer-choice-copy"><span class="drawer-choice-label">${escapeHtml(choice.label)}</span>${choice.detail ? `<small>${escapeHtml(choice.detail)}</small>` : ""}</span>
      <span class="drawer-choice-mark">${current ? "✓" : ""}</span>
    </button>`;
  }).join("");
  elements.drawerContent.querySelectorAll(".drawer-choice").forEach((button) => button.addEventListener("click", () => selectDrawerChoice(button.dataset.value)));
  elements.drawerBackdrop.classList.remove("hidden");
  elements.drawerContent.querySelector(".drawer-choice.current")?.focus();
}

function closeChoiceDrawer() {
  if (state.drawer?.busy) return;
  state.drawer = null;
  elements.drawerBackdrop.classList.add("hidden");
}

async function selectDrawerChoice(value) {
  const drawer = state.drawer;
  if (!drawer || drawer.busy) return;
  if (String(value) === String(drawer.selected)) {
    closeChoiceDrawer();
    return;
  }
  drawer.busy = true;
  elements.drawerContent.querySelectorAll(".drawer-choice").forEach((button) => { button.disabled = true; });
  try {
    await drawer.onSelect(value);
    drawer.busy = false;
    closeChoiceDrawer();
  } catch (error) {
    drawer.busy = false;
    elements.drawerContent.querySelectorAll(".drawer-choice").forEach((button) => { button.disabled = false; });
    toast(error.message, true);
  }
}

async function openDirectoryBrowser(mode) {
  try {
    const listing = await api("/api/gateway/directories", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: state.gateway.gateway_root || null }),
    });
    openModal({
      kind: "directory",
      title: mode === "create" ? "新建工作区" : "打开工作区",
      description: "请选择 ME Gateway 宿主机上的目录。",
      choices: [], selected: null, confirmLabel: mode === "create" ? "创建并打开" : "打开",
      html: `<div class="directory-browser"></div>`,
      onOpen: () => renderDirectoryListing(listing, mode),
      onConfirm: async () => {
        const current = state.modal?.directory?.path;
        if (!current) throw new Error("请选择目录");
        let result;
        if (mode === "create") {
          const name = elements.modalContent.querySelector("#new-workspace-name")?.value.trim();
          if (!name) throw new Error("请输入工作区名称");
          result = await api("/api/gateway/workspaces/create", {
            method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ parent: current, name }),
          });
        } else {
          result = await api("/api/gateway/workspaces/open", {
            method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ path: current }),
          });
        }
        await refreshGatewayState();
        activateWorkspace(result.workspace_id);
      },
    });
  } catch (error) { toast(error.message, true); }
}

function directoryParentRequest(listing) {
  if (listing.parent) return { path: listing.parent, roots: false };
  if (listing.parent_is_root_selector) return { path: null, roots: true };
  return null;
}

function displayHostPath(path) {
  const value = String(path || "");
  if (value.startsWith("\\\\?\\UNC\\")) return `\\\\${value.slice(8)}`;
  if (value.startsWith("\\\\?\\")) return value.slice(4);
  return value;
}

async function loadDirectoryListing(path, mode, roots = false) {
  const browser = elements.modalContent.querySelector(".directory-browser");
  const controls = [...elements.modalContent.querySelectorAll("[data-directory-up], [data-directory-path]")];
  browser?.classList.add("loading");
  controls.forEach((button) => { button.disabled = true; });
  try {
    const listing = await api("/api/gateway/directories", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path, roots }),
    });
    if (state.modal) renderDirectoryListing(listing, mode);
  } catch (error) {
    browser?.classList.remove("loading");
    controls.forEach((button) => { button.disabled = false; });
    toast(error.message, true);
  }
}

function renderDirectoryListing(listing, mode) {
  if (!state.modal) return;
  const workspaceName = elements.modalContent.querySelector("#new-workspace-name")?.value || "";
  const directories = listing.directories || [];
  const rootSelector = listing.root_selector === true;
  const parentRequest = directoryParentRequest(listing);
  const currentLocation = rootSelector ? "此电脑" : displayHostPath(listing.path);
  const itemCount = `${directories.length} 个${rootSelector ? "磁盘" : "文件夹"}`;
  const folderIcon = `<svg class="directory-folder-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M3.5 6.5h6l2 2h9v9.5a2 2 0 0 1-2 2h-15v-13.5Z"/><path d="M3.5 10h17"/></svg>`;
  const driveIcon = `<svg class="directory-folder-icon" viewBox="0 0 24 24" aria-hidden="true"><rect x="3.5" y="5" width="17" height="14" rx="2"/><path d="M3.5 14h17"/><circle cx="17" cy="16.5" r=".8"/></svg>`;
  const locationIcon = rootSelector ? driveIcon : folderIcon;
  const entryIcon = rootSelector ? driveIcon : folderIcon;
  const emptyTitle = rootSelector ? "没有可用磁盘" : "此位置没有子目录";
  const emptyDescription = rootSelector ? "当前宿主机没有可浏览的逻辑盘符。" : "可以选择当前目录，或返回上一级继续浏览。";
  state.modal.directory = listing;
  elements.modalContent.innerHTML = `<div class="directory-browser${rootSelector ? " root-selector" : ""}">
    <div class="directory-toolbar">
      <button type="button" class="directory-up" aria-label="返回上一级" title="返回上一级" ${parentRequest ? `data-directory-up="true"` : "disabled"}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m15 18-6-6 6-6"/></svg></button>
      <div class="directory-current">
        <span class="directory-location-icon">${locationIcon}</span>
        <span class="directory-current-path" title="${escapeAttr(currentLocation)}">${escapeHtml(currentLocation)}</span>
        <span class="directory-count">${escapeHtml(itemCount)}</span>
      </div>
    </div>
    <div class="directory-table">
      <div class="directory-list-header" aria-hidden="true"><span></span><span>名称</span><span>位置</span><span></span></div>
      <div class="directory-list" role="list">
        ${directories.map((directory) => {
          const displayedPath = displayHostPath(directory.path);
          return `<button type="button" class="directory-entry" role="listitem" data-directory-path="${escapeAttr(directory.path)}" aria-label="打开目录 ${escapeAttr(directory.name)}"><span class="directory-entry-icon">${entryIcon}</span><span class="directory-entry-name">${escapeHtml(directory.name)}</span><span class="directory-entry-path" title="${escapeAttr(displayedPath)}">${escapeHtml(displayedPath)}</span><svg class="directory-entry-arrow" viewBox="0 0 24 24" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg></button>`;
        }).join("")}
        ${directories.length ? "" : `<div class="directory-empty"><span class="directory-entry-icon">${locationIcon}</span><strong>${emptyTitle}</strong><span>${emptyDescription}</span></div>`}
      </div>
    </div>
    ${rootSelector ? `<p class="directory-help">请选择一个磁盘继续浏览。</p>` : mode === "create" ? `<label class="directory-name">工作区名称<input id="new-workspace-name" type="text" autocomplete="off" value="${escapeAttr(workspaceName)}" placeholder="例如：my-project"></label>` : `<p class="directory-help">当前目录将作为工作区打开。</p>`}
  </div>`;
  elements.modalConfirm.disabled = !listing.path;
  elements.modalContent.querySelector("[data-directory-up]")?.addEventListener("click", () => {
    void loadDirectoryListing(parentRequest.path, mode, parentRequest.roots);
  });
  elements.modalContent.querySelectorAll("[data-directory-path]").forEach((button) => button.addEventListener("click", () => {
    void loadDirectoryListing(button.dataset.directoryPath, mode);
  }));
  elements.modalContent.querySelector("#new-workspace-name")?.focus();
}

function blankGatewayModel() {
  return {
    original_name: null, name: "", provider: "openai-compatible", reserve_output_context: false,
    base_url: "", endpoint: "/chat/completions", api_key_env: null, credential_file: null,
    model: "", source_url: null, timeout_seconds: 120,
    capabilities: { context_window: 128000, max_output_tokens: null, input_modalities: ["text"], output_modalities: ["text"], reasoning_modes: [], reasoning_efforts: ["unset"], tools: true, streaming: true },
    parameters: {}, effort_parameters: {}, api_key: null, clear_inline_api_key: false,
  };
}

function modelSettingsHtml(model, index) {
  const value = (name) => escapeAttr(model[name] ?? "");
  return `<details class="settings-model" data-settings-model="${index}">
    <summary>
      <span class="settings-model-title">
        <svg class="settings-model-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3 4.5 7.2v9.6L12 21l7.5-4.2V7.2L12 3Z"/><path d="m4.8 7.4 7.2 4 7.2-4M12 11.4V21"/></svg>
        <strong>${escapeHtml(model.name || "新模型")}</strong>
      </span>
      <span class="settings-model-provider">${escapeHtml(model.provider)}</span>
    </summary>
    <div class="settings-grid">
      <label>显示名称<input data-setting="name" value="${value("name")}"></label>
      <label>Provider<select data-setting="provider"><option value="openai-compatible" ${model.provider === "openai-compatible" ? "selected" : ""}>OpenAI Compatible</option><option value="anthropic" ${model.provider === "anthropic" ? "selected" : ""}>Anthropic</option><option value="codex-oauth" ${model.provider === "codex-oauth" ? "selected" : ""}>Codex OAuth</option></select></label>
      <label>Base URL<input data-setting="base_url" value="${value("base_url")}"></label>
      <label>Endpoint<input data-setting="endpoint" value="${value("endpoint")}"></label>
      <label>模型标识<input data-setting="model" value="${value("model")}"></label>
      <label>请求超时（秒）<input data-setting="timeout_seconds" type="number" min="1" value="${Number(model.timeout_seconds) || 120}"></label>
      <label>API Key 环境变量<input data-setting="api_key_env" value="${value("api_key_env")}"></label>
      <label>凭据文件<input data-setting="credential_file" value="${value("credential_file")}"></label>
      <label>来源地址<input data-setting="source_url" value="${value("source_url")}"></label>
      <label>API Key<input data-setting="api_key" type="text" autocomplete="off" value="${value("api_key")}" placeholder="留空表示不设置"></label>
      <label class="settings-check"><input data-setting="reserve_output_context" type="checkbox" ${model.reserve_output_context ? "checked" : ""}>为输出预留上下文</label>
      <label class="settings-check"><input data-setting="clear_inline_api_key" type="checkbox" ${model.clear_inline_api_key ? "checked" : ""}>清除已保存的 API Key</label>
    </div>
    <label>能力配置（JSON）<textarea data-setting="capabilities" rows="7">${escapeHtml(JSON.stringify(model.capabilities, null, 2))}</textarea></label>
    <label>请求参数（JSON）<textarea data-setting="parameters" rows="5">${escapeHtml(JSON.stringify(model.parameters || {}, null, 2))}</textarea></label>
    <label>推理强度参数（JSON）<textarea data-setting="effort_parameters" rows="5">${escapeHtml(JSON.stringify(model.effort_parameters || {}, null, 2))}</textarea></label>
    <button type="button" class="ghost-button danger settings-remove-model" data-remove-model="${index}">移除模型</button>
  </details>`;
}

function renderSettingsModal() {
  const settings = state.modal?.settings;
  if (!settings) return;
  elements.modalContent.innerHTML = `<div class="settings-editor">
    <label>默认模型<select id="settings-default-model">${settings.models.map((model) => `<option value="${escapeAttr(model.name)}" ${model.name === settings.default_model ? "selected" : ""}>${escapeHtml(model.name || "未命名模型")}</option>`).join("")}</select></label>
    <div class="settings-models">${settings.models.map(modelSettingsHtml).join("")}</div>
    <button id="settings-add-model" type="button" class="ghost-button">新增模型</button>
    <p class="settings-help">保存后不会更改正在运行的工作区。请重启 ME Gateway 以使用新设置。</p>
  </div>`;
  elements.modalContent.querySelector("#settings-add-model")?.addEventListener("click", () => {
    if (!captureSettingsEditor()) return;
    state.modal.settings.models.push(blankGatewayModel());
    renderSettingsModal();
  });
  elements.modalContent.querySelectorAll("[data-remove-model]").forEach((button) => button.addEventListener("click", () => {
    if (!captureSettingsEditor()) return;
    state.modal.settings.models.splice(Number(button.dataset.removeModel), 1);
    renderSettingsModal();
  }));
}

function parseSettingsJson(value, label) {
  try { return JSON.parse(value || "{}"); }
  catch (_) { throw new Error(`${label}不是有效的 JSON`); }
}

function resolveEditedDefaultModel(previousModels, editedModels, selectedDefault) {
  const index = previousModels.findIndex((model) => model.name === selectedDefault);
  return index >= 0 ? editedModels[index]?.name || "" : selectedDefault;
}

function collectSettings() {
  const settings = state.modal?.settings;
  if (!settings) throw new Error("设置页面已关闭");
  const cards = [...elements.modalContent.querySelectorAll("[data-settings-model]")];
  const models = cards.map((card, index) => {
    const previous = settings.models[index];
    const field = (name) => card.querySelector(`[data-setting="${name}"]`);
    const optional = (name) => field(name).value.trim() || null;
    return {
      ...previous, original_name: previous.original_name, name: field("name").value.trim(),
      provider: field("provider").value, reserve_output_context: field("reserve_output_context").checked,
      base_url: field("base_url").value.trim(), endpoint: field("endpoint").value.trim(),
      api_key_env: optional("api_key_env"), credential_file: optional("credential_file"),
      model: field("model").value.trim(), source_url: optional("source_url"),
      timeout_seconds: Number(field("timeout_seconds").value),
      capabilities: parseSettingsJson(field("capabilities").value, "能力配置"),
      parameters: parseSettingsJson(field("parameters").value, "请求参数"),
      effort_parameters: parseSettingsJson(field("effort_parameters").value, "推理强度参数"),
      api_key: optional("api_key"), clear_inline_api_key: field("clear_inline_api_key").checked,
    };
  });
  const selectedDefault = elements.modalContent.querySelector("#settings-default-model")?.value || "";
  return {
    version: settings.version,
    default_model: resolveEditedDefaultModel(settings.models, models, selectedDefault),
    models,
  };
}

function captureSettingsEditor() {
  try {
    state.modal.settings = collectSettings();
    return true;
  } catch (error) {
    toast(error.message, true);
    return false;
  }
}

async function openGatewaySettings() {
  try {
    const settings = await api("/api/gateway/settings");
    openModal({
      title: "设置", description: "编辑 ME 的全局模型设置。",
      choices: [], selected: null, confirmLabel: "保存设置", html: `<div class="settings-editor"></div>`,
      settings, onOpen: renderSettingsModal,
      onConfirm: async () => {
        await api("/api/gateway/settings", {
          method: "POST", headers: { "Content-Type": "application/json" },
          body: JSON.stringify(collectSettings()),
        });
        toast("设置已保存；重启 ME Gateway 后生效");
      },
    });
  } catch (error) { toast(error.message, true); }
}

async function closeExternalWorkspace(workspaceId) {
  const workspace = (state.gateway.workspaces || []).find((item) => item.id === workspaceId);
  if (!workspace || workspace.builtin) return;
  openConfirm("关闭工作区？", `“${workspace.name}”将从工作列表中移除，正在运行的会话也会停止。工作目录不会被删除。`, "关闭工作区", async () => {
    await api(`/api/gateway/workspaces/${encodeURIComponent(workspaceId)}/close`, {
      method: "POST", headers: { "Content-Type": "application/json" }, body: "{}",
    });
    await refreshGatewayState();
    if (state.workspaceId === workspaceId) activateWorkspace("chat");
  }, true);
}


function openConfirm(title, description, confirmLabel, onConfirm, danger = false) {
  openModal({ title, description, choices: [], selected: null, confirmLabel, onConfirm, danger });
}

function openModal(modal) {
  const choices = modal.choices || [];
  state.modal = { ...modal, choices };
  elements.modalTitle.textContent = modal.title;
  elements.modalDescription.textContent = modal.description || "";
  elements.modalDescription.classList.toggle("hidden", !modal.description);
  elements.modalConfirm.disabled = false;
  elements.modalConfirm.textContent = modal.confirmLabel || "确认";
  elements.modalConfirm.classList.toggle("danger", !!modal.danger);
  elements.modalConfirm.classList.toggle("hidden", modal.confirmLabel === null);
  elements.modalContent.innerHTML = modal.html != null ? modal.html
    : choices.length ? `<div class="choice-list">${choices.map((choice) => `<label class="choice ${String(choice.value) === String(modal.selected) ? "selected" : ""}"><input type="radio" name="modal-choice" value="${escapeAttr(choice.value)}" ${String(choice.value) === String(modal.selected) ? "checked" : ""}><span>${escapeHtml(choice.label)}${choice.detail ? `<small>${escapeHtml(choice.detail)}</small>` : ""}</span></label>`).join("")}</div>` : "";
  elements.modalContent.classList.toggle("hidden", modal.html == null && !choices.length);
  if (modal.html == null) elements.modalContent.querySelectorAll("input").forEach((input) => input.addEventListener("change", () => {
    state.modal.selected = input.value;
    elements.modalContent.querySelectorAll(".choice").forEach((choice) => choice.classList.toggle("selected", choice.contains(input)));
  }));
  elements.modalBackdrop.classList.toggle("directory-modal-backdrop", modal.kind === "directory");
  elements.modalBackdrop.classList.remove("hidden");
  modal.onOpen?.(elements.modalContent);
}

function closeModal() {
  state.modal = null;
  elements.modalBackdrop.classList.add("hidden");
  elements.modalBackdrop.classList.remove("directory-modal-backdrop");
}

async function confirmModal() {
  const modal = state.modal;
  if (!modal) return;
  elements.modalConfirm.disabled = true;
  try {
    if (modal.onConfirm) await modal.onConfirm(modal.selected);
    closeModal();
  } catch (error) { toast(error.message, true); }
  finally { elements.modalConfirm.disabled = false; }
}

async function sendCommand(payload, workspaceId = state.workspaceId) {
  if (workspaceId === state.workspaceId && !state.connected) {
    const error = new Error("连接尚未恢复，请稍候");
    error.commandResultKnown = true;
    throw error;
  }
  try {
    const response = await api("/api/command", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    }, workspaceId);
    if (workspaceId === state.workspaceId) requestHttpSyncNow();
    return response;
  } catch (error) {
    if (error.status === 401) showLogin("登录已失效，请重新登录");
    else if (workspaceId === state.workspaceId && (!error.status || [502, 503, 504].includes(error.status))) handlePollingFailure(error);
    throw error;
  }
}

function promptSubmissionBoundary(meta, store) {
  if (meta?.last_event_id != null) {
    const snapshotBoundary = Number(meta.last_event_id);
    if (Number.isSafeInteger(snapshotBoundary)) return snapshotBoundary;
  }
  const lastEvent = store.events[store.events.length - 1];
  const eventBoundary = lastEvent ? Number(eventParts(lastEvent)[1].id) : -1;
  return Number.isSafeInteger(eventBoundary) ? eventBoundary : -1;
}

function commandResultIsUnknown(error) {
  return !error.commandResultKnown
    && (!error.status || [502, 503, 504].includes(error.status));
}

function cancelPendingPromptSubmission(agentId, pending) {
  const workspaceId = pending.workspaceId || state.workspaceId;
  const bucket = workspaceId === state.workspaceId ? state : gatewayWorkspaceState(workspaceId);
  const store = bucket.stores.get(agentId);
  if (!store || store.pendingPromptSubmission !== pending) return false;
  pending.settled = true;
  store.pendingPromptSubmission = null;
  bucket.drafts.set(agentId, pending.displayContent);
  const sync = bucket.draftSync.get(agentId);
  if (sync) {
    sync.paused = workspaceId !== state.workspaceId;
    sync.desired = pending.displayContent;
    if (!sync.paused && sync.sent !== sync.desired) void runDraftSync(agentId, sync);
  }
  if (workspaceId === state.workspaceId && state.selectedAgent === agentId) {
    elements.input.value = pending.displayContent;
    autoSizeInput();
    renderSlashMenu();
    requestAnimationFrame(() => elements.input.focus());
  }
  return true;
}

function finishPendingPromptSubmission(agentId) {
  const activeStore = state.stores.get(agentId);
  const pending = activeStore?.pendingPromptSubmission;
  if (!pending) return false;
  const workspaceId = pending.workspaceId || state.workspaceId;
  const bucket = workspaceId === state.workspaceId ? state : gatewayWorkspaceState(workspaceId);
  const store = bucket.stores.get(agentId);
  if (!store || store.pendingPromptSubmission !== pending) return false;
  pending.settled = true;
  store.pendingPromptSubmission = null;
  const meta = bucket.snapshot.agents.find((agent) => agent.id === agentId);
  const observedRevision = Number(meta?.input_draft_revision);
  const hasCurrentDraft = Number.isSafeInteger(observedRevision)
    && observedRevision >= store.inputDraftRevision;
  const revision = hasCurrentDraft ? observedRevision : store.inputDraftRevision;
  const content = hasCurrentDraft ? String(meta?.input_draft || "") : "";
  const sync = bucket.draftSync.get(agentId);
  if (sync) sync.paused = workspaceId !== state.workspaceId;
  adoptInputDraft(agentId, store, revision, content);
  if (sync && !sync.paused && sync.sent !== sync.desired) void runDraftSync(agentId, sync);
  if (workspaceId === state.workspaceId && state.selectedAgent === agentId) {
    requestAnimationFrame(() => {
      if (state.workspaceId === workspaceId && state.selectedAgent === agentId && !store.pendingPromptSubmission) elements.input.focus();
    });
  }
  return true;
}

async function submitPrompt() {
  const displayContent = elements.input.value;
  const content = displayContent.trim();
  if (!content || !state.selectedAgent || agentMeta()?.kind === "sub-agent") return;
  if (content.startsWith("/") && COMMANDS.some(([name]) => name === content)) return openSlashCommand(content);
  const workspaceId = state.workspaceId;
  const agentId = state.selectedAgent;
  const store = currentStore();
  if (!store || store.pendingPromptSubmission) return;
  const pending = {
    workspaceId,
    content,
    displayContent,
    afterEventId: promptSubmissionBoundary(agentMeta(), store),
    status: "sending",
    settled: false,
  };
  store.pendingPromptSubmission = pending;
  state.drafts.set(agentId, displayContent);
  renderComposer();
  await pauseDraftSyncForSubmission(agentId);
  try {
    const response = await sendCommand({ command: "submit_user_prompt", agent_id: agentId, content }, workspaceId);
    const revision = Number(response?.receipt?.prompt_submission_revision);
    const inputDraftRevision = Number(response?.receipt?.input_draft_revision);
    if (!Number.isSafeInteger(revision) || !Number.isSafeInteger(inputDraftRevision)) {
      const error = new Error("消息发送失败：服务返回了无效结果");
      error.commandResultKnown = true;
      throw error;
    }
    store.promptSubmissionRevision = Math.max(store.promptSubmissionRevision, revision);
    store.inputDraftRevision = Math.max(store.inputDraftRevision, inputDraftRevision);
    if (pending.settled) return;
    pending.status = "accepted";
    if (state.workspaceId === workspaceId && state.selectedAgent === agentId) renderComposer();
  } catch (error) {
    if (pending.settled) return;
    if (commandResultIsUnknown(error)) {
      pending.status = "confirming";
      if (state.workspaceId === workspaceId && state.selectedAgent === agentId) renderComposer();
      return;
    }
    cancelPendingPromptSubmission(agentId, pending);
    if (state.workspaceId === workspaceId && state.selectedAgent === agentId) renderComposer();
    toast(error.message, true);
  }
}

function openSendSettings() {
  openChoiceDrawer("发送设置", "选择键盘发送方式。", [
    { value: SEND_SHORTCUT_ENTER, label: "Enter 发送", detail: "Shift/Alt+Enter 换行" },
    { value: SEND_SHORTCUT_MODIFIED_ENTER, label: "Shift/Alt+Enter 发送", detail: "Enter 换行" },
  ], state.sendShortcut, async (value) => setSendShortcut(value));
}

function submitOrOpenSendSettings() {
  if (currentStore()?.pendingPromptSubmission) return;
  if (!elements.input.value.trim()) {
    openSendSettings();
    return;
  }
  void submitPrompt();
}

async function stopGeneration() {
  if (!state.selectedAgent || !canControlRuntime()) return;
  if (currentProjection().turnState?.state !== "active") return;
  elements.stop.disabled = true;
  try { await sendCommand({ command: "abort_turn", agent_id: state.selectedAgent }); }
  catch (error) { renderComposer(); toast(error.message, true); }
}

async function escapeAction() {
  if (!state.selectedAgent || agentMeta()?.kind === "sub-agent") return;
  if (currentStore()?.pendingPromptSubmission) {
    toast("消息正在发送，请稍候");
    return;
  }
  flushPendingRender();
  const turn = currentProjection().turnState;
  try {
    if (turn?.state === "active") await sendCommand({ command: "abort_turn", agent_id: state.selectedAgent });
    else if (turn?.state === "aborting") toast("正在等待当前生成中止");
    else if (turn?.state === "aborted") await sendCommand({ command: "rewind_context", agent_id: state.selectedAgent, event_id: turn.promptId });
    else { elements.input.value = ""; saveDraft(); autoSizeInput(); renderSlashMenu(); }
  } catch (error) { toast(error.message, true); }
}

function saveDraft() {
  if (!state.selectedAgent) return;
  const agentId = state.selectedAgent;
  if (state.stores.get(agentId)?.pendingPromptSubmission) return;
  const content = elements.input.value;
  const previous = state.drafts.get(agentId) || "";
  state.drafts.set(agentId, content);
  if (state.composing) {
    let sync = state.draftSync.get(agentId);
    if (!sync) {
      sync = {
        workspaceId: state.workspaceId,
        desired: content, sent: previous, sending: false, paused: false,
        inFlight: null, pendingRemote: null, waiters: [],
      };
      state.draftSync.set(agentId, sync);
    } else sync.desired = content;
    return;
  }
  queueDraftUpdate(agentId, content);
}

function beginInputComposition() {
  state.composing = true;
  const agentId = state.selectedAgent;
  if (!agentId || state.draftSync.has(agentId)) return;
  const content = elements.input.value;
  state.draftSync.set(agentId, {
    workspaceId: state.workspaceId,
    desired: content, sent: content, sending: false, paused: false,
    inFlight: null, pendingRemote: null, waiters: [],
  });
}

function endInputComposition() {
  state.composing = false;
  state.lastInputAt = performance.now();
  const agentId = state.selectedAgent;
  const sync = state.draftSync.get(agentId);
  const store = state.stores.get(agentId);
  if (sync?.pendingRemote) {
    if (store && sync.pendingRemote.revision > store.inputDraftRevision) {
      store.inputDraftRevision = sync.pendingRemote.revision;
      sync.sent = sync.pendingRemote.content;
    }
    sync.pendingRemote = null;
  }
  saveDraft();
}

function queueDraftUpdate(agentId, content) {
  let sync = state.draftSync.get(agentId);
  if (!sync) {
    sync = {
      workspaceId: state.workspaceId,
      desired: content, sent: null, sending: false, paused: false,
      inFlight: null, pendingRemote: null, waiters: [],
    };
    state.draftSync.set(agentId, sync);
  } else sync.desired = content;
  void runDraftSync(agentId, sync);
}

async function runDraftSync(agentId, sync) {
  const workspaceId = sync.workspaceId || state.workspaceId;
  sync.workspaceId = workspaceId;
  const active = workspaceId === state.workspaceId;
  if (sync.sending || sync.paused || !active || !state.connected
      || (sync.retryAfter && Date.now() < sync.retryAfter)) return;
  const bucket = gatewayWorkspaceState(workspaceId);
  sync.sending = true;
  let failed = false;
  try {
    while (!sync.paused
        && workspaceId === state.workspaceId
        && !(state.composing && state.selectedAgent === agentId)
        && sync.sent !== sync.desired) {
      const content = sync.desired;
      const store = bucket.stores.get(agentId);
      if (!store) return;
      const expectedRevision = store.inputDraftRevision;
      const flight = { expectedRevision, content };
      sync.inFlight = flight;
      let response;
      try {
        response = await sendCommand({
          command: "update_input_draft",
          agent_id: agentId,
          expected_revision: expectedRevision,
          content,
        }, workspaceId);
      } finally {
        if (sync.inFlight === flight) sync.inFlight = null;
      }
      const revision = Number(response?.receipt?.input_draft_revision);
      const accepted = response?.receipt?.accepted === true;
      if (!Number.isSafeInteger(revision)) throw new Error("输入同步返回了无效结果");
      if (revision < store.inputDraftRevision) continue;
      if (!accepted) {
        sync.sent = sync.desired;
        break;
      }
      store.inputDraftRevision = revision;
      sync.sent = content;
      sync.errorNotified = false;
      sync.retryAfter = 0;
    }
  } catch (error) {
    failed = true;
    sync.sent = null;
    sync.retryAfter = Date.now() + 1000;
    if (workspaceId === state.workspaceId && state.connected && !sync.errorNotified) {
      sync.errorNotified = true;
      toast(`输入同步失败：${error.message}`, true);
    }
  } finally {
    const shouldRetry = sync.sent !== sync.desired;
    sync.sending = false;
    const waiters = sync.waiters.splice(0);
    waiters.forEach((resolve) => resolve());
    if (!failed && workspaceId === state.workspaceId && state.connected && !sync.paused
        && !(state.composing && state.selectedAgent === agentId) && shouldRetry) {
      void runDraftSync(agentId, sync);
    }
  }
}

async function pauseDraftSyncForSubmission(agentId) {
  let sync = state.draftSync.get(agentId);
  if (!sync) {
    sync = {
      workspaceId: state.workspaceId,
      desired: "", sent: null, sending: false, paused: true,
      inFlight: null, pendingRemote: null, waiters: [],
    };
    state.draftSync.set(agentId, sync);
  } else {
    sync.desired = "";
    sync.paused = true;
  }
  while (sync.sending) await new Promise((resolve) => sync.waiters.push(resolve));
}

function restoreDraft() {
  const pending = currentStore()?.pendingPromptSubmission;
  elements.input.value = pending?.displayContent ?? state.drafts.get(state.selectedAgent) ?? "";
  autoSizeInput();
}

function flushDraftBeforePageCloses() {
  captureActiveWorkspace();
  for (const [workspaceId, bucket] of state.workspaceStates) {
    const drafts = new Map();
    for (const [agentId, sync] of bucket.draftSync) {
      if (bucket.stores.get(agentId)?.pendingPromptSubmission) continue;
      if (sync.sent !== sync.desired) drafts.set(agentId, sync.desired);
    }
    if (workspaceId === state.workspaceId && state.selectedAgent
        && agentMeta()?.kind !== "sub-agent" && !currentStore()?.pendingPromptSubmission) {
      drafts.set(state.selectedAgent, elements.input.value);
    }
    for (const [agentId, content] of drafts) {
      const expectedRevision = bucket.stores.get(agentId)?.inputDraftRevision ?? 0;
      const body = JSON.stringify({
        command: "update_input_draft", agent_id: agentId, expected_revision: expectedRevision, content,
      });
      const url = scopedApiPath("/api/command", workspaceId);
      try {
        const data = typeof Blob === "function" ? new Blob([body], { type: "application/json" }) : null;
        if (data && navigator.sendBeacon?.(url, data)) continue;
        void fetch(url, {
          method: "POST", headers: { "Content-Type": "application/json" }, body, keepalive: true,
        });
      } catch (_) {}
    }
  }
}

function autoSizeInput() {
  if (state.inputResizeFrame !== null) return;
  state.inputResizeFrame = requestAnimationFrame(() => {
    state.inputResizeFrame = null;
    elements.input.style.height = "auto";
    elements.input.style.height = `${Math.min(elements.input.scrollHeight, 180)}px`;
  });
}

function positionToastRegion() {
  const tabs = elements.tabs.getBoundingClientRect();
  elements.toasts.style.top = `${Math.ceil(tabs.bottom + 10)}px`;
}

function toast(message, error = false) {
  positionToastRegion();
  const node = document.createElement("div");
  node.className = `toast ${error ? "error" : ""}`;
  node.textContent = message;
  elements.toasts.append(node);
  setTimeout(() => node.remove(), 3500);
}

function childStateLabel(events) {
  const latest = [...events].reverse().map(eventParts).find(([kind]) => kind === "AgentTurn")?.[1];
  if (!latest) return "working";
  return normalize(latest.state);
}

function planSymbol(value) { return ({ planned: "□", active: "■", completed: "✓", cancelled: "×", superseded: "×" })[normalize(value)] || "·"; }
function objectiveSymbol(value) { return ({ active: "■", completed: "✓", cancelled: "×", superseded: "×" })[normalize(value)] || "·"; }
function memoryLabel(memory) {
  if (memory.kind === "agreement") return "AGREEMENT";
  return `FACT${memory.basis ? ` · ${memory.basis.split("_").join(" ").toUpperCase()}` : ""}`;
}
function normalize(value) { return String(value || "").replace(/([a-z])([A-Z])/g, "$1-$2").split("_").join("-").toLowerCase(); }
function collapse(value) { return String(value || "").trim().replace(/\s+/g, " ").slice(0, 120); }
function formatBytes(value) { if (value < 1024) return `${value} B`; if (value < 1024 ** 2) return `${(value / 1024).toFixed(1)} KiB`; return `${(value / 1024 ** 2).toFixed(1)} MiB`; }
function formatDuration(ms) { if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`; return `${(ms / 1000).toFixed(ms % 1000 ? 1 : 0)}s`; }
function formatTurnElapsed(ms) {
  const totalSeconds = Math.max(0, Math.floor(Number(ms) / 1000));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor(totalSeconds % 3600 / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours}h ${String(minutes).padStart(2, "0")}m ${String(seconds).padStart(2, "0")}s`;
  if (minutes > 0) return `${minutes}m ${String(seconds).padStart(2, "0")}s`;
  return `${seconds}s`;
}

function formatTurnCompletedAt(timestamp, now = Date.now()) {
  const completed = new Date(Number(timestamp));
  const current = new Date(Number(now));
  if (Number.isNaN(completed.getTime()) || Number.isNaN(current.getTime())) return "—";
  const calendarDay = (value) => Date.UTC(value.getFullYear(), value.getMonth(), value.getDate());
  const daysAgo = Math.max(0, Math.round((calendarDay(current) - calendarDay(completed)) / 86400000));
  const day = daysAgo === 0 ? "今天" : daysAgo === 1 ? "昨天" : daysAgo === 2 ? "前天" : `${daysAgo} 天前`;
  const time = `${String(completed.getHours()).padStart(2, "0")}:${String(completed.getMinutes()).padStart(2, "0")}`;
  return `${day} ${time}`;
}

function completedTurnContextGrowth(completedApiUsage, promptId, contextBaseline) {
  const calls = [...completedApiUsage.values()].filter((entry) => entry.promptId === promptId);
  const latest = calls[calls.length - 1];
  if (!calls.length || !calls[0].usage || !latest.usage) return null;
  const baseline = contextBaseline ?? calls[0].usage.input_tokens;
  return Math.max(0, Number(latest.usage.total_tokens) - Number(baseline));
}

function formatTurnTokens(tokens) {
  return tokens == null ? "—" : `${(Math.max(0, Number(tokens)) / 1000).toFixed(1)}k`;
}
function formatTokens(value) { return value == null ? "—" : `${(value / 1000).toFixed(1)}k`; }
function formatLimit(value) { if (value == null) return "—"; return value % 1000 === 0 ? `${value / 1000}k` : `${(value / 1000).toFixed(1)}k`; }
function formatEstimatedTokens(value) { if (value == null) return "—"; return value < 1000 ? `≈${Math.round(value)} tok` : `≈${(value / 1000).toFixed(1)}k`; }
function formatContextCategoryTokens(category, value) { return category === "reserve" ? formatTokens(value) : formatEstimatedTokens(value); }
function escapeHtml(value) { return String(value ?? "").replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[character]); }
function escapeAttr(value) { return escapeHtml(value); }

function renderMarkdown(source) {
  return globalThis.MeMarkdown.render(source);
}

elements.tabs.querySelectorAll("button[data-view]").forEach((button) => button.addEventListener("click", () => {
  showView({ kind: button.dataset.view, sessionId: null });
}));
elements.objective.addEventListener("click", (event) => {
  const button = event.target.closest?.("[data-objective-toggle]");
  if (!button || !elements.objective.contains(button)) return;
  state.objectiveDisclosure.expanded = !state.objectiveDisclosure.expanded;
  renderObjective();
  transcriptBottomFollower.layoutChanged();
});
globalThis.MeTheme.bindControls(elements.themeCycle, elements.themeMode, (message) => toast(message));
elements.loginForm.addEventListener("submit", submitLogin);
elements.connectionRetry.addEventListener("click", () => {
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = null;
  state.syncGeneration += 1;
  state.syncController?.abort();
  state.syncController = null;
  state.connected = false;
  state.connecting = false;
  state.syncInFlight = false;
  startHttpPolling();
});
elements.addAgent.addEventListener("click", () => { closeMobileSidebar(); activateWorkspace("chat"); openAddAgent(); });
elements.createWorkspace.addEventListener("click", () => { closeMobileSidebar(); void openDirectoryBrowser("create"); });
elements.openWorkspace.addEventListener("click", () => { closeMobileSidebar(); void openDirectoryBrowser("open"); });
elements.openSettings.addEventListener("click", () => { closeMobileSidebar(); void openGatewaySettings(); });
elements.mobileDeleteAgent.addEventListener("click", () => { closeMobileSidebar(); openDeleteAgent(); });
elements.mobileSidebarToggle.addEventListener("click", openMobileSidebar);
elements.mobileSidebarBackdrop.addEventListener("click", closeMobileSidebar);
if (typeof PORTRAIT_LAYOUT.addEventListener === "function") {
  PORTRAIT_LAYOUT.addEventListener("change", closeMobileSidebar);
} else if (typeof PORTRAIT_LAYOUT.addListener === "function") {
  PORTRAIT_LAYOUT.addListener(closeMobileSidebar);
}
elements.deleteAgentMenu.addEventListener("click", () => {
  const menu = state.agentMenu;
  closeAgentMenu();
  closeMobileSidebar();
  if (menu) void openDeleteAgent(menu.agentId);
});
elements.closeWorkspaceMenu.addEventListener("click", () => {
  const menu = state.workspaceMenu;
  closeWorkspaceMenu();
  closeMobileSidebar();
  if (menu) void closeExternalWorkspace(menu.workspaceId);
});
elements.copyUserMessage.addEventListener("click", async () => {
  const menu = state.userMenu;
  closeUserMessageMenu();
  if (!menu) return;
  try { await copyTextToClipboard(menu.content); toast("已复制"); }
  catch (error) { toast(error.message, true); }
});
elements.rewindUserMessage.addEventListener("click", () => {
  const menu = state.userMenu;
  closeUserMessageMenu();
  if (!menu?.rewindable) return;
  openConfirm("撤回这条消息？", "该消息及其后的内容将从当前会话中永久移除。", "撤回", () => sendCommand({
    command: "rewind_context",
    agent_id: menu.agentId,
    event_id: menu.eventId,
  }), true);
});
elements.deleteUserTurn.addEventListener("click", () => {
  const menu = state.userMenu;
  closeUserMessageMenu();
  if (!menu?.deletable) return;
  openConfirm("删除这一轮？", "这条用户消息及其对应回复将被永久移除。", "删除", () => sendCommand({
    command: "delete_turn",
    agent_id: menu.agentId,
    prompt_id: menu.eventId,
  }), true);
});
elements.stop.addEventListener("click", stopGeneration);
elements.send.addEventListener("click", submitOrOpenSendSettings);
elements.scrollToBottom.addEventListener("click", scrollTranscriptToBottomAfterLayout);
elements.statusModelTrigger.addEventListener("click", openModelDrawer);
elements.statusEffortTrigger.addEventListener("click", openEffortDrawer);
elements.statusContextTrigger.addEventListener("click", openContextDrawer);
elements.input.addEventListener("input", () => {
  state.lastInputAt = performance.now();
  saveDraft(); state.slashIndex = 0; autoSizeInput(); renderSlashMenu();
});
elements.input.addEventListener("compositionstart", beginInputComposition);
elements.input.addEventListener("compositionend", endInputComposition);
function enterSubmitsPrompt(event) {
  return sendShortcutPressed(event, state.sendShortcut);
}
elements.input.addEventListener("keydown", (event) => {
  if (state.composing || event.isComposing || event.keyCode === 229) return;
  const visible = !elements.slashMenu.classList.contains("hidden");
  const matches = COMMANDS.filter(([name]) => name.startsWith(elements.input.value));
  if (visible && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
    event.preventDefault();
    state.slashIndex = (state.slashIndex + (event.key === "ArrowDown" ? 1 : -1) + matches.length) % matches.length;
    renderSlashMenu();
  } else if (visible && enterSubmitsPrompt(event)) {
    event.preventDefault(); openSlashCommand(matches[state.slashIndex]?.[0]);
  } else if (enterSubmitsPrompt(event)) {
    event.preventDefault(); submitPrompt();
  } else if (event.key === "Escape") {
    event.preventDefault(); escapeAction();
  }
});
elements.modalClose.addEventListener("click", closeModal);
elements.modalCancel.addEventListener("click", closeModal);
elements.modalConfirm.addEventListener("click", confirmModal);
elements.modalBackdrop.addEventListener("click", (event) => { if (event.target === elements.modalBackdrop) closeModal(); });
elements.drawerClose.addEventListener("click", closeChoiceDrawer);
elements.drawerBackdrop.addEventListener("click", (event) => { if (event.target === elements.drawerBackdrop) closeChoiceDrawer(); });
elements.contextDrawerClose.addEventListener("click", closeContextDrawer);
elements.contextDrawerBackdrop.addEventListener("click", (event) => { if (event.target === elements.contextDrawerBackdrop) closeContextDrawer(); });
elements.contextBreakdown.addEventListener("click", (event) => {
  const button = event.target.closest(".context-detail-help");
  if (button?.dataset.detail === "compact" && state.contextCompactContent !== null) {
    openContextDetail(
      "上下文压缩",
      compactPreviewMarkdown(state.contextCompactAnalysis, state.contextCompactContent),
      true,
    );
  } else if (button?.dataset.detail === "memory" && state.contextMemoryContent !== null) {
    openContextDetail("记忆", state.contextMemoryContent);
  }
});
elements.contextClear.addEventListener("click", confirmContextClear);
elements.compactSummaryClose.addEventListener("click", closeCompactSummary);
elements.compactSummaryBackdrop.addEventListener("click", (event) => { if (event.target === elements.compactSummaryBackdrop) closeCompactSummary(); });
document.addEventListener("click", (event) => {
  if (state.userMenu && !elements.userMessageMenu.contains(event.target)) closeUserMessageMenu();
  if (state.agentMenu && !elements.agentMenu.contains(event.target)) closeAgentMenu();
  if (state.workspaceMenu && !elements.workspaceMenu.contains(event.target)) closeWorkspaceMenu();
});
window.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  if (!elements.compactSummaryBackdrop.classList.contains("hidden")) closeCompactSummary(); else if (state.contextDrawerOpen) closeContextDrawer(); else if (state.drawer) closeChoiceDrawer(); else if (state.modal) closeModal(); else if (state.userMenu) closeUserMessageMenu(); else if (state.agentMenu) closeAgentMenu(); else if (state.workspaceMenu) closeWorkspaceMenu();
});
window.addEventListener("resize", () => {
  closeUserMessageMenu();
  closeAgentMenu();
  closeWorkspaceMenu();
  positionToastRegion();
  updateScrollToBottomButton();
  if (state.view.kind === "terminal") {
    if (state.terminalFollowBottom) requestAnimationFrame(scrollTerminalToBottom);
    void renderTerminal();
  }
});
window.addEventListener("pagehide", () => {
  state.pageClosing = true;
  flushDraftBeforePageCloses();
});
window.addEventListener("pageshow", () => {
  state.pageClosing = false;
  if ((!state.authRequired || state.authenticated) && !state.connected) startHttpPolling();
});
const transcriptBottomFollower = createTranscriptBottomFollower(
  elements.transcript,
  elements.transcriptContent,
  updateScrollToBottomButton,
);
elements.transcript.addEventListener("scroll", () => {
  closeUserMessageMenu();
  transcriptBottomFollower.noteScroll();
}, { passive: true });
elements.agents.addEventListener("scroll", closeAgentMenu, { passive: true });
elements.workspaceList.addEventListener("scroll", closeWorkspaceMenu, { passive: true });
elements.transcript.addEventListener("wheel", suspendTranscriptAutoFollow, { passive: true });
if (typeof window.PointerEvent === "function") {
  elements.transcript.addEventListener("pointerdown", beginTranscriptUserInteraction, { passive: true });
  elements.transcript.addEventListener("pointerup", endTranscriptUserInteraction, { passive: true });
  elements.transcript.addEventListener("pointercancel", endTranscriptUserInteraction, { passive: true });
} else {
  elements.transcript.addEventListener("touchstart", beginTranscriptUserInteraction, { passive: true });
  elements.transcript.addEventListener("touchend", endTranscriptUserInteraction, { passive: true });
  elements.transcript.addEventListener("touchcancel", endTranscriptUserInteraction, { passive: true });
}
if ("onscrollend" in elements.transcript) {
  elements.transcript.addEventListener("scrollend", finishTranscriptScrolling, { passive: true });
}
elements.terminalView.addEventListener("scroll", () => {
  if (state.view.kind === "terminal") {
    state.terminalFollowBottom = terminalIsNearBottom(elements.terminalView);
  }
}, { passive: true });
function refreshRunningToolElapsed() {
  if (inputHasPriority()) return;
  elements.transcriptContent.querySelectorAll("[data-running-started]").forEach((node) => {
    const text = `Running ... ${formatDuration(Date.now() - Number(node.dataset.runningStarted))}`;
    if (node.textContent !== text) node.textContent = text;
  });
}
setInterval(refreshRunningToolElapsed, 100);
setInterval(() => {
  if (performance.now() - state.lastInputAt < INPUT_ANIMATION_QUIET_MS) return;
  state.apiAnimationTick += 1;
  elements.apiSpinner.textContent = (state.apiActivity.active
    || API_ACTIVE.has(currentProjection().apiState))
    ? API_SPINNER_FRAMES[state.apiAnimationTick % API_SPINNER_FRAMES.length]
    : "";
}, 100);
setInterval(() => {
  if (state.authenticated && !state.pageClosing) void refreshGatewayState();
}, 1500);

initializeAuthentication();
