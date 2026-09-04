"use strict";

const HTTP_SYNC_ACTIVE_MS = 250;
const HTTP_SYNC_IDLE_MS = 1000;
const HTTP_SYNC_TIMEOUT_MS = 15000;
const BACKGROUND_SYNC_IDLE_MS = 5000;
const BACKGROUND_SYNC_RETRY_MAX_MS = 30000;
const EVENT_RECOVERY_THRESHOLD = 100;
const RECONNECT_MAX_MS = 5000;
const DRAFT_BATCH_MS = 80;
const CONNECTION_DEGRADED_GRACE_MS = 2000;
const CONNECTION_STABILIZE_MS = 1000;
const CONNECTION_STABILIZE_SUCCESSES = 2;
const INPUT_ANIMATION_QUIET_MS = 250;
const UI_ANIMATION_INTERVAL_MS = 100;
const TRANSCRIPT_BOTTOM_THRESHOLD_PX = 24;
const SYSTEM_STATIC_PROMPT_MAX_BYTES = 32 * 1024;
const TERMINAL_RENDER_OVERSCAN_ROWS = 80;
const TERMINAL_RENDER_MIN_ROWS = 240;
function browserPort(locationValue = document.location) {
  const explicitPort = String(locationValue?.port || "");
  const protocol = String(locationValue?.protocol || "").toLowerCase();
  return Number(explicitPort || (protocol === "https:" ? "443" : "80"));
}
function portScopedCookieName(prefix, locationValue = document.location) {
  return `${prefix}_p${browserPort(locationValue)}`;
}
const BROWSER_PORT = browserPort();
const SEND_SHORTCUT_COOKIE = portScopedCookieName("me_send_shortcut");
const SEND_SHORTCUT_PREFERENCE = "me-send-shortcut";
const SEND_SHORTCUT_ENTER = "enter";
const SEND_SHORTCUT_MODIFIED_ENTER = "modified-enter";
const WINDOW_BORDER_STYLE_PREFERENCE = "me-window-border-style";
const WINDOW_BORDER_DEFAULT = "default";
const WINDOW_BORDER_THEME = "theme";
const WORKSPACE_DISCLOSURE_STORAGE_KEY = "me-gateway.workspace-disclosure.v1";
const API_ACTIVE = new Set(["Requesting", "Streaming", "Retrying"]);
const API_SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PORTRAIT_LAYOUT = matchMedia("(orientation: portrait)");

const frontendRuntime = globalThis.MeFrontendRuntime;
if (!frontendRuntime) throw new Error("ME frontend runtime is unavailable");
const devicePreferences = frontendRuntime.devicePreferences || null;
const rememberedDevices = frontendRuntime.rememberedDevices || null;
const runtimeCapabilities = Object.freeze({
  multipleWorkspaces: false, gatewaySettings: false, targetConfiguration: false,
  nativeDownload: false, dynamicWindowTitle: false, windowBorderStyle: false,
  pageTitle: "ME", brandTitle: "ME", cacheStorageLabel: "当前浏览器",
  sessionSectionTitle: "聊天", newSessionLabel: "新建聊天",
  ...(frontendRuntime.capabilities || {}),
});
const edbCache = frontendRuntime.createEdbCache();
document.documentElement.classList.toggle("single-workspace", !runtimeCapabilities.multipleWorkspaces);
document.documentElement.classList.toggle("target-configuration", runtimeCapabilities.targetConfiguration);
document.documentElement.classList.toggle("remembered-device-logins", Boolean(rememberedDevices));
document.title = runtimeCapabilities.pageTitle;
const loginBrandTitle = document.querySelector(".login-brand strong");
if (loginBrandTitle) loginBrandTitle.textContent = runtimeCapabilities.brandTitle;
const builtinSectionTitle = document.querySelector("#builtin-section-title");
if (builtinSectionTitle) builtinSectionTitle.textContent = runtimeCapabilities.sessionSectionTitle;
const addAgentButton = document.querySelector("#add-agent");
if (addAgentButton) {
  addAgentButton.title = runtimeCapabilities.newSessionLabel;
  addAgentButton.setAttribute("aria-label", runtimeCapabilities.newSessionLabel);
}
function isIosWebKit(navigatorValue = navigator) {
  const userAgent = String(navigatorValue?.userAgent || "");
  const platform = String(navigatorValue?.platform || "");
  const iosDevice = /iPhone|iPad|iPod/i.test(userAgent)
    || (platform === "MacIntel" && Number(navigatorValue?.maxTouchPoints) > 1);
  return iosDevice && /AppleWebKit/i.test(userAgent);
}
const IOS_WEBKIT = isIosWebKit();
document.documentElement?.classList?.toggle("ios-webkit", IOS_WEBKIT);

const state = {
  gateway: { workspaces: [], selected_workspace_id: "chat", selected_agent_id: null, notices: [] },
  workspaceId: null,
  workspaceStates: new Map(),
  gatewayRefreshInFlight: false,
  lastNoticeId: 0,
  workspaceMenu: null,
  workspaceDisclosure: readWorkspaceDisclosure(),
  backgroundSyncTimer: null,
  backgroundSyncDueAt: null,
  backgroundSyncOperation: null,
  backgroundSyncCursor: 0,
  startupMetadataPending: false,
  activeCatchUpPending: true,
  snapshot: {
    revision: 0, environment: null, agents: [], models: [], orchestrators: [], default_orchestrator: null,
    chatbot_default_static_prompt: "",
    tool_visibility: { hidden_names: [], hidden_prefixes: [], activity_names: [] }
  },
  stores: new Map(),
  drafts: new Map(),
  draftSync: new Map(),
  promptDrafts: new Map(),
  selectedAgent: null,
  apiActivity: { agentId: null, active: false, receivedSseEvents: 0 },
  pendingAgentSelection: null,
  eventRecovery: null,
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
  inputHeight: null,
  apiAnimationTick: 0,
  uiAnimationTimer: null,
  runningToolNodes: [],
  lastInputAt: Number.NEGATIVE_INFINITY,
  composing: false,
  sendShortcut: readSendShortcutPreference(),
  windowBorderStyle: readWindowBorderStylePreference(),
  userMenu: null,
  agentMenu: null,
  modal: null,
  drawer: null,
  contextDrawerOpen: false,
  contextDrawerSignature: null,
  contextCompactContent: null,
  contextCompactAnalysis: null,
  contextMemoryContent: null,
  loginView: rememberedDevices ? "devices" : "form",
  loginBusy: false,
  localDevice: { endpoint: "http://127.0.0.1:38200", online: false, requiresPassword: false },
  authRequired: false,
  authenticated: false,
  connected: false,
  connecting: false,
  snapshotInitialized: false,
  edbCacheInitialized: false,
  syncGeneration: 0,
  syncController: null,
  syncInFlight: false,
  syncTimer: null,
  reconnectTimer: null,
  reconnectAttempt: 0,
  connectionPhase: "initial",
  connectionHadSuccess: false,
  connectionFailureStartedAt: null,
  connectionFailureDetail: "",
  degradedTimer: null,
  stabilizingSince: null,
  stabilizingSuccesses: 0,
  connectionOverlayMode: null,
  terminalFrames: new Map(),
  terminalFramesUnavailable: new Set(),
  pageClosing: false,
};
let transcriptVirtualizer = null;
let terminalWindowRenderFrame = null;


let workspacePanelSequence = 0;

const $ = (selector) => document.querySelector(selector);
const elements = {
  app: $("#app"),
  workspace: $(".workspace"),
  sessionSyncOverlay: $("#session-sync-overlay"),
  sessionSyncProgress: $("#session-sync-progress"),
  loginScreen: $("#login-screen"),
  loginSettings: $("#login-settings"),
  loginForm: $("#login-form"),
  loginPassword: $("#login-password"),
  loginEndpoint: $("#login-endpoint"),
  loginError: $("#login-error"),
  loginSubmit: $("#login-submit"),
  loginTitle: $("#login-title"),
  loginDescription: $("#login-description"),
  loginRemember: $("#login-remember"),
  loginFormBack: $("#login-form-back"),
  loginDevicePicker: $("#login-device-picker"),
  loginLocalRow: $("#login-local-row"),
  loginLocalForget: $("#login-local-forget"),
  loginLocalDevice: $("#login-local-device"),
  loginLocalDetail: $("#login-local-detail"),
  loginLocalStatus: $("#login-local-status"),
  loginRememberedList: $("#login-remembered-list"),
  loginRemoteDevice: $("#login-remote-device"),
  connectionOverlay: $("#connection-overlay"),
  connectionOverlayTitle: $("#connection-overlay-title"),
  connectionOverlayMessage: $("#connection-overlay-message"),
  connectionRetry: $("#connection-retry"),
  themeCycle: $("#theme-cycle"),
  themeMode: $("#theme-mode"),
  sidebarScroll: $(".sidebar-scroll"),
  workspaceList: $("#workspace-list"),
  createWorkspace: $("#create-workspace"),
  openWorkspace: $("#open-workspace"),
  openSettings: $("#open-settings"),
  agents: $("#agent-list"),
  addAgent: $("#add-agent"),
  mobileSidebarToggle: $("#mobile-sidebar-toggle"),
  mobileSidebarBackdrop: $("#mobile-sidebar-backdrop"),
  tabs: $("#view-tabs"),
  terminalTabs: $("#terminal-tabs"),
  chatView: $("#chat-view"),
  systemPromptView: $("#system-prompt-view"),
  systemPromptMode: $("#system-prompt-mode"),
  systemPromptInput: $("#system-prompt-input"),
  systemPromptStatus: $("#system-prompt-status"),
  systemPromptRestore: $("#system-prompt-restore"),
  systemPromptApply: $("#system-prompt-apply"),
  workmapView: $("#workmap-view"),
  filesView: $("#files-view"),
  fileManager: $("#file-manager"),
  sessionTerminalView: $("#session-terminal-view"),
  sessionTerminalScreen: $("#session-terminal-screen"),
  sessionTerminalControls: $("#session-terminal-controls"),
  remoteControlView: $("#remote-control-view"),
  remoteControl: $("#remote-control"),
  terminalView: $("#terminal-view"),
  transcript: $("#transcript"),
  transcriptContent: $("#transcript-content"),
  scrollToBottom: $("#scroll-to-bottom"),
  objective: $("#objective-summary"),
  composer: $("#composer-shell"),
  input: $("#prompt-input"),
  inputMirror: $("#prompt-input-mirror"),
  stop: $("#stop-generation"),
  send: $("#send-prompt"),
  sendSpinner: $("#send-prompt-spinner"),
  sendLabel: $("#send-prompt-label"),
  inputHint: $("#input-hint"),
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

function bindSidebarScrollbar(element, runtime = {}) {
  if (!element?.addEventListener) return () => {};
  const schedule = runtime.setTimeout || globalThis.setTimeout.bind(globalThis);
  const cancel = runtime.clearTimeout || globalThis.clearTimeout.bind(globalThis);
  const delay = runtime.delay ?? 900;
  let hideTimer = null;
  const hide = () => {
    if (hideTimer !== null) cancel(hideTimer);
    hideTimer = null;
    element.classList.remove("scrollbar-active");
  };
  const reveal = () => {
    element.classList.add("scrollbar-active");
    if (hideTimer !== null) cancel(hideTimer);
    hideTimer = schedule(() => {
      hideTimer = null;
      element.classList.remove("scrollbar-active");
    }, delay);
  };
  element.addEventListener("scroll", reveal, { passive: true });
  element.addEventListener("pointermove", reveal, { passive: true });
  element.addEventListener("pointerleave", hide, { passive: true });
  return () => {
    hide();
    element.removeEventListener("scroll", reveal);
    element.removeEventListener("pointermove", reveal);
    element.removeEventListener("pointerleave", hide);
  };
}

let sessionTerminalIdentityKey = null;
let sessionTerminalController = null;

function getSessionTerminalController() {
  if (!sessionTerminalController) {
    sessionTerminalController = globalThis.MeSessionTerminal.create({
      container: elements.sessionTerminalScreen,
      controls: elements.sessionTerminalControls,
      request: (path, options, identity) => api(path, options, identity.workspaceId),
      onUnauthorized: () => showLogin("登录已失效，请重新登录"),
    });
  }
  return sessionTerminalController;
}

let remoteControlController = null;

function remoteControlUnauthorized() {
  remoteControlController?.authenticationLost();
  showLogin("登录已失效，请重新登录");
}

function getRemoteControlController() {
  if (!remoteControlController) {
    remoteControlController = globalThis.MeRemoteControl.create({
      container: elements.remoteControl,
      request: (action, options) => fetch(
        frontendRuntime.apiPath(`/api/remote-control/${encodeURIComponent(action)}`, "chat"),
        { cache: "no-store", ...options },
      ),
      onUnauthorized: remoteControlUnauthorized,
      notify: (message, kind) => toast(message, kind === "error"),
    });
  }
  return remoteControlController;
}

function deactivateRemoteControlView() {
  remoteControlController?.deactivate();
}

function syncRemoteControlView() {
  if (state.view.kind === "remote-control") getRemoteControlController().activate();
  else deactivateRemoteControlView();
}

let fileManagerController = null;

function getFileManagerController() {
  if (!fileManagerController) {
    fileManagerController = globalThis.MeFileManager.create({
      container: elements.fileManager,
      request: (path, options, identity) => api(path, options, identity.workspaceId),
      downloadUrl: (downloadId, identity) => frontendRuntime.apiPath(`/api/files/downloads/${encodeURIComponent(downloadId)}/content`, identity.workspaceId),
      downloadFile: runtimeCapabilities.nativeDownload ? (download, identity) => frontendRuntime.downloadFile(
        frontendRuntime.apiPath(`/api/files/downloads/${encodeURIComponent(download.download_id)}/content`, identity.workspaceId),
        download.filename,
      ) : null,
      onUnauthorized: () => showLogin("登录已失效，请重新登录"),
      writeClipboard: copyTextToClipboard,
      notify: (message, kind) => toast(message, kind === "error"),
    });
  }
  return fileManagerController;
}

function syncFileManagerView() {
  if (state.view.kind !== "files" || !state.workspaceId || !state.selectedAgent || !state.snapshot.environment?.workspace) return;
  getFileManagerController().attach({
    key: `${state.workspaceId}:${state.selectedAgent}`,
    workspaceId: state.workspaceId,
    agentId: state.selectedAgent,
    defaultPath: state.snapshot.environment.workspace,
  });
}

function eventParts(event) {
  const entry = Object.entries(event)[0];
  return entry || ["Unknown", {}];
}

function replaceElementChildren(element, ...children) {
  while (element.firstChild) element.removeChild(element.firstChild);
  for (const child of children) element.appendChild(child);
}

function readLocalPreference(key) {
  try {
    return devicePreferences?.getItem(key) ?? browserLocalStorage()?.getItem(key) ?? null;
  } catch (_) {
    return null;
  }
}

function persistLocalPreference(key, value) {
  if (persistDevicePreference(key, value)) return true;
  try {
    const storage = browserLocalStorage();
    if (!storage) return false;
    storage.setItem(key, value);
    return true;
  } catch (_) {
    return false;
  }
}


function normalizeWindowBorderStyle(value) {
  return value === WINDOW_BORDER_THEME ? WINDOW_BORDER_THEME : WINDOW_BORDER_DEFAULT;
}

function readWindowBorderStylePreference() {
  return normalizeWindowBorderStyle(readLocalPreference(WINDOW_BORDER_STYLE_PREFERENCE));
}

function applyWindowBorderStyle() {
  document.documentElement.dataset.windowBorderStyle = state.windowBorderStyle;
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

function readSendShortcutPreference() {
  if (devicePreferences) {
    try { return normalizeSendShortcut(devicePreferences.getItem(SEND_SHORTCUT_PREFERENCE)); }
    catch (_) { return SEND_SHORTCUT_MODIFIED_ENTER; }
  }
  return readSendShortcutCookie(typeof document.cookie === "string" ? document.cookie : "");
}

function persistDevicePreference(key, value) {
  if (!devicePreferences) return false;
  try {
    const result = devicePreferences.setItem(key, value);
    result?.catch?.((error) => console.warn("Unable to persist device preference", error));
  } catch (error) {
    console.warn("Unable to persist device preference", error);
  }
  return true;
}

function restoreRuntimeDevicePreferences() {
  state.sendShortcut = readSendShortcutPreference();
  state.windowBorderStyle = readWindowBorderStylePreference();
  applyWindowBorderStyle();
  elements.loginSettings?.classList.toggle("hidden", !runtimeCapabilities.windowBorderStyle);
  const activeTheme = globalThis.MeTheme.apply(
    globalThis,
    globalThis.MeTheme.readStored(globalThis),
  );
  globalThis.MeTheme.syncControls(elements.themeCycle, elements.themeMode, activeTheme);
}

function setSendShortcut(value) {
  state.sendShortcut = normalizeSendShortcut(value);
  if (!persistDevicePreference(SEND_SHORTCUT_PREFERENCE, state.sendShortcut)) {
    document.cookie = `${SEND_SHORTCUT_COOKIE}=${encodeURIComponent(state.sendShortcut)}; Max-Age=31536000; Path=/; SameSite=Lax`;
  }
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

function browserLocalStorage() {
  try { return globalThis.localStorage || null; } catch (_) { return null; }
}

function readWorkspaceDisclosure(storage = browserLocalStorage()) {
  try {
    const raw = storage?.getItem(WORKSPACE_DISCLOSURE_STORAGE_KEY);
    if (!raw) return new Map();
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return new Map();
    return new Map(Object.entries(parsed).filter(([workspaceId, expanded]) =>
      workspaceId && typeof expanded === "boolean"));
  } catch (_) {
    return new Map();
  }
}

function persistWorkspaceDisclosure(disclosure, storage = browserLocalStorage()) {
  try {
    if (!storage) return false;
    const entries = [...disclosure]
      .filter(([workspaceId, expanded]) => workspaceId && typeof expanded === "boolean")
      .sort(([left], [right]) => String(left).localeCompare(String(right)));
    if (!entries.length) {
      storage.removeItem(WORKSPACE_DISCLOSURE_STORAGE_KEY);
      return true;
    }
    const serialized = {};
    for (const [workspaceId, expanded] of entries) serialized[workspaceId] = expanded;
    storage.setItem(WORKSPACE_DISCLOSURE_STORAGE_KEY, JSON.stringify(serialized));
    return true;
  } catch (_) {
    return false;
  }
}

function workspaceExpanded(workspaceId, disclosure = state.workspaceDisclosure) {
  return disclosure.get(String(workspaceId)) !== false;
}

function setWorkspaceExpanded(workspaceId, expanded, disclosure = state.workspaceDisclosure, storage = browserLocalStorage()) {
  const normalized = String(workspaceId || "");
  if (!normalized) return false;
  disclosure.set(normalized, Boolean(expanded));
  persistWorkspaceDisclosure(disclosure, storage);
  return true;
}

function pruneWorkspaceDisclosure(validWorkspaceIds, disclosure = state.workspaceDisclosure, storage = browserLocalStorage()) {
  let changed = false;
  for (const workspaceId of disclosure.keys()) {
    if (validWorkspaceIds.has(workspaceId)) continue;
    disclosure.delete(workspaceId);
    changed = true;
  }
  if (changed) persistWorkspaceDisclosure(disclosure, storage);
  return changed;
}


function agentMeta() {
  return state.snapshot.agents.find((agent) => agent.id === state.selectedAgent) || null;
}

function synchronizeWindowTitle(meta = agentMeta()) {
  if (!runtimeCapabilities.dynamicWindowTitle) return;
  const sessionTitle = String(meta?.title || meta?.id || "").trim();
  const title = sessionTitle ? `${sessionTitle} - ${runtimeCapabilities.pageTitle}` : runtimeCapabilities.pageTitle;
  document.title = title;
  const update = frontendRuntime.setWindowTitle?.(title);
  update?.catch?.((error) => console.error("Unable to update frontend window title", error));
}

function isWorkerAgent(meta = agentMeta()) {
  return meta?.orchestrator === "worker-agent";
}

function isChatbotAgent(meta = agentMeta()) {
  return meta?.orchestrator === "chatbot";
}

function canControlRuntime(meta = agentMeta()) {
  return !!meta && (meta.kind !== "sub-agent" || isWorkerAgent(meta));
}

function edbCacheScope(snapshot = state.snapshot) {
  return String(snapshot?.environment?.workspace || "");
}

function cacheEntriesByAgent(snapshot, entries) {
  const byAgent = new Map(entries
    .filter((entry) => entry?.agentId)
    .map((entry) => [entry.agentId, entry]));
  return new Map((snapshot.agents || []).map((meta) => [meta.id, byAgent.get(meta.id)]));
}

function cacheEntryEventCount(entry) {
  if (!entry) return 0;
  if (Number.isFinite(Number(entry.eventCount))) return Math.max(0, Number(entry.eventCount));
  return Array.isArray(entry.events) ? entry.events.length : 0;
}

function cacheEntryValid(cached, meta) {
  if (!cached) return false;
  const eventCount = cacheEntryEventCount(cached);
  const authoritativeCount = Number(meta.event_count || 0);
  return (!cached.edbId || cached.edbId === meta.edb_id)
    && cached.mutationRevision === Number(meta.mutation_revision || 0)
    && eventCount <= authoritativeCount
    && (eventCount === 0 || typeof cached.lastEventHash === "string")
    && (eventCount !== authoritativeCount
      || cached.lastEventHash === (meta.last_event_hash ?? null));
}

async function loadEdbCacheEntries(snapshot) {
  const scope = edbCacheScope(snapshot);
  if (!scope) return [];
  return frontendRuntime.loadCachedSessions(edbCache, snapshot, scope);
}

function discardStoredAgentEdb(snapshot, agentId, store = null) {
  const scope = edbCacheScope(snapshot);
  const key = store?.cacheKey || (scope
    ? frontendRuntime.cacheKey(scope, agentId, store?.edbId || "")
    : "");
  if (key) void edbCache.discardSession(key);
}

function createAgentLoadProgress(meta, localEventCount, localMutationRevision) {
  const mutationRevision = Number(meta.mutation_revision) || 0;
  const targetEventCount = Math.max(0, Number(meta.event_count) || 0);
  const sameMutation = (Number(localMutationRevision) || 0) === mutationRevision;
  const startEventCount = sameMutation ? Math.max(0, Number(localEventCount) || 0) : 0;
  return startEventCount < targetEventCount
    ? { mutationRevision, startEventCount, targetEventCount } : null;
}

function loadProgressSignature(store) {
  const progress = store?.loadProgress;
  return progress
    ? `${progress.mutationRevision}:${progress.startEventCount}:${progress.targetEventCount}:${store.eventCount}` : "";
}

function prepareAgentLoadProgress(store, meta, payload, previousEventCount, previousMutationRevision) {
  const mutationRevision = Number(payload?.mutation_revision ?? meta.mutation_revision) || 0;
  const targetEventCount = Math.max(0, Number(payload?.event_count ?? meta.event_count) || 0);
  if (store.loadProgress && store.loadProgress.mutationRevision !== mutationRevision) {
    store.loadProgress = null;
  }
  if (!store.loadProgress) {
    const reset = Boolean(payload?.reset)
      || (Number(previousMutationRevision) || 0) !== mutationRevision;
    const startEventCount = reset ? 0 : Math.max(0, Number(previousEventCount) || 0);
    if (startEventCount < targetEventCount) {
      store.loadProgress = { mutationRevision, startEventCount, targetEventCount };
    }
  }
}

function settleAgentLoadProgress(store) {
  if (store.loadProgress && store.eventCount >= store.loadProgress.targetEventCount) {
    store.loadProgress = null;
  }
}

function createAgentStore(meta, cached = null, snapshot = state.snapshot) {
  const events = Array.isArray(cached?.events) ? cached.events : [];
  const eventCount = events.length;
  const scope = edbCacheScope(snapshot);
  const edbId = String(meta.edb_id || "");
  const mutationRevision = cached ? Number(cached.mutationRevision) || 0 : Number(meta.mutation_revision) || 0;
  return {
    edbId,
    cacheKey: cached?.key || (scope ? frontendRuntime.cacheKey(scope, meta.id, edbId) : ""),
    events,
    eventCount,
    mutationRevision,
    lastEventHash: cached?.lastEventHash ?? null,
    promptSubmissionRevision: Number(meta.prompt_submission_revision || 0),
    inputDraftRevision: Number(meta.input_draft_revision || 0),
    pendingPromptSubmission: null,
    projection: emptyProjection(),
    workmap: emptyWorkMap(),
    turnHistory: null,
    summary: projectAgentSummary(events),
    projectedOrder: 0,
    needsReplay: true,
    needsTurnHistory: false,
    loadProgress: createAgentLoadProgress(meta, eventCount, mutationRevision),
  };
}

async function hydrateEdbCache(snapshot) {
  if (!snapshot) throw new Error("同步响应未提供缓存元数据");
  state.snapshot = snapshot;
  state.snapshotInitialized = true;
  reconcileAgents();
  renderAgents();
  const entries = await loadEdbCacheEntries(snapshot);
  const agentIds = new Set((snapshot.agents || []).map((agent) => agent.id));
  for (const entry of entries) {
    if (entry.agentId && !agentIds.has(entry.agentId) && entry.key) void edbCache.discardSession(entry.key);
  }
  const cachedByAgent = cacheEntriesByAgent(snapshot, entries);
  for (const meta of snapshot.agents || []) {
    const cached = cachedByAgent.get(meta.id);
    if (!cached || cacheEntryValid(cached, meta)) continue;
    cachedByAgent.delete(meta.id);
    if (cached.key) await edbCache.discardSession(cached.key);
  }
  state.stores.clear();
  for (const meta of snapshot.agents || []) {
    const cached = cachedByAgent.get(meta.id);
    state.stores.set(meta.id, createAgentStore(meta, cached || null, snapshot));
    state.drafts.set(meta.id, String(meta.input_draft || ""));
  }
  state.edbCacheInitialized = true;
  restoreDraft();
  renderAll();
  renderConnectionOverlayForPhase();
}

function persistWorkspaceAgentEdb(snapshot, meta, store, replace = false, batch = null) {
  const scope = edbCacheScope(snapshot);
  if (!store || !scope || !store.edbId) return;
  edbCache.saveSession({
    edbId: store.edbId,
    scope,
    agentId: meta.id,
    mutationRevision: store.mutationRevision,
    lastEventHash: store.lastEventHash,
    gatewayLabel: frontendRuntime.endpoint || "",
    workspaceLabel: MeEdbCache.workspaceName(scope),
    sessionLabel: meta.title || meta.id,
    events: store.events,
    replace,
    delta: batch ? { ...batch, reset: replace } : null,
  });
}

function persistAgentEdb(meta, store, replace = false, batch = null) {
  persistWorkspaceAgentEdb(state.snapshot, meta, store, replace, batch);
}

function setWindowBorderStyle(value) {
  state.windowBorderStyle = normalizeWindowBorderStyle(value);
  applyWindowBorderStyle();
  persistLocalPreference(WINDOW_BORDER_STYLE_PREFERENCE, state.windowBorderStyle);
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

async function api(path, options = {}, workspaceId = state.workspaceId) {
  const response = await fetch(frontendRuntime.apiPath(path, workspaceId), { cache: "no-store", ...options });
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
    .then(() => frontendRuntime.persistSelection(api, workspaceId, agentId))
    .catch((error) => {
      if (error.status === 401) showLogin("登录已失效，请重新登录");
    });
  return gatewaySelectionSave;
}

function emptyGatewayWorkspaceState() {
  return {
    snapshot: {
      revision: 0, environment: null, agents: [], models: [], orchestrators: [], default_orchestrator: null,
      chatbot_default_static_prompt: "",
      tool_visibility: { hidden_names: [], hidden_prefixes: [], activity_names: [] },
    },
    stores: new Map(), drafts: new Map(), draftSync: new Map(), promptDrafts: new Map(), selectedAgent: null,
    apiActivity: { agentId: null, active: false, receivedSseEvents: 0 },
    eventRecovery: null,
    pendingAgentSelection: null, view: { kind: "chat", sessionId: null }, terminals: [],
    terminalRevisions: new Map(), terminalFollowBottom: true, expandedTools: new Set(),
    expandedHistoryObjectives: new Set(), workerActivityIndexes: new Map(),
    terminalFrames: new Map(), terminalFramesUnavailable: new Set(), snapshotInitialized: false,
    edbCacheInitialized: false, cacheValidated: false, catchUpPending: true,
    backgroundNextSyncAt: 0, backgroundFailures: 0,
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
    promptDrafts: state.promptDrafts, selectedAgent: state.selectedAgent,
    apiActivity: state.apiActivity, eventRecovery: state.eventRecovery,
    pendingAgentSelection: state.pendingAgentSelection, view: state.view, terminals: state.terminals,
    terminalRevisions: state.terminalRevisions, terminalFollowBottom: state.terminalFollowBottom,
    expandedTools: state.expandedTools, expandedHistoryObjectives: state.expandedHistoryObjectives,
    workerActivityIndexes: state.workerActivityIndexes, terminalFrames: state.terminalFrames,
    terminalFramesUnavailable: state.terminalFramesUnavailable, snapshotInitialized: state.snapshotInitialized,
    edbCacheInitialized: state.edbCacheInitialized, catchUpPending: state.activeCatchUpPending,
    backgroundNextSyncAt: 0,
    scrollTop: elements.transcript.scrollTop, followBottom: transcriptBottomFollower.isFollowing(),
  });
  for (const sync of workspace.draftSync.values()) {
    clearDraftBatch(sync);
    sync.paused = true;
  }
}


function activateWorkspace(workspaceId, preferredAgent = null, beginPolling = true) {
  if (!workspaceId) return;
  if (state.workspaceId === workspaceId) {
    if (preferredAgent) selectAgent(preferredAgent);
    return;
  }
  cancelBackgroundWorkspaceSync();
  deactivateSessionTerminalView();
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
  state.promptDrafts = workspace.promptDrafts;
  state.selectedAgent = preferredAgent || workspace.selectedAgent;
  state.apiActivity = workspace.apiActivity;
  state.pendingAgentSelection = workspace.pendingAgentSelection;
  state.eventRecovery = workspace.eventRecovery;
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
  state.edbCacheInitialized = workspace.edbCacheInitialized;
  state.activeCatchUpPending = !workspace.edbCacheInitialized || workspace.catchUpPending;
  const selectedMeta = state.snapshot.agents.find((agent) => agent.id === state.selectedAgent);
  prepareSelectedEventRecovery(selectedMeta, null, true);
  resetConnectionForInitialSync();
  state.pendingRender = emptyRenderRequest();
  state.composing = false;
  delete elements.terminalScreen.dataset.terminalKey;
  delete elements.terminalScreen.dataset.revision;
  restoreDraft();
  transcriptBottomFollower.restore(workspace.followBottom);
  transcriptVirtualizer?.prepareScroll(workspace.scrollTop, workspace.followBottom);
  renderAll();
  requestAnimationFrame(() => {
    elements.transcript.scrollTop = workspace.followBottom ? elements.transcript.scrollHeight : workspace.scrollTop;
    transcriptVirtualizer?.noteScroll();
    transcriptBottomFollower.restore(workspace.followBottom);
  });
  persistGatewaySelection(workspaceId, state.selectedAgent);
  if (state.workspaceId !== workspaceId) return;
  const meta = state.snapshot.agents.find((agent) => agent.id === state.selectedAgent);
  prepareSelectedEventRecovery(meta, null, true);
  renderAll();
  if (beginPolling) startHttpPolling();
  scheduleBackgroundWorkspaceSync(0);
}

function backgroundSyncProgressSignature(workspace) {
  return JSON.stringify({
    snapshotRevision: workspace.snapshotInitialized ? workspace.snapshot.revision : null,
    agents: [...workspace.stores]
      .map(([id, store]) => [id, store.eventCount ?? store.events.length, store.mutationRevision])
      .sort((left, right) => String(left[0]).localeCompare(String(right[0]))),
  });
}

function backgroundSyncRequestBody(workspace) {
  return {
    snapshot_revision: workspace.snapshotInitialized ? workspace.snapshot.revision : null,
    agents: [...workspace.stores].map(([id, store]) => ({
      id,
      event_count: store.eventCount ?? store.events.length,
      mutation_revision: store.mutationRevision,
      cursor_event_hash: workspace.cacheValidated ? null : store.lastEventHash ?? null,
    })),
    cache_metadata_only: !workspace.edbCacheInitialized,
    selected_agent: null,
    terminal_session: null,
    terminal_revision: null,
  };
}

function backgroundSyncCanRun() {
  return !state.pageClosing
    && !state.startupMetadataPending
    && (!state.authRequired || state.authenticated)
    && state.connectionPhase === "connected"
    && !state.activeCatchUpPending;
}

function backgroundSyncOperationCurrent(operation) {
  return state.backgroundSyncOperation === operation
    && !state.pageClosing
    && state.workspaceId !== operation.workspaceId
    && state.workspaceStates.get(operation.workspaceId) === operation.workspace;
}

function cancelBackgroundWorkspaceSync(workspaceId = null) {
  if (workspaceId === null) {
    clearTimeout(state.backgroundSyncTimer);
    state.backgroundSyncTimer = null;
    state.backgroundSyncDueAt = null;
  }
  const operation = state.backgroundSyncOperation;
  if (!operation || (workspaceId !== null && operation.workspaceId !== workspaceId)) return;
  state.backgroundSyncOperation = null;
  operation.controller?.abort();
}

function scheduleBackgroundWorkspaceSync(delay = 0) {
  if (!backgroundSyncCanRun() || state.backgroundSyncOperation) return;
  const dueAt = Date.now() + Math.max(0, Number(delay) || 0);
  if (state.backgroundSyncTimer !== null && state.backgroundSyncDueAt <= dueAt) return;
  clearTimeout(state.backgroundSyncTimer);
  state.backgroundSyncDueAt = dueAt;
  state.backgroundSyncTimer = setTimeout(() => {
    state.backgroundSyncTimer = null;
    state.backgroundSyncDueAt = null;
    void requestBackgroundWorkspaceSync();
  }, Math.max(0, dueAt - Date.now()));
}

function nextBackgroundWorkspace(now = Date.now()) {
  const ids = (state.gateway.workspaces || [])
    .map((workspace) => workspace.id)
    .filter((workspaceId) => workspaceId !== state.workspaceId);
  if (!ids.length) return { workspaceId: null, workspace: null, wait: BACKGROUND_SYNC_IDLE_MS };
  const start = state.backgroundSyncCursor % ids.length;
  let earliest = Number.POSITIVE_INFINITY;
  for (let offset = 0; offset < ids.length; offset += 1) {
    const index = (start + offset) % ids.length;
    const workspaceId = ids[index];
    const workspace = gatewayWorkspaceState(workspaceId);
    const nextAt = Math.max(0, Number(workspace.backgroundNextSyncAt) || 0);
    earliest = Math.min(earliest, nextAt);
    if (nextAt <= now) {
      state.backgroundSyncCursor = (index + 1) % ids.length;
      return { workspaceId, workspace, wait: 0 };
    }
  }
  return {
    workspaceId: null,
    workspace: null,
    wait: Math.max(0, Number.isFinite(earliest) ? earliest - now : BACKGROUND_SYNC_IDLE_MS),
  };
}

function observeBackgroundInputDraft(workspace, meta, store) {
  if (store.pendingPromptSubmission) return false;
  const revision = Number(meta.input_draft_revision || 0);
  if (revision <= store.inputDraftRevision) return false;
  const content = String(meta.input_draft || "");
  const sync = workspace.draftSync.get(meta.id);
  store.inputDraftRevision = revision;
  if (sync && (sync.inFlight || sync.desired !== sync.sent)) {
    sync.sent = content;
    return false;
  }
  workspace.drafts.set(meta.id, content);
  if (sync) {
    sync.desired = content;
    sync.sent = content;
    sync.pendingRemote = null;
  }
  return true;
}

function reconcileBackgroundAgents(workspace, snapshot) {
  const ids = new Set((snapshot.agents || []).map((agent) => agent.id));
  let changed = false;
  for (const id of workspace.stores.keys()) {
    if (ids.has(id)) continue;
    discardStoredAgentEdb(snapshot, id, workspace.stores.get(id));
    workspace.stores.delete(id);
    workspace.drafts.delete(id);
    clearDraftBatch(workspace.draftSync.get(id));
    workspace.draftSync.delete(id);
    workspace.workerActivityIndexes.delete(id);
    changed = true;
  }
  for (const meta of snapshot.agents || []) {
    let store = workspace.stores.get(meta.id);
    if (!store) {
      store = createAgentStore(meta, null, snapshot);
      workspace.stores.set(meta.id, store);
      workspace.drafts.set(meta.id, String(meta.input_draft || ""));
      changed = true;
    } else {
      observeBackgroundInputDraft(workspace, meta, store);
      store.promptSubmissionRevision = Number(meta.prompt_submission_revision || 0);
    }
  }
  if (workspace.pendingAgentSelection && ids.has(workspace.pendingAgentSelection)) {
    workspace.selectedAgent = workspace.pendingAgentSelection;
    workspace.pendingAgentSelection = null;
  }
  if (!workspace.selectedAgent || !ids.has(workspace.selectedAgent)) {
    workspace.selectedAgent = (snapshot.agents || []).find((agent) => agent.id === "main")?.id
      || snapshot.agents?.[0]?.id || null;
  }
  return changed;
}

async function hydrateBackgroundEdbCache(operation, snapshot) {
  if (!snapshot) throw new Error("同步响应未提供缓存元数据");
  if (!backgroundSyncOperationCurrent(operation)) return false;
  const workspace = operation.workspace;
  workspace.snapshot = snapshot;
  reconcileBackgroundAgents(workspace, snapshot);
  renderAgents();
  const entries = await loadEdbCacheEntries(snapshot);
  if (!backgroundSyncOperationCurrent(operation)) return false;
  const agentIds = new Set((snapshot.agents || []).map((agent) => agent.id));
  for (const entry of entries) {
    if (entry.agentId && !agentIds.has(entry.agentId) && entry.key) void edbCache.discardSession(entry.key);
  }
  const cachedByAgent = cacheEntriesByAgent(snapshot, entries);
  workspace.snapshot = snapshot;
  workspace.snapshotInitialized = true;
  workspace.stores.clear();
  for (const meta of snapshot.agents || []) {
    const cached = cachedByAgent.get(meta.id);
    const valid = cacheEntryValid(cached, meta);
    if (cached && !valid && cached.key) await edbCache.discardSession(cached.key);
    workspace.stores.set(meta.id, createAgentStore(meta, valid ? cached : null, snapshot));
    workspace.drafts.set(meta.id, String(meta.input_draft || ""));
  }
  workspace.edbCacheInitialized = true;
  workspace.cacheValidated = false;
  workspace.catchUpPending = true;
  reconcileBackgroundAgents(workspace, snapshot);
  return true;
}

function syncBackgroundAgentEvents(workspace, meta, payload) {
  let store = workspace.stores.get(meta.id);
  const loadingBefore = loadProgressSignature(store);
  let changed = false;
  if (!store) {
    store = createAgentStore(meta, null, workspace.snapshot);
    workspace.stores.set(meta.id, store);
    workspace.drafts.set(meta.id, String(meta.input_draft || ""));
    changed = true;
  }
  observeBackgroundInputDraft(workspace, meta, store);
  store.promptSubmissionRevision = Number(meta.prompt_submission_revision || 0);
  const previousEventCount = store.eventCount;
  const previousMutationRevision = store.mutationRevision;
  settleAgentLoadProgress(store);
  prepareAgentLoadProgress(store, meta, payload, previousEventCount, previousMutationRevision);
  if (!payload && store.eventCount === meta.event_count
      && store.mutationRevision === meta.mutation_revision) {
    return {
      changed, summaryChanged: false,
      loadChanged: loadingBefore !== loadProgressSignature(store),
    };
  }
  if (!payload) {
    return {
      changed, summaryChanged: false,
      loadChanged: loadingBefore !== loadProgressSignature(store),
    };
  }
  const previousSummary = JSON.stringify(store.summary);
  const events = Array.isArray(payload.events) ? payload.events : [];
  if (payload.reset) {
    store.events = events;
    store.eventCount = events.length;
    store.summary = projectAgentSummary(events);
    store.projectedOrder = 0;
    store.needsReplay = true;
    workspace.workerActivityIndexes.delete(meta.id);
  } else {
    store.events.push(...events);
    store.eventCount = previousEventCount + events.length;
    updateAgentSummary(store.summary, events);
  }
  store.mutationRevision = payload.mutation_revision;
  store.lastEventHash = payload.cursor_event_hash ?? null;
  settleAgentLoadProgress(store);
  if (payload.reset || events.length > 0) {
    persistWorkspaceAgentEdb(workspace.snapshot, meta, store, Boolean(payload.reset), {
      startOrder: payload.reset ? 0 : previousEventCount,
      eventCount: Number(payload.event_count ?? store.eventCount),
      expectedEventCount: previousEventCount,
      expectedMutationRevision: previousMutationRevision,
      events,
    });
  }
  return {
    changed: true,
    summaryChanged: previousSummary !== JSON.stringify(store.summary),
    loadChanged: loadingBefore !== loadProgressSignature(store),
  };
}

function applyBackgroundSyncState(workspace, payload) {
  const previousSnapshot = workspace.snapshot;
  if (payload.snapshot) {
    workspace.snapshot = payload.snapshot;
    workspace.snapshotInitialized = true;
  }
  if (!workspace.snapshotInitialized) throw new Error("同步响应未提供初始状态");
  const presentationChanged = snapshotPresentationSignature(previousSnapshot)
    !== snapshotPresentationSignature(workspace.snapshot);
  const structureChanged = reconcileBackgroundAgents(workspace, workspace.snapshot);
  const updates = new Map((payload.event_updates || []).map((update) => [update.agent_id, update]));
  const eventChanges = workspace.snapshot.agents.map((meta) =>
    syncBackgroundAgentEvents(workspace, meta, updates.get(meta.id)));
  workspace.cacheValidated = true;
  return presentationChanged || structureChanged
    || eventChanges.some((change) => change.changed || change.summaryChanged || change.loadChanged);
}

async function requestBackgroundWorkspaceSync() {
  if (!backgroundSyncCanRun() || state.backgroundSyncOperation) return;
  const candidate = nextBackgroundWorkspace();
  if (!candidate.workspaceId) {
    scheduleBackgroundWorkspaceSync(candidate.wait);
    return;
  }
  const controller = typeof AbortController === "function" ? new AbortController() : null;
  const operation = {
    workspaceId: candidate.workspaceId, workspace: candidate.workspace, controller,
  };
  state.backgroundSyncOperation = operation;
  const progressBefore = backgroundSyncProgressSignature(candidate.workspace);
  const timeout = setTimeout(() => controller?.abort(), HTTP_SYNC_TIMEOUT_MS);
  let sidebarChanged = false;
  try {
    const message = await api("/api/sync", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      signal: controller?.signal,
      body: JSON.stringify(backgroundSyncRequestBody(candidate.workspace)),
    }, candidate.workspaceId);
    if (!backgroundSyncOperationCurrent(operation)) return;
    if (message.cache_metadata_only) {
      if (!await hydrateBackgroundEdbCache(operation, message.snapshot)) return;
      sidebarChanged = true;
      candidate.workspace.backgroundFailures = 0;
      candidate.workspace.backgroundNextSyncAt = 0;
      candidate.workspace.catchUpPending = true;
    } else {
      sidebarChanged = applyBackgroundSyncState(candidate.workspace, message);
      if (!backgroundSyncOperationCurrent(operation)) return;
      const madeProgress = progressBefore !== backgroundSyncProgressSignature(candidate.workspace);
      candidate.workspace.backgroundFailures = 0;
      candidate.workspace.catchUpPending = Boolean(message.more_events);
      candidate.workspace.backgroundNextSyncAt = message.more_events && madeProgress
        ? 0 : Date.now() + (message.more_events ? HTTP_SYNC_ACTIVE_MS : BACKGROUND_SYNC_IDLE_MS);
    }
    if (sidebarChanged) renderAgents();
  } catch (error) {
    if (!backgroundSyncOperationCurrent(operation)) return;
    if (error.status === 401) {
      showLogin("登录已失效，请重新登录");
      return;
    }
    candidate.workspace.backgroundFailures += 1;
    const retry = Math.min(
      BACKGROUND_SYNC_RETRY_MAX_MS,
      HTTP_SYNC_IDLE_MS * (2 ** Math.min(candidate.workspace.backgroundFailures - 1, 5)),
    );
    candidate.workspace.backgroundNextSyncAt = Date.now() + retry;
  } finally {
    clearTimeout(timeout);
    if (state.backgroundSyncOperation === operation) {
      state.backgroundSyncOperation = null;
      scheduleBackgroundWorkspaceSync(0);
    }
  }
}

function applyGatewaySnapshot(snapshot) {
  state.gateway = snapshot;
  const ids = new Set((snapshot.workspaces || []).map((workspace) => workspace.id));
  for (const workspace of snapshot.workspaces || []) gatewayWorkspaceState(workspace.id);
  for (const notice of snapshot.notices || []) {
    if (Number(notice.id) > state.lastNoticeId) toast(notice.message, true);
    state.lastNoticeId = Math.max(state.lastNoticeId, Number(notice.id) || 0);
  }
  if (state.workspaceId && !ids.has(state.workspaceId)) activateWorkspace("chat");
  for (const id of state.workspaceStates.keys()) {
    if (ids.has(id)) continue;
    cancelBackgroundWorkspaceSync(id);
    state.workspaceStates.delete(id);
  }
  renderAgents();
}

async function refreshGatewayState() {
  if (state.gatewayRefreshInFlight || !state.authenticated) return;
  state.gatewayRefreshInFlight = true;
  try {
    const snapshot = await frontendRuntime.loadGatewayState(api);
    applyGatewaySnapshot(snapshot);
    scheduleBackgroundWorkspaceSync(0);
  } catch (error) {
    if (error.status === 401) showLogin("登录已失效，请重新登录");
  } finally {
    state.gatewayRefreshInFlight = false;
  }
}


function applyGatewayStartupMetadata(workspaceId, snapshot) {
  const workspace = gatewayWorkspaceState(workspaceId);
  if (workspace.edbCacheInitialized) return;
  workspace.snapshot = snapshot;
  const ids = new Set((snapshot.agents || []).map((agent) => agent.id));
  if (!workspace.selectedAgent || !ids.has(workspace.selectedAgent)) {
    workspace.selectedAgent = (snapshot.agents || []).find((agent) => agent.id === "main")?.id
      || snapshot.agents?.[0]?.id || null;
  }
}

async function loadGatewayStartupMetadata(activeWorkspaceId) {
  state.startupMetadataPending = true;
  try {
    const workspaceIds = (state.gateway.workspaces || [])
      .map((workspace) => workspace.id)
      .filter((workspaceId) => workspaceId !== activeWorkspaceId);
    for (const workspaceId of workspaceIds) {
      if (state.pageClosing || !state.authenticated) break;
      try {
        const snapshot = await api("/api/snapshot", {}, workspaceId);
        if (workspaceId === state.workspaceId
            || !(state.gateway.workspaces || []).some((workspace) => workspace.id === workspaceId)) continue;
        applyGatewayStartupMetadata(workspaceId, snapshot);
        renderAgents();
      } catch (error) {
        if (error.status === 401) {
          showLogin("登录已失效，请重新登录");
          return;
        }
      }
    }
  } finally {
    state.startupMetadataPending = false;
    renderAgents();
    scheduleBackgroundWorkspaceSync(0);
  }
}

async function initializeGateway() {
  const snapshot = await frontendRuntime.loadGatewayState(api);
  applyGatewaySnapshot(snapshot);
  const ids = new Set((snapshot.workspaces || []).map((workspace) => workspace.id));
  const workspaceId = ids.has(snapshot.selected_workspace_id) ? snapshot.selected_workspace_id : "chat";
  activateWorkspace(workspaceId, snapshot.selected_agent_id, false);
  void loadGatewayStartupMetadata(workspaceId);
}

function setLoginView(view) {
  const next = rememberedDevices && view === "devices" ? "devices" : "form";
  state.loginView = next;
  elements.loginScreen.dataset.loginView = next;
  if (next === "devices") {
    elements.loginTitle.textContent = "选择设备";
    elements.loginDescription.textContent = "连接本机或已记住的设备。";
  } else if (rememberedDevices) {
    elements.loginTitle.textContent = "连接远程设备";
    elements.loginDescription.textContent = "输入服务地址和访问密码。";
  } else {
    elements.loginTitle.textContent = "登录";
    elements.loginDescription.textContent = "请输入访问密码。";
  }
}

function rememberedDeviceLabel(endpoint) {
  try {
    const url = new URL(endpoint);
    return url.host || endpoint;
  } catch (_) {
    return endpoint;
  }
}

function rememberedDeviceForEndpoint(endpoint) {
  return rememberedDevices?.list().find((device) => device.endpoint === endpoint) || null;
}

function loginDeviceIcon() {
  const icon = document.createElement("span");
  icon.className = "login-device-icon";
  icon.setAttribute("aria-hidden", "true");
  icon.innerHTML = '<svg viewBox="0 0 24 24"><rect x="5" y="4" width="14" height="6" rx="2"></rect><rect x="5" y="14" width="14" height="6" rx="2"></rect><path d="M9 7h.01M9 17h.01M13 7h3M13 17h3"></path></svg>';
  return icon;
}

function loginDeviceStatus(online) {
  const status = document.createElement("span");
  status.className = `login-device-status ${online ? "online" : "offline"}`;
  status.textContent = online ? "在线" : "离线";
  return status;
}

function renderLoginDevices() {
  if (!rememberedDevices) return;
  const local = state.localDevice;
  const localRemembered = rememberedDeviceForEndpoint(local.endpoint);
  elements.loginLocalRow.classList.toggle("has-remembered-password", Boolean(localRemembered));
  elements.loginLocalForget.classList.toggle("hidden", !localRemembered);
  elements.loginLocalForget.disabled = state.loginBusy;
  elements.loginLocalDevice.disabled = state.loginBusy || !local.online;
  elements.loginLocalDetail.textContent = local.online
    ? local.endpoint : "未发现正在运行的 ME Gateway";
  elements.loginLocalStatus.className = `login-device-status ${local.online ? "online" : "offline"}`;
  elements.loginLocalStatus.textContent = local.online ? "在线" : "离线";
  elements.loginRemoteDevice.disabled = state.loginBusy;
  elements.loginRememberedList.textContent = "";
  for (const device of rememberedDevices.list()) {
    if (device.endpoint === local.endpoint) continue;
    const row = document.createElement("div");
    row.className = "login-remembered-row";
    const connect = document.createElement("button");
    connect.type = "button";
    connect.className = "login-device-row";
    connect.dataset.loginEndpoint = device.endpoint;
    connect.disabled = state.loginBusy || !device.online;
    connect.append(loginDeviceIcon());
    const copy = document.createElement("span");
    copy.className = "login-device-copy";
    const title = document.createElement("strong");
    title.textContent = rememberedDeviceLabel(device.endpoint);
    const detail = document.createElement("span");
    detail.textContent = device.endpoint;
    copy.append(title, detail);
    connect.append(copy, loginDeviceStatus(device.online));
    const forget = document.createElement("button");
    forget.type = "button";
    forget.className = "login-device-forget";
    forget.dataset.forgetEndpoint = device.endpoint;
    forget.disabled = state.loginBusy;
    forget.title = `忘记 ${title.textContent}`;
    forget.setAttribute("aria-label", forget.title);
    forget.innerHTML = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 7h16M9 7V4h6v3M7 7l1 13h8l1-13M10 11v5M14 11v5"></path></svg>';
    row.append(connect, forget);
    elements.loginRememberedList.append(row);
  }
}

function setLoginBusy(busy) {
  state.loginBusy = Boolean(busy);
  elements.loginSubmit.disabled = state.loginBusy;
  elements.loginSubmit.textContent = state.loginBusy ? "正在连接…" : "登录";
  elements.loginEndpoint.disabled = state.loginBusy;
  elements.loginPassword.disabled = state.loginBusy;
  elements.loginRemember.disabled = state.loginBusy;
  elements.loginFormBack.disabled = state.loginBusy;
  renderLoginDevices();
}

function markFrontendWindowReady() {
  const readiness = frontendRuntime.windowReady?.();
  readiness?.catch?.((error) => console.error("Unable to reveal frontend window", error));
}

function showLogin(message = "") {
  const alreadyVisible = !state.authenticated
    && !elements.loginScreen.classList.contains("hidden");
  if (rememberedDevices) setLoginView("devices");
  else setLoginView("form");
  elements.loginError.textContent = message;
  renderLoginDevices();
  synchronizeWindowTitle(null);
  if (alreadyVisible) {
    markFrontendWindowReady();
    return;
  }

  deactivateSessionTerminalView();
  stopHttpPolling();
  cancelBackgroundWorkspaceSync();
  state.authenticated = false;
  state.connectionHadSuccess = false;
  state.reconnectAttempt = 0;
  setConnectionPhase("failed");
  hideConnectionOverlay();
  elements.app.classList.add("hidden");
  elements.loginScreen.classList.remove("hidden");
  const target = state.loginView === "devices"
    ? (elements.loginLocalDevice.disabled ? elements.loginRemoteDevice : elements.loginLocalDevice)
    : (runtimeCapabilities.targetConfiguration && !frontendRuntime.endpoint
      ? elements.loginEndpoint : elements.loginPassword);
  target?.focus();
  markFrontendWindowReady();
}

function showLoginPreservingView(message) {
  const view = state.loginView;
  showLogin(message);
  if (rememberedDevices) setLoginView(view);
  elements.loginError.textContent = message;
}

function showApplication() {
  elements.loginScreen.classList.add("hidden");
  elements.app.classList.remove("hidden");
  elements.loginError.textContent = "";
  elements.addAgent.disabled = true;
  synchronizeWindowTitle(null);
  markFrontendWindowReady();
}

async function initializeAuthentication() {
  try {
    const bootstrap = await frontendRuntime.initialize();
    restoreRuntimeDevicePreferences();
    if (runtimeCapabilities.targetConfiguration) {
      state.authRequired = true;
      state.localDevice = {
        endpoint: String(bootstrap.localDevice?.endpoint || "http://127.0.0.1:38200"),
        online: Boolean(bootstrap.localDevice?.online),
        requiresPassword: Boolean(bootstrap.localDevice?.requiresPassword),
      };
      if (elements.loginEndpoint) elements.loginEndpoint.value = bootstrap.endpoint || "";
      renderLoginDevices();
      if (rememberedDevices) {
        showLogin();
        return;
      }
      if (!bootstrap.endpoint) {
        showLogin();
        return;
      }
    }
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
    showLogin(runtimeCapabilities.targetConfiguration
      ? error.message : `无法读取登录状态：${error.message}`);
  }
}

async function performLogin({ endpoint, password, remember }) {
  setLoginBusy(true);
  elements.loginError.textContent = "";
  try {
    let configuredEndpoint = "";
    if (runtimeCapabilities.targetConfiguration) {
      const configured = await frontendRuntime.configureTarget(endpoint);
      configuredEndpoint = configured.endpoint;
      elements.loginEndpoint.value = configuredEndpoint;
      const status = await api("/api/auth/status");
      state.authRequired = Boolean(status.required);
      state.authenticated = Boolean(status.authenticated);
    }
    if (!runtimeCapabilities.targetConfiguration || (state.authRequired && !state.authenticated)) {
      await api("/api/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ password, browser_port: BROWSER_PORT }),
      });
      state.authenticated = true;
    }
    if (rememberedDevices && remember && configuredEndpoint) {
      await rememberedDevices.remember(configuredEndpoint, password);
    }
    elements.loginPassword.value = "";
    renderLoginDevices();
    showApplication();
    await initializeGateway();
    restoreDraft();
    startHttpPolling();
  } catch (error) {
    showLoginPreservingView(error.message);
    if (state.loginView === "form") {
      if (runtimeCapabilities.targetConfiguration && !frontendRuntime.endpoint) elements.loginEndpoint?.select();
      else elements.loginPassword.select();
    }
  } finally {
    setLoginBusy(false);
  }
}

async function submitLogin(event) {
  event.preventDefault();
  await performLogin({
    endpoint: elements.loginEndpoint?.value || "",
    password: elements.loginPassword.value,
    remember: Boolean(elements.loginRemember?.checked),
  });
}

async function loginRememberedDevice(endpoint) {
  const device = rememberedDeviceForEndpoint(endpoint);
  if (!device?.online) return;
  await performLogin({ endpoint: device.endpoint, password: device.password, remember: true });
}

async function loginLocalDevice() {
  const local = state.localDevice;
  if (!local.online) return;
  const remembered = rememberedDeviceForEndpoint(local.endpoint);
  await performLogin({
    endpoint: local.endpoint,
    password: remembered?.password || "",
    remember: Boolean(remembered),
  });
}

async function forgetRememberedDevice(endpoint) {
  if (!rememberedDevices) return;
  setLoginBusy(true);
  elements.loginError.textContent = "";
  try {
    await rememberedDevices.forget(endpoint);
    if (elements.loginEndpoint.value === endpoint) {
      elements.loginPassword.value = "";
      elements.loginRemember.checked = false;
    }
  } catch (error) {
    elements.loginError.textContent = error.message;
  } finally {
    setLoginBusy(false);
  }
}

function setConnectionPhase(phase) {
  const changed = state.connectionPhase !== phase;
  state.connectionPhase = phase;
  state.connected = phase === "connected" || phase === "degraded";
  state.connecting = ["initial", "degraded", "reconnecting", "stabilizing"].includes(phase);
  return changed;
}

function connectionCanPoll() {
  return state.connectionPhase !== "failed"
    && !state.pageClosing
    && (!state.authRequired || state.authenticated);
}

function clearDegradedTimer() {
  clearTimeout(state.degradedTimer);
  state.degradedTimer = null;
  state.connectionFailureStartedAt = null;
}

function cancelActiveHttpSync() {
  clearTimeout(state.syncTimer);
  clearTimeout(state.reconnectTimer);
  state.syncTimer = null;
  state.reconnectTimer = null;
  state.syncGeneration += 1;
  state.syncController?.abort();
  state.syncController = null;
  state.syncInFlight = false;
}

function resetConnectionForInitialSync() {
  cancelActiveHttpSync();
  state.activeCatchUpPending = true;
  clearDegradedTimer();
  state.connectionHadSuccess = false;
  state.connectionFailureDetail = "";
  state.reconnectAttempt = 0;
  state.stabilizingSince = null;
  state.stabilizingSuccesses = 0;
  setConnectionPhase("initial");
}

function retryConnectionNow() {
  cancelActiveHttpSync();
  clearDegradedTimer();
  state.reconnectAttempt = 0;
  state.connectionFailureDetail = "";
  state.stabilizingSince = null;
  state.stabilizingSuccesses = 0;
  setConnectionPhase(state.connectionHadSuccess ? "reconnecting" : "initial");
  renderConnectionOverlayForPhase();
  startHttpPolling();
}

function startHttpPolling() {
  if (state.pageClosing || (state.authRequired && !state.authenticated)) return;
  if (state.syncInFlight) return;
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = null;
  if (state.connectionPhase === "failed") {
    setConnectionPhase(state.connectionHadSuccess ? "reconnecting" : "initial");
  } else {
    setConnectionPhase(state.connectionPhase);
  }
  renderConnectionOverlayForPhase();
  state.syncGeneration += 1;
  void requestHttpSync();
}

function stopHttpPolling() {
  cancelActiveHttpSync();
  clearDegradedTimer();
}

function schedulePollingRetry(delay) {
  clearTimeout(state.reconnectTimer);
  const generation = state.syncGeneration;
  state.reconnectTimer = setTimeout(async () => {
    state.reconnectTimer = null;
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

function enterReconnecting(error) {
  clearDegradedTimer();
  state.connectionFailureDetail = error instanceof Error
    ? error.message : String(error || "网络连接不可用");
  state.stabilizingSince = null;
  state.stabilizingSuccesses = 0;
  setConnectionPhase("reconnecting");
  state.reconnectAttempt += 1;
  const delay = Math.min(RECONNECT_MAX_MS, 250 * (2 ** Math.min(state.reconnectAttempt - 1, 5)));
  showConnectionOverlay(
    "正在重新连接",
    `${state.connectionFailureDetail}，将在 ${Math.max(1, Math.ceil(delay / 1000))} 秒内重试。`,
  );
  schedulePollingRetry(delay);
}

function promoteDegradedConnection() {
  if (state.connectionPhase !== "degraded") return;
  const detail = state.connectionFailureDetail || "网络连接持续不可用";
  cancelActiveHttpSync();
  enterReconnecting(new Error(detail));
}

function enterDegraded(error) {
  const now = Date.now();
  if (state.connectionPhase !== "degraded") {
    state.connectionFailureStartedAt = now;
    setConnectionPhase("degraded");
  }
  state.connectionFailureDetail = error instanceof Error
    ? error.message : String(error || "网络连接不可用");
  const elapsed = now - (state.connectionFailureStartedAt ?? now);
  if (elapsed >= CONNECTION_DEGRADED_GRACE_MS) {
    promoteDegradedConnection();
    return;
  }
  if (state.degradedTimer === null) {
    state.degradedTimer = setTimeout(
      promoteDegradedConnection,
      CONNECTION_DEGRADED_GRACE_MS - elapsed,
    );
  }
  schedulePollingRetry(HTTP_SYNC_ACTIVE_MS);
}

function handlePollingFailure(error, { timedOut = false } = {}) {
  const phase = state.connectionPhase;
  cancelActiveHttpSync();
  if (typeof cancelBackgroundWorkspaceSync === "function") {
    cancelBackgroundWorkspaceSync();
  }
  if (state.pageClosing || (state.authRequired && !state.authenticated) || phase === "failed") return;
  if (!timedOut && state.connectionHadSuccess
      && (phase === "connected" || phase === "degraded")) {
    enterDegraded(error);
    return;
  }
  enterReconnecting(error);
}

function failHttpSync(title, error) {
  const detail = error instanceof Error ? error.message : String(error || "未知错误");
  console.error(title, error);
  cancelActiveHttpSync();
  if (typeof cancelBackgroundWorkspaceSync === "function") {
    cancelBackgroundWorkspaceSync();
  }
  clearDegradedTimer();
  state.connectionFailureDetail = detail;
  state.stabilizingSince = null;
  state.stabilizingSuccesses = 0;
  setConnectionPhase("failed");
  showConnectionOverlay(title, `${detail}。请点击“立即重试”。`);
}

function showConnectionOverlay(title, message) {
  if (elements.connectionOverlayTitle.textContent !== title) elements.connectionOverlayTitle.textContent = title;
  if (elements.connectionOverlayMessage.textContent !== message) elements.connectionOverlayMessage.textContent = message;
  if (state.connectionOverlayMode === "connection") return;
  elements.connectionRetry.classList.remove("hidden");
  elements.connectionOverlay.classList.remove("hidden");
  elements.app.inert = true;
  state.connectionOverlayMode = "connection";
  if (elements.app.contains(document.activeElement)) document.activeElement.blur();
}

function hideConnectionOverlay() {
  if (state.connectionOverlayMode === "hidden") return;
  elements.connectionOverlay.classList.add("hidden");
  elements.connectionRetry.classList.remove("hidden");
  elements.app.inert = false;
  state.connectionOverlayMode = "hidden";
}

function renderConnectionOverlayForPhase() {
  if (state.connectionPhase === "connected" || state.connectionPhase === "degraded") {
    hideConnectionOverlay();
  } else if (state.connectionPhase === "initial") {
    showConnectionOverlay("正在连接", "正在同步当前界面，请稍候。");
  } else if (state.connectionPhase === "reconnecting") {
    showConnectionOverlay("正在重新连接", state.connectionFailureDetail || "正在恢复与服务的连接。");
  } else if (state.connectionPhase === "stabilizing") {
    showConnectionOverlay("连接正在恢复", "正在确认连接已稳定，请稍候。");
  }
}

function markConnectionStable() {
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = null;
  clearDegradedTimer();
  state.connectionHadSuccess = true;
  state.connectionFailureDetail = "";
  state.reconnectAttempt = 0;
  state.stabilizingSince = null;
  state.stabilizingSuccesses = 0;
  setConnectionPhase("connected");
}

function notePollingSuccess(authoritativeSelection, recoveryComplete = authoritativeSelection) {
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = null;
  if (state.connectionPhase === "connected") return;
  if (state.connectionPhase === "degraded") {
    markConnectionStable();
    return;
  }
  if (state.connectionPhase === "initial") {
    if (authoritativeSelection) markConnectionStable();
    return;
  }
  if (state.connectionPhase === "reconnecting") {
    if (!state.connectionHadSuccess) {
      if (authoritativeSelection) markConnectionStable();
      else setConnectionPhase("initial");
      return;
    }
    state.stabilizingSince = Date.now();
    state.stabilizingSuccesses = 1;
    setConnectionPhase("stabilizing");
    return;
  }
  if (state.connectionPhase !== "stabilizing") return;
  state.stabilizingSuccesses += 1;
  const stableFor = Date.now() - (state.stabilizingSince ?? Date.now());
  if (recoveryComplete
      && !bulkEventRecoveryActive()
      && state.stabilizingSuccesses >= CONNECTION_STABILIZE_SUCCESSES
      && stableFor >= CONNECTION_STABILIZE_MS) {
    markConnectionStable();
  }
}

function scheduleHttpSync(delay) {
  clearTimeout(state.syncTimer);
  if (!connectionCanPoll()) return;
  state.syncTimer = setTimeout(requestHttpSync, delay);
}

function httpSyncProgressSignature() {
  const terminalKey = state.view.kind === "terminal" && state.selectedAgent && state.view.sessionId
    ? `${state.selectedAgent}:${state.view.sessionId}` : null;
  return JSON.stringify({
    snapshotRevision: state.snapshotInitialized ? state.snapshot.revision : null,
    agents: [...state.stores]
      .map(([id, store]) => [id, store.eventCount ?? store.events.length, store.mutationRevision])
      .sort((left, right) => String(left[0]).localeCompare(String(right[0]))),
    selectedAgent: state.selectedAgent,
    terminalSession: state.view.kind === "terminal" ? state.view.sessionId : null,
    terminalRevision: terminalKey ? state.terminalRevisions.get(terminalKey) ?? null : null,
  });
}

async function requestHttpSync() {
  if (state.syncInFlight || state.pageClosing) return;
  const generation = state.syncGeneration;
  const terminalKey = state.view.kind === "terminal" && state.selectedAgent && state.view.sessionId
    ? `${state.selectedAgent}:${state.view.sessionId}` : null;
  const progressBefore = httpSyncProgressSignature();
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
          event_count: store.eventCount ?? store.events.length,
          mutation_revision: store.mutationRevision,
          cursor_event_hash: ["initial", "reconnecting"].includes(state.connectionPhase)
              || (id === state.selectedAgent && store.needsTurnHistory)
            ? store.lastEventHash ?? null : null,
        })),
        cache_metadata_only: !state.edbCacheInitialized,
        selected_agent: state.selectedAgent,
        terminal_session: state.view.kind === "terminal" ? state.view.sessionId : null,
        terminal_revision: terminalKey ? state.terminalRevisions.get(terminalKey) ?? null : null,
      }),
    }, state.workspaceId);
    if (generation !== state.syncGeneration || state.pageClosing) return;
    if (message.cache_metadata_only) {
      await hydrateEdbCache(message.snapshot);
      if (generation !== state.syncGeneration || state.pageClosing) return;
      state.syncInFlight = false;
      state.syncController = null;
      captureActiveWorkspace();
      scheduleHttpSync(0);
      return;
    }
    state.syncInFlight = false;
    state.syncController = null;
    try {
      applySyncState(message);
    } catch (error) {
      return failHttpSync("无法更新界面", error);
    }
    state.activeCatchUpPending = Boolean(message.more_events)
      || (message.selected_agent ?? null) !== state.selectedAgent;
    if (!state.activeCatchUpPending) scheduleBackgroundWorkspaceSync(0);
    const madeProgress = progressBefore !== httpSyncProgressSignature();
    const delay = state.connectionPhase === "stabilizing"
      || message.more_events || state.apiActivity.active || state.view.kind === "terminal"
      ? HTTP_SYNC_ACTIVE_MS : HTTP_SYNC_IDLE_MS;
    scheduleHttpSync(message.more_events && madeProgress ? 0 : delay);
  } catch (error) {
    state.activeCatchUpPending = true;
    if (generation !== state.syncGeneration || state.pageClosing) return;
    state.syncInFlight = false;
    state.syncController = null;
    if (error.status === 401) return showLogin("登录已失效，请重新登录");
    if (error.status && ![502, 503, 504].includes(error.status)) {
      return failHttpSync("界面同步失败", error);
    }
    const timedOut = error.name === "AbortError";
    const failure = timedOut ? new Error("界面同步超时") : error;
    handlePollingFailure(failure, { timedOut });
  } finally {
    clearTimeout(timeout);
  }
}

function requestHttpSyncNow() {
  clearTimeout(state.syncTimer);
  state.syncTimer = null;
  if (!state.syncInFlight && connectionCanPoll()) void requestHttpSync();
}

function applySyncState(payload) {
  const phaseBefore = state.connectionPhase;
  const startingRecoveryCycle = phaseBefore === "initial" || phaseBefore === "reconnecting";
  const hadBulkRecovery = bulkEventRecoveryActive();
  const previousSnapshot = state.snapshot;
  if (payload.snapshot) {
    state.snapshot = payload.snapshot;
    state.snapshotInitialized = true;
  }
  if (!state.snapshotInitialized) throw new Error("同步响应未提供初始状态");
  const presentationChanged = snapshotPresentationSignature(previousSnapshot)
    !== snapshotPresentationSignature(state.snapshot);
  const selectionChanged = reconcileAgents();
  const updates = new Map((payload.event_updates || []).map((update) => [update.agent_id, update]));
  const selectedMeta = state.snapshot.agents.find((meta) => meta.id === state.selectedAgent);
  const selectedUpdate = updates.get(state.selectedAgent);
  prepareSelectedEventRecovery(
    selectedMeta,
    selectedUpdate,
    startingRecoveryCycle || selectionChanged || Boolean(selectedUpdate?.reset),
  );
  const recoveryTransitionedToIncremental = hadBulkRecovery && !bulkEventRecoveryActive();
  const eventChanges = state.snapshot.agents.map((meta) => syncAgentEvents(meta, updates.get(meta.id)));
  const selectedEventsChanged = eventChanges.some((change) =>
    change.changed && change.agentId === state.selectedAgent);
  const selectedWorkerChanged = eventChanges.some((change) =>
    change.changed && isWorkerForSelectedAgent(change.agentId));
  const agentSummaryChanged = eventChanges.some((change) => change.summaryChanged);
  const agentLoadChanged = eventChanges.some((change) => change.loadChanged);
  const responseMatchesSelection = (payload.selected_agent ?? null) === state.selectedAgent;
  const apiActivityChanged = responseMatchesSelection
    ? syncApiActivity(payload.api_activity || {}) : false;
  const terminalChanged = responseMatchesSelection
    ? syncTerminals(payload.terminals || []) : false;
  if (responseMatchesSelection && payload.terminal_frame_updated) {
    syncTerminalFrame(payload.terminal_session, payload.terminal_frame ?? null);
  }
  const store = currentStore();
  const recoveryReady = responseMatchesSelection && selectedEventRecoveryReady(
    state.eventRecovery,
    state.selectedAgent,
    store?.mutationRevision,
    store?.events.length,
  );
  let forceRecoveredReplay = false;
  if (recoveryReady) {
    store.projectedOrder = 0;
    store.needsReplay = true;
    state.eventRecovery = null;
    forceRecoveredReplay = true;
  }
  const bulkRecoveryPending = bulkEventRecoveryActive();
  notePollingSuccess(
    responseMatchesSelection,
    responseMatchesSelection && !payload.more_events,
  );
  const connectionChanged = phaseBefore !== state.connectionPhase;
  requestRender({
    full: !bulkRecoveryPending && (forceRecoveredReplay || startingRecoveryCycle || selectionChanged),
    connection: connectionChanged,
    agents: presentationChanged || agentSummaryChanged || agentLoadChanged,
    tabs: presentationChanged || terminalChanged,
    currentEvents: !bulkRecoveryPending && !forceRecoveredReplay && selectedEventsChanged,
    workerEvents: !bulkRecoveryPending && !forceRecoveredReplay && selectedWorkerChanged,
    apiActivity: !bulkRecoveryPending && !forceRecoveredReplay && apiActivityChanged,
    status: !bulkRecoveryPending && !forceRecoveredReplay && apiActivityChanged,
  });
  if (bulkRecoveryPending) suppressBulkEventRecoveryRender();
  if (forceRecoveredReplay || recoveryTransitionedToIncremental) flushPendingRender();
  else if (!inputHasPriority()) flushPendingRender();
  else if (state.view.kind === "terminal") renderTerminal();
  renderConnectionOverlayForPhase();
  for (const [agentId, sync] of state.draftSync) {
    if (sync.sent !== sync.desired) void runDraftSync(agentId, sync);
  }
  if (!responseMatchesSelection) requestHttpSyncNow();
}

function eventRecoveryBacklog(authoritativeEventCount, localEventCount) {
  return Math.max(0, Math.max(0, Number(authoritativeEventCount) || 0)
    - Math.max(0, Number(localEventCount) || 0));
}

function shouldUseBulkEventRecovery(authoritativeEventCount, localEventCount) {
  return eventRecoveryBacklog(authoritativeEventCount, localEventCount) > EVENT_RECOVERY_THRESHOLD;
}

function createEventRecovery(agentId, mutationRevision, authoritativeEventCount, localEventCount) {
  if (!agentId || !shouldUseBulkEventRecovery(authoritativeEventCount, localEventCount)) return null;
  return {
    agentId,
    mutationRevision: Number(mutationRevision) || 0,
    startEventCount: Math.max(0, Number(localEventCount) || 0),
    targetEventCount: Math.max(0, Number(authoritativeEventCount) || 0),
  };
}

function eventRecoveryProgress(recovery, localEventCount) {
  if (!recovery) return 0;
  const start = Math.max(0, Number(recovery.startEventCount) || 0);
  const target = Math.max(start, Number(recovery.targetEventCount) || 0);
  if (target === start) return 1;
  const current = Math.max(0, Number(localEventCount) || 0);
  return Math.min(1, Math.max(0, current - start) / (target - start));
}

function eventRecoveryMatches(recovery, agentId, mutationRevision) {
  return Boolean(recovery) && recovery.agentId === agentId
    && recovery.mutationRevision === (Number(mutationRevision) || 0);
}

function selectedEventRecoveryReady(recovery, agentId, mutationRevision, localEventCount) {
  return eventRecoveryMatches(recovery, agentId, mutationRevision)
    && Math.max(0, Number(localEventCount) || 0) >= recovery.targetEventCount;
}

function prepareSelectedEventRecovery(meta, update, startCycle) {
  if (!meta || meta.id !== state.selectedAgent) {
    state.eventRecovery = null;
    return false;
  }
  const mutationRevision = Number(update?.mutation_revision ?? meta.mutation_revision) || 0;
  if (state.eventRecovery
      && !eventRecoveryMatches(state.eventRecovery, meta.id, mutationRevision)) {
    state.eventRecovery = null;
  }
  if (state.eventRecovery) return true;
  if (!startCycle) return false;
  const store = state.stores.get(meta.id);
  const localEventCount = update?.reset || store?.mutationRevision !== mutationRevision
    ? 0 : store?.events.length || 0;
  const authoritativeEventCount = update?.event_count ?? meta.event_count;
  state.eventRecovery = createEventRecovery(
    meta.id, mutationRevision, authoritativeEventCount, localEventCount,
  );
  if (state.eventRecovery) suppressBulkEventRecoveryRender();
  return Boolean(state.eventRecovery);
}

function bulkEventRecoveryActive() {
  return Boolean(state.eventRecovery) && state.eventRecovery.agentId === state.selectedAgent;
}

function suppressBulkEventRecoveryRender() {
  state.pendingRender.full = false;
  state.pendingRender.currentEvents = false;
  state.pendingRender.workerEvents = false;
  state.pendingRender.apiActivity = false;
  state.pendingRender.status = false;
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
    chatbotDefaultStaticPrompt: snapshot.chatbot_default_static_prompt || "",
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
      discardStoredAgentEdb(state.snapshot, id, state.stores.get(id));
      state.stores.delete(id);
      state.drafts.delete(id);
      clearDraftBatch(state.draftSync.get(id));
      state.promptDrafts.delete(id);
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
  if (isChatbotAgent() && !["chat", "system-prompt"].includes(state.view.kind)) {
    state.view = { kind: "chat", sessionId: null };
  } else if (!isChatbotAgent() && state.view.kind === "system-prompt") {
    state.view = { kind: "chat", sessionId: null };
  }
  return changed || previousAgent !== state.selectedAgent
    || previousView !== `${state.view.kind}:${state.view.sessionId || ""}`;
}

function syncAgentEvents(meta, payload) {
  let store = state.stores.get(meta.id);
  const loadingBefore = loadProgressSignature(store);
  let changed = false;
  if (!store) {
    store = createAgentStore(meta, null, state.snapshot);
    state.stores.set(meta.id, store);
    const initialDraft = String(meta.input_draft || "");
    state.drafts.set(meta.id, initialDraft);
    if (state.selectedAgent === meta.id && elements.input.value !== initialDraft) {
      elements.input.value = initialDraft;
      autoSizeInput(true);
    }
    changed = true;
  }
  observeInputDraft(meta, store);
  const previousEventCount = store.eventCount;
  const previousMutationRevision = store.mutationRevision;
  settleAgentLoadProgress(store);
  prepareAgentLoadProgress(store, meta, payload, previousEventCount, previousMutationRevision);
  if (!payload && store.eventCount === meta.event_count
      && store.mutationRevision === meta.mutation_revision) {
    observePromptSubmission(meta, store);
    return {
      agentId: meta.id, changed, summaryChanged: false,
      loadChanged: loadingBefore !== loadProgressSignature(store),
    };
  }
  // A large initial replay is transferred in bounded batches. Agents without a
  // batch in this response remain pending and are requested again immediately.
  if (!payload) {
    return {
      agentId: meta.id, changed, summaryChanged: false,
      loadChanged: loadingBefore !== loadProgressSignature(store),
    };
  }
  const previousSummary = JSON.stringify(store.summary);
  const events = Array.isArray(payload.events) ? payload.events : [];
  if (payload.reset) {
    store.events = events;
    store.eventCount = events.length;
    store.summary = projectAgentSummary(events);
    store.projectedOrder = 0;
    store.needsReplay = true;
    state.workerActivityIndexes.delete(meta.id);
  } else {
    store.events.push(...events);
    store.eventCount = previousEventCount + events.length;
    updateAgentSummary(store.summary, events);
  }
  store.mutationRevision = payload.mutation_revision;
  store.lastEventHash = payload.cursor_event_hash ?? null;
  settleAgentLoadProgress(store);
  if (payload.reset || events.length > 0) {
    persistAgentEdb(meta, store, Boolean(payload.reset), {
      startOrder: payload.reset ? 0 : previousEventCount,
      eventCount: Number(payload.event_count ?? store.eventCount),
      expectedEventCount: previousEventCount,
      expectedMutationRevision: previousMutationRevision,
      events,
    });
  }
  if (payload.turn_history_updated) {
    store.turnHistory = payload.turn_history ?? null;
    store.needsTurnHistory = false;
  }
  observePromptSubmission(meta, store);
  return {
    agentId: meta.id,
    changed: true,
    summaryChanged: previousSummary !== JSON.stringify(store.summary),
    loadChanged: loadingBefore !== loadProgressSignature(store),
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
    autoSizeInput(true);
  }
  return true;
}

function projectAgentSummary(events) {
  const summary = { turnState: null };
  updateAgentSummary(summary, events);
  return summary;
}

function updateAgentSummary(summary, events) {
  for (const event of events) {
    const [kind, value] = eventParts(event);
    if (kind === "AgentTurn") summary.turnState = value.state;
  }
}

function sidebarAgentActive(summary) {
  return normalize(summary?.turnState) === "started";
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
              && assistantContentHasRenderableContent(assistant.content)) {
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
          output: "", updates: [], result: null, revision: value.id,
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
          tool.updates.push(value.content);
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
      case "SystemStaticPromptChange":
        addNotice(
          projection,
          changes,
          normalize(value.mode) === "custom"
            ? `系统提示词已更新\n${String(value.content ?? "")}`
            : "系统提示词已恢复默认",
          value,
        );
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
  renderAgents();
  renderTabs();
  renderAgentControls();
  renderSystemPromptEditor();
  renderTranscript(true, 0);
  refreshRunningToolNodes();
  renderObjective();
  renderWorkMap();
  if (promptConfirmed) finishPendingPromptSubmission(state.selectedAgent);
  renderComposer();
  renderStatus();
  transcriptBottomFollower.layoutChanged();
  synchronizeWindowTitle();
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
  if (request.agents) renderAgents();
  if (request.tabs) renderTabs();
  if (request.tabs || request.currentEvents) renderSystemPromptEditor();
  if (changes.transcript) {
    renderTranscript(Boolean(changes.fullReplay), changes.transcriptFrom ?? 0);
    transcriptChanged = true;
  } else if (request.workerEvents && state.view.kind === "chat") {
    refreshWorkerActivityCards();
    transcriptChanged = true;
  }
  if (transcriptChanged) refreshRunningToolNodes();
  if (changes.workmap) {
    renderObjective();
    if (state.view.kind === "workmap") renderWorkMap();
  }
  if (promptConfirmed) finishPendingPromptSubmission(state.selectedAgent);
  if (changes.turn || promptConfirmed) renderComposer();
  if (request.status || changes.status) renderStatus();
  if (transcriptChanged || promptConfirmed) {
    transcriptVirtualizer?.layoutChanged();
    transcriptBottomFollower.layoutChanged();
  }
  synchronizeWindowTitle();
  if (state.view.kind === "terminal") void renderTerminal();
}

function advanceCurrentProjection() {
  if (bulkEventRecoveryActive()) return emptyProjectionChanges();
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
  transcriptVirtualizer?.follow();
  transcriptBottomFollower.layoutChanged();
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


function workspaceUiState(workspaceId) {
  return workspaceId === state.workspaceId ? state : gatewayWorkspaceState(workspaceId);
}

function workspaceMetadataReady(workspaceId) {
  return Boolean(workspaceUiState(workspaceId)?.snapshot?.environment);
}

function agentLoadingState(workspaceId, agentId) {
  const bucket = workspaceUiState(workspaceId);
  const meta = bucket.snapshot?.agents?.find((agent) => agent.id === agentId);
  if (!meta) return { loading: false, percent: null };
  const store = bucket.stores.get(agentId);
  if (!bucket.edbCacheInitialized || !store) return { loading: true, percent: null };
  if (!store.loadProgress) return { loading: false, percent: null };
  return {
    loading: true,
    percent: Math.floor(eventRecoveryProgress(store.loadProgress, store.eventCount) * 100),
  };
}

function sessionSelectionAllowed(workspaceId, agentId) {
  return !agentLoadingState(workspaceId, agentId).loading;
}

function selectedAgentLoadingState() {
  if (!state.workspaceId || !state.selectedAgent) return { loading: false, percent: null };
  return agentLoadingState(state.workspaceId, state.selectedAgent);
}

function renderSessionSyncOverlay() {
  const loadingState = selectedAgentLoadingState();
  const loading = loadingState.loading;
  const wasVisible = !elements.sessionSyncOverlay.classList.contains("hidden");
  elements.sessionSyncOverlay.classList.toggle("hidden", !loading);
  elements.sessionSyncOverlay.setAttribute("aria-hidden", String(!loading));
  elements.sessionSyncProgress.textContent = loadingState.percent == null
    ? "正在准备会话，请稍候。" : `已完成 ${loadingState.percent}%`;
  elements.workspace.querySelectorAll(":scope > .view, :scope > .statusbar").forEach((node) => {
    node.inert = loading;
  });
  if (loading && elements.workspace.contains(document.activeElement)
      && document.activeElement !== elements.mobileSidebarToggle) {
    elements.sessionSyncOverlay.focus({ preventScroll: true });
  } else if (!loading && wasVisible && document.activeElement === elements.sessionSyncOverlay
      && state.view.kind === "chat") {
    elements.input.focus({ preventScroll: true });
  }
}

function renderAgents() {
  const workspaces = state.gateway.workspaces || [];
  const chat = workspaces.find((workspace) => workspace.builtin);
  elements.addAgent.disabled = !workspaceMetadataReady(chat?.id || "chat");
  const external = workspaces.filter((workspace) => !workspace.builtin);
  const externalIds = new Set(external.map((workspace) => workspace.id));
  pruneWorkspaceDisclosure(externalIds);
  if (!external.length) {
    if (!elements.workspaceList.querySelector(":scope > .empty-state")) {
      elements.workspaceList.innerHTML = `<div class="empty-state">暂无工作区</div>`;
    }
  } else {
    if (elements.workspaceList.querySelector(":scope > .empty-state")) replaceElementChildren(elements.workspaceList);
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
  }
  renderWorkspaceAgentRows(elements.agents, chat?.id || "chat");
  if (state.workspaceMenu) {
    const trigger = [...elements.workspaceList.querySelectorAll("[data-workspace-menu]")]
      .find((button) => button.dataset.workspaceMenu === state.workspaceMenu.workspaceId);
    if (trigger) {
      state.workspaceMenu.trigger = trigger;
      trigger.setAttribute("aria-expanded", "true");
    } else closeWorkspaceMenu();
  }
  renderSessionSyncOverlay();
}

function createWorkspaceGroup(workspace) {
  const template = document.createElement("template");
  template.innerHTML = `<section class="workspace-group" data-workspace-group="${escapeAttr(workspace.id)}">
    <header class="workspace-row">
      <button class="workspace-select" type="button" data-workspace-select="${escapeAttr(workspace.id)}" aria-expanded="${workspaceExpanded(workspace.id)}">
        <svg class="workspace-disclosure-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg>
        <svg class="workspace-folder-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M3.5 7.5h6l1.8 2h9.2l-1.6 8.5H4.4L3.5 7.5Z"/><path d="M4 7.5V5.5h5.2l1.8 2"/></svg>
        <span class="workspace-name"></span>
      </button>
      <button class="workspace-add-agent" type="button" data-workspace-add="${escapeAttr(workspace.id)}">＋</button>
      <button class="workspace-actions" type="button" data-workspace-menu="${escapeAttr(workspace.id)}" aria-haspopup="menu" aria-expanded="false">···</button>
    </header>
    <div class="workspace-agent-list" data-workspace-agents="${escapeAttr(workspace.id)}" hidden></div>
  </section>`;
  const group = template.content.firstElementChild;
  const panelId = `workspace-agents-${++workspacePanelSequence}`;
  group.querySelector("[data-workspace-select]").setAttribute("aria-controls", panelId);
  group.querySelector("[data-workspace-agents]").id = panelId;
  group.querySelector("[data-workspace-select]").addEventListener("click", () => {
    setWorkspaceExpanded(workspace.id, !workspaceExpanded(workspace.id));
    updateWorkspaceGroup(group, workspace);
  });
  group.querySelector("[data-workspace-add]").addEventListener("click", () => {
    if (!workspaceMetadataReady(workspace.id)) return;
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
  const expanded = workspaceExpanded(workspace.id);
  const metadataReady = workspaceMetadataReady(workspace.id);
  group.classList.toggle("expanded", expanded);
  const select = group.querySelector("[data-workspace-select]");
  const add = group.querySelector("[data-workspace-add]");
  const actions = group.querySelector("[data-workspace-menu]");
  const agents = group.querySelector("[data-workspace-agents]");
  const title = select.querySelector(".workspace-name");
  if (title.textContent !== workspace.name) title.textContent = workspace.name;
  select.setAttribute("aria-expanded", String(expanded));
  select.setAttribute("aria-label", `${expanded ? "折叠" : "展开"} ${workspace.name}`);
  select.title = `${expanded ? "折叠" : "展开"} ${workspace.name}`;
  agents.hidden = !expanded;
  add.disabled = !metadataReady;
  add.setAttribute("aria-label", `在 ${workspace.name}中新建会话`);
  add.title = metadataReady ? `在 ${workspace.name}中新建会话` : "正在加载工作区";
  actions.setAttribute("aria-label", `打开 ${workspace.name} 的工作区选项`);
  actions.title = `打开 ${workspace.name} 的工作区选项`;
}

function renderWorkspaceAgentRows(container, workspaceId) {
  if (!container) return;
  const bucket = workspaceUiState(workspaceId);
  const agents = bucket.snapshot?.agents || [];
  if (!agents.length) {
    const label = !bucket.snapshot?.environment ? "正在加载会话" : "暂无会话";
    let empty = container.querySelector(":scope > .empty-state");
    if (!empty) {
      container.innerHTML = `<div class="empty-state"></div>`;
      empty = container.querySelector(":scope > .empty-state");
    }
    if (empty.textContent !== label) empty.textContent = label;
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
  const loadingState = agentLoadingState(workspaceId, agent.id);
  const summary = bucket.stores.get(agent.id)?.summary;
  const active = !loadingState.loading && sidebarAgentActive(summary);
  const label = agent.title || agent.id;
  const loadingLabel = loadingState.percent == null ? "正在加载" : `正在加载 ${loadingState.percent}%`;
  row.classList.toggle("session-loading", loadingState.loading);
  row.classList.toggle("active", workspaceId === state.workspaceId && agent.id === state.selectedAgent);
  row.setAttribute("aria-busy", String(loadingState.loading));
  const item = row.querySelector(".agent-item");
  item.title = loadingState.loading ? loadingLabel : "";
  item.disabled = loadingState.loading;
  const dot = row.querySelector(".agent-dot");
  dot.classList.toggle("loading", loadingState.loading);
  dot.classList.toggle("active", active);
  const title = row.querySelector(".agent-label");
  if (title.textContent !== label) title.textContent = label;
  const progress = row.querySelector(".agent-load-progress");
  const progressText = loadingState.percent == null ? "" : `${loadingState.percent}%`;
  if (progress.textContent !== progressText) progress.textContent = progressText;
  progress.classList.toggle("hidden", !loadingState.loading || loadingState.percent == null);
  const deleteButton = row.querySelector(".agent-delete");
  deleteButton.setAttribute("aria-label", `删除 ${label}`);
  deleteButton.title = `删除 ${label}`;
  deleteButton.disabled = loadingState.loading;
}

function createAgentRow(agent, workspaceId = state.workspaceId) {
  const template = document.createElement("template");
  template.innerHTML = `<div class="agent-row" data-agent-row="${escapeAttr(agent.id)}" data-workspace-id="${escapeAttr(workspaceId)}">
    <button class="agent-item" type="button" data-agent="${escapeAttr(agent.id)}">
      <span class="agent-dot" aria-hidden="true"></span>
      <span class="agent-label"></span>
      <span class="agent-load-progress hidden" aria-hidden="true"></span>
    </button>
    <button class="agent-delete" type="button" data-agent-delete="${escapeAttr(agent.id)}" title="删除会话" aria-label="删除会话">
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 7h16M9 7V4h6v3M7 7l1 13h8l1-13M10 11v5M14 11v5"/></svg>
    </button>
  </div>`;
  const row = template.content.firstElementChild;
  row.querySelector(".agent-item").addEventListener("click", () => selectWorkspaceAgent(workspaceId, agent.id));
  row.querySelector(".agent-delete").addEventListener("click", (event) => {
    event.stopPropagation();
    selectWorkspaceAgent(workspaceId, agent.id);
    requestAnimationFrame(() => void openDeleteAgent(agent.id));
  });
  return row;
}

function selectWorkspaceAgent(workspaceId, agentId) {
  if (!sessionSelectionAllowed(workspaceId, agentId)) return;
  closeMobileSidebar();
  if (state.workspaceId !== workspaceId) activateWorkspace(workspaceId, agentId);
  else selectAgent(agentId);
}

function finishAgentSelection(id) {
  closeContextDrawer();
  closeMobileSidebar();
  closeUserMessageMenu();
  closeAgentMenu();
  closeWorkspaceMenu();
  deactivateSessionTerminalView();
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
  const meta = state.snapshot.agents.find((agent) => agent.id === id);
  prepareSelectedEventRecovery(meta, null, true);
  renderAll();
  persistGatewaySelection(state.workspaceId, id);
  requestHttpSyncNow();
}

function selectAgent(id) {
  if (!sessionSelectionAllowed(state.workspaceId, id)) return;
  finishAgentSelection(id);
}

function deactivateSessionTerminalView() {
  sessionTerminalController?.deactivate();
  sessionTerminalIdentityKey = null;
}

function syncSessionTerminalView() {
  if (state.view.kind !== "session-terminal" || !state.workspaceId || !state.selectedAgent) {
    if (sessionTerminalIdentityKey !== null) deactivateSessionTerminalView();
    return;
  }
  const key = `${state.workspaceId}:${state.selectedAgent}`;
  if (sessionTerminalIdentityKey === key) return;
  getSessionTerminalController().attach({
    key,
    workspaceId: state.workspaceId,
    agentId: state.selectedAgent,
  });
  sessionTerminalIdentityKey = key;
}

function renderTabs() {
  const chatbot = isChatbotAgent();
  const allowed = chatbot ? new Set(["chat", "system-prompt"]) : null;
  if ((chatbot && !allowed.has(state.view.kind)) || (!chatbot && state.view.kind === "system-prompt")) {
    state.view = { kind: "chat", sessionId: null };
  }
  elements.tabs.querySelectorAll("[data-work-only]").forEach((node) => {
    node.classList.toggle("hidden", chatbot);
  });
  elements.tabs.querySelectorAll("[data-chatbot-only]").forEach((node) => {
    node.classList.toggle("hidden", !chatbot);
  });
  elements.tabs.querySelectorAll("button[data-view]").forEach((button) => {
    button.classList.toggle("active", !button.classList.contains("hidden")
      && state.view.kind === button.dataset.view);
  });
  elements.terminalTabs.innerHTML = chatbot ? "" : state.terminals.map((session) =>
    `<button data-terminal="${escapeAttr(session.session_id)}" class="${state.view.kind === "terminal" && state.view.sessionId === session.session_id ? "active" : ""}">Terminal · ${escapeHtml(session.session_id)}</button>`
  ).join("");
  elements.terminalTabs.querySelectorAll("[data-terminal]").forEach((button) => button.addEventListener("click", () => {
    showView({ kind: "terminal", sessionId: button.dataset.terminal });
  }));
  elements.chatView.classList.toggle("active", state.view.kind === "chat");
  elements.systemPromptView.classList.toggle("active", state.view.kind === "system-prompt");
  elements.workmapView.classList.toggle("active", state.view.kind === "workmap");
  elements.sessionTerminalView.classList.toggle("active", state.view.kind === "session-terminal");
  elements.remoteControlView.classList.toggle("active", state.view.kind === "remote-control");
  elements.filesView.classList.toggle("active", state.view.kind === "files");
  elements.terminalView.classList.toggle("active", state.view.kind === "terminal");
  syncSessionTerminalView();
  syncFileManagerView();
  syncRemoteControlView();
}

function showView(view) {
  flushPendingRender();
  const chatbot = isChatbotAgent();
  const nextView = (chatbot && !["chat", "system-prompt"].includes(view.kind))
    || (!chatbot && view.kind === "system-prompt")
    ? { kind: "chat", sessionId: null } : view;
  if (nextView.kind === "terminal"
      && (state.view.kind !== "terminal" || state.view.sessionId !== nextView.sessionId)) {
    state.terminalFollowBottom = true;
  }
  state.view = nextView;
  renderTabs();
  renderSystemPromptEditor();
  renderObjective();
  if (state.view.kind === "workmap") renderWorkMap();
  renderComposer();
  renderStatus();
  updateScrollToBottomButton();
  if (state.view.kind === "terminal") void renderTerminal();
}

function latestSystemPromptState(agentId = state.selectedAgent) {
  const defaultContent = String(state.snapshot.chatbot_default_static_prompt ?? "");
  const events = state.stores.get(agentId)?.events || [];
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const [kind, value] = eventParts(events[index]);
    if (kind !== "SystemStaticPromptChange") continue;
    const custom = normalize(value.mode) === "custom";
    return {
      mode: custom ? "Custom" : "Default",
      content: custom ? String(value.content ?? "") : defaultContent,
      eventId: Number(value.id),
    };
  }
  return { mode: "Default", content: defaultContent, eventId: null };
}

function systemPromptChangeMatches(value, pending) {
  const mode = normalize(value.mode) === "custom" ? "Custom" : "Default";
  return mode === pending.mode
    && (mode === "Default" || String(value.content ?? "") === pending.content);
}

function systemPromptEditorState(agentId = state.selectedAgent) {
  const meta = state.snapshot.agents.find((agent) => agent.id === agentId);
  if (!agentId || !isChatbotAgent(meta)) return null;
  const saved = latestSystemPromptState(agentId);
  let draft = state.promptDrafts.get(agentId);
  if (!draft) {
    draft = {
      content: saved.content, dirty: false, pending: null,
      savedMode: saved.mode, savedContent: saved.content, savedEventId: saved.eventId,
    };
    state.promptDrafts.set(agentId, draft);
  }
  if (draft.pending) {
    const events = state.stores.get(agentId)?.events || [];
    const confirmation = events.find((event) => {
      const [kind, value] = eventParts(event);
      return kind === "SystemStaticPromptChange"
        && Number(value.id) > draft.pending.afterEventId
        && systemPromptChangeMatches(value, draft.pending);
    });
    if (confirmation) {
      const confirmed = draft.pending;
      draft.pending = null;
      draft.dirty = false;
      draft.content = saved.content;
      toast(confirmed.mode === "Custom" ? "系统提示词已更新" : "系统提示词已恢复默认");
    }
  }
  const savedChanged = draft.savedMode !== saved.mode
    || draft.savedContent !== saved.content || draft.savedEventId !== saved.eventId;
  if (savedChanged && !draft.dirty && !draft.pending) draft.content = saved.content;
  draft.savedMode = saved.mode;
  draft.savedContent = saved.content;
  draft.savedEventId = saved.eventId;
  return { draft, saved };
}

function systemPromptContentBytes(content) {
  return new TextEncoder().encode(content).length;
}

function renderSystemPromptEditor() {
  const context = systemPromptEditorState();
  if (!context) return;
  const { draft, saved } = context;
  const pending = draft.pending;
  const valid = draft.content.trim().length > 0
    && systemPromptContentBytes(draft.content) <= SYSTEM_STATIC_PROMPT_MAX_BYTES;
  if (elements.systemPromptInput.value !== draft.content) {
    elements.systemPromptInput.value = draft.content;
  }
  elements.systemPromptMode.textContent = saved.mode === "Custom" ? "自定义" : "内置默认";
  elements.systemPromptMode.dataset.mode = normalize(saved.mode);
  elements.systemPromptInput.disabled = Boolean(pending);
  elements.systemPromptInput.setAttribute("aria-busy", String(Boolean(pending)));
  elements.systemPromptStatus.dataset.state = pending ? "pending" : draft.dirty ? "dirty" : "synced";
  elements.systemPromptStatus.textContent = pending
    ? pending.status === "unknown"
      ? "暂时无法确认是否应用成功，请稍候…"
      : "正在应用更改…"
    : draft.content.trim().length === 0
      ? "内容不能为空"
      : systemPromptContentBytes(draft.content) > SYSTEM_STATIC_PROMPT_MAX_BYTES
        ? "内容过长，请适当精简"
        : draft.dirty ? "有尚未应用的更改" : "已应用";
  elements.systemPromptApply.disabled = Boolean(pending) || !draft.dirty || !valid;
  elements.systemPromptRestore.disabled = Boolean(pending)
    || (saved.mode === "Default" && !draft.dirty);
}

function lastPhysicalEventId(agentId) {
  return (state.stores.get(agentId)?.events || []).reduce((highest, event) => {
    const [, value] = eventParts(event);
    const id = Number(value.id);
    return Number.isFinite(id) ? Math.max(highest, id) : highest;
  }, -1);
}

async function submitSystemPromptChange(mode) {
  const workspaceId = state.workspaceId;
  const agentId = state.selectedAgent;
  const context = systemPromptEditorState(agentId);
  if (!workspaceId || !context || context.draft.pending) return;
  const content = mode === "Custom" ? context.draft.content : null;
  if (mode === "Custom") {
    if (!content.trim()) return toast("内容不能为空", true);
    if (systemPromptContentBytes(content) > SYSTEM_STATIC_PROMPT_MAX_BYTES) {
      return toast("内容过长，请适当精简", true);
    }
  }
  context.draft.pending = {
    mode, content, workspaceId, afterEventId: lastPhysicalEventId(agentId), status: "submitting",
  };
  renderSystemPromptEditor();
  try {
    await sendCommand({
      command: "change_system_static_prompt", agent_id: agentId, mode, content,
    }, workspaceId);
    if (context.draft.pending) context.draft.pending.status = "waiting";
  } catch (error) {
    if (commandResultIsUnknown(error)) {
      if (context.draft.pending) context.draft.pending.status = "unknown";
      if (workspaceId === state.workspaceId) requestHttpSyncNow();
      else scheduleBackgroundWorkspaceSync(0);
      renderSystemPromptEditor();
      return;
    }
    context.draft.pending = null;
    toast(error.message, true);
  }
  if (workspaceId === state.workspaceId) requestHttpSyncNow();
  else scheduleBackgroundWorkspaceSync(0);
  renderSystemPromptEditor();
}

function updateSystemPromptDraft() {
  const context = systemPromptEditorState();
  if (!context || context.draft.pending) return;
  context.draft.content = elements.systemPromptInput.value;
  context.draft.dirty = context.draft.content !== context.saved.content
    || context.saved.mode === "Default";
  renderSystemPromptEditor();
}

function renderAgentControls() {}

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
  if (!transcriptVirtualizer) {
    if (!messages.length) renderEmptyTranscript(elements.transcriptContent);
    else reconcileTranscript(elements.transcriptContent, messages, 0, messages.length, null);
    return;
  }
  transcriptVirtualizer.update(messages, {
    scopeKey: `${state.workspaceId || ""}:${state.selectedAgent || ""}`,
    changedFrom: forceFull ? 0 : changedFrom,
    force: forceFull,
    following: transcriptBottomFollower.isFollowing(),
  });
}

function renderEmptyTranscript(container) {
  const environment = state.snapshot.environment;
  const rendered = `<div class="empty-state"><div><strong>${escapeHtml(runtimeCapabilities.brandTitle)}</strong><p>从这里开始一段对话。</p>${environment ? `<small>${escapeHtml(environment.workspace)}<br>${escapeHtml(environment.system)}</small>` : ""}</div></div>`;
  MeTranscript.reconcileHtmlChildren(container, rendered);
}

function estimateTranscriptMessageHeight(message) {
  if (!messageIsVisible(message)) return 0;
  if (message.kind === "tool" || message.kind === "worker-activity") return 51;
  if (message.kind === "turn-toolbar") return 40;
  if (message.kind === "user") return 72;
  if (message.kind === "assistant") return 120;
  return 45;
}

function transcriptMessageContext(message) {
  return messageIsVisible(message) ? message.kind : null;
}

function reconcileTranscript(container, messages, start, end, previousKind = null) {
  const existing = new Map([...container.children]
    .filter((node) => node.dataset.messageKey)
    .map((node) => [node.dataset.messageKey, node]));
  let position = 0;
  for (let index = start; index < end; index += 1) {
    const message = messages[index];
    const visible = messageIsVisible(message);
    if (!visible) continue;
    const afterTool = isToolLikeKind(previousKind) && message.kind === "assistant";
    const followsTool = previousKind === "tool" && message.kind === "tool";
    const key = messageDomKey(message, index);
    const revision = messageRenderRevision(message, afterTool, followsTool);
    let node = container.children[position];
    if (!node || node.dataset.messageKey !== key) {
      node = existing.get(key) || createMessageNode(message, afterTool, followsTool, index);
      container.insertBefore(node, container.children[position] || null);
    }
    node.dataset.messageIndex = String(index);
    if (node.meRenderRevision !== revision) updateMessageNode(node, message, afterTool, followsTool, index);
    previousKind = message.kind;
    position += 1;
  }
  while (container.children.length > position) container.lastElementChild.remove();
}

const ASSISTANT_ONLY_NON_RENDERING_CHARACTERS = /^[\p{White_Space}\p{Default_Ignorable_Code_Point}\p{Cc}]*$/u;

function assistantContentHasRenderableContent(content) {
  return !ASSISTANT_ONLY_NON_RENDERING_CHARACTERS.test(String(content || ""));
}

function messageIsVisible(message) {
  if (message.kind === "assistant") return assistantContentHasRenderableContent(message.content);
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
  if (visible && node.dataset.messageKind !== message.kind) {
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
  if (message.kind === "notice" || message.kind === "session") {
    const content = node.querySelector(`:scope > .${message.kind}-content`);
    if (!content) {
      node.replaceWith(createMessageNode(message, afterTool, followsTool, index));
      return;
    }
    content.textContent = message.content;
    node.meRenderRevision = messageRenderRevision(message, afterTool, followsTool);
    return;
  }
  node.replaceWith(createMessageNode(message, afterTool, followsTool, index));
}

function initializeMessageNode(node, message, afterTool, followsTool, index) {
  node.dataset.messageKey = messageDomKey(message, index);
  node.dataset.messageIndex = String(index);
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
  if (message.kind === "user") return `<div class="message-block user"><div class="user-message-content">${escapeHtml(message.content)}</div><button class="user-message-actions" type="button" aria-label="消息操作" aria-haspopup="menu" aria-expanded="false">···</button></div>`;
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
  const toggle = (event) => {
    if (event?.target?.closest?.(".tool-details")) return;
    if (window.getSelection()?.toString()) return;
    const key = `${state.selectedAgent}:${card.dataset.toolCard}`;
    if (state.expandedTools.has(key)) state.expandedTools.delete(key); else state.expandedTools.add(key);
    const messages = currentProjection().messages;
    const index = messages.findIndex((message) =>
      message.kind === "tool" && String(message.tool.id) === card.dataset.toolCard);
    if (index >= 0) {
      updateToolCardNode(card, messages[index].tool);
      refreshRunningToolNodes();
      card.meRenderRevision = messageRenderRevision(messages[index], false, card.classList.contains("follows-tool"));
      transcriptVirtualizer?.layoutChanged();
    }
  };
  card.addEventListener("click", toggle);
  card.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") { event.preventDefault(); toggle(event); }
  });
}

function toolPresentationOutput(tool) {
  if (!tool.result && !tool.output && !tool.updates?.length) return undefined;
  return {
    text: tool.output || "",
    updates: tool.updates || [],
    result: tool.result ? {
      state: tool.result.state,
      exitCode: tool.result.exitCode,
      value: safeJson(tool.result.detail) ?? tool.result.detail,
      rawDetail: tool.result.detail,
    } : null,
  };
}

function renderToolCard(tool, followsTool = false) {
  const view = toolCardView(tool);
  return `<div class="tool-card ${view.status} ${view.expanded ? "expanded" : ""} ${followsTool ? "follows-tool" : ""}" data-tool-card="${escapeAttr(tool.id)}" role="button" tabindex="0" aria-expanded="${view.expanded}">
    <div class="tool-header"><span class="tool-marker">●</span><span class="tool-name" title="${escapeAttr(tool.name)}">${escapeHtml(view.title)}</span><span class="tool-brief">${escapeHtml(view.brief)}</span><span class="tool-time"${view.runningStarted == null ? "" : ` data-running-started="${view.runningStarted}"`}>${escapeHtml(view.time)}</span></div>
    ${view.details ? MeToolPresenters.renderDetails(view.details) : ""}
  </div>`;
}

function toolCardView(tool) {
  const resultState = normalize(tool.result?.state || "");
  const expanded = state.expandedTools.has(`${state.selectedAgent}:${tool.id}`);
  const summary = MeToolPresenters.summarize(tool.name, tool.args || {});
  const runningStarted = !tool.queued && !tool.result ? tool.started : null;
  return {
    expanded,
    status: tool.queued ? "queued" : !tool.result ? "running" : resultState === "succeeded" ? "succeeded" : "failed",
    title: summary.title,
    brief: summary.summary,
    time: tool.queued ? "排队中" : !tool.result ? formatDuration(Date.now() - tool.started) : formatDuration(Math.max(0, tool.result.finished - tool.started)),
    runningStarted,
    details: expanded ? MeToolPresenters.describe(tool.name, tool.args || {}, toolPresentationOutput(tool)) : null,
  };
}

function updateToolCardNode(node, tool, followsTool = node.classList.contains("follows-tool")) {
  const view = toolCardView(tool);
  node.className = `tool-card ${view.status} ${view.expanded ? "expanded" : ""} ${followsTool ? "follows-tool" : ""}`;
  node.setAttribute("aria-expanded", String(view.expanded));
  const name = node.querySelector(":scope > .tool-header .tool-name");
  const brief = node.querySelector(":scope > .tool-header .tool-brief");
  const time = node.querySelector(":scope > .tool-header .tool-time");
  if (name.textContent !== view.title) name.textContent = view.title;
  name.title = tool.name;
  if (brief.textContent !== view.brief) brief.textContent = view.brief;
  if (time.textContent !== view.time) time.textContent = view.time;
  if (view.runningStarted == null) delete time.dataset.runningStarted;
  else time.dataset.runningStarted = String(view.runningStarted);
  const details = node.querySelector(":scope > .tool-details");
  if (!view.expanded) {
    details?.remove();
    return;
  }
  const template = document.createElement("template");
  template.innerHTML = MeToolPresenters.renderDetails(view.details);
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
  return MeToolPresenters.summarize(tool.name, tool.args || {}).summary;
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
  const details = expanded ? `${current.objective.description ? `<div class="objective-description">${escapeHtml(current.objective.description)}</div>` : ""}
    ${current.plans.map(({ plan, notes }) => `<div class="objective-plan ${plan.state === "active" ? "active" : ""}"><span>${planSymbol(plan.state)}</span><span>${escapeHtml(plan.title)}${notes.length ? ` (${notes.length} ${notes.length === 1 ? "note" : "notes"})` : ""}</span></div>`).join("")}` : "";
  return `<div class="objective-header"><div class="objective-title">${active ? "■" : "□"} ${escapeHtml(current.objective.title)}</div>
    <span class="objective-toggle" aria-hidden="true"><span class="objective-toggle-icon">${expanded ? "▾" : "▸"}</span></span></div>
    <div id="objective-details" class="objective-details${expanded ? "" : " hidden"}">${details}</div>`;
}

function objectiveDisclosureAttributes(expanded) {
  const label = expanded ? "折叠 Objective 详情" : "展开 Objective 详情";
  return { role: "button", tabindex: "0", "aria-expanded": String(expanded), "aria-controls": "objective-details", "aria-label": label, title: label };
}

const OBJECTIVE_INTERACTIVE_SELECTOR = "a[href], button, input, select, textarea, [contenteditable], [role=\"button\"], [role=\"link\"], [data-objective-interactive]";

function objectiveEventActivates(event, objective) {
  if (event.type === "keydown" && event.key !== "Enter" && event.key !== " ") return false;
  const interactive = event.target.closest?.(OBJECTIVE_INTERACTIVE_SELECTOR);
  return !interactive || interactive === objective;
}

function setObjectiveDisclosureControl(expanded = null) {
  const names = ["role", "tabindex", "aria-expanded", "aria-controls", "aria-label", "title"];
  if (expanded == null) {
    for (const name of names) elements.objective.removeAttribute(name);
    return;
  }
  const attributes = objectiveDisclosureAttributes(expanded);
  for (const [name, value] of Object.entries(attributes)) elements.objective.setAttribute(name, value);
}

function renderObjective() {
  const current = currentStore()?.workmap.current;
  if (!current) {
    syncObjectiveDisclosure(state.objectiveDisclosure, null, null);
    setObjectiveDisclosureControl();
    elements.objective.classList.add("hidden");
    MeTranscript.reconcileHtmlChildren(elements.objective, "");
    return;
  }
  const scopeId = JSON.stringify([state.workspaceId, state.selectedAgent]);
  const disclosure = syncObjectiveDisclosure(state.objectiveDisclosure, scopeId, current.objective.id);
  if (state.view.kind !== "chat") {
    setObjectiveDisclosureControl();
    elements.objective.classList.add("hidden");
    MeTranscript.reconcileHtmlChildren(elements.objective, "");
    return;
  }
  setObjectiveDisclosureControl(disclosure.expanded);
  elements.objective.classList.remove("hidden");
  MeTranscript.reconcileHtmlChildren(elements.objective, objectiveSummaryHtml(current, disclosure.expanded));
}

function toggleObjectiveDisclosure(event) {
  if (elements.objective.classList.contains("hidden") || !objectiveEventActivates(event, elements.objective)) return;
  if (event.type === "keydown") event.preventDefault();
  state.objectiveDisclosure.expanded = !state.objectiveDisclosure.expanded;
  renderObjective();
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
  elements.input.placeholder = readOnly ? `${worker ? "Worker" : "子 Agent"} 对话只读 · ${childStateLabel(currentStore()?.events || [])}` : "发送消息";
  elements.inputHint.textContent = sending
    ? "消息进入列表后即可继续输入"
    : worker ? "可调整模型、推理强度或停止当前任务"
      : readOnly ? "子 Agent 仅允许查看" : `${sendShortcutHint()} · Esc 中止/撤回/清空`;
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
  const active = apiSpinnerIsActive();
  setApiSpinner(active ? API_SPINNER_FRAMES[state.apiAnimationTick % API_SPINNER_FRAMES.length] : "");
  syncUiAnimationScheduler();
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
  openConfirm("清空上下文？", "当前会话将从空白上下文继续，已有消息记录不会被删除。", "清空上下文", () => sendCommand({ command: "clear_context", agent_id: agentId }), true);
}

function terminalRowHeight() {
  const computed = globalThis.getComputedStyle?.(elements.terminalScreen);
  const lineHeight = Number.parseFloat(computed?.lineHeight);
  if (Number.isFinite(lineHeight) && lineHeight > 0) return lineHeight;
  const fontSize = Number.parseFloat(computed?.fontSize);
  return Number.isFinite(fontSize) && fontSize > 0 ? fontSize * 1.25 : 17.5;
}

function terminalWindowRange(rowCount, scrollTop, clientHeight, rowHeight, followBottom) {
  const total = Math.max(0, Math.floor(Number(rowCount) || 0));
  const height = Number.isFinite(rowHeight) && rowHeight > 0 ? rowHeight : 17.5;
  const visibleRows = Math.max(1, Math.ceil(Math.max(0, Number(clientHeight) || 0) / height));
  const windowRows = Math.min(total, Math.max(
    TERMINAL_RENDER_MIN_ROWS,
    visibleRows + TERMINAL_RENDER_OVERSCAN_ROWS * 2,
  ));
  if (total <= windowRows) return { start: 0, end: total };
  if (followBottom) return { start: total - windowRows, end: total };
  const firstVisible = Math.max(0, Math.floor(Math.max(0, Number(scrollTop) || 0) / height));
  const desiredStart = Math.max(0, firstVisible - TERMINAL_RENDER_OVERSCAN_ROWS);
  const start = Math.min(
    total - windowRows,
    Math.floor(desiredStart / TERMINAL_RENDER_OVERSCAN_ROWS) * TERMINAL_RENDER_OVERSCAN_ROWS,
  );
  return { start, end: Math.min(total, start + windowRows) };
}

function terminalRowHtml(frame, row, styles) {
  const runs = (row.runs || []).map((run) => `<span style="left:${run.col}ch;${terminalStyle(styles.get(run.style))}">${escapeHtml(run.text)}</span>`).join("");
  const cursor = frame.cursor.visible && frame.cursor.row === row.row
    ? `<span class="terminal-cursor" style="left:${frame.cursor.col}ch;width:${frame.cursor.wide ? 2 : 1}ch"></span>` : "";
  return `<div class="terminal-row" style="width:${frame.width}ch;position:relative"><span style="position:absolute">${" ".repeat(frame.width)}</span>${runs}${cursor}</div>`;
}

function scheduleTerminalWindowRender() {
  if (terminalWindowRenderFrame != null) return;
  terminalWindowRenderFrame = requestAnimationFrame(() => {
    terminalWindowRenderFrame = null;
    if (state.view.kind === "terminal") renderTerminal();
  });
}

function renderTerminal() {
  const sessionId = state.view.sessionId;
  if (!sessionId || !state.selectedAgent) return;
  const revisionKey = `${state.selectedAgent}:${sessionId}`;
  const frame = state.terminalFrames.get(revisionKey);
  if (!frame) {
    const unavailable = state.terminalFramesUnavailable.has(revisionKey);
    showTerminalMessage(unavailable
      ? `Terminal ${sessionId} 已不可用`
      : `正在同步 Terminal ${sessionId}…`);
    if (!unavailable) requestHttpSyncNow();
    return;
  }
  if (state.view.kind !== "terminal" || state.view.sessionId !== sessionId) return;
  const previousRevision = state.terminalRevisions.get(revisionKey) || 0;
  if (frame.revision < previousRevision) return;
  const switchingTerminal = elements.terminalScreen.dataset.terminalKey !== revisionKey;
  const rowHeight = terminalRowHeight();
  const range = terminalWindowRange(
    frame.rows?.length || 0,
    elements.terminalView.scrollTop,
    elements.terminalView.clientHeight,
    rowHeight,
    state.terminalFollowBottom || switchingTerminal,
  );
  const sameRenderedFrame = !switchingTerminal
    && Number(elements.terminalScreen.dataset.revision) === frame.revision;
  const sameRenderedWindow = Number(elements.terminalScreen.dataset.windowStart) === range.start
    && Number(elements.terminalScreen.dataset.windowEnd) === range.end
    && Number(elements.terminalScreen.dataset.rowHeight) === rowHeight;
  if (sameRenderedFrame && sameRenderedWindow) {
    if (state.terminalFollowBottom) scrollTerminalToBottom();
    return;
  }
  const scroll = captureTerminalScroll(
    elements.terminalView,
    state.terminalFollowBottom || switchingTerminal,
  );
  state.terminalRevisions.set(revisionKey, frame.revision);
  elements.terminalMessage.classList.add("hidden");
  elements.terminalScreen.classList.remove("hidden");
  elements.terminalScreen.style.width = `${frame.width}ch`;
  const styles = new Map((frame.style_defs || []).map((definition) => [definition.id, definition.style]));
  const topHeight = range.start * rowHeight;
  const bottomHeight = Math.max(0, ((frame.rows?.length || 0) - range.end) * rowHeight);
  const topSpacer = topHeight > 0 ? `<div class="terminal-spacer" style="height:${topHeight}px"></div>` : "";
  const bottomSpacer = bottomHeight > 0 ? `<div class="terminal-spacer" style="height:${bottomHeight}px"></div>` : "";
  const rows = (frame.rows || []).slice(range.start, range.end)
    .map((row) => terminalRowHtml(frame, row, styles)).join("");
  elements.terminalScreen.innerHTML = `${topSpacer}${rows}${bottomSpacer}`;
  elements.terminalScreen.dataset.terminalKey = revisionKey;
  elements.terminalScreen.dataset.revision = String(frame.revision);
  elements.terminalScreen.dataset.windowStart = String(range.start);
  elements.terminalScreen.dataset.windowEnd = String(range.end);
  elements.terminalScreen.dataset.rowHeight = String(rowHeight);
  restoreTerminalScroll(elements.terminalView, scroll);
  state.terminalFollowBottom = scroll.followBottom;
}

function showTerminalMessage(message) {
  elements.terminalMessage.textContent = message;
  elements.terminalMessage.classList.remove("hidden");
  elements.terminalScreen.classList.add("hidden");
  delete elements.terminalScreen.dataset.terminalKey;
  delete elements.terminalScreen.dataset.revision;
  delete elements.terminalScreen.dataset.windowStart;
  delete elements.terminalScreen.dataset.windowEnd;
  delete elements.terminalScreen.dataset.rowHeight;
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

const DIRECTORY_NAME_COLLATOR = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
const DIRECTORY_DATE_FORMATTER = new Intl.DateTimeFormat(undefined, {
  year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit",
});
const DIRECTORY_FILE_TYPES = {
  txt: "文本文件", md: "Markdown 文档", markdown: "Markdown 文档", pdf: "PDF 文档",
  doc: "Word 文档", docx: "Word 文档", xls: "Excel 工作簿", xlsx: "Excel 工作簿",
  ppt: "PowerPoint 演示文稿", pptx: "PowerPoint 演示文稿", csv: "CSV 表格",
  png: "PNG 图像", jpg: "JPEG 图像", jpeg: "JPEG 图像", gif: "GIF 图像", webp: "WebP 图像", svg: "SVG 图像",
  mp3: "MP3 音频", wav: "WAV 音频", m4a: "M4A 音频", mp4: "MP4 视频", mov: "QuickTime 视频",
  zip: "ZIP 归档", gz: "GZip 归档", tar: "TAR 归档", zst: "Zstandard 归档",
  json: "JSON 文件", toml: "TOML 配置", yaml: "YAML 配置", yml: "YAML 配置", xml: "XML 文件",
  js: "JavaScript 文件", mjs: "JavaScript 文件", ts: "TypeScript 文件", tsx: "TypeScript 文件",
  html: "HTML 文档", css: "CSS 样式表", rs: "Rust 源文件", py: "Python 文件", sh: "Shell 脚本",
  log: "日志文件", lock: "锁定文件",
};

function directoryEntryType(entry) {
  if (entry?.kind === "drive") return "磁盘";
  if (entry?.kind === "directory") return "文件夹";
  const name = String(entry?.name || "");
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return "文件";
  const extension = name.slice(dot + 1).toLowerCase();
  return DIRECTORY_FILE_TYPES[extension] || `${extension.toUpperCase()} 文件`;
}

function formatDirectorySize(sizeBytes, kind = "file") {
  const bytes = Number(sizeBytes);
  if (kind !== "file" || sizeBytes == null || !Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  const unit = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)) - 1, units.length - 1);
  const value = bytes / (1024 ** (unit + 1));
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}

function formatDirectoryModified(modifiedAtMs) {
  const value = Number(modifiedAtMs);
  if (modifiedAtMs == null || !Number.isFinite(value) || value < 0) return "—";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "—" : DIRECTORY_DATE_FORMATTER.format(date);
}

function filterDirectoryEntries(entries, query) {
  const normalized = String(query || "").trim().toLocaleLowerCase();
  if (!normalized) return [...(entries || [])];
  return (entries || []).filter((entry) => String(entry.name || "").toLocaleLowerCase().includes(normalized));
}

function directorySortValue(entry, sortKey) {
  if (sortKey === "modified") return entry.modified_at_ms != null && Number.isFinite(Number(entry.modified_at_ms)) ? Number(entry.modified_at_ms) : null;
  if (sortKey === "type") return directoryEntryType(entry);
  if (sortKey === "size") return entry.kind === "file" && entry.size_bytes != null && Number.isFinite(Number(entry.size_bytes)) ? Number(entry.size_bytes) : null;
  return String(entry.name || "");
}

function sortDirectoryEntries(entries, sortKey = "name", direction = "asc") {
  const multiplier = direction === "desc" ? -1 : 1;
  return [...(entries || [])].sort((left, right) => {
    const leftGroup = left.kind === "file" ? 1 : 0;
    const rightGroup = right.kind === "file" ? 1 : 0;
    if (leftGroup !== rightGroup) return leftGroup - rightGroup;
    const leftValue = directorySortValue(left, sortKey);
    const rightValue = directorySortValue(right, sortKey);
    const leftMissing = leftValue == null;
    const rightMissing = rightValue == null;
    if (leftMissing !== rightMissing) return leftMissing ? 1 : -1;
    let primary = 0;
    if (!leftMissing) primary = typeof leftValue === "number"
      ? leftValue - rightValue
      : DIRECTORY_NAME_COLLATOR.compare(String(leftValue), String(rightValue));
    if (primary) return primary * multiplier;
    return DIRECTORY_NAME_COLLATOR.compare(String(left.name || ""), String(right.name || ""));
  });
}

function directoryParentRequest(listing) {
  if (listing.parent) return { path: listing.parent, roots: false };
  if (listing.parent_is_root_selector) return { path: null, roots: true };
  return null;
}


function directoryEntryIcon(entry) {
  if (entry.kind === "drive") return `<svg class="directory-file-icon drive" viewBox="0 0 24 24" aria-hidden="true"><rect x="3.5" y="5" width="17" height="14" rx="2"/><path d="M3.5 14h17"/><circle cx="17" cy="16.5" r=".8"/></svg>`;
  if (entry.kind === "directory") return `<svg class="directory-file-icon folder" viewBox="0 0 24 24" aria-hidden="true"><path d="M3.5 6.5h6l2 2h9v9.5a2 2 0 0 1-2 2h-15v-13.5Z"/><path d="M3.5 10h17"/></svg>`;
  return `<svg class="directory-file-icon file" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3.5h8l4 4V20H6z"/><path d="M14 3.5V8h4"/></svg>`;
}

function directoryVisibleEntries(directory) {
  return sortDirectoryEntries(
    filterDirectoryEntries(directory.listing.entries || [], directory.searchQuery),
    directory.sortKey,
    directory.sortDirection,
  );
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
      choices: [], selected: null, confirmLabel: mode === "create" ? "创建并打开" : "打开",
      html: `<div class="directory-browser"></div>`,
      directory: {
        mode, listing, selectedPath: null, searchQuery: "", sortKey: "name", sortDirection: "asc",
        workspaceName: "", creatingFolder: false,
      },
      onOpen: renderDirectoryBrowser,
      onConfirm: async () => {
        const directoryModal = state.modal;
        const directory = directoryModal?.directory;
        const current = directory?.listing?.path;
        if (!current) throw new Error("请选择目录");
        let result;
        if (mode === "create") {
          const name = String(directory.workspaceName || "").trim();
          if (!name) throw new Error("请输入工作区名称");
          result = await api("/api/gateway/workspaces/create", {
            method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ parent: current, name }),
          });
        } else {
          result = await api("/api/gateway/workspaces/open", {
            method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ path: current, initialize: false }),
          });
          if (result.status === "requires_initialization") {
            openWorkspaceInitializationConfirm(directoryModal, result.path || current);
            return;
          }
        }
        await refreshGatewayState();
        activateWorkspace(result.workspace_id);
      },
    });
  } catch (error) { toast(error.message, true); }
}

function openWorkspaceInitializationConfirm(directoryModal, path) {
  openModal({
    title: "创建工作区？",
    description: `所选目录“${path}”尚不是 ME 工作区。是否在此创建并打开？`,
    choices: [], selected: null, confirmLabel: "创建并打开",
    onCancel: () => openModal(directoryModal),
    onConfirm: async () => {
      const result = await api("/api/gateway/workspaces/open", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path, initialize: true }),
      });
      if (result.status !== "opened" || !result.workspace_id) throw new Error("无法创建该工作区");
      await refreshGatewayState();
      activateWorkspace(result.workspace_id);
    },
  });
}

async function loadDirectoryListing(path, roots = false, preserveState = false) {
  const directory = state.modal?.directory;
  if (!directory) return;
  const browser = elements.modalContent.querySelector(".directory-browser");
  const controls = [...elements.modalContent.querySelectorAll("[data-directory-control]")];
  browser?.classList.add("loading");
  controls.forEach((control) => { control.disabled = true; });
  try {
    const listing = await api("/api/gateway/directories", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path, roots }),
    });
    if (state.modal?.directory !== directory) return;
    const previousSelection = preserveState ? directory.selectedPath : null;
    directory.listing = listing;
    directory.selectedPath = (listing.entries || []).some((entry) => entry.path === previousSelection) ? previousSelection : null;
    if (!preserveState) directory.searchQuery = "";
    directory.creatingFolder = false;
    renderDirectoryBrowser();
  } catch (error) {
    browser?.classList.remove("loading");
    controls.forEach((control) => { control.disabled = false; });
    toast(error.message, true);
  }
}

function directorySortHeader(key, label, directory) {
  const active = directory.sortKey === key;
  const ariaSort = active ? (directory.sortDirection === "asc" ? "ascending" : "descending") : "none";
  const indicator = active ? (directory.sortDirection === "asc" ? "↑" : "↓") : "";
  return `<button type="button" class="directory-sort${active ? " active" : ""}" data-directory-sort="${key}" aria-sort="${ariaSort}" data-directory-control><span>${label}</span><span class="directory-sort-indicator" aria-hidden="true">${indicator}</span></button>`;
}

function renderDirectoryBrowser() {
  const directory = state.modal?.directory;
  if (!directory) return;
  const listing = directory.listing;
  const rootSelector = listing.root_selector === true;
  const parentRequest = directoryParentRequest(listing);
  const currentLocation = rootSelector ? "此电脑" : String(listing.path || "");
  const locationIcon = directoryEntryIcon({ kind: rootSelector ? "drive" : "directory" });
  elements.modalContent.innerHTML = `<div class="directory-browser${rootSelector ? " root-selector" : ""}">
    <div class="directory-toolbar">
      <div class="directory-navigation">
        <button type="button" class="directory-control directory-up" aria-label="返回上一级" title="返回上一级" ${parentRequest ? "data-directory-up" : "disabled"} data-directory-control><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m15 18-6-6 6-6"/></svg></button>
        <div class="directory-current">
          <span class="directory-location-icon">${locationIcon}</span>
          <span class="directory-current-path" title="${escapeAttr(currentLocation)}">${escapeHtml(currentLocation)}</span>
          <span class="directory-count"></span>
        </div>
      </div>
      <div class="directory-tools">
        <button type="button" class="directory-control directory-new-folder" aria-label="新建文件夹" title="新建文件夹" ${listing.path ? "data-directory-new-folder" : "disabled"} data-directory-control><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3.5 7h6l2 2h9v9.5a2 2 0 0 1-2 2h-15z"/><path d="M12 12v6m-3-3h6"/></svg></button>
        <button type="button" class="directory-control directory-refresh" aria-label="刷新" title="刷新" data-directory-refresh data-directory-control><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M19 8a7 7 0 1 0 1 5"/><path d="M19 4v4h-4"/></svg></button>
        <label class="directory-search">
          <svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="6.5"/><path d="m16 16 4 4"/></svg>
          <input type="search" value="${escapeAttr(directory.searchQuery)}" placeholder="筛选当前目录" aria-label="筛选当前目录">
          <button type="button" class="directory-search-clear${directory.searchQuery ? "" : " hidden"}" aria-label="清空筛选" title="清空筛选">×</button>
        </label>
      </div>
    </div>
    ${directory.creatingFolder ? `<form class="directory-new-folder-form"><label>新建文件夹<input id="directory-new-folder-name" autocomplete="off" placeholder="文件夹名称"></label><button type="submit" class="ghost-button">创建</button><button type="button" class="ghost-button" data-directory-new-folder-cancel>取消</button></form>` : ""}
    <div class="directory-table">
      <div class="directory-list-header" role="row">
        ${directorySortHeader("name", "名称", directory)}
        ${directorySortHeader("modified", "修改时间", directory)}
        ${directorySortHeader("type", "类型", directory)}
        ${directorySortHeader("size", "大小", directory)}
        <span aria-hidden="true"></span>
      </div>
      <div class="directory-list" role="listbox" aria-label="${rootSelector ? "磁盘" : "当前目录内容"}"></div>
    </div>
    ${directory.mode === "create" ? `<label class="directory-name">工作区名称<input id="new-workspace-name" type="text" autocomplete="off" value="${escapeAttr(directory.workspaceName)}" placeholder="例如：my-project"></label>` : `<div class="directory-selection-summary"><span class="directory-target">当前目录将作为工作区打开。</span><span class="directory-selected-item"></span></div>`}
  </div>`;
  elements.modalConfirm.disabled = !listing.path;
  elements.modalContent.querySelector("[data-directory-up]")?.addEventListener("click", () => {
    void loadDirectoryListing(parentRequest.path, parentRequest.roots);
  });
  elements.modalContent.querySelector("[data-directory-refresh]")?.addEventListener("click", () => {
    void loadDirectoryListing(listing.path, rootSelector, true);
  });
  elements.modalContent.querySelector("[data-directory-new-folder]")?.addEventListener("click", () => {
    directory.creatingFolder = true;
    renderDirectoryBrowser();
    elements.modalContent.querySelector("#directory-new-folder-name")?.focus();
  });
  elements.modalContent.querySelector("[data-directory-new-folder-cancel]")?.addEventListener("click", () => {
    directory.creatingFolder = false;
    renderDirectoryBrowser();
  });
  elements.modalContent.querySelector(".directory-new-folder-form")?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const name = elements.modalContent.querySelector("#directory-new-folder-name")?.value.trim();
    if (!name) { toast("请输入文件夹名称", true); return; }
    try {
      await api("/api/gateway/directories/create", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ parent: listing.path, name }),
      });
      await loadDirectoryListing(listing.path);
    } catch (error) { toast(error.message, true); }
  });
  const search = elements.modalContent.querySelector(".directory-search input");
  search?.addEventListener("input", () => {
    directory.searchQuery = search.value;
    renderDirectoryRows();
    elements.modalContent.querySelector(".directory-search-clear")?.classList.toggle("hidden", !directory.searchQuery);
  });
  elements.modalContent.querySelector(".directory-search-clear")?.addEventListener("click", () => {
    directory.searchQuery = "";
    search.value = "";
    renderDirectoryRows();
    elements.modalContent.querySelector(".directory-search-clear")?.classList.add("hidden");
    search.focus();
  });
  elements.modalContent.querySelectorAll("[data-directory-sort]").forEach((button) => button.addEventListener("click", () => {
    const key = button.dataset.directorySort;
    if (directory.sortKey === key) directory.sortDirection = directory.sortDirection === "asc" ? "desc" : "asc";
    else { directory.sortKey = key; directory.sortDirection = "asc"; }
    renderDirectoryBrowser();
    elements.modalContent.querySelector(`[data-directory-sort="${key}"]`)?.focus();
  }));
  const workspaceName = elements.modalContent.querySelector("#new-workspace-name");
  workspaceName?.addEventListener("input", () => { directory.workspaceName = workspaceName.value; });
  renderDirectoryRows();
  if (directory.creatingFolder) elements.modalContent.querySelector("#directory-new-folder-name")?.focus();
  else if (directory.mode === "create") workspaceName?.focus();
}

function updateDirectorySelection(directory, list, allEntries = directory.listing.entries || []) {
  list.querySelectorAll("[data-directory-entry]").forEach((row) => {
    const selected = row.dataset.directoryEntry === directory.selectedPath;
    row.classList.toggle("selected", selected);
    row.setAttribute("aria-selected", String(selected));
  });
  const selectedEntry = allEntries.find((entry) => entry.path === directory.selectedPath);
  const selectedItem = elements.modalContent.querySelector(".directory-selected-item");
  if (selectedItem) selectedItem.textContent = selectedEntry
    ? selectedEntry.kind === "file" ? `已选择“${selectedEntry.name}”（文件仅供查看）` : `已选择“${selectedEntry.name}”（双击进入）`
    : "双击文件夹进入；文件仅供查看。";
}


function renderDirectoryRows() {
  const directory = state.modal?.directory;
  const list = elements.modalContent.querySelector(".directory-list");
  if (!directory || !list) return;
  const entries = directoryVisibleEntries(directory);
  const allEntries = directory.listing.entries || [];
  const rootSelector = directory.listing.root_selector === true;
  const queryActive = String(directory.searchQuery || "").trim().length > 0;
  list.innerHTML = entries.map((entry) => {
    const navigable = entry.kind !== "file";
    const selected = entry.path === directory.selectedPath;
    const modified = formatDirectoryModified(entry.modified_at_ms);
    const type = directoryEntryType(entry);
    const size = formatDirectorySize(entry.size_bytes, entry.kind);
    const label = navigable ? `${type} ${entry.name}，双击进入` : `文件 ${entry.name}，仅供查看`;
    return `<div class="directory-entry${selected ? " selected" : ""}" role="option" tabindex="0" aria-selected="${selected}" aria-label="${escapeAttr(label)}" data-directory-entry="${escapeAttr(entry.path)}" data-entry-kind="${entry.kind}">
      <span class="directory-entry-name"><span class="directory-entry-icon">${directoryEntryIcon(entry)}</span><span class="directory-entry-copy"><span class="directory-entry-title" title="${escapeAttr(entry.name)}">${escapeHtml(entry.name)}</span><span class="directory-entry-mobile-meta"><span>${escapeHtml(modified)}</span><span>${escapeHtml(type)}</span><span>${escapeHtml(size)}</span></span></span></span>
      <span class="directory-entry-modified">${escapeHtml(modified)}</span>
      <span class="directory-entry-type">${escapeHtml(type)}</span>
      <span class="directory-entry-size">${escapeHtml(size)}</span>
      ${navigable ? `<button type="button" class="directory-entry-enter" data-directory-enter="${escapeAttr(entry.path)}" aria-label="进入 ${escapeAttr(entry.name)}" title="进入"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg></button>` : `<span class="directory-entry-action" aria-hidden="true"></span>`}
    </div>`;
  }).join("");
  if (!entries.length) {
    const title = queryActive ? "没有匹配项" : rootSelector ? "没有可用磁盘" : "此目录为空";
    const detail = queryActive ? "请尝试其他筛选词。" : rootSelector ? "当前宿主机没有可浏览的逻辑盘符。" : "可以在此新建文件夹，或返回上一级。";
    list.innerHTML = `<div class="directory-empty"><span class="directory-entry-icon">${directoryEntryIcon({ kind: rootSelector ? "drive" : "directory" })}</span><strong>${title}</strong><span>${detail}</span></div>`;
  }
  const count = elements.modalContent.querySelector(".directory-count");
  if (count) count.textContent = queryActive ? `${entries.length} / ${allEntries.length} 项` : `${allEntries.length} 项`;
  updateDirectorySelection(directory, list, allEntries);
  list.querySelectorAll("[data-directory-entry]").forEach((row) => {
    row.addEventListener("click", () => {
      directory.selectedPath = row.dataset.directoryEntry;
      updateDirectorySelection(directory, list, allEntries);
      row.focus();
    });
    row.addEventListener("dblclick", () => {
      if (row.dataset.entryKind !== "file") void loadDirectoryListing(row.dataset.directoryEntry);
    });
    row.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && row.dataset.entryKind !== "file") {
        event.preventDefault();
        void loadDirectoryListing(row.dataset.directoryEntry);
      } else if (event.key === " ") {
        event.preventDefault();
        directory.selectedPath = row.dataset.directoryEntry;
        updateDirectorySelection(directory, list, allEntries);
      }
    });
  });
  list.querySelectorAll("[data-directory-enter]").forEach((button) => button.addEventListener("click", (event) => {
    event.stopPropagation();
    void loadDirectoryListing(button.dataset.directoryEnter);
  }));
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
  const identity = model.model || "尚未填写模型标识";
  return `<details class="settings-model" data-settings-model="${index}">
    <summary>
      <span class="settings-model-title">
        <svg class="settings-model-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3 4.5 7.2v9.6L12 21l7.5-4.2V7.2L12 3Z"/><path d="m4.8 7.4 7.2 4 7.2-4M12 11.4V21"/></svg>
        <span class="settings-model-heading"><strong>${escapeHtml(model.name || "新模型")}</strong><small>${escapeHtml(identity)}</small></span>
      </span>
      <span class="settings-model-summary-meta">
        <span class="settings-model-provider">${escapeHtml(model.provider)}</span>
        <svg class="settings-model-chevron" viewBox="0 0 24 24" aria-hidden="true"><path d="m9 7 5 5-5 5"/></svg>
      </span>
    </summary>
    <div class="settings-model-body">
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
      <div class="settings-model-advanced">
        <label>能力配置（JSON）<textarea data-setting="capabilities" rows="7">${escapeHtml(JSON.stringify(model.capabilities, null, 2))}</textarea></label>
        <label>请求参数（JSON）<textarea data-setting="parameters" rows="5">${escapeHtml(JSON.stringify(model.parameters || {}, null, 2))}</textarea></label>
        <label>推理强度参数（JSON）<textarea data-setting="effort_parameters" rows="5">${escapeHtml(JSON.stringify(model.effort_parameters || {}, null, 2))}</textarea></label>
      </div>
      <div class="settings-model-danger"><button type="button" class="ghost-button danger settings-remove-model" data-remove-model="${index}">移除模型</button></div>
    </div>
  </details>`;
}

function resolveGatewayEdbCacheLabel(entry) {
  let workspaceId = null;
  let snapshot = null;
  if (state.snapshot?.environment?.workspace === entry.scope) {
    workspaceId = state.workspaceId;
    snapshot = state.snapshot;
  } else {
    for (const [candidateId, workspace] of state.workspaceStates) {
      if (workspace.snapshot?.environment?.workspace !== entry.scope) continue;
      workspaceId = candidateId;
      snapshot = workspace.snapshot;
      break;
    }
  }
  const meta = snapshot?.agents?.find((agent) => agent.id === entry.agentId);
  const gatewayWorkspace = state.gateway.workspaces?.find((item) => item.id === workspaceId);
  return {
    workspace: gatewayWorkspace?.name || entry.workspaceLabel || MeEdbCache.workspaceName(entry.scope),
    title: meta?.title || entry.sessionLabel || null,
  };
}

function localPreferenceSettingsHtml() {
  if (!runtimeCapabilities.windowBorderStyle) return "";
  return `<section class="settings-section settings-local-section">
    <header class="settings-section-header">
      <div class="settings-section-heading"><h3>本机偏好</h3><p>仅保存在当前设备，并会立即生效。</p></div>
    </header>
    <div class="settings-preference-list">
      <label class="settings-preference-row settings-preference-select">
        <span class="settings-preference-copy"><strong>边框样式</strong><small>选择桌面窗口边缘使用的颜色。</small></span>
        <select data-local-preference="window-border-style">
          <option value="default" ${state.windowBorderStyle === WINDOW_BORDER_DEFAULT ? "selected" : ""}>默认</option>
          <option value="theme" ${state.windowBorderStyle === WINDOW_BORDER_THEME ? "selected" : ""}>主题</option>
        </select>
      </label>
    </div>
  </section>`;
}

function bindLocalPreferenceSettings(container = elements.modalContent) {
  const borderStyle = container.querySelector('[data-local-preference="window-border-style"]');
  borderStyle?.addEventListener("change", () => setWindowBorderStyle(borderStyle.value));
}

function openLocalSettings() {
  openModal({
    kind: "settings",
    title: "设置",
    description: "管理此设备的本机偏好。",
    choices: [],
    selected: null,
    confirmLabel: null,
    html: `<div class="settings-editor">${localPreferenceSettingsHtml()}</div>`,
    onOpen: bindLocalPreferenceSettings,
  });
}

function renderGatewayEdbCacheSettings() {
  const container = elements.modalContent.querySelector("#settings-edb-cache-manager");
  if (!container) return;
  void edbCache.renderManager(container, {
    resolveLabel: resolveGatewayEdbCacheLabel,
    storageLabel: runtimeCapabilities.cacheStorageLabel,
    onRemoved: () => toast("会话缓存已清除"),
    onError: (error) => toast(error?.message || "无法清除会话缓存", true),
  });
}

function openEdbCacheSettings() {
  openModal({
    kind: "settings",
    title: "设置",
    description: "管理本机偏好与此设备保存的会话缓存。",
    choices: [],
    selected: null,
    confirmLabel: null,
    html: `<div class="settings-editor">${localPreferenceSettingsHtml()}<div id="settings-edb-cache-manager" class="edb-cache-manager settings-cache-manager"></div></div>`,
    onOpen: (container) => {
      bindLocalPreferenceSettings(container);
      renderGatewayEdbCacheSettings();
    },
  });
}

function renderSettingsModal() {
  const settings = state.modal?.settings;
  if (!settings) return;
  elements.modalContent.innerHTML = `<div class="settings-editor">
    ${localPreferenceSettingsHtml()}
    <section class="settings-section settings-model-section">
      <header class="settings-section-header">
        <div class="settings-section-heading"><h3>模型</h3><p>管理默认模型与模型预设。</p></div>
      </header>
      <div class="settings-section-content">
        <label class="settings-default-model">
          <span class="settings-default-copy"><strong>默认模型</strong></span>
          <select id="settings-default-model">${settings.models.map((model) => `<option value="${escapeAttr(model.name)}" ${model.name === settings.default_model ? "selected" : ""}>${escapeHtml(model.name || "未命名模型")}</option>`).join("")}</select>
        </label>
        <div class="settings-subsection-header">
          <div><h4>模型预设</h4><p>选择预设以查看或修改配置。</p></div>
          <button id="settings-add-model" type="button" class="ghost-button settings-section-action"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg><span>新增模型</span></button>
        </div>
        <div class="settings-models">${settings.models.map(modelSettingsHtml).join("")}</div>
        <p class="settings-help">保存后不会更改正在运行的工作区。请重启 ME Gateway 以使用新设置。</p>
      </div>
    </section>
    <div id="settings-edb-cache-manager" class="edb-cache-manager settings-cache-manager"></div>
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
  bindLocalPreferenceSettings(elements.modalContent);
  renderGatewayEdbCacheSettings();
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
      kind: "settings",
      title: "设置", description: "管理本机偏好、模型配置与此设备保存的会话缓存。",
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
  const messageOnly = modal.html == null && !choices.length;
  state.modal = { ...modal, choices, busy: false };
  elements.modalTitle.textContent = modal.title;
  elements.modalDescription.textContent = modal.description || "";
  elements.modalDescription.classList.toggle("hidden", !modal.description);
  elements.modalConfirm.disabled = false;
  elements.modalCancel.disabled = false;
  elements.modalClose.disabled = false;
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
  elements.modalBackdrop.classList.toggle("message-modal-backdrop", messageOnly);
  elements.modalBackdrop.classList.toggle("directory-modal-backdrop", modal.kind === "directory");
  elements.modalBackdrop.classList.toggle("settings-modal-backdrop", modal.kind === "settings");
  elements.modalBackdrop.classList.remove("hidden");
  state.modal.onOpen?.(elements.modalContent);
}

function closeModal() {
  state.modal = null;
  elements.modalBackdrop.classList.add("hidden");
  elements.modalBackdrop.classList.remove("directory-modal-backdrop", "message-modal-backdrop", "settings-modal-backdrop");
}

function cancelModal() {
  const modal = state.modal;
  if (!modal || modal.busy) return;
  if (modal.onCancel) modal.onCancel();
  else closeModal();
}

async function confirmModal() {
  const modal = state.modal;
  if (!modal || modal.busy) return;
  modal.busy = true;
  elements.modalConfirm.disabled = true;
  elements.modalCancel.disabled = true;
  elements.modalClose.disabled = true;
  try {
    if (modal.onConfirm) await modal.onConfirm(modal.selected);
    if (state.modal === modal) closeModal();
  } catch (error) {
    if (state.modal === modal) toast(error.message, true);
  } finally {
    if (state.modal === modal) {
      modal.busy = false;
      elements.modalConfirm.disabled = false;
      elements.modalCancel.disabled = false;
      elements.modalClose.disabled = false;
    }
  }
}

async function sendCommand(payload, workspaceId = state.workspaceId, { refresh = true } = {}) {
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
    if (refresh && workspaceId === state.workspaceId) requestHttpSyncNow();
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
    autoSizeInput(true);
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
    else { elements.input.value = ""; saveDraft(); autoSizeInput(true); }
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
        inFlight: null, pendingRemote: null, waiters: [], batchTimer: null,
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
  if (!agentId) return;
  const existing = state.draftSync.get(agentId);
  if (existing) {
    clearDraftBatch(existing);
    return;
  }
  const content = elements.input.value;
  state.draftSync.set(agentId, {
    workspaceId: state.workspaceId,
    desired: content, sent: content, sending: false, paused: false,
    inFlight: null, pendingRemote: null, waiters: [], batchTimer: null,
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

function clearDraftBatch(sync) {
  if (!sync || sync.batchTimer == null) return;
  clearTimeout(sync.batchTimer);
  sync.batchTimer = null;
}

function scheduleDraftSync(agentId, sync) {
  if (sync.batchTimer != null || sync.sending || sync.paused) return;
  sync.batchTimer = setTimeout(() => {
    sync.batchTimer = null;
    void runDraftSync(agentId, sync);
  }, DRAFT_BATCH_MS);
}

function queueDraftUpdate(agentId, content) {
  let sync = state.draftSync.get(agentId);
  if (!sync) {
    sync = {
      workspaceId: state.workspaceId,
      desired: content, sent: null, sending: false, paused: false,
      inFlight: null, pendingRemote: null, waiters: [], batchTimer: null,
    };
    state.draftSync.set(agentId, sync);
  } else sync.desired = content;
  scheduleDraftSync(agentId, sync);
}

async function runDraftSync(agentId, sync) {
  if (sync.sending) return;
  clearDraftBatch(sync);
  const workspaceId = sync.workspaceId || state.workspaceId;
  sync.workspaceId = workspaceId;
  const active = workspaceId === state.workspaceId;
  if (sync.paused || !active || !state.connected
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
        }, workspaceId, { refresh: false });
      } finally {
        if (sync.inFlight === flight) sync.inFlight = null;
      }
      const revision = Number(response?.receipt?.input_draft_revision);
      const accepted = response?.receipt?.accepted === true;
      if (!Number.isSafeInteger(revision)) throw new Error("输入同步返回了无效结果");
      if (revision < store.inputDraftRevision) continue;
      if (!accepted) {
        sync.sent = sync.desired;
        requestHttpSyncNow();
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
      scheduleDraftSync(agentId, sync);
    }
  }
}

async function pauseDraftSyncForSubmission(agentId) {
  let sync = state.draftSync.get(agentId);
  if (!sync) {
    sync = {
      workspaceId: state.workspaceId,
      desired: "", sent: null, sending: false, paused: true,
      inFlight: null, pendingRemote: null, waiters: [], batchTimer: null,
    };
    state.draftSync.set(agentId, sync);
  } else {
    clearDraftBatch(sync);
    sync.desired = "";
    sync.paused = true;
  }
  while (sync.sending) await new Promise((resolve) => sync.waiters.push(resolve));
}

function restoreDraft() {
  const pending = currentStore()?.pendingPromptSubmission;
  elements.input.value = pending?.displayContent ?? state.drafts.get(state.selectedAgent) ?? "";
  autoSizeInput(true);
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
      const url = frontendRuntime.apiPath("/api/command", workspaceId);
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
    elements.inputMirror.value = elements.input.value;
    elements.inputMirror.style.height = "0px";
    const target = Math.min(Math.max(elements.inputMirror.scrollHeight, 29), 180);
    if (state.inputHeight !== target) {
      const height = `${target}px`;
      if (elements.input.style.height !== height) elements.input.style.height = height;
      state.inputHeight = target;
    }
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
bindSidebarScrollbar(elements.sidebarScroll);
elements.objective.addEventListener("click", toggleObjectiveDisclosure);
elements.objective.addEventListener("keydown", toggleObjectiveDisclosure);
globalThis.MeTheme.bindControls(elements.themeCycle, elements.themeMode, (message) => toast(message));
elements.loginSettings?.addEventListener("click", openLocalSettings);
elements.loginForm.addEventListener("submit", submitLogin);
elements.loginRemoteDevice?.addEventListener("click", () => {
  elements.loginError.textContent = "";
  setLoginView("form");
  elements.loginEndpoint?.focus({ preventScroll: true });
  elements.loginScreen.scrollTop = 0;
});
elements.loginFormBack?.addEventListener("click", () => {
  elements.loginError.textContent = "";
  setLoginView("devices");
  renderLoginDevices();
  (elements.loginLocalDevice.disabled ? elements.loginRemoteDevice : elements.loginLocalDevice)?.focus();
});
elements.loginLocalDevice?.addEventListener("click", () => { void loginLocalDevice(); });
elements.loginLocalForget?.addEventListener("click", () => {
  void forgetRememberedDevice(state.localDevice.endpoint);
});
elements.loginRememberedList?.addEventListener("click", (event) => {
  const forget = event.target.closest("button[data-forget-endpoint]");
  if (forget) {
    void forgetRememberedDevice(forget.dataset.forgetEndpoint);
    return;
  }
  const connect = event.target.closest("button[data-login-endpoint]");
  if (connect) void loginRememberedDevice(connect.dataset.loginEndpoint);
});
elements.connectionRetry.addEventListener("click", retryConnectionNow);
elements.addAgent.addEventListener("click", () => { if (workspaceMetadataReady("chat")) { closeMobileSidebar(); activateWorkspace("chat"); openAddAgent(); } });
if (runtimeCapabilities.multipleWorkspaces) {
  elements.createWorkspace.addEventListener("click", () => { closeMobileSidebar(); void openDirectoryBrowser("create"); });
  elements.openWorkspace.addEventListener("click", () => { closeMobileSidebar(); void openDirectoryBrowser("open"); });
}
elements.openSettings.addEventListener("click", () => {
  closeMobileSidebar();
  if (runtimeCapabilities.gatewaySettings) void openGatewaySettings();
  else openEdbCacheSettings();
});
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
elements.systemPromptInput.addEventListener("input", updateSystemPromptDraft);
elements.systemPromptApply.addEventListener("click", () => { void submitSystemPromptChange("Custom"); });
elements.systemPromptRestore.addEventListener("click", () => { void submitSystemPromptChange("Default"); });
elements.scrollToBottom.addEventListener("click", scrollTranscriptToBottomAfterLayout);
elements.statusModelTrigger.addEventListener("click", openModelDrawer);
elements.statusEffortTrigger.addEventListener("click", openEffortDrawer);
elements.statusContextTrigger.addEventListener("click", openContextDrawer);
elements.input.addEventListener("input", () => {
  state.lastInputAt = performance.now();
  saveDraft();
  autoSizeInput();
});
elements.input.addEventListener("compositionstart", beginInputComposition);
elements.input.addEventListener("compositionend", endInputComposition);
function enterSubmitsPrompt(event) {
  return sendShortcutPressed(event, state.sendShortcut);
}
elements.input.addEventListener("keydown", (event) => {
  if (state.composing || event.isComposing || event.keyCode === 229) return;
  if (enterSubmitsPrompt(event)) {
    event.preventDefault(); submitPrompt();
  } else if (event.key === "Escape") {
    event.preventDefault(); escapeAction();
  }
});
elements.modalClose.addEventListener("click", cancelModal);
elements.modalCancel.addEventListener("click", cancelModal);
elements.modalConfirm.addEventListener("click", confirmModal);
elements.modalBackdrop.addEventListener("click", (event) => { if (event.target === elements.modalBackdrop) cancelModal(); });
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
  if (!elements.compactSummaryBackdrop.classList.contains("hidden")) closeCompactSummary(); else if (state.contextDrawerOpen) closeContextDrawer(); else if (state.drawer) closeChoiceDrawer(); else if (state.modal) cancelModal(); else if (state.userMenu) closeUserMessageMenu(); else if (state.agentMenu) closeAgentMenu(); else if (state.workspaceMenu) closeWorkspaceMenu();
});
window.addEventListener("resize", () => {
  closeUserMessageMenu();
  closeAgentMenu();
  closeWorkspaceMenu();
  positionToastRegion();
  updateScrollToBottomButton();
  transcriptVirtualizer?.layoutChanged();
  if (state.view.kind === "terminal") {
    if (state.terminalFollowBottom) requestAnimationFrame(scrollTerminalToBottom);
    void renderTerminal();
  }
});
window.addEventListener("pagehide", () => {
  state.pageClosing = true;
  cancelBackgroundWorkspaceSync();
  stopUiAnimation();
  deactivateSessionTerminalView();
  deactivateRemoteControlView();
  flushDraftBeforePageCloses();
});
window.addEventListener("pageshow", () => {
  state.pageClosing = false;
  syncUiAnimationScheduler();
  if ((!state.authRequired || state.authenticated) && !state.connected) startHttpPolling();
  scheduleBackgroundWorkspaceSync(0);
  if (state.view.kind === "session-terminal" || state.view.kind === "remote-control") renderTabs();
});
document.addEventListener("visibilitychange", () => {
  if (document.hidden) stopUiAnimation(); else syncUiAnimationScheduler();
});
const transcriptBottomFollower = createTranscriptBottomFollower(
  elements.transcript,
  elements.transcriptContent,
  updateScrollToBottomButton,
);
transcriptVirtualizer = MeTranscript.createVirtualTranscript(
  elements.transcript,
  elements.transcriptContent,
  {
    key: messageDomKey,
    revision: (message) => messageRenderRevision(message, false, false),
    context: transcriptMessageContext,
    estimateHeight: estimateTranscriptMessageHeight,
    renderRange: reconcileTranscript,
    renderEmpty: renderEmptyTranscript,
    isFollowing: () => transcriptBottomFollower.isFollowing(),
    onLayoutChange: () => transcriptBottomFollower.layoutChanged(),
  },
);
elements.transcript.addEventListener("scroll", () => {
  closeUserMessageMenu();
  transcriptBottomFollower.noteScroll();
  transcriptVirtualizer.noteScroll();
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
    if (!state.terminalFollowBottom) scheduleTerminalWindowRender();
  }
}, { passive: true });
function apiSpinnerIsActive() {
  return state.apiActivity.active || API_ACTIVE.has(currentProjection().apiState);
}

function setApiSpinner(text) {
  if (elements.apiSpinner.textContent !== text) elements.apiSpinner.textContent = text;
}

function stopUiAnimation() {
  clearTimeout(state.uiAnimationTimer);
  state.uiAnimationTimer = null;
}

function uiAnimationNeeded() {
  return !state.pageClosing && !document.hidden
    && (apiSpinnerIsActive() || state.runningToolNodes.length > 0);
}

function syncUiAnimationScheduler() {
  if (!apiSpinnerIsActive()) setApiSpinner("");
  if (!uiAnimationNeeded()) {
    stopUiAnimation();
    return;
  }
  if (state.uiAnimationTimer === null) {
    state.uiAnimationTimer = setTimeout(refreshUiAnimation, UI_ANIMATION_INTERVAL_MS);
  }
}

function refreshRunningToolNodes() {
  state.runningToolNodes = [
    ...elements.transcriptContent.querySelectorAll("[data-running-started]"),
  ];
  syncUiAnimationScheduler();
}

function refreshUiAnimation() {
  state.uiAnimationTimer = null;
  if (!uiAnimationNeeded()) {
    syncUiAnimationScheduler();
    return;
  }
  if (!inputHasPriority()) {
    if (apiSpinnerIsActive()) {
      state.apiAnimationTick += 1;
      setApiSpinner(API_SPINNER_FRAMES[state.apiAnimationTick % API_SPINNER_FRAMES.length]);
    } else {
      setApiSpinner("");
    }
    const now = Date.now();
    state.runningToolNodes = state.runningToolNodes.filter((node) => {
      if (node.isConnected === false || node.dataset.runningStarted == null) return false;
      const text = `Running ... ${formatDuration(now - Number(node.dataset.runningStarted))}`;
      if (node.textContent !== text) node.textContent = text;
      return true;
    });
  }
  syncUiAnimationScheduler();
}
if (runtimeCapabilities.multipleWorkspaces) {
  setInterval(() => {
    if (state.authenticated && !state.pageClosing) void refreshGatewayState();
  }, 1500);
}

initializeAuthentication();
