"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function loadScheduler(relative, options = {}) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const start = source.indexOf("function apiSpinnerIsActive()");
  const end = source.indexOf("\n\ninitializeAuthentication();", start);
  if (start < 0 || end < 0) throw new Error(`could not isolate scheduler from ${relative}`);

  const factory = new Function(
    "state", "elements", "document", "currentProjection", "API_ACTIVE", "API_SPINNER_FRAMES",
    "setTimeout", "clearTimeout", "inputHasPriority", "formatDuration", "Date", "UI_ANIMATION_INTERVAL_MS",
    `${source.slice(start, end)}
    return {
      apiSpinnerIsActive, setApiSpinner, stopUiAnimation, uiAnimationNeeded,
      syncUiAnimationScheduler, refreshRunningToolNodes, refreshUiAnimation,
    };`,
  );

  const timers = new Map();
  let nextTimerId = 1;
  const setTimeoutFake = (callback, delay) => {
    const id = nextTimerId++;
    timers.set(id, { callback, delay });
    return id;
  };
  const clearTimeoutFake = (id) => timers.delete(id);
  const spinnerWrites = [];
  let spinnerText = "";
  let transcriptQueries = 0;
  let runningNodes = options.runningNodes || [];
  let now = options.now ?? 1000;
  let projection = { apiState: options.apiState || "Completed" };
  const state = {
    apiActivity: { active: Boolean(options.apiActive) },
    apiAnimationTick: 0,
    uiAnimationTimer: null,
    runningToolNodes: [],
    pageClosing: false,
  };
  const document = { hidden: Boolean(options.hidden) };
  const elements = {
    apiSpinner: {
      get textContent() { return spinnerText; },
      set textContent(value) { spinnerText = String(value); spinnerWrites.push(spinnerText); },
    },
    transcriptContent: {
      querySelectorAll(selector) {
        expect(selector).toBe("[data-running-started]");
        transcriptQueries += 1;
        return runningNodes;
      },
    },
  };
  const runtime = factory(
    state,
    elements,
    document,
    () => projection,
    new Set(["Requesting", "Streaming", "Retrying"]),
    ["0", "1", "2"],
    setTimeoutFake,
    clearTimeoutFake,
    () => Boolean(options.inputPriority),
    (milliseconds) => `${milliseconds}ms`,
    { now: () => now },
    100,
  );
  return {
    ...runtime,
    state,
    document,
    spinnerWrites,
    spinnerText: () => spinnerText,
    transcriptQueries: () => transcriptQueries,
    pendingTimers: () => [...timers.values()],
    runNextTimer() {
      const next = timers.entries().next();
      if (next.done) return null;
      const [id, entry] = next.value;
      timers.delete(id);
      entry.callback();
      return entry.delay;
    },
    setApiActive(active) { state.apiActivity.active = active; },
    setApiState(apiState) { projection = { apiState }; },
    setRunningNodes(nodes) { runningNodes = nodes; },
    setNow(value) { now = value; },
  };
}

const WEBUIS = ["../src/webui/app.js", "../src/gateway_webui/app.js"];

describe("WebUI on-demand animation scheduler", () => {
  test("stays completely idle without API or running tools", () => {
    for (const relative of WEBUIS) {
      const runtime = loadScheduler(relative);
      runtime.syncUiAnimationScheduler();
      runtime.syncUiAnimationScheduler();
      expect(runtime.pendingTimers()).toHaveLength(0);
      expect(runtime.transcriptQueries()).toBe(0);
      expect(runtime.spinnerWrites).toEqual([]);
    }
  });

  test("animates only while API activity is visible and clears once", () => {
    for (const relative of WEBUIS) {
      const runtime = loadScheduler(relative, { apiActive: true });
      runtime.syncUiAnimationScheduler();
      expect(runtime.pendingTimers().map(({ delay }) => delay)).toEqual([100]);
      expect(runtime.runNextTimer()).toBe(100);
      expect(runtime.spinnerText()).toBe("1");
      expect(runtime.pendingTimers()).toHaveLength(1);

      runtime.setApiActive(false);
      runtime.syncUiAnimationScheduler();
      expect(runtime.spinnerText()).toBe("");
      expect(runtime.pendingTimers()).toHaveLength(0);
      const writesAfterStop = runtime.spinnerWrites.length;
      runtime.syncUiAnimationScheduler();
      expect(runtime.spinnerWrites).toHaveLength(writesAfterStop);
    }
  });

  test("caches running nodes once and never scans transcript on animation ticks", () => {
    for (const relative of WEBUIS) {
      const node = {
        isConnected: true,
        dataset: { runningStarted: "400" },
        textContent: "Running ... 0ms",
      };
      const runtime = loadScheduler(relative, { runningNodes: [node], now: 1000 });
      runtime.refreshRunningToolNodes();
      expect(runtime.transcriptQueries()).toBe(1);
      expect(runtime.pendingTimers()).toHaveLength(1);
      runtime.runNextTimer();
      expect(node.textContent).toBe("Running ... 600ms");
      expect(runtime.transcriptQueries()).toBe(1);
      expect(runtime.pendingTimers()).toHaveLength(1);

      node.isConnected = false;
      runtime.runNextTimer();
      expect(runtime.state.runningToolNodes).toEqual([]);
      expect(runtime.pendingTimers()).toHaveLength(0);
      expect(runtime.transcriptQueries()).toBe(1);
    }
  });

  test("hidden pages cancel active animation and resume only when visible", () => {
    for (const relative of WEBUIS) {
      const runtime = loadScheduler(relative, { apiState: "Streaming" });
      runtime.syncUiAnimationScheduler();
      expect(runtime.pendingTimers()).toHaveLength(1);
      runtime.document.hidden = true;
      runtime.syncUiAnimationScheduler();
      expect(runtime.pendingTimers()).toHaveLength(0);
      runtime.document.hidden = false;
      runtime.syncUiAnimationScheduler();
      expect(runtime.pendingTimers()).toHaveLength(1);
    }
  });
});
