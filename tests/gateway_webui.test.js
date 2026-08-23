"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function loadRuntime(relative) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error(`could not isolate ${relative}`);
  const factory = new Function("document", "performance", "matchMedia", "MeTranscript", `${source.slice(0, eventBindings)}
    return { state, emptyProjection, projectChat, consumeChatEvents, chatAppendNeedsReplay,
      projectAgentSummary, updateAgentSummary, sidebarAgentActive,
      emptyWorkMap, projectWorkMap, consumeWorkMapEvents, scopedApiPath: typeof scopedApiPath === "function" ? scopedApiPath : null,
      eventRecoveryBacklog, shouldUseBulkEventRecovery, createEventRecovery, eventRecoveryProgress,
      eventRecoveryMatches, selectedEventRecoveryReady,
      emptyGatewayWorkspaceState: typeof emptyGatewayWorkspaceState === "function" ? emptyGatewayWorkspaceState : null,
      resolveEditedDefaultModel: typeof resolveEditedDefaultModel === "function" ? resolveEditedDefaultModel : null,
      blankGatewayModel: typeof blankGatewayModel === "function" ? blankGatewayModel : null,
      modelSettingsHtml: typeof modelSettingsHtml === "function" ? modelSettingsHtml : null,
      persistGatewaySelection: typeof persistGatewaySelection === "function" ? persistGatewaySelection : null,
      directoryParentRequest: typeof directoryParentRequest === "function" ? directoryParentRequest : null,
      displayHostPath: typeof displayHostPath === "function" ? displayHostPath : null,
      renderTranscript: typeof renderTranscript === "function" ? renderTranscript : null,
      emptyObjectiveDisclosure: typeof emptyObjectiveDisclosure === "function" ? emptyObjectiveDisclosure : null,
      syncObjectiveDisclosure: typeof syncObjectiveDisclosure === "function" ? syncObjectiveDisclosure : null,
      objectiveSummaryHtml: typeof objectiveSummaryHtml === "function" ? objectiveSummaryHtml : null,
      objectiveDisclosureAttributes: typeof objectiveDisclosureAttributes === "function" ? objectiveDisclosureAttributes : null,
      objectiveEventActivates: typeof objectiveEventActivates === "function" ? objectiveEventActivates : null,
      bindSidebarScrollbar: typeof bindSidebarScrollbar === "function" ? bindSidebarScrollbar : null,
      elements: typeof elements === "object" ? elements : null };
  `);
  const runtime = factory(
    { querySelector: () => null, cookie: "" },
    { now: () => 0 },
    () => ({ matches: false, addEventListener() {} }),
    { reconcileHtmlChildren(container, html) { container.innerHTML = html; } },
  );
  runtime.state.snapshot.tool_visibility = {
    hidden_names: ["SetTitle"], hidden_prefixes: ["WorkMap.", "Worker."], activity_names: ["Worker.Wait"],
  };
  return runtime;
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
  test("projects the same EDB fixture as the me-s WebUI", () => {
    const single = loadRuntime("../src/webui/app.js");
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    expect(visibleProjection(gateway.projectChat(fixture)))
      .toEqual(visibleProjection(single.projectChat(fixture)));
    const gatewayWorkMap = gateway.projectWorkMap(fixture);
    const singleWorkMap = single.projectWorkMap(fixture);
    expect({ ...gatewayWorkMap, _records: undefined })
      .toEqual({ ...singleWorkMap, _records: undefined });
  });

  test("keeps sidebar activity open across API loops until the Agent turn closes", () => {
    for (const relative of ["../src/webui/app.js", "../src/gateway_webui/app.js"]) {
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

  test("keeps bulk recovery current-session scoped and commits only at its fixed target", () => {
    for (const relative of ["../src/webui/app.js", "../src/gateway_webui/app.js"]) {
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
      expect(source).toContain("delete elements.terminalScreen.dataset.revision;\n  restoreDraft();\n  const meta =");
      expect(source).toContain("!wasConnected || selectionChanged || Boolean(selectedUpdate?.reset)");
      expect(source).toContain("const recoveryReady = responseMatchesSelection && selectedEventRecoveryReady(");
      expect(source).toContain("const bulkRecoveryPending = bulkEventRecoveryActive() && !recoveryReady;");
      expect(source).toContain("currentEvents: !bulkRecoveryPending && !recoveryReady && selectedEventsChanged");
      expect(source).toContain("workerEvents: !bulkRecoveryPending && !recoveryReady && selectedWorkerChanged");
      expect(source).toContain("if (bulkRecoveryPending) suppressBulkEventRecoveryRender();");
      expect(source).toContain(`if (recoveryReady) {
    store.projectedOrder = 0;
    store.needsReplay = true;
    state.eventRecovery = null;
    requestRender({ full: true, connection: !wasConnected });
    flushPendingRender();`);
      expect(source).toContain("if (bulkEventRecoveryActive()) return emptyProjectionChanges();");
      expect(source).toContain('elements.connectionRetry.classList.add("hidden")');
      expect(source).toContain('elements.eventRecoveryProgress.setAttribute("aria-valuenow", String(percent))');
      expect(source).toContain('elements.eventRecoveryProgressFill.style.transform = `scaleX(${progress})`');
      expect(source).toContain('elements.app.inert = true;');
    }
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    const first = gateway.emptyGatewayWorkspaceState();
    const second = gateway.emptyGatewayWorkspaceState();
    first.eventRecovery = { agentId: "main", mutationRevision: 1, startEventCount: 0, targetEventCount: 101 };
    expect(second.eventRecovery).toBeNull();
    const gatewaySource = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    expect(gatewaySource).toContain("eventRecovery: state.eventRecovery");
    expect(gatewaySource).toContain("state.eventRecovery = workspace.eventRecovery;");
  });

  test("closes the portrait sidebar before selecting any Gateway session", () => {
    const gatewaySource = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    expect(gatewaySource).toContain(`function selectWorkspaceAgent(workspaceId, agentId) {
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
    for (const relative of ["../src/webui/style.css", "../src/gateway_webui/style.css"]) {
      const style = readFileSync(join(import.meta.dir, relative), "utf8");
      expect(style).toContain(objectiveTitleRule);
    }

    const current = {
      objective: { id: "objective-1", title: "Ship safely", description: "Release details" },
      plans: [{ plan: { id: "plan-1", title: "Build", state: "active" }, notes: [{}] }],
    };
    for (const relative of ["../src/webui/app.js", "../src/gateway_webui/app.js"]) {
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
    const singleSource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const gatewaySource = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    expect(singleSource).toContain("syncObjectiveDisclosure(state.objectiveDisclosure, state.selectedAgent, current.objective.id)");
    expect(gatewaySource).toContain("JSON.stringify([state.workspaceId, state.selectedAgent])");
    for (const stylePath of ["../src/webui/style.css", "../src/gateway_webui/style.css"]) {
      const styles = readFileSync(join(import.meta.dir, stylePath), "utf8");
      expect(styles).toContain(".objective-summary:focus-visible");
      expect(styles).toContain(".objective-summary:hover");
      expect(styles).toContain("pointer-events: none;");
    }
  });

  test("namespaces child APIs and allocates independent Workspace stores", () => {
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    gateway.state.workspaceId = "w-one";
    expect(gateway.scopedApiPath("/api/sync")).toBe("/api/workspaces/w-one/sync");
    expect(gateway.scopedApiPath("/api/session-terminal/main/read"))
      .toBe("/api/workspaces/w-one/session-terminal/main/read");
    expect(gateway.scopedApiPath("/api/auth/status")).toBe("/api/auth/status");
    const first = gateway.emptyGatewayWorkspaceState();
    const second = gateway.emptyGatewayWorkspaceState();
    first.stores.set("main", { marker: 1 });
    first.drafts.set("main", "workspace one");
    expect(second.stores.has("main")).toBe(false);
    expect(second.drafts.has("main")).toBe(false);
    expect(first.terminalFrames).not.toBe(second.terminalFrames);
    expect(first.workerActivityIndexes).not.toBe(second.workerActivityIndexes);
  });

  test("refreshes empty transcript metadata when switching Workspaces", () => {
    const gateway = loadRuntime("../src/gateway_webui/app.js");
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

  test("renders icon settings and collapsed model cards with visible API Keys", () => {
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    const model = {
      ...gateway.blankGatewayModel(), name: "model-a", provider: "openai-compatible", api_key: "visible-key",
    };
    const html = gateway.modelSettingsHtml(model, 0);
    expect(html.startsWith('<details class="settings-model" data-settings-model="0">')).toBe(true);
    expect(html).toContain('class="settings-model-icon"');
    expect(html).toContain('data-setting="api_key" type="text"');
    expect(html).toContain('value="visible-key"');
    const index = readFileSync(join(import.meta.dir, "../src/gateway_webui/index.html"), "utf8");
    expect(index).toContain('id="open-settings" class="sidebar-settings" type="button" title="设置" aria-label="设置"><svg');
  });

  test("keeps login branding free of marketing taglines", () => {
    const singleIndex = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const gatewayIndex = readFileSync(join(import.meta.dir, "../src/gateway_webui/index.html"), "utf8");
    const singleStyles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const gatewayStyles = readFileSync(join(import.meta.dir, "../src/gateway_webui/style.css"), "utf8");
    expect(singleIndex).toContain("<strong>ME-S</strong>");
    expect(gatewayIndex).toContain("<strong>ME</strong>");
    for (const index of [singleIndex, gatewayIndex]) expect(index).not.toContain("智能工作台");
    for (const styles of [singleStyles, gatewayStyles]) expect(styles).not.toContain(".login-brand span");
  });

  test("uses a fixed, refined, compact directory window", () => {
    const source = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    const styles = readFileSync(join(import.meta.dir, "../src/gateway_webui/style.css"), "utf8");
    expect(source).toContain('kind: "directory"');
    expect(source).toContain('class="directory-list-header"');
    expect(source).toContain('class="directory-folder-icon"');
    expect(source).toContain('class="directory-count"');
    expect(source).toContain('class="directory-entry-icon"');
    expect(styles).toContain(".directory-modal-backdrop .modal {");
    expect(styles).toContain("height: min(680px, calc(100dvh - 40px));");
    expect(styles).toContain(".directory-list { min-height: 0; flex: 1; overflow: auto;");
    expect(styles).toContain(".directory-entry-icon {");
    expect(styles).toContain("min-height: 38px;");
  });

  test("locks the document while keeping content areas internally scrollable", () => {
    const single = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const gateway = readFileSync(join(import.meta.dir, "../src/gateway_webui/style.css"), "utf8");
    for (const styles of [single, gateway]) {
      expect(styles).toContain("overflow: hidden; overscroll-behavior: none;");
      expect(styles).toContain("body { position: fixed; inset: 0; }");
      expect(styles).toContain("height: 100%; height: 100dvh; min-height: 0;");
      expect(styles).toContain(".login-screen { display: grid; width: 100%; height: 100%; min-height: 0;");
      expect(styles).toContain("overflow: auto; overscroll-behavior: contain; padding: 24px;");
      expect(styles).toContain(".transcript { contain: layout paint style; flex: 1; min-height: 0; overflow: auto;");
      expect(styles).toContain("overscroll-behavior-y: contain;");
    }
  });

  test("routes Windows drive roots through the host root selector", () => {
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    expect(gateway.directoryParentRequest({ parent: "C:\\Users", parent_is_root_selector: false }))
      .toEqual({ path: "C:\\Users", roots: false });
    expect(gateway.directoryParentRequest({ parent: null, parent_is_root_selector: true }))
      .toEqual({ path: null, roots: true });
    expect(gateway.directoryParentRequest({ parent: null, parent_is_root_selector: false }))
      .toBeNull();
    expect(gateway.displayHostPath("\\\\?\\C:\\Users")).toBe("C:\\Users");
    expect(gateway.displayHostPath("\\\\?\\UNC\\server\\share")).toBe("\\\\server\\share");
    const source = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    expect(source).toContain('JSON.stringify({ path, roots })');
    expect(source).toContain('rootSelector ? "此电脑"');
  });


  test("keeps the selected default model attached when that model is renamed", () => {
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    const previous = [{ name: "model-a" }, { name: "model-b" }];
    const edited = [{ name: "model-a" }, { name: "model-renamed" }];
    expect(gateway.resolveEditedDefaultModel(previous, edited, "model-b")).toBe("model-renamed");
    expect(gateway.resolveEditedDefaultModel(previous, edited, "unknown")).toBe("unknown");
  });

  test("serializes persisted Workspace selections in user action order", async () => {
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    const originalFetch = globalThis.fetch;
    const calls = [];
    const releases = [];
    globalThis.fetch = (url, options) => new Promise((resolve) => {
      calls.push({ url, body: JSON.parse(options.body) });
      releases.push(() => resolve({
        ok: true, status: 200, json: async () => ({ ok: true }),
      }));
    });
    try {
      const first = gateway.persistGatewaySelection("w-one", "main");
      const second = gateway.persistGatewaySelection("w-two", "agent-2");
      await Promise.resolve();
      await Promise.resolve();
      expect(calls.map((call) => call.body.workspace_id)).toEqual(["w-one"]);
      releases.shift()();
      await first;
      await Promise.resolve();
      expect(calls.map((call) => call.body.workspace_id)).toEqual(["w-one", "w-two"]);
      releases.shift()();
      await second;
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  test("renders compact accessible sidebar rows and status metadata in both WebUIs", () => {
    const singleIndex = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const gatewayIndex = readFileSync(join(import.meta.dir, "../src/gateway_webui/index.html"), "utf8");
    const singleSource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const gatewaySource = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    const singleStyles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const gatewayStyles = readFileSync(join(import.meta.dir, "../src/gateway_webui/style.css"), "utf8");
    const themeStyles = readFileSync(join(import.meta.dir, "../src/webui/theme.css"), "utf8");

    for (const [index, source, styles] of [
      [singleIndex, singleSource, singleStyles],
      [gatewayIndex, gatewaySource, gatewayStyles],
    ]) {
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
      expect(source).toContain("const active = sidebarAgentActive(summary);");
      expect(source).not.toContain("const active = API_ACTIVE.has(summary?.apiState);");
      expect(styles).toContain(".agent-label { display: block; min-width: 0; flex: 1; overflow: hidden; font-size: 13px; font-weight: 700;");
      expect(styles).toContain(".agent-dot.active + .agent-label { color: transparent; background:");
      expect(styles).not.toContain(".agent-dot.active + .agent-label { color: transparent; font-weight:");
      expect(styles).toContain(".agent-row { display: grid; min-width: 0; min-height: 34px;");
      expect(styles).toContain(".agent-item { display: flex; min-width: 0; width: 100%; min-height: 34px;");
      expect(styles).toContain(".agent-row.active { background: var(--agent-selected-bg); }");
      expect(styles).not.toContain(".agent-row.active { background: var(--agent-selected-bg); box-shadow:");
      expect(styles).toContain("animation: agent-dot-breathe 3s ease-in-out infinite;");
      expect(styles).toContain("linear-gradient(100deg, var(--text) 0 36%, var(--activity-sweep) 46% 54%, var(--text) 64% 100%)");
      expect(styles).toContain("animation: agent-label-sweep 3s ease-in-out infinite;");
      expect(styles).toContain("@keyframes agent-dot-breathe { 0%, 66.667%, 100% { opacity: 1; } 33.333% { opacity: .35; } }");
      expect(styles).toContain("@keyframes agent-label-sweep { 0% { background-position: 100% 0; } 66.667%, 100% { background-position: 0 0; } }");
      expect(styles).toContain(".statusbar { contain: layout paint style;");
      expect(styles).toContain("font-weight: 700; white-space: nowrap;");
      expect(styles).toContain(".status-model-icon {");
      expect(styles).toContain(".sidebar-scroll.scrollbar-active");
    }

    expect(themeStyles).toContain("--activity-sweep: color-mix(in srgb, var(--text) 42%, var(--bg));");
    expect(themeStyles).toContain("--agent-selected-bg: color-mix(in srgb, var(--accent) 22%, var(--panel));");
    expect(gatewayIndex).toContain('class="sidebar-divider" aria-hidden="true"');
    expect(gatewaySource).toContain("expandedWorkspaces: new Set()");
    expect(gatewaySource).toContain('class="workspace-disclosure-icon"');
    expect(gatewaySource).toContain('class="workspace-folder-icon"');
    expect(gatewaySource).toContain('class="workspace-name"></span>');
    expect(gatewaySource).toContain('aria-expanded="false"');
    expect(gatewaySource).toContain("agents.hidden = !expanded");
    expect(gatewaySource).not.toContain("if (state.workspaceId !== workspace.id) activateWorkspace(workspace.id);");
    expect(gatewaySource).not.toContain('group.classList.toggle("active", active);');
    expect(gatewayStyles).not.toContain(".workspace-group.active > .workspace-row");
    expect(gatewayStyles).toContain(".workspace-select { display: grid; min-width: 0; min-height: 34px;");
    expect(gatewayStyles).toContain(".workspace-agent-list .agent-row { min-height: 34px; }");
    expect(gatewaySource).not.toContain("select.title = workspace.path");
    expect(gatewayStyles).toContain(".workspace-name { display: block; min-width: 0; overflow: hidden; font-size: 13px; font-weight: 750;");
    expect(gatewayStyles).toContain(".sidebar-settings { display: grid; width: 32px; min-width: 32px; height: 32px; flex: 0 0 32px;");
  });

  test("auto-hides only the themed scrollbar appearance without intercepting scrolling", () => {
    for (const relative of ["../src/webui/app.js", "../src/gateway_webui/app.js"]) {
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
