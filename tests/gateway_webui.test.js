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
      emptyWorkMap, projectWorkMap, consumeWorkMapEvents, scopedApiPath: typeof scopedApiPath === "function" ? scopedApiPath : null,
      emptyGatewayWorkspaceState: typeof emptyGatewayWorkspaceState === "function" ? emptyGatewayWorkspaceState : null,
      resolveEditedDefaultModel: typeof resolveEditedDefaultModel === "function" ? resolveEditedDefaultModel : null,
      blankGatewayModel: typeof blankGatewayModel === "function" ? blankGatewayModel : null,
      modelSettingsHtml: typeof modelSettingsHtml === "function" ? modelSettingsHtml : null,
      persistGatewaySelection: typeof persistGatewaySelection === "function" ? persistGatewaySelection : null,
      directoryParentRequest: typeof directoryParentRequest === "function" ? directoryParentRequest : null,
      displayHostPath: typeof displayHostPath === "function" ? displayHostPath : null,
      renderTranscript: typeof renderTranscript === "function" ? renderTranscript : null,
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

  test("namespaces child APIs and allocates independent Workspace stores", () => {
    const gateway = loadRuntime("../src/gateway_webui/app.js");
    gateway.state.workspaceId = "w-one";
    expect(gateway.scopedApiPath("/api/sync")).toBe("/api/workspaces/w-one/sync");
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
});
