"use strict";
const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const root = join(import.meta.dir, "..");
const read = (path) => readFileSync(join(root, path), "utf8");
const settle = async () => { for (let i = 0; i < 8; i += 1) await Promise.resolve(); };
class Node {
  constructor(tag = "div") { this.tag = tag; this.nodes = {}; this._html = ""; this.textContent = ""; this.value = ""; this.isConnected = true; this.disabled = false; this.children = []; this.listeners = new Map(); this.classes = new Set(); this.classList = { add: (key) => this.classes.add(key), remove: (key) => this.classes.delete(key) }; }
  set innerHTML(value) {
    this._html = value; this.nodes = {}; this.children = [];
    for (const match of value.matchAll(/<([a-z]+)\b[^>]*\b(data-gais-[\w-]+)(?:\s|>|=)/g)) this.nodes[`[${match[2]}]`] = new Node(match[1]);
  }
  get innerHTML() { return this._html; }
  querySelector(selector) { return this.nodes[selector] || this.children.map((child) => child.querySelector(selector)).find(Boolean) || null; }
  querySelectorAll(selector) { return [...Object.values(this.nodes), ...this.children.flatMap((child) => child.querySelectorAll(selector))].filter((node) => selector.split(",").includes(node.tag)); }
  addEventListener(name, callback) { this.listeners.set(name, callback); }
  async fire(name) { if (!this.disabled) await this.listeners.get(name)?.(); }
  append(node) { node.parent = this; this.children.push(node); }
  remove() { this.parent.children = this.parent.children.filter((child) => child !== this); }
}
function fixture() {
  const timers = new Map(); let nextTimer = 0;
  const world = {};
  new Function("globalThis", "document", "setTimeout", "clearTimeout", "AbortController", `${read("src/webui/gais.js")}; return globalThis.MeGais;`)(world,
    { createElement: (tag) => new Node(tag) },
    (callback, delay) => { const id = ++nextTimer; timers.set(id, { callback, delay }); return id; },
    (id) => timers.delete(id), AbortController);
  const container = new Node();
  return { api: world.MeGais, container, timers, get nodes() { return Object.fromEntries(Object.entries(container.nodes).map(([selector, node]) => [selector.match(/gais-([\w-]+)/)[1], node])); } };
}
const servers = [{ id: "03a7f2bc", name: "图像 服务" }, { id: "abc12345", name: "图像 服务" }];
const status = (tag = "current", id = servers[0].id) => ({ ok: true, servers, selected: servers.find((server) => server.id === id), unavailable: 0, host: { cpu: { utilization_percent: 18 }, memory: { used_bytes: 1024 ** 3, total_bytes: 8 * 1024 ** 3 }, queued_count: 1, gpus: [{ name: tag }] }, tasks: [{ task: "task-a", model: "video", state: "running", progress: { stage: "sampling", completed: 2, total: 8 } }] });

describe("shared GAIS page", () => {
  test("does not connect at creation; only one serial 3-second poll exists for the active identity", async () => {
    const f = fixture(); const calls = [];
    const controller = f.api.create({ container: f.container, request: async (...args) => { calls.push(args); return status(); }, onUnauthorized() {} });
    expect(calls).toHaveLength(0);
    controller.activate("session-a"); controller.activate("session-a"); await settle();
    expect(calls).toHaveLength(1); expect(calls[0][0]).toBe("/api/gais/status");
    expect(calls[0][1].method).toBe("POST"); expect(JSON.parse(calls[0][1].body)).toEqual({ id: null });
    expect(f.nodes.server.innerHTML).toContain("图像 服务"); expect(f.nodes.server.value).toBe(servers[0].id);
    expect(f.timers.size).toBe(1); expect([...f.timers.values()][0].delay).toBe(3000);
    expect(f.nodes.host.innerHTML).toContain("18.0%"); expect(f.nodes.tasks.children[0].innerHTML).toContain("2");
    const [id, timer] = [...f.timers][0]; f.timers.delete(id); await timer.callback();
    expect(JSON.parse(calls[1][1].body)).toEqual({ id: servers[0].id });
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
  test("switching servers rejects late data and preserves a vanished id until the user selects again", async () => {
    const f = fixture(); const pending = [];
    const controller = f.api.create({ container: f.container, request: (path, options) => new Promise((resolve) => pending.push({ resolve, options })), onUnauthorized() {} });
    controller.activate("a"); pending[0].resolve(status("first")); await settle();
    const tick = async () => { const [key, timer] = [...f.timers][0]; f.timers.delete(key); void timer.callback(); await settle(); };
    await tick(); expect(pending).toHaveLength(2);
    f.nodes.server.value = servers[1].id; await f.nodes.server.fire("change");
    expect(pending[1].options.signal.aborted).toBe(true); expect(f.nodes.host.innerHTML).toBe("");
    pending[1].resolve(status("late-first")); await settle();
    expect(f.nodes.host.innerHTML).toBe(""); expect(pending).toHaveLength(3);
    expect(JSON.parse(pending[2].options.body)).toEqual({ id: servers[1].id });
    pending[2].resolve(status("second", servers[1].id)); await settle(); expect(f.nodes.host.innerHTML).toContain("second");
    await tick(); pending[3].resolve({ servers: [servers[0]], selected: null, host: null, tasks: [], message: "请重新选择" }); await settle();
    expect(f.nodes.server.value).toBe(""); expect(f.nodes.host.innerHTML).toBe(""); expect(f.nodes.tasks.children).toHaveLength(0);
    expect(f.nodes.message.textContent).toBe("请重新选择");
    await tick(); expect(JSON.parse(pending[4].options.body)).toEqual({ id: servers[1].id });
    f.nodes.server.value = servers[0].id; await f.nodes.server.fire("change");
    pending[4].resolve(status("late-second", servers[1].id)); await settle();
    expect(JSON.parse(pending[5].options.body)).toEqual({ id: servers[0].id });
    pending[5].resolve(status("first-again")); await settle();
    expect(f.nodes.host.innerHTML).toContain("first-again"); controller.deactivate();
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
  test("settings use the shared product title and follow Codex usage before models", () => {
    const f = fixture();
    const app = read("src/webui/app.js");
    const source = app.slice(app.indexOf("function localPreferenceSettingsHtml()"), app.indexOf("async function bindLocalPreferenceSettings("));
    const render = new Function("runtimeCapabilities", "state", "frontendRuntime", "globalThis", "escapeHtml", "WINDOW_BORDER_DEFAULT", "WINDOW_BORDER_THEME", `${source}; return localPreferenceSettingsHtml();`);
    for (const capabilities of [{}, { multipleWorkspaces: true }, { targetConfiguration: true, windowBorderStyle: true }]) {
      const html = render(capabilities, { authenticated: true }, { clientVersion: "test" }, { MeGais: f.api }, (value) => value, "default", "theme");
      expect(html).toContain("<h3>生成式 AI 服务</h3>");
      expect(html).not.toContain("ME-GAIS");
      expect(html.indexOf("settings-codex-section")).toBeLessThan(html.indexOf("settings-gais-section"));
      expect(html.match(/settings-gais-section/g)).toHaveLength(1);
      const offline = render(capabilities, { authenticated: false }, {}, { MeGais: f.api }, (value) => value, "default", "theme");
      expect(offline).not.toContain("settings-gais-section");
    }
    const modal = app.slice(app.indexOf("function renderSettingsModal()"), app.indexOf("function parseSettingsJson("));
    expect(modal.indexOf("${localPreferenceSettingsHtml()}")).toBeLessThan(modal.indexOf("settings-model-section"));
    expect(read("src/webui/style.css")).toMatch(/\.gais-settings\s*\{[^}]*padding-top:\s*12px/);
  });
  test("test and save use the current form separately, with no direct GAIS URL requests", async () => {
    const f = fixture(); f.container.innerHTML = f.api.settingsHtml();
    const form = f.container.querySelector("[data-gais-settings]"); form.innerHTML = f.api.settingsHtml();
    const field = (node, key) => node.querySelector(`[data-gais-${key}]`);
    const calls = []; let saved = 0; let draft;
    await f.api.bindSettings(f.container, { current: () => true, onUnauthorized() {}, onSaved() { saved += 1; }, onDraft(value) { draft = value; }, request: async (path, options) => { calls.push({ path, options }); return options ? { ok: true, server: { id: servers[0].id, name: "未保存服务" } } : { ok: true, settings: { servers: [{ url: "http://saved:1234", token: "old" }] } }; } });
    const entries = field(form, "entries"); const first = entries.children[0];
    field(first, "url").value = "http://unsaved:5678"; field(first, "token").value = "new"; await field(first, "url").fire("input");
    await field(first, "test").fire("click");
    expect(calls[1].path).toBe("/api/gais/test"); expect(JSON.parse(calls[1].options.body)).toEqual({ url: "http://unsaved:5678", token: "new" }); expect(saved).toBe(0);
    expect(field(first, "entry-title").textContent).toBe("未保存服务"); expect(field(first, "entry-result").textContent).toContain(servers[0].id);
    await field(form, "add").fire("click"); expect(entries.children).toHaveLength(2);
    const second = entries.children[1]; field(second, "url").value = "http://second:1234"; field(second, "token").value = "second-token"; await field(second, "token").fire("input");
    expect(draft.servers).toHaveLength(2);
    await field(form, "save").fire("click"); expect(calls[2].path).toBe("/api/gais/settings"); expect(saved).toBe(1);
    expect(JSON.parse(calls[2].options.body)).toEqual(draft);
    await field(first, "remove").fire("click"); expect(entries.children).toHaveLength(1); expect(draft.servers[0].url).toBe("http://second:1234");
    await field(second, "remove").fire("click"); expect(field(form, "empty").hidden).toBe(false);
    await field(form, "save").fire("click"); expect(JSON.parse(calls[3].options.body)).toEqual({ servers: [] });
    expect(calls.every(({ path }) => path.startsWith("/api/gais/"))).toBe(true);
  });
  test("modal drafts survive rebinding and closed settings ignore late test results", async () => {
    const f = fixture(); f.container.innerHTML = f.api.settingsHtml();
    const form = f.container.querySelector("[data-gais-settings]"); form.innerHTML = f.api.settingsHtml();
    let alive = true; let complete; let calls = 0; let saved = 0;
    await f.api.bindSettings(f.container, { draft: { servers: [{ url: "http://draft:1234", token: "draft" }] }, current: () => alive, onDraft() {}, onSaved() { saved += 1; }, onUnauthorized() {}, request() { calls += 1; return new Promise((resolve) => { complete = resolve; }); } });
    expect(calls).toBe(0);
    const row = form.querySelector("[data-gais-entries]").children[0];
    expect(row.querySelector("[data-gais-url]").value).toBe("http://draft:1234");
    const test = row.querySelector("[data-gais-test]").fire("click"); expect(calls).toBe(1);
    expect(form.querySelector("[data-gais-save]").disabled).toBe(true);
    alive = false; complete({ server: servers[0] }); await test;
    expect(row.querySelector("[data-gais-entry-title]").textContent).toBe("服务 1"); expect(saved).toBe(0);
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
