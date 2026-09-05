"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const theme = require("../src/webui/theme.js");
const appSource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
const clientSource = readFileSync(join(import.meta.dir, "../me-client/client-runtime.js"), "utf8");

function node() {
  const classes = new Set();
  const attributes = new Map();
  return {
    style: {}, dataset: {}, children: [], value: "", textContent: "", disabled: false,
    classList: {
      add: (value) => classes.add(value), remove: (value) => classes.delete(value),
      contains: (value) => classes.has(value),
      toggle(value, active) { if (active ?? !classes.has(value)) classes.add(value); else classes.delete(value); },
    },
    setAttribute: (key, value) => attributes.set(key, String(value)),
    getAttribute: (key) => attributes.get(key) ?? null,
    removeAttribute: (key) => attributes.delete(key),
    append(...children) { this.children.push(...children); },
    prepend(...children) { this.children.unshift(...children); },
    addEventListener() {}, querySelector() { return null; }, focus() {},
  };
}

function startupHarness({ client = true, ios = false, multipleWorkspaces = true, status = { required: true, authenticated: false } } = {}) {
  const nodes = new Map();
  const listeners = new Map();
  const calls = [];
  const shown = [];
  let resolveBootstrap, rejectBootstrap;
  const bootstrap = new Promise((resolve, reject) => { resolveBootstrap = resolve; rejectBootstrap = reject; });
  const document = {
    documentElement: node(), body: node(), cookie: "", title: "", hidden: false,
    baseURI: "http://fixture.invalid/", location: { protocol: "http:", port: "45182" },
    querySelector(selector) { if (!nodes.has(selector)) nodes.set(selector, node()); return nodes.get(selector); },
    querySelectorAll() { return []; }, createElement: node, addEventListener() {},
  };
  document.querySelector("#login-screen").classList.add("hidden");
  document.querySelector("#app").classList.add("hidden");
  const snapshot = () => ({
    theme: document.documentElement.getAttribute("data-theme"),
    mode: document.documentElement.getAttribute("data-mode"),
    visibility: document.documentElement.style.visibility,
    devices: runtime.rememberedDevices?.list().length ?? 0,
    error: document.querySelector("#login-error").textContent,
  });
  const response = async (path) => {
    calls.push(path);
    if (path === "/api/auth/status") {
      if (status instanceof Error) throw status;
      return new Response(JSON.stringify(status), { status: 200 });
    }
    return new Response(JSON.stringify({ ok: false, error: "not authenticated" }), { status: 401 });
  };
  const sandbox = {
    document, MeTheme: theme, MeEdbCache: { create() { return { renderManager() {} }; } },
    navigator: { platform: ios ? "iPhone" : "MacIntel", userAgent: ios ? "iPhone" : "" },
    addEventListener(type, callback) { listeners.set(type, callback); },
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    fetch: response,
    // A hidden WebView may never schedule animation frames. Startup must not wait on one.
    requestAnimationFrame() { throw new Error("startup must not wait for an animation frame"); },
    __TAURI__: { core: { async invoke(command, payload) {
      if (command === "client_bootstrap") return bootstrap;
      if (command === "client_window_action") {
        if (payload.action === "show") shown.push(snapshot());
        return { maximized: false, fullscreen: false };
      }
      if (command === "gateway_request") {
        const result = await response(payload.request.path);
        return { status: result.status, headers: {}, bodyText: await result.text() };
      }
      throw new Error(`unexpected native call: ${command}`);
    } } },
  };
  if (client) new Function("globalThis", "document", clientSource)(sandbox, document);
  else sandbox.MeFrontendRuntime = {
    capabilities: { multipleWorkspaces }, initialize: () => bootstrap,
    apiPath: (path) => path, createEdbCache: () => ({}),
  };
  const runtime = sandbox.MeFrontendRuntime;
  theme.initialize(sandbox);
  const pageshowStart = appSource.indexOf('window.addEventListener("pageshow"');
  const pageshowEnd = appSource.indexOf('document.addEventListener("visibilitychange"', pageshowStart);
  const factory = new Function("globalThis", "window", "document", "performance", "matchMedia", "fetch", "setTimeout", "clearTimeout", `
    ${appSource.slice(0, appSource.indexOf("\nelements.tabs.querySelectorAll"))}
    renderAgents = () => {};
    deactivateSessionTerminalView = () => {};
    cancelBackgroundWorkspaceSync = () => {};
    scheduleBackgroundWorkspaceSync = () => {};
    initializeGateway = async () => {};
    restoreDraft = () => {};
    function syncUiAnimationScheduler() {}
    ${appSource.slice(pageshowStart, pageshowEnd)}
    return { state, elements, initializeAuthentication, showLogin, showApplication,
      startHttpPolling, requestHttpSync, backgroundSyncCanRun };
  `);
  const core = factory(sandbox, sandbox, document, performance, sandbox.matchMedia,
    sandbox.fetch, () => 1, () => {});
  return {
    ...core, document, runtime, calls, shown, snapshot,
    pageshow(persisted = false) { listeners.get("pageshow")({ persisted }); },
    resolve() { resolveBootstrap({
      endpoint: "http://fixture.invalid", clientVersion: "test",
      devicePreferences: { "me-theme": "ocean", "me-color-mode": "light" },
      rememberedDevices: [{ endpoint: "http://fixture.invalid", password: "fixture", online: true }],
      localDevice: { endpoint: "http://local.invalid", online: false },
    }); },
    reject() { rejectBootstrap(new Error("bootstrap unavailable")); },
  };
}

const drain = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };

describe("shared frontend startup ownership", () => {
  test("delayed native preferences survive first pageshow and early login transitions without a default-theme reveal", async () => {
    const r = startupHarness();
    const initialization = r.initializeAuthentication();
    r.pageshow();
    r.pageshow(true);
    r.startHttpPolling();
    await r.requestHttpSync();
    r.showLogin("an early unauthorized response");
    r.showApplication();
    await drain();
    expect(r.calls).toEqual([]);
    expect(r.shown).toEqual([]);
    expect(r.snapshot().visibility).toBe("hidden");
    r.resolve();
    await initialization;
    expect(r.shown).toEqual([{ theme: "ocean", mode: "light", visibility: "", devices: 1, error: "" }]);
    expect(r.calls).toEqual([]);
    r.showLogin("expired");
    r.showApplication();
    await drain();
    expect(r.shown).toHaveLength(1);
  });

  test("bootstrap failure renders an error and releases startup exactly once", async () => {
    const r = startupHarness();
    const initialization = r.initializeAuthentication();
    r.pageshow();
    r.reject();
    await initialization;
    expect(r.calls).toEqual([]);
    expect(r.shown).toHaveLength(1);
    expect(r.shown[0].error).toBe("bootstrap unavailable");
    expect(r.snapshot().visibility).toBe("");
  });

  test("iOS uses the same preference-before-document-reveal boundary without desktop show", async () => {
    const r = startupHarness({ ios: true });
    const initialization = r.initializeAuthentication();
    r.pageshow();
    expect(r.snapshot().visibility).toBe("hidden");
    r.resolve();
    await initialization;
    expect(r.snapshot()).toMatchObject({ theme: "ocean", mode: "light", visibility: "", devices: 1 });
    expect(r.shown).toEqual([]);
    expect(r.calls).toEqual([]);
  });

  test("only authenticated restored pages resume sync, and a later 401 returns to login without revealing again", async () => {
    const r = startupHarness();
    const initialization = r.initializeAuthentication();
    r.resolve();
    await initialization;
    r.pageshow(true);
    expect(r.calls).toEqual([]);
    r.state.authenticated = true;
    r.pageshow(false);
    expect(r.calls).toEqual([]);
    r.pageshow(true);
    await drain();
    expect(r.calls).toEqual(["/api/workspaces/chat/sync"]);
    expect(r.state.authenticated).toBe(false);
    expect(r.shown).toHaveLength(1);
  });

  for (const multipleWorkspaces of [false, true]) {
    test(`${multipleWorkspaces ? "Gateway" : "direct"} browser waits for authentication status including no-password services`, async () => {
      const r = startupHarness({ client: false, multipleWorkspaces, status: { required: false, authenticated: true } });
      const initialization = r.initializeAuthentication();
      r.pageshow();
      await r.requestHttpSync();
      expect(r.calls).toEqual([]);
      r.resolve();
      await initialization;
      await drain();
      expect(r.calls).toEqual(["/api/auth/status", "/api/sync"]);
      expect(r.document.documentElement.style.visibility).toBeUndefined();
    });
  }

  test("browser status failure keeps login usable and cannot start anonymous sync", async () => {
    const r = startupHarness({ client: false, status: new Error("offline") });
    const initialization = r.initializeAuthentication();
    r.resolve();
    await initialization;
    r.pageshow(true);
    await r.requestHttpSync();
    expect(r.calls).toEqual(["/api/auth/status"]);
    expect(r.elements.loginError.textContent).toContain("offline");
    expect(r.elements.loginScreen.classList.contains("hidden")).toBe(false);
  });
});
