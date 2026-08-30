"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
require("../src/webui/edb-cache.js");
const { installDirectFrontendRuntime } = require("./webui_runtime_stub.js");


function loadTerminalScrollHelpers() {
  installDirectFrontendRuntime();
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI terminal preview runtime");
  const factory = new Function("document", "performance", "matchMedia", `${source.slice(0, eventBindings)}
    return { terminalIsNearBottom, captureTerminalScroll, restoreTerminalScroll, terminalWindowRange };`);
  return factory(
    { querySelector: () => null, documentElement: { classList: { toggle() {} } } },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
  );
}

function viewport(scrollHeight, clientHeight, scrollTop) {
  return {
    scrollHeight,
    clientHeight,
    _scrollTop: scrollTop,
    get scrollTop() { return this._scrollTop; },
    set scrollTop(value) {
      this._scrollTop = Math.max(0, Math.min(Number(value), this.scrollHeight - this.clientHeight));
    },
  };
}

describe("WebUI Terminal windowed full-buffer preview", () => {
  const helpers = loadTerminalScrollHelpers();

  test("continues following appended output while already at the bottom", () => {
    const view = viewport(1_000, 300, 700);
    const saved = helpers.captureTerminalScroll(view, false);
    expect(saved.followBottom).toBe(true);
    view.scrollHeight = 1_500;
    helpers.restoreTerminalScroll(view, saved);
    expect(view.scrollTop).toBe(1_200);
  });

  test("preserves the reader's position while inspecting older output", () => {
    const view = viewport(1_000, 300, 240);
    const saved = helpers.captureTerminalScroll(view, false);
    expect(saved.followBottom).toBe(false);
    view.scrollHeight = 1_500;
    helpers.restoreTerminalScroll(view, saved);
    expect(view.scrollTop).toBe(240);
  });

  test("a newly selected Terminal starts at its latest output", () => {
    const view = viewport(1_000, 300, 100);
    const saved = helpers.captureTerminalScroll(view, true);
    view.scrollHeight = 2_000;
    helpers.restoreTerminalScroll(view, saved);
    expect(view.scrollTop).toBe(1_700);
  });

  test("keeps a bounded DOM window across very large Terminal histories", () => {
    expect(helpers.terminalWindowRange(100_000, 0, 700, 17.5, true))
      .toEqual({ start: 99_760, end: 100_000 });
    const middle = helpers.terminalWindowRange(100_000, 17_500, 700, 17.5, false);
    expect(middle).toEqual({ start: 880, end: 1_120 });
    expect(middle.end - middle.start).toBe(240);
  });

  test("does not immediately resync a Terminal already known to be unavailable", () => {
    const script = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(script).toContain("if (!unavailable) requestHttpSyncNow();");
    expect(script).toContain('(frame.rows || []).slice(range.start, range.end)');
  });

  test("removes viewport-size rejection and clips horizontal overflow", () => {
    const script = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const css = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(script).not.toContain("请扩大窗口");
    expect(script).not.toContain("terminalCapacity");
    expect(css).toContain("overflow-x: hidden");
    expect(css).toContain("overflow-y: auto");
    expect(css).toContain(".terminal-spacer { width: 1px;");
  });
});
