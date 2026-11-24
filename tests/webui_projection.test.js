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

function loadProjectionRuntime(options = {}) {
  Object.assign(installDirectFrontendRuntime(), options.runtime || {});
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI projection runtime");
  const factory = new Function("document", "performance", "matchMedia", "MeToolPresenters", `${source.slice(0, eventBindings)}
    const notices = []; toast = (...args) => notices.push(args);
    return {
      state,
      createAgentStore, installProjectionState, applyCompactApiActivity, projectedContextBreakdown,
      workerActivityView, messageRenderRevision,
      toolBrief,
      renderToolCard,
      toolImageItems,
      toolFileCardsHtml, updateToolFileCards, downloadToolFile, toolFileDownloads, notices,
      updateToolImageGallery,
      renderMessageHtml,
    };`);
  const runtime = factory(
    { querySelector: () => null, querySelectorAll: () => options.buttons || [], documentElement: { classList: { toggle() {} } } },
    { now: options.now || (() => 0) },
    () => ({ matches: false, addEventListener: () => {} }),
    loadToolPresenters(),
  );
  return runtime;
}

describe("WebUI projection presentation", () => {
  test("file cards render without fetching contents, including image files", () => {
    const runtime = loadProjectionRuntime();
    const tool = { id: 8, name: "File.Send", args: {}, started: 0, result: { state: "Succeeded", finished: 1, detail: JSON.stringify({ files: [
      { path: "/output/picture.png", name: '<picture>.png', size: 1024 },
      { path: "/output/empty.txt", name: "empty.txt", size: 0 },
    ] }) } };
    const html = runtime.renderToolCard(tool);
    expect(html).toContain('class="tool-files"');
    expect(html).toContain("&lt;picture&gt;.png");
    expect(html).toContain("1.0 KiB");
    expect(html).toContain("0 B");
    expect(html).not.toMatch(/<img|<me-image-gallery|href=|src=/);
    const files = [{ path: "/output/a", name: "a", size: 1 }];
    const existing = { dataset: { files: JSON.stringify(files) } };
    runtime.updateToolFileCards({ querySelector: () => existing }, { name: "File.Send", result: { state: "Succeeded", detail: JSON.stringify({ files }) } });
  });

  test("click download uses captured workspace and reports progress only inside its file card", async () => {
    let report, finish, now = 1000; const calls = [];
    const status = { textContent: "" };
    const key = JSON.stringify(["w1", "main", 8, 0]);
    const button = { dataset: { downloadKey: key, workspace: "w1", fileDownload: "/output/report.pdf" },
      setAttribute() {}, closest: () => ({ querySelector: () => status }) };
    const runtime = loadProjectionRuntime({ buttons: [button], now: () => now, runtime: {
      apiPath: (path, workspace) => `/api/workspaces/${workspace}${path.slice(4)}`,
      fetch: async (path, options) => { calls.push([path, JSON.parse(options.body)]); return { ok: true, json: async () => ({ state: "ready", download_id: "one", filename: "report.pdf", size_bytes: 2048 }) }; },
      downloadFile: (path, filename, options) => {
        calls.push([path, filename]); report = options.onProgress;
        return new Promise((resolve) => { finish = resolve; });
      },
    } });
    const pending = runtime.downloadToolFile(button);
    await new Promise((resolve) => setTimeout(resolve, 0));
    runtime.state.workspaceId = "other";
    now = 1500;
    report(1024);
    expect(status.textContent).toContain("1.0 KiB / 2.0 KiB");
    expect(status.textContent).toContain("2.0 KiB/s");
    runtime.state.workspaceId = "w1"; runtime.state.selectedAgent = "main";
    expect(runtime.toolFileCardsHtml({ id: 8, name: "File.Send", result: { state: "Succeeded", detail: JSON.stringify({ files: [{ path: "/output/report.pdf", name: "report.pdf", size: 2048 }] }) } })).toContain('aria-valuenow="50"');
    runtime.state.workspaceId = "other";
    expect(button.innerHTML).toContain('aria-valuenow="50"');
    expect(button.disabled).toBe(true);
    await runtime.downloadToolFile(button);
    expect(calls).toHaveLength(2);
    finish({ size: 2048 }); await pending;
    expect(calls[0]).toEqual(["/api/workspaces/w1/files/downloads/create", { sources: ["/output/report.pdf"], file_only: true }]);
    expect(calls[1]).toEqual(["/api/workspaces/w1/files/downloads/one/content", "report.pdf"]);
    expect(calls[2][0]).toBe("/api/workspaces/w1/files/downloads/cancel");
    expect(status.textContent).toBe("下载完成 · 2.0 KiB / 2.0 KiB");
    expect(button.disabled).toBe(false);
    expect(runtime.toolFileDownloads.size).toBe(0);
    expect(runtime.notices).toEqual([]);
  });

  test("failed file download clears progress and can be retried", async () => {
    let fail = true; const status = { textContent: "" };
    const button = { dataset: { downloadKey: "failure", workspace: "w1", fileDownload: "/output/a" },
      setAttribute() {}, closest: () => ({ querySelector: () => status }) };
    const runtime = loadProjectionRuntime({ buttons: [button], runtime: {
      fetch: async () => ({ ok: true, json: async () => ({ state: "ready", download_id: "one", filename: "a", size_bytes: 0 }) }),
      downloadFile: async () => { if (fail) throw new Error("连接中断"); return { size: 0 }; },
    } });
    await runtime.downloadToolFile(button);
    expect(status.textContent).toBe("下载失败，点击重试");
    expect(runtime.notices).toEqual([["连接中断", true]]);
    expect(runtime.toolFileDownloads.size).toBe(0);
    expect(button.disabled).toBe(false);
    fail = false; await runtime.downloadToolFile(button);
    expect(status.textContent).toBe("下载完成 · 0 B / 0 B");
    expect(runtime.toolFileDownloads.size).toBe(0);
  });

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
