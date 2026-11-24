"use strict";
const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const root = join(import.meta.dir, "..");
const read = (path) => readFileSync(join(root, path), "utf8");
const settle = async () => { for (let i = 0; i < 8; i += 1) await Promise.resolve(); };
class Node {
  constructor() { this.innerHTML = ""; this.textContent = ""; this.value = ""; this.isConnected = true; this.disabled = false; this.children = []; this.listeners = new Map(); this.classes = new Set(); this.classList = { add: (key) => this.classes.add(key), remove: (key) => this.classes.delete(key) }; }
  querySelector(selector) { return this.nodes?.[selector] || null; }
  querySelectorAll() { return Object.values(this.nodes || {}); }
  addEventListener(name, callback) { this.listeners.set(name, callback); }
  async fire(name) { await this.listeners.get(name)?.(); }
  append(node) { this.children.push(node); }
  remove() { this.removed = true; }
}
function fixture() {
  const timers = new Map(); let nextTimer = 0;
  const world = {};
  new Function("globalThis", "document", "setTimeout", "clearTimeout", "AbortController", `${read("src/webui/gais.js")}; return globalThis.MeGais;`)(world,
    { createElement: () => new Node() },
    (callback, delay) => { const id = ++nextTimer; timers.set(id, { callback, delay }); return id; },
    (id) => timers.delete(id), AbortController);
  const container = new Node(); container.nodes = {};
  for (const key of ["updated", "message", "host", "count", "tasks", "more"]) container.nodes[`[data-gais-${key}]`] = new Node();
  return { api: world.MeGais, container, timers, nodes: Object.fromEntries(Object.entries(container.nodes).map(([selector, node]) => [selector.match(/gais-(\w+)/)[1], node])) };
}
const status = (tag = "current") => ({ ok: true, host: { cpu: { utilization_percent: 18 }, memory: { used_bytes: 1024 ** 3, total_bytes: 8 * 1024 ** 3 }, queued_count: 1, gpus: [{ name: tag }] }, tasks: [{ task: "task-a", model: "video", state: "running", progress: { stage: "sampling", completed: 2, total: 8 } }] });

describe("shared GAIS page", () => {
  test("does not connect at creation; only one serial 3-second poll exists for the active identity", async () => {
    const f = fixture(); const calls = [];
    const controller = f.api.create({ container: f.container, request: async (...args) => { calls.push(args); return status(); }, onUnauthorized() {} });
    expect(calls).toHaveLength(0);
    controller.activate("session-a"); controller.activate("session-a"); await settle();
    expect(calls).toHaveLength(1); expect(calls[0][0]).toBe("/api/gais/status");
    expect(f.timers.size).toBe(1); expect([...f.timers.values()][0].delay).toBe(3000);
    expect(f.nodes.host.innerHTML).toContain("18.0%"); expect(f.nodes.tasks.children[0].innerHTML).toContain("2");
    const [id, timer] = [...f.timers][0]; f.timers.delete(id); await timer.callback();
    expect(calls).toHaveLength(2); expect(f.timers.size).toBe(1);
    controller.deactivate(); expect(f.timers.size).toBe(0); expect(calls.every(([path]) => path === "/api/gais/status")).toBe(true);
  });
  test("a slow old response cannot fill a new session or cause overlapping requests", async () => {
    const f = fixture(); const pending = [];
    const controller = f.api.create({ container: f.container, request: (path, options) => new Promise((resolve) => pending.push({ resolve, options })), onUnauthorized() {} });
    controller.activate("old"); controller.activate("new");
    expect(pending).toHaveLength(1); expect(pending[0].options.signal.aborted).toBe(true);
    pending[0].resolve(status("old-host")); await settle();
    expect(f.nodes.host.innerHTML).not.toContain("old-host"); expect(pending).toHaveLength(2);
    pending[1].resolve(status("new-host")); await settle();
    expect(f.nodes.host.innerHTML).toContain("new-host"); expect(f.timers.size).toBe(1);
    controller.deactivate(); expect(f.timers.size).toBe(0);
  });
  test("deactivation blocks late data and authentication loss ends polling", async () => {
    const f = fixture(); let complete;
    const controller = f.api.create({ container: f.container, request: () => new Promise((resolve) => { complete = resolve; }), onUnauthorized() {} });
    controller.activate("a"); controller.deactivate(); complete(status()); await settle();
    expect(f.nodes.host.innerHTML).toBe(""); expect(f.timers.size).toBe(0);
    let lost = 0; let authController;
    authController = f.api.create({ container: f.container, request: async () => { throw Object.assign(new Error("登录失效"), { status: 401 }); }, onUnauthorized() { lost += 1; authController.deactivate(); } });
    authController.activate("b"); await settle(); expect(lost).toBe(1); expect(f.timers.size).toBe(0);
  });
  test("service errors remain inside the page and retry without cancelling remote tasks", async () => {
    const f = fixture();
    const controller = f.api.create({ container: f.container, request: async () => { throw new Error("请先在设置中配置 ME-GAIS"); }, onUnauthorized() { throw new Error("must not log out for GAIS settings errors"); } });
    controller.activate("a"); await settle(); expect(f.nodes.message.textContent).toContain("配置"); expect(f.timers.size).toBe(1); controller.deactivate();
  });
});

describe("ME-GAIS settings", () => {
  test("test and save use the current form separately, with no direct GAIS URL requests", async () => {
    const f = fixture(); const form = new Node(); const nodes = {};
    for (const key of ["url", "token", "result", "test", "save"]) nodes[`[data-gais-${key}]`] = new Node();
    form.nodes = nodes; const container = new Node(); container.nodes = { "[data-gais-settings]": form };
    const calls = []; let saved = 0;
    await f.api.bindSettings(container, { current: () => true, onUnauthorized() {}, onSaved() { saved += 1; }, onDraft() {}, request: async (path, options) => { calls.push({ path, options }); return options ? { ok: true, message: path.endsWith("test") ? "连接成功" : "已保存" } : { ok: true, settings: { url: "http://saved:1234", token: "old" } }; } });
    nodes["[data-gais-url]"].value = "http://unsaved:5678"; nodes["[data-gais-token]"].value = "new";
    await nodes["[data-gais-test]"].fire("click");
    expect(calls[1].path).toBe("/api/gais/test"); expect(JSON.parse(calls[1].options.body)).toEqual({ url: "http://unsaved:5678", token: "new" }); expect(saved).toBe(0);
    await nodes["[data-gais-save]"].fire("click"); expect(calls[2].path).toBe("/api/gais/settings"); expect(saved).toBe(1);
    expect(calls.every(({ path }) => path.startsWith("/api/gais/"))).toBe(true);
  });
  test("all clients share the page, settings, asset and lifecycle wiring, including chatbot", () => {
    const app = read("src/webui/app.js"); const html = read("src/webui/index.html");
    expect(app).toContain('new Set(["chat", "system-prompt", "gais"])');
    expect(app).toContain('!["chat", "system-prompt", "gais"].includes(view.kind)');
    expect(html).toContain('<button data-view="gais">GAIS</button>'); expect(html.indexOf('/gais.js')).toBeLessThan(html.indexOf('/app.js'));
    expect(read("me-client/build-frontend.js")).toContain('[resolve(webuiRoot, "gais.js"), "gais.js"]');
    expect(read("src/webui.rs")).toContain('include_str!("webui/gais.js")');
    expect(read("src/gateway_webui.rs")).toContain('crate::webui::gais_response(request, &path)');
    expect(app.slice(app.indexOf('function showLogin('), app.indexOf('function showLoginPreservingView('))).toContain('gaisController?.deactivate()');
    expect(app.slice(app.indexOf('window.addEventListener("pagehide"'), app.indexOf('window.addEventListener("pageshow"'))).toContain('gaisController?.deactivate()');
    expect(app).toContain('state.authenticated ? globalThis.MeGais.settingsHtml() : ""');
  });
});
