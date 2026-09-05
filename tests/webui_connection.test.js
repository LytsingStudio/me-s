"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");

function connectionHarness() {
  let now = 0;
  let nextTimer = 0;
  let renders = 0;
  const timers = new Map();
  const notices = [];
  const state = {
    connectionPhase: "initial", connectionHadSuccess: false,
    connectionFailureStartedAt: null, connectionFailureDetail: "",
    degradedTimer: null, stabilizingSince: null, stabilizingSuccesses: 0,
    connected: false, connecting: true, reconnectAttempt: 0,
    reconnectTimer: null, syncTimer: null, syncGeneration: 0,
    syncController: null, syncInFlight: false, pageClosing: false,
    authRequired: false, authenticated: true,
  };
  const factory = new Function("state", "renderAgents", "cancelBackgroundWorkspaceSync",
    "toast", "console", "bulkEventRecoveryActive", "api", "showLogin", "requestHttpSync",
    "setTimeout", "clearTimeout", "Date", `
      const RECONNECT_MAX_MS = 5000, HTTP_SYNC_ACTIVE_MS = 250;
      const CONNECTION_DEGRADED_GRACE_MS = 2000, CONNECTION_STABILIZE_MS = 1000;
      const CONNECTION_STABILIZE_SUCCESSES = 2;
      ${source.slice(source.indexOf("function setConnectionPhase"), source.indexOf("\nfunction scheduleHttpSync"))}
      return { setConnectionPhase, handlePollingFailure, notePollingSuccess, failHttpSync };
    `);
  const runtime = factory(state, () => renders++, () => {},
    (text) => notices.push(text), { error() {} }, () => false,
    async () => ({ authenticated: true }), () => {}, () => {},
    (callback, delay) => { const id = ++nextTimer; timers.set(id, { callback, delay }); return id; },
    (id) => timers.delete(id), { now: () => now });
  return { ...runtime, state, timers, notices, renders: () => renders,
    setNow(value) { now = value; },
    runTimer(delay) {
      const [id, timer] = [...timers].find(([, timer]) => timer.delay === delay);
      timers.delete(id);
      timer.callback();
    },
  };
}

describe("nonblocking WebUI connection", () => {
  test("transient failures preserve grace and sustained failures retry with backoff", () => {
    const r = connectionHarness();
    r.state.connectionHadSuccess = true;
    r.setConnectionPhase("connected");
    r.handlePollingFailure(new Error("temporary"));
    expect(r.state.connectionPhase).toBe("degraded");
    expect(r.state.connected).toBe(true);
    expect([...r.timers.values()].map((timer) => timer.delay).sort((a, b) => a - b)).toEqual([250, 2000]);
    r.notePollingSuccess(true, true);
    expect(r.state.connectionPhase).toBe("connected");
    expect(r.timers.size).toBe(0);
    r.handlePollingFailure(new Error("offline"));
    r.runTimer(2000);
    expect(r.state.connectionPhase).toBe("reconnecting");
    expect(r.state.connecting).toBe(true);
    expect(r.state.reconnectAttempt).toBe(1);
    expect(r.notices).toEqual([]);
  });

  test("recovery updates sidebar state without repeated rendering during ordinary polls", () => {
    const r = connectionHarness();
    r.state.connectionHadSuccess = true;
    r.setConnectionPhase("reconnecting");
    r.notePollingSuccess(true, true);
    expect(r.state.connectionPhase).toBe("stabilizing");
    r.setNow(1200);
    r.notePollingSuccess(true, true);
    expect(r.state.connectionPhase).toBe("connected");
    expect(r.state.connecting).toBe(false);
    const count = r.renders();
    for (let i = 0; i < 20; i++) r.notePollingSuccess(true, true);
    expect(r.renders()).toBe(count);
  });

  test("initial success is immediate and timeouts retain retry backoff", () => {
    const r = connectionHarness();
    r.notePollingSuccess(true, true);
    expect(r.state.connectionPhase).toBe("connected");
    r.state.reconnectAttempt = 2;
    r.handlePollingFailure(new Error("timeout"), { timedOut: true });
    expect(r.state.reconnectAttempt).toBe(3);
    r.notePollingSuccess(true, true);
    r.handlePollingFailure(new Error("again"));
    expect(r.state.reconnectAttempt).toBe(4);
  });

  test("deterministic failures give one lightweight notice and keep automatic recovery available", () => {
    const r = connectionHarness();
    r.failHttpSync("无法更新界面", new Error("invalid response"));
    r.failHttpSync("无法更新界面", new Error("invalid response"));
    expect(r.notices).toEqual(["无法更新界面，将自动重试。"]);
    expect(r.state.connectionPhase).toBe("reconnecting");
    expect([...r.timers.values()].map((timer) => timer.delay)).toEqual([5000]);
  });

  test("connection and session loading have no overlay, focus change or inert subtree", () => {
    const html = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    expect(html).not.toContain('id="connection-overlay"');
    expect(html).not.toContain('id="session-sync-overlay"');
    expect(source).not.toContain(".inert =");
    expect(source).not.toContain("sessionSelectionAllowed");
    expect(source).not.toContain("item.disabled = loadingState.loading");
    expect(source).not.toContain("deleteButton.disabled = loadingState.loading");
    expect(source).not.toContain("连接尚未恢复，请稍候");
  });
});

function syncHarness() {
  let resolveRequest, resolveApply;
  let calls = 0;
  const schedules = [];
  const logins = [];
  const state = {
    syncInFlight: false, pageClosing: false, syncGeneration: 1, workspaceId: "chat",
    view: { kind: "chat" }, stores: new Map(), selectedAgent: "main",
    snapshotInitialized: false, apiActivity: {}, connectionPhase: "connected",
  };
  const factory = new Function("state", "api", "applySyncState", "scheduleHttpSync", "showLogin", `
    const HTTP_SYNC_TIMEOUT_MS = 15000, HTTP_SYNC_ACTIVE_MS = 250, HTTP_SYNC_IDLE_MS = 1000;
    const httpSyncProgressSignature = () => "cursor";
    const usesUiProjection = () => true;
    const scheduleBackgroundWorkspaceSync = () => {};
    const failHttpSync = () => { throw new Error("unexpected sync failure"); };
    const handlePollingFailure = failHttpSync;
    ${source.slice(source.indexOf("async function requestHttpSync()"), source.indexOf("\nfunction requestHttpSyncNow"))}
    return requestHttpSync;
  `);
  const request = factory(state,
    () => { calls++; return new Promise((resolve, reject) => { resolveRequest = { resolve, reject }; }); },
    () => new Promise((resolve) => { resolveApply = resolve; }),
    (delay) => schedules.push(delay), (message) => logins.push(message));
  return { state, request, schedules, logins, calls: () => calls,
    response(value) { resolveRequest.resolve(value); }, fail(error) { resolveRequest.reject(error); },
    applied() { resolveApply(); },
  };
}

describe("serialized projection synchronization", () => {
  test("in-flight covers applying the body as well as fetching it", async () => {
    const r = syncHarness();
    const pending = r.request();
    r.response({ selected_agent: "main" });
    await Promise.resolve();
    expect(r.state.syncInFlight).toBe(true);
    await r.request();
    expect(r.calls()).toBe(1);
    r.applied();
    await pending;
    expect(r.state.syncInFlight).toBe(false);
    expect(r.schedules).toEqual([1000]);
  });

  test("an old operation cannot release or schedule over a new generation", async () => {
    const r = syncHarness();
    const pending = r.request();
    r.response({ selected_agent: "main" });
    await Promise.resolve();
    r.state.syncGeneration++;
    const newer = {};
    r.state.syncController = newer;
    r.applied();
    await pending;
    expect(r.state.syncInFlight).toBe(true);
    expect(r.state.syncController).toBe(newer);
    expect(r.schedules).toEqual([]);
  });

  test("real authentication expiry still returns to login", async () => {
    const r = syncHarness();
    const pending = r.request();
    r.fail(Object.assign(new Error("unauthorized"), { status: 401 }));
    await pending;
    expect(r.logins).toEqual(["登录已失效，请重新登录"]);
    expect(r.state.syncInFlight).toBe(false);
  });
});
