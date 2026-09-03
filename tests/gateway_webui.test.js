"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

require("../src/webui/edb-cache.js");
globalThis.MeMarkdown = require("../src/webui/markdown.js");

globalThis.MeFrontendRuntime = {
  capabilities: { multipleWorkspaces: true, gatewaySettings: true },
  apiPath(path, workspaceId = "chat") {
    const child = path === "/api/sync" || path === "/api/snapshot" || path === "/api/command"
      || path.startsWith("/api/deletion-blocker/") || path.startsWith("/api/session-terminal/")
      || path.startsWith("/api/remote-control/") || path.startsWith("/api/files/");
    return child ? `/api/workspaces/${workspaceId}${path.slice(4)}` : path;
  },
  createEdbCache() {
    return {
      loadScope: async () => [], loadScopeMetadata: async () => [], loadSession: async () => null,
      discardSession: async () => {}, saveSession() {}, renderManager() {},
    };
  },
  loadCachedSessions(cache, _snapshot, scope) { return cache.loadScope(scope); },
  loadCachedSessionMetadata(cache, _snapshot, scope) { return cache.loadScopeMetadata(scope); },
  loadCachedSession(cache, _snapshot, scope, agentId) { return cache.loadSession(`${scope}::${agentId}`); },
  cacheKey(scope, agentId) { return `${scope}::${agentId}`; },
  persistSelection() { return Promise.resolve(); },
  loadGatewayState() { return Promise.resolve({ workspaces: [] }); },
  get endpoint() { return ""; },
};

function loadToolPresenters() {
  const source = readFileSync(join(import.meta.dir, "../src/webui/tool-presenters.js"), "utf8");
  new Function(source)();
  return globalThis.MeToolPresenters;
}

function loadRuntime(relative) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error(`could not isolate ${relative}`);
  const factory = new Function("document", "performance", "matchMedia", "MeTranscript", "MeToolPresenters", `${source.slice(0, eventBindings)}
    return { state, emptyProjection, projectChat, consumeChatEvents, chatAppendNeedsReplay,
      projectAgentSummary, updateAgentSummary, sidebarAgentActive,
      emptyWorkMap, projectWorkMap, consumeWorkMapEvents, apiPath: frontendRuntime.apiPath,
      eventRecoveryBacklog, shouldUseBulkEventRecovery, createEventRecovery, eventRecoveryProgress,
      eventRecoveryMatches, selectedEventRecoveryReady, httpSyncProgressSignature, isIosWebKit,
      createAgentLoadProgress: typeof createAgentLoadProgress === "function" ? createAgentLoadProgress : null,
      prepareAgentLoadProgress: typeof prepareAgentLoadProgress === "function" ? prepareAgentLoadProgress : null,
      settleAgentLoadProgress: typeof settleAgentLoadProgress === "function" ? settleAgentLoadProgress : null,
      agentLoadingState: typeof agentLoadingState === "function" ? agentLoadingState : null,
      sessionSelectionAllowed: typeof sessionSelectionAllowed === "function" ? sessionSelectionAllowed : null,
      workspaceMetadataReady: typeof workspaceMetadataReady === "function" ? workspaceMetadataReady : null,
      applyGatewayStartupMetadata: typeof applyGatewayStartupMetadata === "function" ? applyGatewayStartupMetadata : null,
      emptyGatewayWorkspaceState: typeof emptyGatewayWorkspaceState === "function" ? emptyGatewayWorkspaceState : null,
      gatewayWorkspaceState: typeof gatewayWorkspaceState === "function" ? gatewayWorkspaceState : null,
      createAgentStore: typeof createAgentStore === "function" ? createAgentStore : null,
      releaseMaterializedStore: typeof releaseMaterializedStore === "function" ? releaseMaterializedStore : null,
      readPartialLoadingPreference: typeof readPartialLoadingPreference === "function" ? readPartialLoadingPreference : null,
      normalizeWindowBorderStyle: typeof normalizeWindowBorderStyle === "function" ? normalizeWindowBorderStyle : null,
      localPreferenceSettingsHtml: typeof localPreferenceSettingsHtml === "function" ? localPreferenceSettingsHtml : null,
      backgroundSyncRequestBody: typeof backgroundSyncRequestBody === "function" ? backgroundSyncRequestBody : null,
      backgroundSyncCanRun: typeof backgroundSyncCanRun === "function" ? backgroundSyncCanRun : null,
      nextBackgroundWorkspace: typeof nextBackgroundWorkspace === "function" ? nextBackgroundWorkspace : null,
      applyBackgroundSyncState: typeof applyBackgroundSyncState === "function" ? applyBackgroundSyncState : null,
      readWorkspaceDisclosure: typeof readWorkspaceDisclosure === "function" ? readWorkspaceDisclosure : null,
      persistWorkspaceDisclosure: typeof persistWorkspaceDisclosure === "function" ? persistWorkspaceDisclosure : null,
      workspaceExpanded: typeof workspaceExpanded === "function" ? workspaceExpanded : null,
      setWorkspaceExpanded: typeof setWorkspaceExpanded === "function" ? setWorkspaceExpanded : null,
      pruneWorkspaceDisclosure: typeof pruneWorkspaceDisclosure === "function" ? pruneWorkspaceDisclosure : null,
      resolveEditedDefaultModel: typeof resolveEditedDefaultModel === "function" ? resolveEditedDefaultModel : null,
      blankGatewayModel: typeof blankGatewayModel === "function" ? blankGatewayModel : null,
      modelSettingsHtml: typeof modelSettingsHtml === "function" ? modelSettingsHtml : null,
      persistGatewaySelection: typeof persistGatewaySelection === "function" ? persistGatewaySelection : null,
      directoryParentRequest: typeof directoryParentRequest === "function" ? directoryParentRequest : null,
      directoryEntryType: typeof directoryEntryType === "function" ? directoryEntryType : null,
      formatDirectorySize: typeof formatDirectorySize === "function" ? formatDirectorySize : null,
      formatDirectoryModified: typeof formatDirectoryModified === "function" ? formatDirectoryModified : null,
      filterDirectoryEntries: typeof filterDirectoryEntries === "function" ? filterDirectoryEntries : null,
      sortDirectoryEntries: typeof sortDirectoryEntries === "function" ? sortDirectoryEntries : null,
      assistantContentHasRenderableContent: typeof assistantContentHasRenderableContent === "function" ? assistantContentHasRenderableContent : null,
      messageIsVisible: typeof messageIsVisible === "function" ? messageIsVisible : null,
      renderMessageHtml: typeof renderMessageHtml === "function" ? renderMessageHtml : null,
      renderTranscript: typeof renderTranscript === "function" ? renderTranscript : null,
      emptyObjectiveDisclosure: typeof emptyObjectiveDisclosure === "function" ? emptyObjectiveDisclosure : null,
      syncObjectiveDisclosure: typeof syncObjectiveDisclosure === "function" ? syncObjectiveDisclosure : null,
      objectiveSummaryHtml: typeof objectiveSummaryHtml === "function" ? objectiveSummaryHtml : null,
      objectiveDisclosureAttributes: typeof objectiveDisclosureAttributes === "function" ? objectiveDisclosureAttributes : null,
      objectiveEventActivates: typeof objectiveEventActivates === "function" ? objectiveEventActivates : null,
      bindSidebarScrollbar: typeof bindSidebarScrollbar === "function" ? bindSidebarScrollbar : null,
      portScopedCookieName: typeof portScopedCookieName === "function" ? portScopedCookieName : null,
      SEND_SHORTCUT_COOKIE: typeof SEND_SHORTCUT_COOKIE === "string" ? SEND_SHORTCUT_COOKIE : null,
      readSendShortcutCookie: typeof readSendShortcutCookie === "function" ? readSendShortcutCookie : null,
      latestSystemPromptState: typeof latestSystemPromptState === "function" ? latestSystemPromptState : null,
      systemPromptChangeMatches: typeof systemPromptChangeMatches === "function" ? systemPromptChangeMatches : null,
      systemPromptContentBytes: typeof systemPromptContentBytes === "function" ? systemPromptContentBytes : null,
      systemPromptEditorState: typeof systemPromptEditorState === "function" ? systemPromptEditorState : null,
      isChatbotAgent: typeof isChatbotAgent === "function" ? isChatbotAgent : null,
      elements: typeof elements === "object" ? elements : null };
  `);
  const runtime = factory(
    {
      querySelector: () => null, cookie: "", location: { protocol: "http:", port: "38199" },
      documentElement: { classList: { toggle() {} }, dataset: {} },
    },
    { now: () => 0 },
    () => ({ matches: false, addEventListener() {} }),
    { reconcileHtmlChildren(container, html) { container.innerHTML = html; } },
    loadToolPresenters(),
  );
  runtime.state.snapshot.tool_visibility = {
    hidden_names: ["SetTitle"], hidden_prefixes: ["WorkMap.", "Worker."], activity_names: ["Worker.Wait"],
  };
  return runtime;
}

function loadFrontendAdapter(relative) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const sandbox = {
    MeEdbCache: {
      create() { return {}; },
      sessionKey(scope, agentId) { return `${scope}::${agentId}`; },
    },
  };
  const documentValue = { documentElement: { classList: { add() {} } } };
  new Function("globalThis", "document", source)(sandbox, documentValue);
  return sandbox.MeFrontendRuntime;
}

function event(kind, id, value = {}) {
  return { [kind]: { id, timestamp_ms: id * 10, ...value } };
}

function visibleProjection(projection) {
  return {
    messages: projection.messages,
    apiState: projection.apiState,
    apiUsage: projection.apiUsage,
    model: projection.model,
    effort: projection.effort,
    turnState: projection.turnState,
  };
}

const fixture = [
  event("ModelChanged", 1, { model: "model-a", cause: "Initial" }),
  event("ReasoningEffortChanged", 2, { effort: "high", cause: "Initial" }),
  event("UserPrompt", 3, { content: "hello" }),
  event("ApiStateUpdate", 4, { api_call_id: "api-1", prompt_id: 3, state: "Requesting" }),
  event("AssistResponse", 5, { prompt_id: 3, content: "I will check.\n", finished: false }),
  event("ToolCall", 6, { id: 6, api_call_id: "api-1", prompt_id: 3, name: "Terminal.Create", arguments: "{}" }),
  event("ToolInfoUpdate", 7, { tool_call_id: 6, content: { kind: "text", value: "ready\n" } }),
  event("ToolCallResult", 8, { tool_call_id: 6, state: "Succeeded", exit_code: 0, detail: "{}" }),
  event("AssistResponse", 9, { prompt_id: 3, content: "done", finished: true }),
  event("ApiStateUpdate", 10, { api_call_id: "api-1", prompt_id: 3, state: "Completed", usage: { input_tokens: 20, output_tokens: 8, total_tokens: 28 } }),
  event("AgentTurn", 11, { turn_id: 1, prompt_id: 3, state: "Completed" }),
  event("WorkMapMutation", 12, { mutation: { records: [
    { kind: "objective", record: { id: "objective-1", title: "Ship", state: "active", created_at_ms: 1 } },
    { kind: "plan", record: { id: "plan-1", objective_id: "objective-1", title: "Build", state: "active", order: 0 } },
  ] } }),
];

describe("ME Gateway WebUI semantic compatibility", () => {
  test("scopes send shortcut cookies to each WebUI page port", () => {
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      expect(runtime.SEND_SHORTCUT_COOKIE).toBe("me_send_shortcut_p38199");
      expect(runtime.portScopedCookieName("me_send_shortcut", { protocol: "http:", port: "38201" }))
        .toBe("me_send_shortcut_p38201");
      expect(runtime.portScopedCookieName("me_send_shortcut", { protocol: "https:", port: "" }))
        .toBe("me_send_shortcut_p443");
      expect(runtime.readSendShortcutCookie("me_send_shortcut_p38201=enter"))
        .toBe("modified-enter");
      const source = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(source).toContain("Max-Age=31536000; Path=/; SameSite=Lax");
    }
  });

  test("projects the same EDB fixture deterministically from the shared core", () => {
    const first = loadRuntime("../src/webui/app.js");
    const second = loadRuntime("../src/webui/app.js");
    expect(visibleProjection(second.projectChat(fixture)))
      .toEqual(visibleProjection(first.projectChat(fixture)));
    const secondWorkMap = second.projectWorkMap(fixture);
    const firstWorkMap = first.projectWorkMap(fixture);
    expect({ ...secondWorkMap, _records: undefined })
      .toEqual({ ...firstWorkMap, _records: undefined });
  });

  test("projects complete Chatbot prompt-change notices symmetrically", () => {
    const events = [
      event("SystemStaticPromptChange", 1, { mode: "Custom", content: "# Persona\n\n完整多行内容。" }),
      event("SystemStaticPromptChange", 2, { mode: "Default", content: null }),
    ];
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      expect(runtime.projectChat(events).messages.map((message) => message.content)).toEqual([
        "系统提示词已更新\n# Persona\n\n完整多行内容。",
        "系统提示词已恢复默认",
      ]);
    }
  });

  test("derives saved prompt state from raw EDB while preserving dirty local drafts", () => {
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      runtime.state.snapshot.chatbot_default_static_prompt = "内置默认提示";
      runtime.state.snapshot.agents = [{ id: "chat", orchestrator: "chatbot" }];
      runtime.state.selectedAgent = "chat";
      runtime.state.stores.set("chat", { events: [] });
      expect(runtime.latestSystemPromptState("chat")).toEqual({
        mode: "Default", content: "内置默认提示", eventId: null,
      });
      runtime.state.stores.get("chat").events.push(
        event("SystemStaticPromptChange", 5, { mode: "Custom", content: "custom-one" }),
      );
      expect(runtime.latestSystemPromptState("chat")).toEqual({
        mode: "Custom", content: "custom-one", eventId: 5,
      });
      const editor = runtime.systemPromptEditorState("chat");
      editor.draft.content = "本页尚未应用的草稿";
      editor.draft.dirty = true;
      runtime.state.stores.get("chat").events.push(
        event("SystemStaticPromptChange", 8, { mode: "Custom", content: "remote-custom" }),
      );
      expect(runtime.systemPromptEditorState("chat").draft.content).toBe("本页尚未应用的草稿");
      runtime.state.stores.get("chat").events.push(
        event("SystemStaticPromptChange", 9, { mode: "Default", content: null }),
      );
      expect(runtime.latestSystemPromptState("chat")).toEqual({
        mode: "Default", content: "内置默认提示", eventId: 9,
      });
      runtime.state.stores.get("chat").events.pop();
      expect(runtime.latestSystemPromptState("chat").content).toBe("remote-custom");
      expect(runtime.systemPromptChangeMatches(
        { mode: "Custom", content: "精确内容" },
        { mode: "Custom", content: "精确内容" },
      )).toBe(true);
      expect(runtime.systemPromptContentBytes("你")).toBe(3);
    }
  });

  test("keeps Chatbot prompt drafts page-local and Workspace-plus-Agent isolated", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const first = gateway.emptyGatewayWorkspaceState();
    const second = gateway.emptyGatewayWorkspaceState();
    first.promptDrafts.set("main", { content: "workspace-one" });
    second.promptDrafts.set("main", { content: "workspace-two" });
    expect(first.promptDrafts).not.toBe(second.promptDrafts);
    expect(first.promptDrafts.get("main").content).toBe("workspace-one");
    expect(second.promptDrafts.get("main").content).toBe("workspace-two");
  });

  test("keeps Chatbot tabs, authoritative EDB confirmation, and editor styling aligned", () => {
    const paths = [
      ["../src/webui/index.html", "../src/webui/app.js", "../src/webui/style.css"],
    ];
    for (const [htmlPath, appPath, stylePath] of paths) {
      const html = readFileSync(join(import.meta.dir, htmlPath), "utf8");
      const app = readFileSync(join(import.meta.dir, appPath), "utf8");
      const style = readFileSync(join(import.meta.dir, stylePath), "utf8");
      expect(html).toContain('data-view="system-prompt" data-chatbot-only');
      expect(html).toContain('data-view="workmap" data-work-only');
      expect(html).toContain('id="system-prompt-input"');
      expect(html).toContain("设置助手的角色、语气和回答方式。");
      expect(html).toContain('aria-label="系统提示词内容"');
      expect(html).toContain('id="system-prompt-status">已应用</span>');
      expect(html).not.toContain("固定编排协议");
      expect(html).not.toContain("工具契约");
      expect(html).not.toContain("已与 EDB 同步");
      expect(app).toContain("暂时无法确认是否应用成功，请稍候…");
      expect(app).toContain("正在应用更改…");
      expect(app).toContain("内容过长，请适当精简");
      expect(app).not.toContain("命令结果未知，正在等待 EDB 确认…");
      expect(app).not.toContain("命令已接受，正在等待 EDB 确认…");
      expect(app).toContain('querySelectorAll("[data-work-only]")');
      expect(app).toContain('command: "change_system_static_prompt"');
      expect(app).toContain("Number(value.id) > draft.pending.afterEventId");
      expect(app).toContain("if (confirmation) {");
      expect(app).toContain("系统提示词已更新");
      expect(app).toContain("系统提示词已恢复默认");
      expect(style).toContain(".system-prompt-panel {");
      expect(style).toContain(".system-prompt-actions > span[data-state=\"pending\"]");
    }
    const gatewayApp = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(gatewayApp).toContain("promptDrafts: state.promptDrafts");
    expect(gatewayApp).toContain("state.promptDrafts = workspace.promptDrafts;");
  });

  test("hides non-rendering Assistant-only content in both WebUIs without changing real content", () => {
    const invisibleContents = [
      "",
      " \t\r\n",
      "\u0000\u0008\u001f\u007f",
      "\u200b",
      "\u200b\u200b\u200b",
      "\u034f\u200c\u200d\u200e\u202a\u2060\u2066\u2069\ufeff",
      "\ufe0f\u{e0100}",
    ];
    const preservedContents = [
      "\u200b正文",
      "👩‍💻",
      "❤️",
      "می‌خواهم",
    ];
    const visualMarkdown = [
      ["---", "<hr>"],
      ["![](https://example.com/image.png)", "<img"],
      ["$$x^2$$", 'class="math-display"'],
    ];

    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      for (const content of invisibleContents) {
        const message = { kind: "assistant", content };
        expect(runtime.assistantContentHasRenderableContent(content)).toBe(false);
        expect(runtime.messageIsVisible(message)).toBe(false);
        const html = runtime.renderMessageHtml(message, false, false);
        expect(html).toContain('class="message-block projection-hidden hidden"');
        expect(html).not.toContain("block-marker");
      }

      const css = readFileSync(join(import.meta.dir, relative.replace("app.js", "style.css")), "utf8");
      expect(css).toContain(".hidden { display: none !important; }");

      for (const content of preservedContents) {
        const message = { kind: "assistant", content };
        expect(runtime.assistantContentHasRenderableContent(content)).toBe(true);
        expect(runtime.messageIsVisible(message)).toBe(true);
        expect(runtime.renderMessageHtml(message, false, false)).toContain(content);
        expect(message.content).toBe(content);
      }

      for (const [content, expectedHtml] of visualMarkdown) {
        const message = { kind: "assistant", content };
        expect(runtime.messageIsVisible(message)).toBe(true);
        expect(runtime.renderMessageHtml(message, false, false)).toContain(expectedHtml);
      }

      const historicalFinal = runtime.projectChat([
        event("UserPrompt", 1, { content: "old prompt" }),
        event("AssistResponse", 2, { prompt_id: 1, content: "\u200b", finished: true }),
        event("AgentTurn", 3, { turn_id: 1, prompt_id: 1, state: "Completed" }),
      ]);
      expect(historicalFinal.messages.map((message) => message.kind)).toEqual(["user", "assistant"]);
      expect(historicalFinal.messages[1].content).toBe("\u200b");
      expect(runtime.renderMessageHtml(historicalFinal.messages[1], false, false)).not.toContain("block-marker");

      const toolLoopReplay = runtime.projectChat([
        event("UserPrompt", 1, { content: "old tool loop" }),
        event("AssistResponse", 2, { prompt_id: 1, content: "\u200b", finished: true }),
        event("ToolCall", 3, { id: 3, api_call_id: "api-1", prompt_id: 1, name: "Terminal.Create", arguments: "{}" }),
        event("ToolCallResult", 4, { tool_call_id: 3, state: "Succeeded", detail: "{}" }),
        event("AssistResponse", 5, { prompt_id: 1, content: "\u200b正文", finished: true }),
        event("AgentTurn", 6, { turn_id: 1, prompt_id: 1, state: "Completed" }),
      ]);
      expect(toolLoopReplay.messages.map((message) => message.kind))
        .toEqual(["user", "assistant", "tool", "assistant", "turn-toolbar"]);
      expect(toolLoopReplay.messages.filter(runtime.messageIsVisible).map((message) => message.kind))
        .toEqual(["user", "tool", "assistant", "turn-toolbar"]);
      expect(toolLoopReplay.messages[1].content).toBe("\u200b");
      expect(toolLoopReplay.messages[3].content).toBe("\u200b正文");
    }
  });

  test("keeps sidebar activity open across API loops until the Agent turn closes", () => {
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      const summary = runtime.projectAgentSummary([
        event("AgentTurn", 1, { turn_id: 1, prompt_id: 1, state: "Started" }),
        event("ApiStateUpdate", 2, { api_call_id: "api-1", prompt_id: 1, state: "Requesting" }),
      ]);
      expect(summary).toEqual({ turnState: "Started" });
      expect(runtime.sidebarAgentActive(summary)).toBe(true);

      runtime.updateAgentSummary(summary, [
        event("ApiStateUpdate", 3, { api_call_id: "api-1", prompt_id: 1, state: "Completed" }),
        event("ApiStateUpdate", 4, { api_call_id: "api-2", prompt_id: 1, state: "Requesting" }),
        event("ApiStateUpdate", 5, { api_call_id: "api-2", prompt_id: 1, state: "Error" }),
      ]);
      expect(summary).toEqual({ turnState: "Started" });
      expect(runtime.sidebarAgentActive(summary)).toBe(true);

      for (const stateName of ["Completed", "Interrupted", "Failed"]) {
        const terminal = { ...summary };
        runtime.updateAgentSummary(terminal, [
          event("AgentTurn", 6, { turn_id: 1, prompt_id: 1, state: stateName }),
        ]);
        expect(runtime.sidebarAgentActive(terminal)).toBe(false);
      }
    }
  });

  test("keeps bulk recovery current-session scoped and exposes progress on its session row", () => {
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      expect(runtime.shouldUseBulkEventRecovery(99, 0)).toBe(false);
      expect(runtime.shouldUseBulkEventRecovery(100, 0)).toBe(false);
      expect(runtime.shouldUseBulkEventRecovery(101, 0)).toBe(true);
      const recovery = runtime.createEventRecovery("main", 4, 101, 0);
      expect(runtime.eventRecoveryProgress(recovery, 0)).toBe(0);
      expect(runtime.eventRecoveryProgress(recovery, 50)).toBe(50 / 101);
      expect(runtime.eventRecoveryProgress(recovery, 101)).toBe(1);
      expect(runtime.selectedEventRecoveryReady(recovery, "main", 4, 100)).toBe(false);
      expect(runtime.selectedEventRecoveryReady(recovery, "main", 4, 101)).toBe(true);
      expect(runtime.selectedEventRecoveryReady(recovery, "main", 4, 140)).toBe(true);
      expect(runtime.eventRecoveryMatches(recovery, "other", 4)).toBe(false);
      expect(runtime.eventRecoveryMatches(recovery, "main", 5)).toBe(false);

      const source = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(source).toContain("delete elements.terminalScreen.dataset.revision;\n  restoreDraft();\n  const materializing = startSelectedAgentMaterialization(state.workspaceId, id, true);\n  const meta =");
      expect(source).toContain('const startingRecoveryCycle = phaseBefore === "initial" || phaseBefore === "reconnecting";');
      expect(source).toContain("startingRecoveryCycle || selectionChanged || Boolean(selectedUpdate?.reset)");
      expect(source).toContain("const recoveryReady = responseMatchesSelection && selectedEventRecoveryReady(");
      expect(source).toContain("const bulkRecoveryPending = bulkEventRecoveryActive();");
      expect(source).toContain("currentEvents: !bulkRecoveryPending && !forceRecoveredReplay && selectedEventsChanged");
      expect(source).toContain("workerEvents: !bulkRecoveryPending && !forceRecoveredReplay && selectedWorkerChanged");
      expect(source).toContain("if (bulkRecoveryPending) suppressBulkEventRecoveryRender();");
      expect(source).toContain(`if (recoveryReady) {
    store.projectedOrder = 0;
    store.needsReplay = true;
    state.eventRecovery = null;
    forceRecoveredReplay = true;
  }`);
      expect(source).toContain("full: !bulkRecoveryPending && (forceRecoveredReplay || startingRecoveryCycle || selectionChanged)");
      expect(source).toContain("if (forceRecoveredReplay || recoveryTransitionedToIncremental) flushPendingRender();");
      expect(source).toContain("if (bulkEventRecoveryActive()) return emptyProjectionChanges();");
      expect(source).toContain("loadProgress: createAgentLoadProgress(meta, eventCount, mutationRevision)");
      expect(source).toContain("percent: Math.floor(eventRecoveryProgress(store.loadProgress, store.eventCount) * 100)");
      expect(source).not.toContain("showEventRecoveryOverlay");
      expect(source).not.toContain("eventRecoveryProgressFill");
    }
    const gateway = loadRuntime("../src/webui/app.js");
    const first = gateway.emptyGatewayWorkspaceState();
    const second = gateway.emptyGatewayWorkspaceState();
    first.eventRecovery = { agentId: "main", mutationRevision: 1, startEventCount: 0, targetEventCount: 101 };
    expect(second.eventRecovery).toBeNull();
    const gatewaySource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(gatewaySource).toContain("eventRecovery: state.eventRecovery");
    expect(gatewaySource).toContain("state.eventRecovery = workspace.eventRecovery;");
  });


  test("keeps draft, message, paint, and connection stability policies aligned across both WebUIs", () => {
    const sources = [
      readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8"),
    ];
    for (const source of sources) {
      expect(source).toContain("const DRAFT_BATCH_MS = 80;");
      expect(source).toContain("batchTimer: null");
      expect(source).toContain("refresh: false");
      expect(source).toContain('inputMirror: $("#prompt-input-mirror")');
      expect(source).toContain("elements.inputMirror.scrollHeight");
      expect(source).not.toContain("elements.input.scrollHeight");
      expect(source).not.toContain("function inputChangeCanShrink");
      expect(source).not.toContain('elements.input.style.height = "auto"');
      expect(source).toContain("if (state.inputHeight !== target)");
      expect(source).toContain("function refreshRunningToolNodes()");
      expect(source).toContain("state.uiAnimationTimer = setTimeout(refreshUiAnimation, UI_ANIMATION_INTERVAL_MS)");
      expect(source).toContain('document.addEventListener("visibilitychange"');
      expect(source).not.toContain("setInterval(refreshRunningToolElapsed");
      expect(source).toContain("scheduleHttpSync(message.more_events && madeProgress ? 0 : delay)");
      const objectiveToggleStart = source.indexOf("function toggleObjectiveDisclosure");
      const objectiveToggleEnd = source.indexOf("\nfunction renderWorkMap", objectiveToggleStart);
      expect(source.slice(objectiveToggleStart, objectiveToggleEnd))
        .not.toContain("transcriptBottomFollower.layoutChanged");
      expect(source).toContain('message.kind === "notice" || message.kind === "session"');
      expect(source).toContain("content.textContent = message.content;");
      expect(source).toContain("const CONNECTION_DEGRADED_GRACE_MS = 2000;");
      expect(source).toContain("const CONNECTION_STABILIZE_MS = 1000;");
      expect(source).toContain("const CONNECTION_STABILIZE_SUCCESSES = 2;");
      for (const phase of ["degraded", "reconnecting", "stabilizing"]) {
        expect(source).toContain(`"${phase}"`);
      }
      expect(source).toContain('if (state.connectionOverlayMode === "connection") return;');
      expect(source).toContain('if (state.connectionOverlayMode === "hidden") return;');
    }

    for (const relative of ["../src/webui/style.css"]) {
      const style = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(style).toContain(".transcript-window > .message-block, .transcript-window > .tool-card { content-visibility: visible;");
      expect(style).toContain(".message-block { contain: layout paint style; content-visibility: auto;");
      expect(style).toContain(".tool-card { contain: layout paint style; content-visibility: auto;");
      expect(style).toContain(".prompt-input-mirror { position: absolute;");
      expect(style).toContain("contain: layout paint style;");
      expect(style).toContain(".objective-summary { position: relative;");
      expect(style).toContain(".objective-details { position: absolute;");
      expect(style).toContain("bottom: calc(100% + 6px)");
      expect(style).toContain(".ios-webkit .transcript-window > .message-block");
    }

    const transcript = readFileSync(join(import.meta.dir, "../src/webui/transcript.js"), "utf8");
    expect(transcript).toContain("let committedScrollHeight = viewport.scrollHeight;");
    expect(transcript).toContain("scrollHeight !== committedScrollHeight");
    expect(transcript).toContain("const applyFollowNow = (force = forcing)");
    expect(transcript).toContain('style.setProperty("overflow", "hidden", "important")');
  });

  test("detects iOS and iPadOS WebKit without affecting Android or desktop macOS", () => {
    const cases = [
      [{ userAgent: "Mozilla/5.0 (iPhone) AppleWebKit/605.1.15 CriOS/125", platform: "iPhone", maxTouchPoints: 5 }, true],
      [{ userAgent: "Mozilla/5.0 (iPad) AppleWebKit/605.1.15 Safari/604.1", platform: "iPad", maxTouchPoints: 5 }, true],
      [{ userAgent: "Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15 Safari/604.1", platform: "MacIntel", maxTouchPoints: 5 }, true],
      [{ userAgent: "Mozilla/5.0 (Macintosh) AppleWebKit/537.36 Chrome/125", platform: "MacIntel", maxTouchPoints: 0 }, false],
      [{ userAgent: "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/125", platform: "Linux armv8l", maxTouchPoints: 5 }, false],
      [{ userAgent: "Mozilla/5.0 (iPhone) Gecko/20100101 Firefox/125", platform: "iPhone", maxTouchPoints: 5 }, false],
    ];
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      for (const [navigatorValue, expected] of cases) {
        expect(runtime.isIosWebKit(navigatorValue)).toBe(expected);
      }
    }
  });

  test("keeps zero-delay event catch-up conditional on a changed sync cursor", () => {
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      runtime.state.snapshotInitialized = true;
      runtime.state.snapshot.revision = 3;
      runtime.state.selectedAgent = "main";
      runtime.state.stores.set("main", { events: [], mutationRevision: 7 });
      const initial = runtime.httpSyncProgressSignature();
      expect(runtime.httpSyncProgressSignature()).toBe(initial);
      runtime.state.stores.get("main").events.push({});
      expect(runtime.httpSyncProgressSignature()).not.toBe(initial);

      const source = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(source).toContain("const madeProgress = progressBefore !== httpSyncProgressSignature()");
      expect(source).toContain("|| message.more_events || state.apiActivity.active");
      expect(source).toContain("scheduleHttpSync(message.more_events && madeProgress ? 0 : delay)");
    }
  });
  test("closes the portrait sidebar before selecting any Gateway session", () => {
    const gatewaySource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(gatewaySource).toContain(`function selectWorkspaceAgent(workspaceId, agentId) {
  if (!sessionSelectionAllowed(workspaceId, agentId)) return;
  closeMobileSidebar();
  if (state.workspaceId !== workspaceId) activateWorkspace(workspaceId, agentId);
  else selectAgent(agentId);
}`);
    expect(gatewaySource).toContain(`function closeMobileSidebar() {
  document.body.classList.remove("mobile-sidebar-open");
  elements.mobileSidebarToggle.setAttribute("aria-expanded", "false");
  closeAgentMenu();
}`);
  });

  test("keeps Objective details scoped while the whole card is the single accessible control", () => {
    const objectiveTitleRule =
      ".objective-title { min-width: 0; flex: 1; overflow-wrap: anywhere; font-weight: 400; }";
    for (const relative of ["../src/webui/style.css"]) {
      const style = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(style).toContain(objectiveTitleRule);
    }

    const current = {
      objective: { id: "objective-1", title: "Ship safely", description: "Release details" },
      plans: [{ plan: { id: "plan-1", title: "Build", state: "active" }, notes: [{}] }],
    };
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      const disclosure = runtime.emptyObjectiveDisclosure();
      expect(disclosure).toEqual({ scopeId: null, objectiveId: null, expanded: false });
      runtime.syncObjectiveDisclosure(disclosure, "workspace-a:agent-a", "objective-1");
      const collapsed = runtime.objectiveSummaryHtml(current, disclosure.expanded);
      expect(collapsed).toContain("Ship safely");
      expect(collapsed).not.toContain("Release details");
      expect(collapsed).not.toContain(">Build<");
      expect(collapsed).not.toContain("<button");
      expect(collapsed).toContain('class="objective-toggle" aria-hidden="true"');
      expect(collapsed).toContain('id="objective-details" class="objective-details hidden"');
      expect(runtime.objectiveDisclosureAttributes(false)).toEqual({
        role: "button", tabindex: "0", "aria-expanded": "false", "aria-controls": "objective-details",
        "aria-label": "展开 Objective 详情", title: "展开 Objective 详情",
      });

      const objective = {};
      for (const area of ["title", "status", "blank", "description", "plan", "icon"]) {
        const target = { area, closest: () => objective };
        expect(runtime.objectiveEventActivates({ type: "click", target }, objective)).toBe(true);
      }
      const keyboardTarget = { closest: () => objective };
      expect(runtime.objectiveEventActivates({ type: "keydown", key: "Enter", target: keyboardTarget }, objective)).toBe(true);
      expect(runtime.objectiveEventActivates({ type: "keydown", key: " ", target: keyboardTarget }, objective)).toBe(true);
      expect(runtime.objectiveEventActivates({ type: "keydown", key: "ArrowDown", target: keyboardTarget }, objective)).toBe(false);
      const independentControl = {};
      expect(runtime.objectiveEventActivates({
        type: "click", target: { closest: () => independentControl },
      }, objective)).toBe(false);

      disclosure.expanded = true;
      runtime.syncObjectiveDisclosure(disclosure, "workspace-a:agent-a", "objective-1");
      expect(disclosure.expanded).toBe(true);
      const expanded = runtime.objectiveSummaryHtml(current, disclosure.expanded);
      expect(expanded).toContain("Release details");
      expect(expanded).toContain(">Build");
      expect(expanded).toContain("(1 note)");
      expect(runtime.objectiveDisclosureAttributes(true)).toEqual({
        role: "button", tabindex: "0", "aria-expanded": "true", "aria-controls": "objective-details",
        "aria-label": "折叠 Objective 详情", title: "折叠 Objective 详情",
      });

      current.plans[0].plan.state = "completed";
      runtime.syncObjectiveDisclosure(disclosure, "workspace-a:agent-a", "objective-1");
      expect(disclosure.expanded).toBe(true);
      runtime.syncObjectiveDisclosure(disclosure, "workspace-a:agent-b", "objective-1");
      expect(disclosure.expanded).toBe(false);
      disclosure.expanded = true;
      runtime.syncObjectiveDisclosure(disclosure, "workspace-a:agent-b", "objective-2");
      expect(disclosure.expanded).toBe(false);
      disclosure.expanded = true;
      runtime.syncObjectiveDisclosure(disclosure, null, null);
      expect(disclosure.expanded).toBe(false);
      runtime.syncObjectiveDisclosure(disclosure, "workspace-a:agent-b", "objective-3");
      expect(disclosure.expanded).toBe(false);

      const source = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(source).not.toMatch(/(?:localStorage|sessionStorage|document\.cookie)[^\n]*objectiveDisclosure/);
      expect(source).not.toContain("data-objective-toggle");
      expect(source).toContain('elements.objective.addEventListener("click", toggleObjectiveDisclosure)');
      expect(source).toContain('elements.objective.addEventListener("keydown", toggleObjectiveDisclosure)');
      expect(source).toContain('if (event.type === "keydown") event.preventDefault();');
      expect(source.match(/state\.objectiveDisclosure\.expanded = !state\.objectiveDisclosure\.expanded;/g)).toHaveLength(1);
    }
    const sharedSource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(sharedSource).toContain("JSON.stringify([state.workspaceId, state.selectedAgent])");
    for (const stylePath of ["../src/webui/style.css"]) {
      const styles = readFileSync(join(import.meta.dir, stylePath), "utf8");
      expect(styles).toContain(".objective-summary:focus-visible");
      expect(styles).toContain(".objective-summary:hover");
      expect(styles).toContain("pointer-events: none;");
    }
  });

  test("routes child APIs in adapters and allocates independent Workspace stores", () => {
    const shared = loadRuntime("../src/webui/app.js");
    const direct = loadFrontendAdapter("../src/webui/runtime.js");
    const gateway = loadFrontendAdapter("../src/gateway_webui/runtime.js");
    expect(direct.apiPath("/api/sync", "w-one")).toBe("/api/sync");
    expect(gateway.apiPath("/api/sync", "w-one")).toBe("/api/workspaces/w-one/sync");
    expect(gateway.apiPath("/api/session-terminal/main/read", "w-one"))
      .toBe("/api/workspaces/w-one/session-terminal/main/read");
    expect(gateway.apiPath("/api/auth/status", "w-one")).toBe("/api/auth/status");
    const first = shared.emptyGatewayWorkspaceState();
    const second = shared.emptyGatewayWorkspaceState();
    first.stores.set("main", { marker: 1 });
    first.drafts.set("main", "workspace one");
    expect(second.stores.has("main")).toBe(false);
    expect(second.drafts.has("main")).toBe(false);
    expect(first.terminalFrames).not.toBe(second.terminalFrames);
    expect(first.workerActivityIndexes).not.toBe(second.workerActivityIndexes);
  });

  test("background-syncs every inactive Workspace while releasing partial-mode raw Events", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    gateway.state.connectionPhase = "connected";
    gateway.state.activeCatchUpPending = false;
    expect(gateway.backgroundSyncCanRun()).toBe(true);
    gateway.state.activeCatchUpPending = true;
    expect(gateway.backgroundSyncCanRun()).toBe(false);
    gateway.state.activeCatchUpPending = false;

    const meta = {
      id: "main", title: "Main", kind: "primary", parent_agent_id: null, orchestrator: "main-agent",
      event_count: 1, mutation_revision: 0, prompt_submission_revision: 2,
      input_draft: "remote draft", input_draft_revision: 3,
    };
    const snapshot = {
      revision: 4, environment: { workspace: "/workspace-one" }, agents: [meta],
      models: [], orchestrators: [], default_orchestrator: null,
      tool_visibility: { hidden_names: [], hidden_prefixes: [], activity_names: [] },
    };
    const requestWorkspace = gateway.emptyGatewayWorkspaceState();
    requestWorkspace.snapshot = snapshot;
    requestWorkspace.snapshotInitialized = true;
    requestWorkspace.edbCacheInitialized = true;
    requestWorkspace.stores.set("main", {
      events: [], mutationRevision: 0, lastEventHash: "prefix-hash",
    });
    const validating = gateway.backgroundSyncRequestBody(requestWorkspace);
    expect(validating.agents).toEqual([{
      id: "main", event_count: 0, mutation_revision: 0, cursor_event_hash: "prefix-hash",
    }]);
    expect(validating.selected_agent).toBeNull();
    expect(validating.terminal_session).toBeNull();
    expect(validating.terminal_revision).toBeNull();
    requestWorkspace.cacheValidated = true;
    expect(gateway.backgroundSyncRequestBody(requestWorkspace).agents[0].cursor_event_hash).toBeNull();

    const workspace = gateway.emptyGatewayWorkspaceState();
    const changed = gateway.applyBackgroundSyncState(workspace, {
      snapshot,
      event_updates: [{
        agent_id: "main", reset: false, mutation_revision: 0, cursor_event_hash: "event-hash",
        events: [event("AgentTurn", 1, { turn_id: 1, prompt_id: 1, state: "Started" })],
      }],
    });
    expect(changed).toBe(true);
    const backgroundStore = workspace.stores.get("main");
    expect(backgroundStore.events).toEqual([]);
    expect(backgroundStore.eventCount).toBe(1);
    expect(backgroundStore.materialized).toBe(false);
    expect(backgroundStore.mutationRevision).toBe(0);
    expect(backgroundStore.lastEventHash).toBe("event-hash");
    expect(backgroundStore.summary).toEqual({ turnState: "Started" });
    expect(backgroundStore.projection.messages).toEqual([]);
    expect(backgroundStore.turnHistory).toBeNull();
    expect(workspace.drafts.get("main")).toBe("remote draft");

    gateway.state.gateway.workspaces = [{ id: "chat" }, { id: "w-one" }, { id: "w-two" }];
    gateway.state.workspaceId = "chat";
    expect(gateway.nextBackgroundWorkspace(10).workspaceId).toBe("w-one");
    expect(gateway.nextBackgroundWorkspace(10).workspaceId).toBe("w-two");

    const gatewaySource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const applyStart = gatewaySource.indexOf("function applyBackgroundSyncState");
    const applyEnd = gatewaySource.indexOf("\nasync function requestBackgroundWorkspaceSync", applyStart);
    const backgroundApply = gatewaySource.slice(applyStart, applyEnd);
    expect(backgroundApply).not.toContain("renderAll(");
    expect(backgroundApply).not.toContain("applySyncState(");
    expect(backgroundApply).not.toContain("renderConnectionOverlayForPhase(");
    expect(gatewaySource).not.toContain("refreshWorkspaceSummaries");
    expect(gatewaySource).toContain("if (!backgroundSyncCanRun() || state.backgroundSyncOperation) return;");
    expect(gatewaySource).toContain("state.connectionPhase === \"connected\"");

    for (const relative of ["../src/webui/app.js"]) {
      const source = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(source).toContain("agents: [...state.stores].map(([id, store]) => ({");
      expect(source).toContain("const eventChanges = state.snapshot.agents.map((meta) => syncAgentEvents(meta, updates.get(meta.id)));");
    }
  });

  test("tracks cache restoration and catch-up independently for each session", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const readyMeta = {
      id: "ready", edb_id: "a".repeat(64), event_count: 2, mutation_revision: 1,
      prompt_submission_revision: 0, input_draft_revision: 0,
    };
    const pendingMeta = {
      id: "pending", edb_id: "b".repeat(64), event_count: 6, mutation_revision: 1,
      prompt_submission_revision: 0, input_draft_revision: 0,
    };
    gateway.state.workspaceId = "chat";
    gateway.state.snapshot = { environment: { workspace: "/chat" }, agents: [readyMeta, pendingMeta] };
    gateway.state.edbCacheInitialized = true;
    const cached = [event("AgentTurn", 1, { state: "Completed" }), event("AgentTurn", 2, { state: "Completed" })];
    gateway.state.stores.set("ready", gateway.createAgentStore(readyMeta, {
      events: cached, eventCount: 2, mutationRevision: 1, lastEventHash: "ready-hash",
    }));
    const pending = gateway.createAgentStore(pendingMeta, {
      events: cached, eventCount: 2, mutationRevision: 1, lastEventHash: "pending-hash",
    });
    gateway.state.stores.set("pending", pending);
    expect(gateway.agentLoadingState("chat", "ready")).toEqual({ loading: false, percent: null });
    expect(gateway.agentLoadingState("chat", "pending")).toEqual({ loading: true, percent: 0 });
    expect(gateway.sessionSelectionAllowed("chat", "ready")).toBe(true);
    expect(gateway.sessionSelectionAllowed("chat", "pending")).toBe(false);
    pending.eventCount = 4;
    expect(gateway.agentLoadingState("chat", "pending")).toEqual({ loading: true, percent: 50 });
    pending.eventCount = 6;
    gateway.settleAgentLoadProgress(pending);
    expect(gateway.sessionSelectionAllowed("chat", "pending")).toBe(true);
    expect(pending.loadProgress).toBeNull();
    gateway.prepareAgentLoadProgress(
      pending, { ...pendingMeta, event_count: 10 }, null, pending.eventCount, pending.mutationRevision,
    );
    expect(pending.loadProgress).toEqual({
      mutationRevision: 1, startEventCount: 6, targetEventCount: 10,
    });
    expect(gateway.sessionSelectionAllowed("chat", "pending")).toBe(false);

    const inactive = gateway.emptyGatewayWorkspaceState();
    inactive.snapshot = { environment: { workspace: "/other" }, agents: [pendingMeta] };
    gateway.state.workspaceStates.set("w-one", inactive);
    expect(gateway.agentLoadingState("w-one", "pending")).toEqual({ loading: true, percent: null });
  });

  test("defaults to partial residency while preserving the selectable full-cache path", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const edbId = "f".repeat(64);
    const snapshot = { environment: { workspace: "/workspace" }, agents: [] };
    gateway.state.snapshot = snapshot;
    expect(gateway.state.partialLoading).toBe(true);

    const cachedEvents = [{ EdbIdGeneration: { edb_id: edbId } }, { UserPrompt: { content: "cached" } }];
    const metadata = {
      key: edbId, agentId: "retained", edbId, eventCount: cachedEvents.length,
      mutationRevision: 2, lastEventHash: "hash-2",
    };
    const meta = {
      id: "retained", edb_id: edbId, mutation_revision: 2,
      prompt_submission_revision: 0, input_draft_revision: 0,
    };
    const partial = gateway.createAgentStore(meta, metadata, snapshot);
    expect(partial.events).toEqual([]);
    expect(partial.eventCount).toBe(2);
    expect(partial.cacheKey).toBe(edbId);
    expect(partial.materialized).toBe(false);
    expect(partial.projection.messages).toEqual([]);

    gateway.state.partialLoading = false;
    const full = gateway.createAgentStore(meta, { ...metadata, events: cachedEvents }, snapshot);
    expect(full.events).toBe(cachedEvents);
    expect(full.eventCount).toBe(2);
    expect(full.materialized).toBe(true);
    expect(full.needsReplay).toBe(true);

    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(source).toContain("function materializeAgentStore(bucket, meta)");
    expect(source).toContain("function releaseMaterializedStore(store)");
    expect(source).toContain("if (!state.partialLoading) return;");
    expect(source).not.toContain("materializeClientAgentStore");
    expect(source).not.toContain("releaseClientAgentStore");
  });

  test("stores lightweight Gateway session metadata before raw EDB hydration", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const workspace = gateway.emptyGatewayWorkspaceState();
    gateway.state.workspaceStates.set("w-one", workspace);
    const snapshot = {
      revision: 7, environment: { workspace: "/workspace-one" },
      agents: [{ id: "main", title: "Visible immediately" }],
      models: [], orchestrators: [], default_orchestrator: null,
    };
    gateway.applyGatewayStartupMetadata("w-one", snapshot);
    expect(workspace.snapshot).toBe(snapshot);
    expect(workspace.selectedAgent).toBe("main");
    expect(workspace.snapshotInitialized).toBe(false);
    expect(workspace.edbCacheInitialized).toBe(false);
  });
  test("keeps Gateway Workspace disclosure as an origin-local browser preference", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const values = new Map();
    const storage = {
      getItem(key) { return values.has(key) ? values.get(key) : null; },
      setItem(key, value) { values.set(key, value); },
      removeItem(key) { values.delete(key); },
    };
    const disclosure = gateway.readWorkspaceDisclosure(storage);
    expect(gateway.workspaceExpanded("w-one", disclosure)).toBe(true);
    expect(gateway.setWorkspaceExpanded("w-one", false, disclosure, storage)).toBe(true);
    expect(gateway.workspaceExpanded("w-one", disclosure)).toBe(false);
    const restored = gateway.readWorkspaceDisclosure(storage);
    expect(gateway.workspaceExpanded("w-one", restored)).toBe(false);
    gateway.setWorkspaceExpanded("w-two", true, restored, storage);
    expect(gateway.pruneWorkspaceDisclosure(new Set(["w-two"]), restored, storage)).toBe(true);
    expect([...restored]).toEqual([["w-two", true]]);
    expect(gateway.readWorkspaceDisclosure({ getItem() { return "not-json"; } })).toEqual(new Map());

    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(source).toContain('const WORKSPACE_DISCLOSURE_STORAGE_KEY = "me-gateway.workspace-disclosure.v1";');
    expect(source).toContain("workspaceDisclosure: readWorkspaceDisclosure()");
    expect(source).toContain("setWorkspaceExpanded(workspace.id, !workspaceExpanded(workspace.id));");
    expect(source).not.toContain("expandedWorkspaces");
    expect(source).not.toContain("/api/gateway/workspaces/${encodeURIComponent(workspace.id)}/expanded");
  });


  test("refreshes empty transcript metadata when switching Workspaces", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const transcriptContent = {
      value: "",
      querySelector() { return this.value.includes("empty-state") ? {} : null; },
      get innerHTML() { return this.value; },
      set innerHTML(value) { this.value = value; },
    };
    gateway.elements.transcriptContent = transcriptContent;
    gateway.state.snapshot.environment = { workspace: "/chat", system: "macos/aarch64" };
    gateway.renderTranscript(true);
    expect(transcriptContent.innerHTML).toContain("/chat");
    gateway.state.snapshot.environment = { workspace: "/work", system: "linux/x86_64" };
    gateway.renderTranscript(true);
    expect(transcriptContent.innerHTML).toContain("/work");
    expect(transcriptContent.innerHTML).not.toContain("/chat");
  });

  test("renders classic authenticated settings and independent login-page local preferences", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const model = {
      ...gateway.blankGatewayModel(), name: "model-a", provider: "openai-compatible", api_key: "visible-key",
    };
    const html = gateway.modelSettingsHtml(model, 0);
    expect(html.startsWith('<details class="settings-model" data-settings-model="0">')).toBe(true);
    expect(html).toContain('class="settings-model-icon"');
    expect(html).toContain('class="settings-model-body"');
    expect(html).toContain('class="settings-model-advanced"');
    expect(html).not.toContain("<h4>基本与连接</h4>");
    expect(html).not.toContain("<h4>高级配置</h4>");
    expect(html).toContain('data-setting="api_key" type="text"');
    expect(html).toContain('value="visible-key"');

    expect(gateway.state.partialLoading).toBe(true);
    expect(gateway.normalizeWindowBorderStyle("theme")).toBe("theme");
    expect(gateway.normalizeWindowBorderStyle("invalid")).toBe("default");
    const localHtml = gateway.localPreferenceSettingsHtml();
    expect(localHtml).toContain("局部加载");
    expect(localHtml).toContain('data-local-preference="partial-loading"');
    expect(localHtml).not.toContain("边框样式");

    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const index = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const styles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(source).toContain('class="settings-section settings-model-section"');
    expect(source).toContain('class="settings-default-model"');
    expect(source).toContain('class="settings-subsection-header"');
    expect(source).not.toContain('class="settings-section-icon"');
    expect(source).toContain('const PARTIAL_LOADING_PREFERENCE = "me-partial-loading";');
    expect(source).toContain('const WINDOW_BORDER_STYLE_PREFERENCE = "me-window-border-style";');
    expect(source).toContain('return readLocalPreference(PARTIAL_LOADING_PREFERENCE) !== "disabled";');
    expect(source).toContain("const borderSetting = runtimeCapabilities.windowBorderStyle ?");
    expect(source).toContain("<strong>边框样式</strong>");
    expect(source).toContain(">默认</option>");
    expect(source).toContain(">主题</option>");
    expect(source).toContain('partialLoading?.addEventListener("change", async () => {');
    expect(source).toContain('borderStyle?.addEventListener("change", () => setWindowBorderStyle(borderStyle.value));');
    expect(source).toContain("persistLocalPreference(PARTIAL_LOADING_PREFERENCE");
    expect(source).toContain("persistLocalPreference(WINDOW_BORDER_STYLE_PREFERENCE");
    expect(source).toContain('elements.loginSettings?.addEventListener("click", openLocalSettings)');
    const loginStart = source.indexOf("function openLocalSettings()");
    const loginEnd = source.indexOf("\nfunction renderGatewayEdbCacheSettings", loginStart);
    const loginSettings = source.slice(loginStart, loginEnd);
    expect(loginSettings).toContain("localPreferenceSettingsHtml()");
    expect(loginSettings).not.toContain("/api/");
    expect(loginSettings).not.toContain("settings-edb-cache-manager");
    expect(source).toContain('kind: "settings"');
    expect(styles).toContain(".settings-modal-backdrop .modal { width: min(820px, calc(100vw - 40px));");
    expect(styles).toContain(".settings-section { overflow: visible;");
    expect(styles).not.toContain(".settings-section-icon");
    expect(index).toContain('id="login-settings" class="login-settings" type="button" title="设置" aria-label="设置"><svg');
    expect(index).toContain('id="open-settings" class="sidebar-settings" type="button" title="设置" aria-label="设置"><svg');
    expect(index).not.toContain('id="environment-footer"');
  });

  test("keeps runtime-specific login branding free of marketing taglines", () => {
    const index = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const styles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const directRuntime = readFileSync(join(import.meta.dir, "../src/webui/runtime.js"), "utf8");
    const gatewayRuntime = readFileSync(join(import.meta.dir, "../src/gateway_webui/runtime.js"), "utf8");
    expect(index).toContain("<strong>ME</strong>");
    expect(directRuntime).toContain('brandTitle: "ME-S"');
    expect(gatewayRuntime).toContain('brandTitle: "ME"');
    const app = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(app).toContain("${escapeHtml(runtimeCapabilities.brandTitle)}");
    expect(index).not.toContain("智能工作台");
    expect(styles).not.toContain(".login-brand span");
  });

  test("sorts, filters, and formats host directory metadata without recursion", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const entries = [
      { name: "Folder 10", kind: "directory", modified_at_ms: null, size_bytes: null },
      { name: "file10.txt", kind: "file", modified_at_ms: 200, size_bytes: 10 },
      { name: "Folder 2", kind: "directory", modified_at_ms: 100, size_bytes: null },
      { name: "file2.pdf", kind: "file", modified_at_ms: null, size_bytes: 2 },
      { name: "unknown.bin", kind: "file", modified_at_ms: null, size_bytes: null },
    ];
    expect(gateway.sortDirectoryEntries(entries, "name", "asc").map((entry) => entry.name))
      .toEqual(["Folder 2", "Folder 10", "file2.pdf", "file10.txt", "unknown.bin"]);
    expect(gateway.sortDirectoryEntries(entries, "size", "asc").map((entry) => entry.name))
      .toEqual(["Folder 2", "Folder 10", "file2.pdf", "file10.txt", "unknown.bin"]);
    expect(gateway.sortDirectoryEntries(entries, "size", "desc").map((entry) => entry.name))
      .toEqual(["Folder 2", "Folder 10", "file10.txt", "file2.pdf", "unknown.bin"]);
    expect(gateway.filterDirectoryEntries(entries, "FILE").map((entry) => entry.name))
      .toEqual(["file10.txt", "file2.pdf"]);
    expect(gateway.directoryEntryType({ name: "document.pdf", kind: "file" })).toBe("PDF 文档");
    expect(gateway.directoryEntryType({ name: "README", kind: "file" })).toBe("文件");
    expect(gateway.directoryEntryType({ name: "src", kind: "directory" })).toBe("文件夹");
    expect(gateway.formatDirectorySize(0, "file")).toBe("0 B");
    expect(gateway.formatDirectorySize(1024, "file")).toBe("1.0 KB");
    expect(gateway.formatDirectorySize(null, "file")).toBe("—");
    expect(gateway.formatDirectorySize(1024, "directory")).toBe("—");
    expect(gateway.formatDirectoryModified(null)).toBe("—");
  });

  test("uses a fixed responsive Finder-style host directory window", () => {
    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const styles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(source).not.toContain("浏览 ME Gateway 宿主机上的文件和文件夹。");
    expect(source).toContain('class="directory-list-header"');
    expect(source).toContain('data-directory-sort="${key}"');
    expect(source).toContain('placeholder="筛选当前目录"');
    expect(source).toContain('class="directory-entry-mobile-meta"');
    expect(source).toContain('row.addEventListener("dblclick"');
    expect(source).toContain('if (row.dataset.entryKind !== "file")');
    expect(source).toContain('body: JSON.stringify({ path: current, initialize: false })');
    expect(source).toContain('body: JSON.stringify({ path, initialize: true })');
    expect(source).toContain('onCancel: () => openModal(directoryModal)');
    expect(source).toContain('"/api/gateway/directories/create"');
    expect(styles).toContain(".directory-modal-backdrop .modal {");
    expect(styles).toContain("height: min(680px, calc(100dvh - 40px));");
    expect(styles).toContain(".directory-modal-backdrop .modal > header, .directory-modal-backdrop .modal > footer { flex: 0 0 auto; }");
    expect(styles).toContain(".directory-toolbar { display: grid; min-width: 0; grid-row: 1;");
    expect(styles).toContain(".directory-new-folder-form { display: grid; grid-row: 2;");
    expect(styles).toContain(".directory-table { display: flex; min-width: 0; min-height: 0; grid-row: 3;");
    expect(styles).toContain(".directory-selection-summary, .directory-name { grid-row: 4; }");
    expect(styles).toContain(".directory-modal-backdrop { align-items: end; padding: env(safe-area-inset-top) 0 0; }");
    expect(styles).toContain("height: min(680px, calc(100dvh - env(safe-area-inset-top))); max-height: calc(100dvh - env(safe-area-inset-top));");
    expect(styles).toContain("@media (max-width: 800px) and (max-height: 480px) and (orientation: landscape) {");
    expect(styles).toContain(".directory-toolbar { grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: 6px; }");
    expect(styles).toContain("grid-template-columns: minmax(230px, 1fr) 150px 130px 90px 32px;");
    expect(styles).toContain(".directory-entry-mobile-meta { display: flex;");
    expect(styles).toContain(".directory-list { min-width: 0; min-height: 0; flex: 1; overflow: auto;");
  });

  test("locks the shared document while keeping content areas internally scrollable", () => {
    const styles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(styles).toContain("overflow: hidden; overscroll-behavior: none;");
    expect(styles).toContain("html { -webkit-text-size-adjust: 100%; text-size-adjust: 100%; }");
    expect(styles).toContain("body { position: fixed; inset: 0; }");
    expect(styles).toContain("height: 100%; height: 100dvh; min-height: 0;");
    expect(styles).toContain(".login-screen { display: grid; width: 100%; height: 100%; min-height: 0;");
    expect(styles).toContain("overflow: auto; overscroll-behavior: contain; padding: 24px;");
    expect(styles).toContain(".transcript { contain: layout paint style; flex: 1; min-height: 0; overflow: auto;");
    expect(styles).toContain("overscroll-behavior-y: contain;");
  });

  test("balances short confirmation dialogs without resizing content modals", () => {
    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const styles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(source).toContain("当前会话将从空白上下文继续，已有消息记录不会被删除。");
    expect(source).toContain('classList.toggle("message-modal-backdrop", messageOnly)');
    expect(styles).toContain(".message-modal-backdrop .modal { width: min(560px, calc(100vw - 40px)); min-height: min(260px, calc(100dvh - 40px)); }");
    expect(styles).toContain(".message-modal-backdrop .modal > header { min-height: 64px;");
    expect(styles).toContain(".message-modal-backdrop .modal > p { display: flex;");
    expect(styles).toContain(".message-modal-backdrop .modal > footer { min-height: 72px;");
    expect(styles).toContain(".message-modal-backdrop .modal { width: 100%; min-height: min(280px, calc(86dvh - env(safe-area-inset-top))); }");
    expect(source).toContain("const messageOnly = modal.html == null && !choices.length;");
    expect(source).toContain('classList.remove("directory-modal-backdrop", "message-modal-backdrop", "settings-modal-backdrop")');
    expect(source).toContain('html: `<div class="directory-browser"></div>`');
    expect(source).toContain('html: `<div class="settings-editor"></div>`');
  });

  test("routes Windows drive roots through the host root selector", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    expect(gateway.directoryParentRequest({ parent: "C:\\Users", parent_is_root_selector: false }))
      .toEqual({ path: "C:\\Users", roots: false });
    expect(gateway.directoryParentRequest({ parent: null, parent_is_root_selector: true }))
      .toEqual({ path: null, roots: true });
    expect(gateway.directoryParentRequest({ parent: null, parent_is_root_selector: false }))
      .toBeNull();
    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(source).not.toContain("displayHostPath");
    expect(source).toContain('String(listing.path || "")');
    expect(source).toContain('JSON.stringify({ path, roots })');
    expect(source).toContain('rootSelector ? "此电脑"');
  });


  test("keeps the selected default model attached when that model is renamed", () => {
    const gateway = loadRuntime("../src/webui/app.js");
    const previous = [{ name: "model-a" }, { name: "model-b" }];
    const edited = [{ name: "model-a" }, { name: "model-renamed" }];
    expect(gateway.resolveEditedDefaultModel(previous, edited, "model-b")).toBe("model-renamed");
    expect(gateway.resolveEditedDefaultModel(previous, edited, "unknown")).toBe("unknown");
  });

  test("serializes persisted Workspace selections in user action order", async () => {
    const originalPersistSelection = globalThis.MeFrontendRuntime.persistSelection;
    const calls = [];
    const releases = [];
    globalThis.MeFrontendRuntime.persistSelection = (_api, workspaceId, agentId) => new Promise((resolve) => {
      calls.push({ workspaceId, agentId });
      releases.push(resolve);
    });
    try {
      const gateway = loadRuntime("../src/webui/app.js");
      const first = gateway.persistGatewaySelection("w-one", "main");
      const second = gateway.persistGatewaySelection("w-two", "agent-2");
      await Promise.resolve();
      await Promise.resolve();
      expect(calls.map((call) => call.workspaceId)).toEqual(["w-one"]);
      releases.shift()();
      await first;
      await Promise.resolve();
      expect(calls.map((call) => call.workspaceId)).toEqual(["w-one", "w-two"]);
      releases.shift()();
      await second;
    } finally {
      globalThis.MeFrontendRuntime.persistSelection = originalPersistSelection;
    }
  });

  test("renders compact accessible sidebar rows and capability-gated Workspace surfaces", () => {
    const index = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const styles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const themeStyles = readFileSync(join(import.meta.dir, "../src/webui/theme.css"), "utf8");

    expect(index).toContain('class="sidebar-scroll"');
    expect(index).not.toContain('id="mobile-delete-agent"');
    expect(index).toContain('class="status-model-icon"');
    expect(index).toContain('class="status-selector status-effort" type="button" aria-haspopup="dialog" title="切换推理强度"><span');
    expect(index).not.toContain('title="切换推理强度">(<span');
    expect(source).toContain('class="agent-dot" aria-hidden="true"');
    expect(source).toContain('class="agent-label"></span>');
    expect(source).toContain('class="agent-delete"');
    expect(source).toContain('void openDeleteAgent(agent.id)');
    expect(source).toContain('if (kind === "AgentTurn") summary.turnState = value.state;');
    expect(source).toContain("const active = !loadingState.loading && sidebarAgentActive(summary);");
    expect(source).not.toContain("const active = API_ACTIVE.has(summary?.apiState);");
    expect(source).not.toContain("startupPending: true");
    expect(index).toContain('id="session-sync-overlay"');
    expect(index).toContain("<strong>正在同步</strong>");
    expect(source).toContain("if (!sessionSelectionAllowed(workspaceId, agentId)) return;");
    expect(source).toContain("node.inert = loading;");
    expect(source).toContain("renderSessionSyncOverlay();");
    expect(source).toContain('row.classList.toggle("session-loading", loadingState.loading)');
    expect(source).toContain('class="agent-load-progress hidden"');
    expect(source).toContain("item.disabled = loadingState.loading");
    expect(source).toContain("deleteButton.disabled = loadingState.loading");
    expect(styles).toContain(".agent-label { display: block; min-width: 0; flex: 1; overflow: hidden; font-size: 13px; font-weight: 700;");
    expect(styles).toContain(".agent-dot.active + .agent-label { color: transparent; background:");
    expect(styles).not.toContain(".agent-dot.active + .agent-label { color: transparent; font-weight:");
    expect(styles).toContain(".agent-row { display: grid; min-width: 0; min-height: 32px;");
    expect(styles).toContain(".agent-item { display: flex; min-width: 0; width: 100%; min-height: 32px;");
    expect(styles).toContain(".agent-row.active { background: var(--agent-selected-bg); }");
    expect(styles).not.toContain(".agent-row.active { background: var(--agent-selected-bg); box-shadow:");
    expect(styles).toContain("animation: agent-dot-breathe 3s ease-in-out infinite;");
    expect(styles).toContain("linear-gradient(100deg, var(--text) 0 36%, var(--activity-sweep) 46% 54%, var(--text) 64% 100%)");
    expect(styles).toContain("animation: agent-label-sweep 3s ease-in-out infinite;");
    expect(styles).toContain("@keyframes agent-dot-breathe { 0%, 66.667%, 100% { opacity: 1; } 33.333% { opacity: .35; } }");
    expect(styles).toContain("@keyframes agent-label-sweep { 0% { background-position: 100% 0; } 66.667%, 100% { background-position: 0 0; } }");
    expect(styles).toContain(".agent-row.session-loading { opacity: .72; }");
    expect(styles).toContain(".session-sync-overlay { position: absolute;");
    expect(styles).toContain(".session-sync-card { display: grid;");
    expect(styles).toContain(".agent-dot.loading { border-color: var(--border-bright); border-top-color: var(--cyan);");
    expect(styles).toContain("animation: agent-loading-spin .8s linear infinite;");
    expect(styles).toContain("@keyframes agent-loading-spin { to { transform: rotate(360deg); } }");
    expect(styles).toContain(".statusbar { contain: layout paint style;");
    expect(styles).toContain("font-weight: 700; white-space: nowrap;");
    expect(styles).toContain(".status-model-icon {");
    expect(styles).toContain(".sidebar-scroll.scrollbar-active");

    expect(themeStyles).toContain("--activity-sweep: color-mix(in srgb, var(--text) 42%, var(--bg));");
    expect(themeStyles).toContain("--agent-selected-bg: color-mix(in srgb, var(--accent) 22%, var(--panel));");
    expect(index).toContain('class="sidebar-divider" data-multiple-workspaces aria-hidden="true"');
    expect(source).toContain("workspaceDisclosure: readWorkspaceDisclosure()");
    expect(source).toContain('class="workspace-disclosure-icon"');
    expect(source).toContain('class="workspace-folder-icon"');
    expect(source).toContain('class="workspace-name"></span>');
    expect(source).toContain('aria-expanded="${workspaceExpanded(workspace.id)}"');
    expect(source).toContain("agents.hidden = !expanded");
    expect(source).not.toContain("if (state.workspaceId !== workspace.id) activateWorkspace(workspace.id);");
    expect(source).not.toContain('group.classList.toggle("active", active);');
    expect(styles).not.toContain(".workspace-group.active > .workspace-row");
    expect(styles).toContain(".workspace-select { display: grid; min-width: 0; min-height: 32px;");
    expect(styles).toContain(".workspace-agent-list .agent-row { min-height: 32px; }");
    expect(source).not.toContain("select.title = workspace.path");
    expect(styles).toContain(".workspace-name { display: block; min-width: 0; overflow: hidden; font-size: 13px; font-weight: 750;");
    expect(styles).toContain(".sidebar-settings { display: grid; width: 32px; min-width: 32px; height: 32px; flex: 0 0 32px;");
  });

  test("auto-hides only the themed scrollbar appearance without intercepting scrolling", () => {
    for (const relative of ["../src/webui/app.js"]) {
      const runtime = loadRuntime(relative);
      const listeners = new Map();
      const removed = [];
      const classes = new Set();
      let pending = null;
      const element = {
        classList: {
          add(value) { classes.add(value); },
          remove(value) { classes.delete(value); },
        },
        addEventListener(type, listener, options) { listeners.set(type, { listener, options }); },
        removeEventListener(type, listener) { removed.push([type, listener]); },
      };
      const dispose = runtime.bindSidebarScrollbar(element, {
        delay: 321,
        setTimeout(callback, delay) { pending = { callback, delay }; return 7; },
        clearTimeout() { pending = null; },
      });
      expect(listeners.get("scroll").options).toEqual({ passive: true });
      expect(listeners.get("pointermove").options).toEqual({ passive: true });
      expect(listeners.get("pointerleave").options).toEqual({ passive: true });
      listeners.get("scroll").listener();
      expect(classes.has("scrollbar-active")).toBe(true);
      expect(pending.delay).toBe(321);
      pending.callback();
      expect(classes.has("scrollbar-active")).toBe(false);
      listeners.get("pointermove").listener();
      expect(classes.has("scrollbar-active")).toBe(true);
      listeners.get("pointerleave").listener();
      expect(classes.has("scrollbar-active")).toBe(false);
      dispose();
      expect(removed.map(([type]) => type)).toEqual(["scroll", "pointermove", "pointerleave"]);
    }
  });
});
