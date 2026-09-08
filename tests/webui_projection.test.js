"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

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
      createAgentStore, installProjectionState, applyCompactApiActivity, projectedContextBreakdown,
      workerActivityView, messageRenderRevision,
      toolBrief,
      renderToolCard,
      toolImageItems,
      updateToolImageGallery,
      renderMessageHtml,
    };`);
  const runtime = factory(
    { querySelector: () => null, documentElement: { classList: { toggle() {} } } },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
    loadToolPresenters(),
  );
  return runtime;
}

describe("WebUI projection presentation", () => {
  test("View and Send show ordered previews while collapsed and preserve a stable gallery", () => {
    const runtime = loadProjectionRuntime();
    runtime.state.selectedAgent = "main";
    const images = ["a", "b"].map((key) => ({ image_event_id: 12, image: {
      source: "/private/source.png", sha256: key.repeat(64), format: "PNG", width: 1200, height: 800,
    } }));
    for (const name of ["Image.View", "Image.Send"]) {
      const tool = { id: 7, name, args: {}, started: 1000, output: "", updates: [],
        result: { state: "Succeeded", finished: 2000, detail: JSON.stringify(name === "Image.View" ? images[0] : { images }) } };
      const items = runtime.toolImageItems(tool);
      expect(items).toHaveLength(name === "Image.View" ? 1 : 2);
      expect(items[0].preview).toBe(`/api/images/main/${"a".repeat(64)}/preview`);
      if (name === "Image.Send") expect(items[1].original).toContain("b".repeat(64));
      const html = runtime.renderToolCard(tool);
      expect(html).toContain("<me-image-gallery");
      expect(html).not.toContain('class="tool-details"');
      expect(html).not.toContain("/private/source.png");
      const existing = { dataset: { items: JSON.stringify(items) } };
      runtime.updateToolImageGallery({ querySelector: () => existing }, tool);
      runtime.state.expandedTools.add("main:7");
      runtime.updateToolImageGallery({ querySelector: () => existing }, tool);
      runtime.state.expandedTools.clear();
      for (const result of [null, { ...tool.result, state: "Failed" }]) {
        expect(runtime.toolImageItems({ ...tool, result })).toEqual([]);
      }
    }
  });

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

  test("preserves entered user whitespace without adding template spacing and aligns notices", () => {
    const runtime = loadProjectionRuntime();
    const plain = runtime.renderMessageHtml({ kind: "user", content: "plain\nsecond" }, false);
    expect(plain).toContain('<div class="user-message-content">plain\nsecond</div>');
    expect(plain).not.toContain('class="user-message-content"> plain');

    const indented = runtime.renderMessageHtml({ kind: "user", content: "  indented\n\tcontinued" }, false);
    expect(indented).toContain('<div class="user-message-content">  indented\n\tcontinued</div>');
    expect(runtime.renderMessageHtml({ kind: "notice", content: "first\nsecond" }, false))
      .toContain('<div class="notice-content">first\nsecond</div>');

    const styles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(styles).toContain(".user-message-content { min-width: 0; line-height: 1.55;");
    expect(styles).toContain(".message-block.notice, .message-block.session { align-items: baseline; }");
    expect(styles).toContain(".notice-content, .session-content { color: var(--muted); line-height: 1.55;");
  });

  test("uses normalized context categories and optional previews from projection state", () => {
    const runtime = loadProjectionRuntime();
    const context = { total: 10000,
      values: { system: 6000, compact: 0, memory: 0, user: 2000, model: 1000, tool: 1000 },
      compact_content: "summary", compact_analysis: "analysis", memory_content: "history" };
    const store = runtime.createAgentStore({ id: "main" });
    runtime.installProjectionState(store, { agent_id: "main", revision: "1", count: 0, context });
    expect(runtime.projectedContextBreakdown(store)).toEqual({
      total: 10000, values: context.values, compactContent: "summary",
      compactAnalysis: "analysis", memoryContent: "history",
    });
  });

  test("applies transient Compact SSE counts to the resident notice and keeps stage updates", () => {
    const runtime = loadProjectionRuntime();
    for (const [kind, total] of [["MainAgentMultiTurn", 6], ["ManagerMultiTurn", 7], ["WorkerSingleTurn", 1]]) {
      const store = runtime.createAgentStore({ id: "main" });
      for (let stage = 1; stage <= total; stage++) {
        const notice = { key: "compact:1", revision: stage, kind: "notice", content: `正在压缩 (${stage}/${total}) ...` };
        runtime.installProjectionState(store, {
          agent_id: "main", revision: String(stage), count: 1, changed_from: 0,
          compact_activity: { compact_id: 1, kind, total_stages: total, stage, message_key: notice.key },
        }, { agent_id: "main", revision: String(stage), count: 1, start: 0, end: 1, projections: [notice] });
        const projection = store.projection;
        const changes = runtime.applyCompactApiActivity(projection, { active: true, receivedSseEvents: 37 });
        expect(changes.transcriptFrom).toBe(0);
        expect(projection.messages[0].content).toBe(`正在压缩 (${stage}/${total}) ... ↓ 37`);
        expect(runtime.applyCompactApiActivity(projection, { active: true, receivedSseEvents: 37 }).transcript).toBe(false);
        runtime.applyCompactApiActivity(projection, { active: false, receivedSseEvents: 0 });
        expect(projection.messages[0].content).toBe(`正在压缩 (${stage}/${total}) ...`);
      }
      for (const [index, content] of ["上下文已压缩", "压缩中断", "压缩失败"].entries()) {
        const revision = `end-${index}`;
        runtime.installProjectionState(store, { agent_id: "main", revision, count: 1, changed_from: 0 },
          { agent_id: "main", revision, count: 1, start: 0, end: 1,
            projections: [{ key: "compact:1", revision, kind: "notice", content }] });
        expect(runtime.applyCompactApiActivity(store.projection, { active: true, receivedSseEvents: 99 }).transcript).toBe(false);
        expect(store.projection.messages[0].content).toBe(content);
      }
    }
  });

  test("installs WorkMap with the projection revision without deriving mutation records", () => {
    const runtime = loadProjectionRuntime();
    const store = runtime.createAgentStore({ id: "main" });
    const workmap = { memory: { facts: [], agreements: [] }, history: [], recordCount: 2, current: {
      objective: { id: "objective-1", title: "Ship", state: "active" },
      plans: [{ plan: { id: "plan-1", title: "Build", state: "active", order: 0 }, notes: [] }],
    } };
    runtime.installProjectionState(store, { agent_id: "main", revision: "1", count: 0, workmap });
    expect(store.workmap).toBe(workmap);
    expect(store.projectionChanges.workmap).toBe(true);
    const next = structuredClone(workmap);
    next.current.plans[0].plan.state = "completed";
    next.current.plans[0].notes.push({ id: "note-1", content: "verified" });
    runtime.installProjectionState(store, { agent_id: "main", revision: "2", count: 0, workmap: next });
    expect(store.workmap).toBe(next);
    expect(store.projectionRevision).toBe("2");
  });

  test("renders embedded worker activity without reading another session", () => {
    const runtime = loadProjectionRuntime();
    const wait = { id: 7, revision: 1, activity: { revision: 1, state: "running",
      tools: [{ id: 8, name: "File.Read", args: { path: "file.txt" }, result: null }],
    } };
    const message = { key: "tool:7", kind: "worker-activity", tool: wait };
    expect(runtime.workerActivityView(wait)).toMatchObject({ status: "running", title: "正在执行" });
    const previous = runtime.messageRenderRevision(message, false);
    wait.activity = { revision: 2, state: "completed", tools: [{ ...wait.activity.tools[0], result: { state: "Succeeded" } }] };
    expect(runtime.workerActivityView(wait)).toMatchObject({ status: "succeeded", title: "已完成" });
    expect(runtime.messageRenderRevision(message, false)).not.toBe(previous);
    expect(runtime.state.stores.size).toBe(0);
  });
});
