"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

const source = readFileSync(join(import.meta.dir, "../src/webui/tool-presenters.js"), "utf8");
new Function(source)();
const presenters = globalThis.MeToolPresenters;

function succeeded(value, updates = []) {
  return {
    text: "",
    updates,
    result: { state: "Succeeded", exitCode: null, value, rawDetail: JSON.stringify(value) },
  };
}

describe("shared WebUI tool presenters", () => {
  test("explicitly covers every first-party and historical compatibility tool", () => {
    expect(presenters.KNOWN_TOOLS).toHaveLength(57);
    expect(new Set(presenters.KNOWN_TOOLS).size).toBe(57);
    expect(presenters.names().sort()).toEqual([...presenters.KNOWN_TOOLS].sort());
    for (const name of presenters.KNOWN_TOOLS) expect(presenters.has(name)).toBe(true);
  });

  test("keeps every input summary on one stable line", () => {
    for (const name of presenters.KNOWN_TOOLS) {
      const first = presenters.summarize(name, {});
      const second = presenters.summarize(name, {});
      expect(first).toEqual(second);
      expect(first.title).not.toBe("");
      expect(first.summary).not.toMatch(/[\r\n]/);
    }
  });

  test("omits output blocks until output actually exists", () => {
    for (const name of presenters.KNOWN_TOOLS) {
      const pending = presenters.describe(name, {}, undefined);
      expect(pending.inputBlocks.length).toBeGreaterThan(0);
      expect(pending.outputBlocks).toEqual([]);
      expect(presenters.renderDetails(pending)).not.toContain("tool-output-section");

      const completed = presenters.describe(name, {}, succeeded({}));
      expect(completed.outputBlocks.length).toBeGreaterThan(0);
      expect(presenters.renderDetails(completed)).toContain("tool-output-section");
    }
  });

  test("renders CurrentTime as one no-argument instant with all result fields", () => {
    expect(presenters.summarize("CurrentTime", {})).toMatchObject({
      title: "查询当前时间", summary: "",
    });
    const details = presenters.describe("CurrentTime", {}, succeeded({
      local_rfc3339: "2026-08-25T12:34:56.789+08:00",
      utc_rfc3339: "2026-08-25T04:34:56.789Z",
      utc_offset: "+08:00",
      weekday: "Tuesday",
      unix_timestamp_ms: 1787632496789,
    }));
    const html = presenters.renderDetails(details);
    expect(html).toContain("无参数");
    expect(html).toContain("2026-08-25T12:34:56.789+08:00");
    expect(html).toContain("2026-08-25T04:34:56.789Z");
    expect(html).toContain("+08:00");
    expect(html).toContain("Tuesday");
    expect(html).toContain("1787632496789");
  });

  test("renders File.Read as line-numbered content instead of result JSON", () => {
    const input = { path: "src/app.js", start_line: 10, end_line: 11 };
    const summary = presenters.summarize("File.Read", input);
    expect(summary).toMatchObject({ title: "读取文件", summary: "src/app.js · 第 10–11 行" });
    const details = presenters.describe("File.Read", input, succeeded({
      path: "src/app.js", lines: { "10": "const a = 1;", "11": "const b = 2;" },
      editable_ranges: [{ start_line: 10, end_line: 11 }], start_line: 10, end_line: 11,
      total_lines: 30, eof: false, truncated: true, hash: "0123abcd", size: 300,
      encoding: "utf-8", encoding_confidence: 1, bom: false,
    }));
    const html = presenters.renderDetails(details);
    expect(html).toContain("文件内容");
    expect(html).toContain("const a = 1;");
    expect(html).toContain("已授权编辑范围");
  });

  test("renders Terminal updates as terminal text rather than patch JSON", () => {
    const output = succeeded({ session_id: "pty-10", sequence: 2, state: "running", exit_code: null, truncated: false }, [
      { kind: "terminal", value: { rows: [{ row: 2, runs: [{ col: 0, width: 5, text: "hello" }] }] } },
    ]);
    const details = presenters.describe("Terminal.Interact", {
      session_id: "pty-10", input: [{ type: "text", text: "echo hello" }, { type: "key", key: "enter" }],
    }, output);
    const html = presenters.renderDetails(details);
    const primary = html.slice(0, html.indexOf('<details class="tool-raw"'));
    expect(html).toContain("echo hello");
    expect(html).toContain("本次终端更新");
    expect(html).toContain("hello");
    expect(primary).not.toContain("&quot;runs&quot;");
    expect(html).toContain("&quot;runs&quot;");
  });

  test("renders Desktop.Play operations, capture geometry, failure, and cleanup", () => {
    const input = { operations: [
      { kind: "key_down", key: "shift" },
      { kind: "delay", delay_ms: 250 },
      { kind: "capture", clip: { x: 100, y: 200, width: 640, height: 480 } },
    ] };
    expect(presenters.summarize("Desktop.Play", input)).toMatchObject({
      title: "操作桌面", summary: "3 个桌面操作 · 操作后截图",
    });
    const details = presenters.describe("Desktop.Play", input, succeeded({
      state: "failed", operation_count: 3, completed_operations: 3, failed_operation_index: null,
      captures: [{ operation_index: 2, path: ".me/tmp/desktop/capture-00ab12.png", width: 640, height: 480, full_width: 2560, full_height: 1440, clip: { x: 100, y: 200, width: 640, height: 480 } }],
      auto_released: ["key:shift"], cleanup_errors: [],
      error: { code: "desktop_input_failed", message: "release failed", retryable: false },
    }));
    const html = presenters.renderDetails(details);
    expect(html).toContain("执行顺序");
    expect(html).toContain(".me/tmp/desktop/capture-00ab12.png");
    expect(html).toContain("2560×1440");
    expect(html).toContain("(100, 200) 640×480");
    expect(html).toContain("key:shift");
    expect(html).toContain("desktop_input_failed");
  });

  test("renders browser snapshot tree, screenshot path, and browser events", () => {
    const details = presenters.describe("WebBrowser.Snapshot", { page_id: "p0000001", wait_ms: 1000, kind: "both" }, succeeded({
      page_id: "p0000001", snapshot_id: 2, url: "https://example.com", title: "Example", state: "complete",
      kind: "both", accessibility_tree: "- heading Example [ref=e1]", screen_path: ".me/example.png",
      browser_events: [{ kind: "console", level: "warning", message: "warning" }], dropped_browser_events: 0,
    }));
    const html = presenters.renderDetails(details);
    expect(html).toContain("Accessibility Tree");
    expect(html).toContain("heading Example");
    expect(html).toContain(".me/example.png");
    expect(html).toContain("浏览器事件");
  });

  test("shows a clear failure block and keeps raw data collapsed", () => {
    const output = {
      text: "",
      updates: [],
      result: { state: "Failed", exitCode: null, value: { code: "not_found", message: "missing", tip: "check path" }, rawDetail: "{\"code\":\"not_found\",\"message\":\"missing\",\"tip\":\"check path\"}" },
    };
    const html = presenters.renderDetails(presenters.describe("File.Read", { path: "missing.txt" }, output));
    expect(html).toContain("执行失败");
    expect(html).toContain("missing");
    expect(html).toContain("错误码：not_found");
    expect(html).toContain("<details class=\"tool-raw\"");
    expect(html).not.toMatch(/<details[^>]*\sopen(?:[=>\s])/);
  });

  test("uses a safe structured fallback for custom tools", () => {
    const summary = presenters.summarize("Custom.Deploy", { path: "dist", force: true });
    expect(summary).toMatchObject({ title: "Custom.Deploy", summary: "dist", known: false });
    const html = presenters.renderDetails(presenters.describe("Custom.Deploy", { path: "<unsafe>", force: true }, succeeded({ ok: true })));
    expect(html).toContain("Path");
    expect(html).toContain("&lt;unsafe&gt;");
    expect(html).not.toContain("<unsafe>");
  });

  test("presents every WorkMap action and structured detail in direct Chinese", () => {
    const titles = {
      "WorkMap.Read": "查看工作图",
      "WorkMap.ReadHistory": "查看工作图历史",
      "WorkMap.Start": "创建目标",
      "WorkMap.UpdatePlanState": "更新计划",
      "WorkMap.AddNote": "添加笔记",
      "WorkMap.ChangePlan": "修改计划",
      "WorkMap.AddPlan": "添加计划",
      "WorkMap.CloseObjective": "关闭目标",
      "WorkMap.AddMemory": "添加记忆",
      "WorkMap.InvalidateMemory": "更新记忆",
    };
    for (const [name, title] of Object.entries(titles)) {
      expect(presenters.summarize(name, {}).title).toBe(title);
    }

    const current = presenters.describe("WorkMap.Read", {}, succeeded({
      memory: { facts: [{}], agreements: [{}] },
      current: {
        objective: { id: "objective-1", title: "目标", state: "active" },
        plans: [{ plan: { id: "plan-1", order: 1, title: "计划", state: "active" } }],
      },
    }));
    expect(current.outputBlocks.map((block) => block.title)).toEqual(["记忆", "目标", "计划"]);
    expect(current.outputBlocks[0].entries.map((entry) => entry.label)).toEqual(["事实", "约定"]);

    const note = presenters.describe("WorkMap.AddNote", {
      plan_id: "plan-1", kind: "finding", content: "发现内容",
    }, succeeded({ records: [{ kind: "note", record: { id: "note-1", content: "发现内容" } }] }));
    expect(presenters.summarize("WorkMap.AddNote", { plan_id: "plan-1", kind: "finding", content: "发现内容" }).summary).toContain("发现");
    expect(note.inputBlocks[0].title).toBe("笔记");
    expect(note.inputBlocks[0].entries.map((entry) => entry.label)).toEqual(["计划", "类型"]);
    expect(note.outputBlocks[0].title).toBe("已添加笔记");
    expect(note.outputBlocks[0].rows[0].kind).toBe("笔记");

    const memory = presenters.describe("WorkMap.InvalidateMemory", {
      memory_id: "memory-1", reason: "已变化", replacement: { kind: "fact", content: "新内容" },
    }, succeeded({ records: [{ kind: "memory", record: { id: "memory-2", content: "新内容" } }] }));
    expect(presenters.summarize("WorkMap.InvalidateMemory", { memory_id: "memory-1", replacement: {} }).summary).toContain("替换");
    expect(memory.inputBlocks.map((block) => block.title)).toEqual(["原记忆", "替换内容"]);
    expect(memory.outputBlocks[0].title).toBe("记忆已更新");
    expect(memory.outputBlocks[0].rows[0].kind).toBe("记忆");
  });
});
