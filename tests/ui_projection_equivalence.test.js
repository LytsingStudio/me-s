"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { installDirectFrontendRuntime } = require("./webui_runtime_stub.js");
globalThis.MeMarkdown = require("../src/webui/markdown.js");

function loadRuntime() {
  installDirectFrontendRuntime();
  new Function(readFileSync(join(import.meta.dir, "../src/webui/tool-presenters.js"), "utf8"))();
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate shared app.js");
  return new Function("document", "performance", "matchMedia", "MeTranscript", "MeToolPresenters", `
    ${source.slice(0, eventBindings)}
    return { state, createAgentStore, installProjectionState, projectedContextBreakdown,
      latestSystemPromptState, renderMessageHtml };
  `)(
    { querySelector: () => null, cookie: "", location: { protocol: "http:", port: "38199" },
      documentElement: { classList: { toggle() {} }, dataset: {} } },
    { now: () => 0 }, () => ({ matches: false, addEventListener() {} }),
    { reconcileHtmlChildren(container, html) { container.innerHTML = html; } }, globalThis.MeToolPresenters,
  );
}

function publicMessages(messages) {
  return JSON.parse(JSON.stringify(messages, (key, value) => key.startsWith("_") ? undefined : value));
}

function renderedMessages(runtime, messages) {
  return messages.map((message, index) => runtime.renderMessageHtml(
    message,
    index > 0 && ["tool", "worker-activity"].includes(messages[index - 1].kind),
    index + 1 < messages.length && ["tool", "worker-activity"].includes(messages[index + 1].kind),
  ));
}

// The backend tests derive and verify these same fixtures from their authoritative events.
const fixture = JSON.parse(readFileSync(join(import.meta.dir, "fixtures/ui_projection_equivalence.json"), "utf8"));

describe("generic UI projection fixture consumption", () => {
  for (const entry of fixture.cases) {
    test(`${entry.name} preserves the backend projection and its shared rendering`, () => {
      const runtime = loadRuntime();
      const expected = entry.projection;
      const store = runtime.createAgentStore({ id: "main" });
      runtime.state.stores.set("main", store);
      runtime.state.selectedAgent = "main";
      const count = expected.messages.length;
      const projectionState = {
        agent_id: "main", revision: "fixture", count, changed_from: 0,
        api_state: expected.apiState, api_usage: expected.apiUsage, model: expected.model,
        effort: expected.effort, turn_state: expected.turnState, summary: expected.summary,
        workmap: expected.workmap, context: expected.context, system_prompt: expected.systemPrompt,
      };
      runtime.installProjectionState(store, projectionState, {
        agent_id: "main", revision: "fixture", count, start: 0, end: count,
        projections: structuredClone(expected.messages),
      });
      expect(publicMessages(store.projection.messages)).toEqual(expected.messages);
      expect(store.projection.model).toBe(expected.model);
      expect(store.projection.effort).toBe(expected.effort);
      expect(store.projection.apiState).toEqual(expected.apiState);
      expect(store.projection.apiUsage).toEqual(expected.apiUsage);
      expect(store.projection.turnState).toEqual(expected.turnState && {
        state: expected.turnState.state, promptId: expected.turnState.prompt_id,
      });
      expect(store.summary.turnState).toEqual(expected.summary.turn_state);
      expect(store.workmap).toEqual(expected.workmap);
      expect(runtime.projectedContextBreakdown(store)).toEqual({
        total: expected.context.total, values: expected.context.values,
        compactContent: expected.context.compact_content, compactAnalysis: expected.context.compact_analysis,
        memoryContent: expected.context.memory_content,
      });
      expect(runtime.latestSystemPromptState().eventId).toEqual(expected.systemPrompt.event_id);
      expect(renderedMessages(runtime, store.projection.messages)).toEqual(renderedMessages(runtime, expected.messages));
      expect(store).not.toHaveProperty("events");
    });
  }
});
