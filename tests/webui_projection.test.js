"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

require("../src/webui/edb-cache.js");
const { installDirectFrontendRuntime } = require("./webui_runtime_stub.js");

function loadToolPresenters() {
  const source = readFileSync(join(import.meta.dir, "../src/webui/tool-presenters.js"), "utf8");
  new Function(source)();
  return globalThis.MeToolPresenters;
}

function loadProjectionRuntime() {
  installDirectFrontendRuntime();
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI projection runtime");
  const factory = new Function("document", "performance", "matchMedia", "MeToolPresenters", `${source.slice(0, eventBindings)}
    return {
      state,
      emptyProjection,
      projectChat,
      consumeChatEvents,
      chatAppendNeedsReplay,
      emptyWorkMap,
      projectWorkMap,
      consumeWorkMapEvents,
      workerActivityIndex,
      applyCompactApiActivity,
      estimateContextBreakdown,
      toolBrief,
      renderToolCard,
      renderMessageHtml,
      eventRecoveryBacklog,
      shouldUseBulkEventRecovery,
      createEventRecovery,
      eventRecoveryProgress,
      eventRecoveryMatches,
      selectedEventRecoveryReady,
    };`);
  const runtime = factory(
    { querySelector: () => null, documentElement: { classList: { toggle() {} } } },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
    loadToolPresenters(),
  );
  runtime.state.snapshot.tool_visibility = {
    hidden_names: ["SetTitle"],
    hidden_prefixes: ["WorkMap.", "Worker."],
    activity_names: ["Worker.Wait"],
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

function assertIncrementalChatMatchesReplay(runtime, events) {
  let projection = runtime.emptyProjection();
  const prefix = [];
  for (const next of events) {
    prefix.push(next);
    if (runtime.chatAppendNeedsReplay([next])) projection = runtime.projectChat(prefix);
    else runtime.consumeChatEvents(projection, [next]);
    expect(visibleProjection(projection)).toEqual(visibleProjection(runtime.projectChat(prefix)));
  }
}

describe("WebUI incremental event projections", () => {
  test("renders each ordinary tool as one summary line until clicked open", () => {
    const runtime = loadProjectionRuntime();
    runtime.state.selectedAgent = "main";
    const tool = {
      id: 7,
      name: "File.Search",
      args: { path: ".", query: "needle" },
      started: 1_000,
      queued: false,
      output: "",
      updates: [],
      result: { state: "Succeeded", detail: JSON.stringify({ path: ".", matches: [], skipped_binary: 0, returned: 0, truncated: false }), finished: 2_000 }
    };

    expect(runtime.toolBrief(tool)).toBe("“needle” · .");
    const collapsed = runtime.renderToolCard(tool);
    expect(collapsed).toContain('class="tool-name" title="File.Search">搜索文本</span>');
    expect(collapsed).toContain('class="tool-brief">“needle” · .</span>');
    expect(collapsed).not.toContain('class="tool-details"');

    runtime.state.expandedTools.add("main:7");
    const expanded = runtime.renderToolCard(tool);
    expect(expanded).toContain('class="tool-details"');
    expect(expanded).toContain("没有找到匹配内容");

    const terminal = {
      id: 8,
      name: "Terminal.Interact",
      sessionId: "pty-8",
      args: {
        session_id: "pty-8",
        input: [{ type: "text", text: "pwd" }, { type: "key", key: "enter" }],
      },
      started: 1_000,
      queued: false,
      output: "",
      updates: [],
      result: null,
    };
    expect(runtime.toolBrief(terminal)).toBe("pty-8 · pwd Enter");

    const compact = runtime.renderMessageHtml({ kind: "tool", tool }, false, true);
    const separated = runtime.renderMessageHtml({ kind: "tool", tool }, false, false);
    expect(compact).toContain("follows-tool");
    expect(separated).not.toContain("follows-tool");
  });

  test("uses the persisted normalized context categories", () => {
    const runtime = loadProjectionRuntime();
    const usage = { input_tokens: 9_000, output_tokens: 1_000, total_tokens: 10_000 };
    const events = [
      event("ModelChanged", 1, { model: "model-a", cause: "Initial" }),
      event("UserPrompt", 2, { content: "tiny prompt" }),
      event("ApiStateUpdate", 3, {
        api_call_id: 3, prompt_id: 2, state: "Completed", usage,
      }),
      event("ContextUsageEstimate", 4, {
        api_state_event_id: 3,
        values: { system: 6_000, compact: 0, memory: 0, user: 2_000, model: 1_000, tool: 1_000 },
      }),
    ];
    expect(runtime.estimateContextBreakdown(events, usage, null).values).toEqual({
      system: 6_000,
      compact: 0,
      memory: 0,
      user: 2_000,
      model: 1_000,
      tool: 1_000,
    });
  });

  test("matches full replay throughout a streamed tool turn", () => {
    const runtime = loadProjectionRuntime();
    assertIncrementalChatMatchesReplay(runtime, [
      event("ModelChanged", 1, { model: "model-a", cause: "Initial" }),
      event("ReasoningEffortChanged", 2, { effort: "high", cause: "Initial" }),
      event("UserPrompt", 3, { content: "hello" }),
      event("ApiStateUpdate", 4, { api_call_id: "api-1", prompt_id: 3, state: "Requesting" }),
      event("AssistResponse", 5, { prompt_id: 3, content: "I will check.\n", finished: false }),
      event("ToolCall", 6, {
        id: 6, api_call_id: "api-1", prompt_id: 3, name: "Terminal.Create", arguments: "{}",
      }),
      event("ApiStateUpdate", 7, {
        api_call_id: "api-1", prompt_id: 3, state: "Completed",
        usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15 },
      }),
      event("ToolInfoUpdate", 8, { tool_call_id: 6, content: { kind: "text", value: "ready\n" } }),
      event("ToolCallResult", 9, { tool_call_id: 6, state: "Succeeded", exit_code: 0, detail: "{}" }),
      event("ApiStateUpdate", 10, { api_call_id: "api-2", prompt_id: 3, state: "Requesting" }),
      event("AssistResponse", 11, { prompt_id: 3, content: "done", finished: false }),
      event("AssistResponse", 12, { prompt_id: 3, content: "!", finished: true }),
      event("ApiStateUpdate", 13, {
        api_call_id: "api-2", prompt_id: 3, state: "Completed",
        usage: { input_tokens: 20, output_tokens: 8, total_tokens: 28 },
      }),
      event("AgentTurn", 14, { turn_id: 1, prompt_id: 3, state: "Completed" }),
    ]);
  });

  test("reports the earliest changed message instead of invalidating all history", () => {
    const runtime = loadProjectionRuntime();
    const projection = runtime.emptyProjection();
    const user = runtime.consumeChatEvents(projection, [
      event("UserPrompt", 1, { content: "hello" }),
    ]);
    expect(user.transcriptFrom).toBe(0);

    const firstLine = runtime.consumeChatEvents(projection, [
      event("AssistResponse", 2, { prompt_id: 1, content: "line one\n", finished: false }),
    ]);
    expect(firstLine.transcriptFrom).toBe(1);

    const secondLine = runtime.consumeChatEvents(projection, [
      event("AssistResponse", 3, { prompt_id: 1, content: "line two", finished: false }),
    ]);
    expect(secondLine.transcriptFrom).toBe(1);

    const toolCall = runtime.consumeChatEvents(projection, [event("ToolCall", 4, {
      id: 4, api_call_id: "api-1", prompt_id: 1, name: "Terminal.Create", arguments: "{}",
    })]);
    expect(toolCall.transcriptFrom).toBe(2);
    const toolUpdate = runtime.consumeChatEvents(projection, [
      event("ToolInfoUpdate", 5, { tool_call_id: 4, content: { kind: "text", value: "ready" } }),
    ]);
    expect(toolUpdate.transcriptFrom).toBe(2);
  });

  test("projects Compact stages and applies transient SSE activity without persisting it", () => {
    const runtime = loadProjectionRuntime();
    const events = [
      event("CompactStateUpdate", 1, {
        compact_id: 1, tool_call_id: 0, prompt_id: 0,
        kind: "MainAgentMultiTurn", total_stages: 6, state: "Started", stage: null,
      }),
      event("ApiStateUpdate", 2, {
        api_call_id: 100, prompt_id: 0, state: "Completed",
        usage: { input_tokens: 20_000, output_tokens: 1_234, total_tokens: 21_234 },
      }),
      event("CompactStateUpdate", 3, {
        compact_id: 1, tool_call_id: 0, prompt_id: 0,
        kind: "MainAgentMultiTurn", total_stages: 6,
        state: "StageCompleted", stage: "Analysis",
      }),
      event("ApiStateUpdate", 4, {
        api_call_id: 101, prompt_id: 0, state: "Error", detail: "network",
        usage: { input_tokens: 20_000, output_tokens: 2_366, total_tokens: 22_366 },
      }),
      event("ApiStateUpdate", 5, {
        api_call_id: 101, prompt_id: 0, state: "Interrupted", detail: "closed",
      }),
      event("CompactStateUpdate", 6, {
        compact_id: 1, tool_call_id: 0, prompt_id: 0,
        kind: "MainAgentMultiTurn", total_stages: 6,
        state: "Failed", stage: null, detail: "failed",
      }),
    ];
    const projection = runtime.emptyProjection();
    runtime.consumeChatEvents(projection, events.slice(0, 1));
    expect(projection.messages[0].content).toBe("正在压缩 (1/6) ...");
    for (const [kind, total, expected] of [
      ["ManagerMultiTurn", 7, "正在压缩 (1/7) ..."],
      ["WorkerSingleTurn", 1, "正在压缩 (1/1) ..."],
    ]) {
      const kindProjection = runtime.emptyProjection();
      runtime.consumeChatEvents(kindProjection, [event("CompactStateUpdate", 20, {
        compact_id: 20, tool_call_id: 19, prompt_id: 18,
        kind, total_stages: total, state: "Started", stage: null,
      })]);
      expect(kindProjection.messages[0].content).toBe(expected);
    }
    runtime.consumeChatEvents(projection, events.slice(1, 2));
    expect(projection.messages[0].content).toBe("正在压缩 (1/6) ...");
    runtime.applyCompactApiActivity(projection, { active: true, receivedSseEvents: 37 });
    expect(projection.messages[0].content).toBe("正在压缩 (1/6) ... ↓ 37");
    runtime.applyCompactApiActivity(projection, { active: false, receivedSseEvents: 0 });
    expect(projection.messages[0].content).toBe("正在压缩 (1/6) ...");
    runtime.consumeChatEvents(projection, events.slice(2, 3));
    expect(projection.messages[0].content).toBe("正在压缩 (2/6) ...");
    runtime.consumeChatEvents(projection, events.slice(3, 4));
    expect(projection.messages[0].content).toBe("正在压缩 (2/6) ...");
    runtime.consumeChatEvents(projection, events.slice(4));
    expect(projection.messages[0].content).toBe("压缩失败");
    expect(visibleProjection(projection)).toEqual(visibleProjection(runtime.projectChat(events)));

    for (const [state, expected] of [
      ["Completed", "上下文已压缩"],
      ["Interrupted", "压缩中断"],
    ]) {
      const terminal = [events[0], event("CompactStateUpdate", 2, {
        compact_id: 1, tool_call_id: 0, prompt_id: 0,
        kind: "MainAgentMultiTurn", state, stage: null, detail: "terminal",
      })];
      expect(runtime.projectChat(terminal).messages[0].content).toBe(expected);
    }
  });

  test("replays only when an appended event invalidates visible history", () => {
    const runtime = loadProjectionRuntime();
    const events = [
      event("UserPrompt", 1, { content: "retry this" }),
      event("ApiStateUpdate", 2, { api_call_id: "bad", prompt_id: 1, state: "Requesting" }),
      event("AssistResponse", 3, { prompt_id: 1, content: "discard me", finished: false }),
      event("ApiStateUpdate", 4, { api_call_id: "bad", prompt_id: 1, state: "Error", detail: "network" }),
      event("ApiStateUpdate", 5, { api_call_id: "bad", prompt_id: 1, state: "Retrying", retry_count: 1, retry_limit: 10 }),
      event("ApiStateUpdate", 6, { api_call_id: "good", prompt_id: 1, state: "Requesting" }),
      event("AssistResponse", 7, { prompt_id: 1, content: "keep me", finished: true }),
      event("ApiStateUpdate", 8, { api_call_id: "good", prompt_id: 1, state: "Completed" }),
      event("ContextCleared", 9),
      event("UserPrompt", 10, { content: "after clear" }),
      event("ToolCall", 11, {
        id: 11, api_call_id: "compact-api", prompt_id: 10, name: "Compact", arguments: "{}",
      }),
      event("ToolCallResult", 12, { tool_call_id: 11, state: "Succeeded", detail: "{}" }),
      event("CompactStateUpdate", 13, { state: "Completed", tool_call_id: 11, prompt_id: 10 }),
    ];
    expect(runtime.chatAppendNeedsReplay([events[1]])).toBe(false);
    expect(runtime.chatAppendNeedsReplay([events[3]])).toBe(true);
    expect(runtime.chatAppendNeedsReplay([events[8]])).toBe(true);
    expect(runtime.chatAppendNeedsReplay([events[12]])).toBe(true);
    assertIncrementalChatMatchesReplay(runtime, events);
  });

  test("updates WorkMap from appended mutation records", () => {
    const runtime = loadProjectionRuntime();
    const first = event("WorkMapMutation", 1, { mutation: { records: [
      { kind: "objective", record: { id: "objective-1", title: "Ship", state: "active", created_at_ms: 1 } },
      { kind: "plan", record: { id: "plan-1", objective_id: "objective-1", title: "Build", state: "active", order: 0 } },
    ] } });
    const second = event("WorkMapMutation", 2, { mutation: { records: [
      { kind: "plan", record: { id: "plan-1", objective_id: "objective-1", title: "Build", state: "completed", order: 0 } },
      { kind: "note", record: { id: "note-1", plan_id: "plan-1", kind: "finding", content: "verified", sequence: 0 } },
    ] } });
    const incremental = runtime.emptyWorkMap();
    expect(runtime.consumeWorkMapEvents(incremental, [first])).toBe(true);
    expect(runtime.consumeWorkMapEvents(incremental, [second])).toBe(true);
    expect({ ...incremental, _records: undefined })
      .toEqual({ ...runtime.projectWorkMap([first, second]), _records: undefined });
  });

  test("locks the 100 Event recovery boundary and fixed high-water mark", () => {
    const runtime = loadProjectionRuntime();
    expect(runtime.eventRecoveryBacklog(99, 0)).toBe(99);
    expect(runtime.eventRecoveryBacklog(100, 0)).toBe(100);
    expect(runtime.eventRecoveryBacklog(101, 0)).toBe(101);
    expect(runtime.shouldUseBulkEventRecovery(99, 0)).toBe(false);
    expect(runtime.shouldUseBulkEventRecovery(100, 0)).toBe(false);
    expect(runtime.shouldUseBulkEventRecovery(101, 0)).toBe(true);
    expect(runtime.shouldUseBulkEventRecovery(151, 50)).toBe(true);

    const recovery = runtime.createEventRecovery("main", 7, 151, 50);
    expect(recovery).toEqual({ agentId: "main", mutationRevision: 7, startEventCount: 50, targetEventCount: 151 });
    expect(runtime.eventRecoveryProgress(recovery, 50)).toBe(0);
    expect(runtime.eventRecoveryProgress(recovery, 101)).toBe(51 / 101);
    expect(runtime.eventRecoveryProgress(recovery, 151)).toBe(1);
    expect(runtime.eventRecoveryProgress(recovery, 220)).toBe(1);
    expect(runtime.selectedEventRecoveryReady(recovery, "main", 7, 150)).toBe(false);
    expect(runtime.selectedEventRecoveryReady(recovery, "main", 7, 151)).toBe(true);
    expect(runtime.selectedEventRecoveryReady(recovery, "main", 7, 220)).toBe(true);
    expect(runtime.eventRecoveryMatches(recovery, "other", 7)).toBe(false);
    expect(runtime.eventRecoveryMatches(recovery, "main", 8)).toBe(false);
    expect(runtime.createEventRecovery("main", 7, 150, 50)).toBeNull();
  });

  test("advances one Worker activity index without rescanning its prefix", () => {
    const runtime = loadProjectionRuntime();
    const events = [event("ManagerPrompt", 1, { content: "inspect" })];
    runtime.state.stores.set("worker-1", { events, mutationRevision: 0 });
    const worker = { id: "worker-1" };
    const index = runtime.workerActivityIndex(worker);
    events.push(event("ToolCall", 2, {
      id: 2, api_call_id: "api-1", prompt_id: 1, name: "File.Read", arguments: "{}",
    }));
    events.push(event("ToolCallResult", 3, {
      tool_call_id: 2, state: "Succeeded", detail: "read file",
    }));
    expect(runtime.workerActivityIndex(worker)).toBe(index);
    expect(index.turns).toHaveLength(1);
    expect(index.turns[0].tools).toHaveLength(1);
    expect(index.turns[0].tools[0].result.state).toBe("Succeeded");
  });
});
