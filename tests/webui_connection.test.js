"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

const APP_PATHS = ["../src/webui/app.js", "../src/gateway_webui/app.js"];

function connectionHarness(relative) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const start = source.indexOf("function setConnectionPhase");
  const end = source.indexOf("\nfunction scheduleHttpSync", start);
  if (start < 0 || end < 0) throw new Error(`could not isolate connection runtime from ${relative}`);

  let now = 0;
  let nextTimer = 1;
  let bulkRecovery = false;
  const timers = new Map();
  const mutations = { classes: 0, inert: 0 };
  const classList = () => ({
    add() { mutations.classes += 1; },
    remove() { mutations.classes += 1; },
    contains() { return false; },
  });
  const app = { contains: () => false };
  let inert = false;
  Object.defineProperty(app, "inert", {
    get() { return inert; },
    set(value) {
      const next = Boolean(value);
      if (next !== inert) mutations.inert += 1;
      inert = next;
    },
  });
  const elements = {
    app,
    connectionOverlay: { classList: classList() },
    connectionOverlayTitle: { textContent: "" },
    connectionOverlayMessage: { textContent: "" },
    connectionRetry: { classList: classList() },
    eventRecoveryProgress: { classList: classList(), setAttribute() {} },
    eventRecoveryProgressFill: { style: {} },
    eventRecoveryProgressLabel: { textContent: "" },
  };
  const state = {
    connectionPhase: "initial",
    connectionHadSuccess: false,
    connectionFailureStartedAt: null,
    connectionFailureDetail: "",
    degradedTimer: null,
    stabilizingSince: null,
    stabilizingSuccesses: 0,
    connectionOverlayMode: "hidden",
    connected: false,
    connecting: false,
    reconnectAttempt: 0,
    reconnectTimer: null,
    syncTimer: null,
    syncGeneration: 0,
    syncController: null,
    syncInFlight: false,
    pageClosing: false,
    authRequired: false,
    authenticated: true,
    eventRecovery: null,
  };
  const setTimeoutFake = (callback, delay) => {
    const id = nextTimer++;
    timers.set(id, { callback, delay });
    return id;
  };
  const clearTimeoutFake = (id) => { timers.delete(id); };
  const factory = new Function(
    "state", "elements", "document", "renderConnection", "eventRecoveryProgress", "currentStore",
    "bulkEventRecoveryActive", "api", "showLogin", "requestHttpSync", "setTimeout", "clearTimeout", "Date",
    `const RECONNECT_MAX_MS = 5000;
     const HTTP_SYNC_ACTIVE_MS = 250;
     const CONNECTION_DEGRADED_GRACE_MS = 2000;
     const CONNECTION_STABILIZE_MS = 1000;
     const CONNECTION_STABILIZE_SUCCESSES = 2;
     ${source.slice(start, end)}
     return { setConnectionPhase, enterDegraded, promoteDegradedConnection, handlePollingFailure,
       notePollingSuccess, renderConnectionOverlayForPhase, markConnectionStable };`,
  );
  const runtime = factory(
    state,
    elements,
    { activeElement: null },
    () => {},
    () => 0,
    () => null,
    () => bulkRecovery,
    async () => ({ authenticated: true }),
    () => {},
    () => {},
    setTimeoutFake,
    clearTimeoutFake,
    { now: () => now },
  );
  return {
    ...runtime,
    state,
    elements,
    mutations,
    timers,
    setNow(value) { now = value; },
    setBulkRecovery(value) { bulkRecovery = Boolean(value); },
    runTimerWithDelay(delay) {
      const entry = [...timers].find(([, timer]) => timer.delay === delay);
      if (!entry) throw new Error(`missing timer with delay ${delay}`);
      timers.delete(entry[0]);
      entry[1].callback();
    },
  };
}

describe("WebUI connection hysteresis", () => {
  test("a transient connected failure stays usable and recovers without showing the overlay", () => {
    for (const relative of APP_PATHS) {
      const runtime = connectionHarness(relative);
      runtime.state.connectionHadSuccess = true;
      runtime.setConnectionPhase("connected");

      runtime.handlePollingFailure(new Error("temporary gateway error"));
      expect(runtime.state.connectionPhase).toBe("degraded");
      expect(runtime.state.connected).toBe(true);
      expect(runtime.state.connectionOverlayMode).toBe("hidden");
      expect([...runtime.timers.values()].map((timer) => timer.delay).sort((a, b) => a - b))
        .toEqual([250, 2000]);

      runtime.notePollingSuccess(true, true);
      runtime.renderConnectionOverlayForPhase();
      expect(runtime.state.connectionPhase).toBe("connected");
      expect(runtime.state.reconnectAttempt).toBe(0);
      expect(runtime.state.connectionOverlayMode).toBe("hidden");
      expect(runtime.timers.size).toBe(0);
    }
  });

  test("sustained failure shows one reconnect overlay and requires stable authoritative recovery", () => {
    for (const relative of APP_PATHS) {
      const runtime = connectionHarness(relative);
      runtime.state.connectionHadSuccess = true;
      runtime.setConnectionPhase("connected");
      runtime.handlePollingFailure(new Error("network unavailable"));
      runtime.runTimerWithDelay(2000);

      expect(runtime.state.connectionPhase).toBe("reconnecting");
      expect(runtime.state.connected).toBe(false);
      expect(runtime.state.connectionOverlayMode).toBe("connection");
      expect(runtime.elements.app.inert).toBe(true);
      expect(runtime.state.reconnectAttempt).toBe(1);

      runtime.setNow(100);
      runtime.notePollingSuccess(true, true);
      runtime.renderConnectionOverlayForPhase();
      expect(runtime.state.connectionPhase).toBe("stabilizing");
      expect(runtime.state.reconnectAttempt).toBe(1);
      expect(runtime.elements.app.inert).toBe(true);

      runtime.setNow(600);
      runtime.notePollingSuccess(true, true);
      expect(runtime.state.connectionPhase).toBe("stabilizing");
      runtime.setNow(1200);
      runtime.notePollingSuccess(true, false);
      expect(runtime.state.connectionPhase).toBe("stabilizing");
      runtime.setNow(1350);
      runtime.notePollingSuccess(true, true);
      runtime.renderConnectionOverlayForPhase();
      expect(runtime.state.connectionPhase).toBe("connected");
      expect(runtime.state.reconnectAttempt).toBe(0);
      expect(runtime.state.connectionOverlayMode).toBe("hidden");
      expect(runtime.elements.app.inert).toBe(false);
    }
  });

  test("timeout and stabilizing failure enter reconnecting without clearing backoff", () => {
    for (const relative of APP_PATHS) {
      const runtime = connectionHarness(relative);
      runtime.state.connectionHadSuccess = true;
      runtime.state.reconnectAttempt = 2;
      runtime.setConnectionPhase("connected");
      runtime.handlePollingFailure(new Error("sync timeout"), { timedOut: true });
      expect(runtime.state.connectionPhase).toBe("reconnecting");
      expect(runtime.state.reconnectAttempt).toBe(3);

      runtime.notePollingSuccess(true, true);
      expect(runtime.state.connectionPhase).toBe("stabilizing");
      runtime.handlePollingFailure(new Error("failed again"));
      expect(runtime.state.connectionPhase).toBe("reconnecting");
      expect(runtime.state.reconnectAttempt).toBe(4);
      expect(runtime.state.connectionOverlayMode).toBe("connection");
    }
  });

  test("the first authoritative sync never pays the reconnect stabilization delay", () => {
    for (const relative of APP_PATHS) {
      const runtime = connectionHarness(relative);
      runtime.setConnectionPhase("reconnecting");
      runtime.notePollingSuccess(true, true);
      runtime.renderConnectionOverlayForPhase();
      expect(runtime.state.connectionPhase).toBe("connected");
      expect(runtime.state.connectionHadSuccess).toBe(true);
      expect(runtime.state.connectionOverlayMode).toBe("hidden");
    }
  });

  test("ordinary connected polls do not rewrite overlay classes or inert state", () => {
    for (const relative of APP_PATHS) {
      const runtime = connectionHarness(relative);
      runtime.state.connectionHadSuccess = true;
      runtime.setConnectionPhase("connected");
      runtime.mutations.classes = 0;
      runtime.mutations.inert = 0;
      runtime.renderConnectionOverlayForPhase();
      runtime.renderConnectionOverlayForPhase();
      expect(runtime.mutations).toEqual({ classes: 0, inert: 0 });
    }
  });
});
