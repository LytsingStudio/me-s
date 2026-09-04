"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

require("../src/webui/edb-cache.js");
globalThis.MeMarkdown = require("../src/webui/markdown.js");

globalThis.MeFrontendRuntime = {
  capabilities: { multipleWorkspaces: true, gatewaySettings: true },
  apiPath(path) { return path; },
  createEdbCache() {
    return {
      loadScope: async () => [], discardSession: async () => {},
      saveSession() {}, renderManager() {},
    };
  },
  loadCachedSessions(cache, _snapshot, scope) { return cache.loadScope(scope); },
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

function loadRuntime() {
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate shared app.js");
  const factory = new Function(
    "document", "performance", "matchMedia", "MeTranscript", "MeToolPresenters",
    `${source.slice(0, eventBindings)}
      return {
        state, projectChat, projectAgentSummary, projectWorkMap, estimateContextBreakdown,
        renderMessageHtml,
      };
    `,
  );
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
  runtime.state.rawEdbDecoding = true;
  runtime.state.snapshot.tool_visibility = {
    hidden_names: ["SetTitle"], hidden_prefixes: ["Worker."], activity_names: ["Worker.Wait"],
  };
  return runtime;
}

function eventParts(event) {
  const kind = Object.keys(event)[0];
  return [kind, event[kind]];
}

function systemPromptProjection(events) {
  const changes = events.flatMap((event) => {
    const [kind, value] = eventParts(event);
    return kind === "SystemStaticPromptChange"
      ? [{ id: value.id, mode: value.mode, content: value.content ?? null }]
      : [];
  });
  const latest = changes[changes.length - 1];
  return latest ? {
    mode: latest.mode,
    content: latest.content,
    event_id: latest.id,
    changes,
  } : {
    mode: "Default",
    content: null,
    event_id: null,
    changes: [],
  };
}

function publicMessages(messages) {
  return JSON.parse(JSON.stringify(messages, (key, value) =>
    key.startsWith("_") ? undefined : value));
}

function rawPublicProjection(runtime, events, turnHistory) {
  const chat = runtime.projectChat(events);
  const { _records, ...workmap } = runtime.projectWorkMap(events);
  const context = runtime.estimateContextBreakdown(events, chat.apiUsage, turnHistory);
  return {
    messages: publicMessages(chat.messages),
    apiState: chat.apiState,
    apiUsage: chat.apiUsage,
    model: chat.model,
    effort: chat.effort,
    turnState: chat.turnState == null ? null : {
      state: chat.turnState.state,
      prompt_id: chat.turnState.promptId,
    },
    summary: { turn_state: runtime.projectAgentSummary(events).turnState ?? null },
    workmap,
    context: {
      total: context.total,
      values: context.values,
      compact_content: context.compactContent,
      compact_analysis: context.compactAnalysis,
      memory_content: context.memoryContent,
    },
    systemPrompt: systemPromptProjection(events),
  };
}

function renderedMessages(runtime, messages) {
  return messages.map((message, index) => runtime.renderMessageHtml(
    message,
    index > 0 && ["tool", "worker-activity"].includes(messages[index - 1].kind),
    index + 1 < messages.length && ["tool", "worker-activity"].includes(messages[index + 1].kind),
  ));
}

const fixture = JSON.parse(readFileSync(
  join(import.meta.dir, "fixtures/ui_projection_equivalence.json"),
  "utf8",
));

describe("generic UI projection equivalence", () => {
  for (const entry of fixture.cases) {
    test(`${entry.name} matches the raw EDB public projection and shared renderer`, () => {
      const runtime = loadRuntime();
      const raw = rawPublicProjection(
        runtime, entry.events, entry.projection.context.memory_content,
      );
      expect(entry.projection).toEqual(raw);
      expect(renderedMessages(runtime, entry.projection.messages))
        .toEqual(renderedMessages(runtime, raw.messages));
    });
  }
});
