"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function classList(initial = []) {
  const values = new Set(initial);
  return {
    add(value) { values.add(value); },
    remove(value) { values.delete(value); },
    contains(value) { return values.has(value); },
  };
}

function loadShowLogin(relative) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const start = source.indexOf("function showLogin(message = \"\")");
  const end = source.indexOf("\nfunction showApplication", start);
  if (start < 0 || end < 0) throw new Error(`could not isolate showLogin from ${relative}`);

  const state = {
    authenticated: true,
    connectionHadSuccess: true,
    reconnectAttempt: 3,
  };
  const counters = { terminal: 0, polling: 0, background: 0, phase: 0, focus: 0 };
  const elements = {
    app: { classList: classList() },
    loginScreen: { classList: classList(["hidden"]) },
    loginError: { textContent: "" },
    loginPassword: {
      value: "",
      focus() { counters.focus += 1; },
    },
  };
  const factory = new Function(
    "state", "elements", "deactivateSessionTerminalView", "stopHttpPolling",
    "cancelBackgroundWorkspaceSync", "setConnectionPhase",
    "runtimeCapabilities", "frontendRuntime", "rememberedDevices", "setLoginView",
    "renderLoginDevices", "synchronizeWindowTitle", "markFrontendWindowReady",
    `${source.slice(start, end)}\nreturn showLogin;`,
  );
  const showLogin = factory(
    state,
    elements,
    () => { counters.terminal += 1; },
    () => { counters.polling += 1; },
    () => { counters.background += 1; },
    () => { counters.phase += 1; },
    { targetConfiguration: false },
    { endpoint: "" },
    null,
    () => {},
    () => {},
    () => {},
    () => {},
  );
  return { showLogin, state, elements, counters };
}

function loadSubmitLogin(relative, browserPort = 45182, client = false) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const start = source.indexOf("async function performLogin");
  const end = source.indexOf("\nasync function loginRememberedDevice", start);
  if (start < 0 || end < 0) throw new Error(`could not isolate login submission from ${relative}`);

  const calls = [];
  const remembered = [];
  const elements = {
    loginSubmit: { disabled: false, textContent: "登录" },
    loginError: { textContent: "" },
    loginPassword: { value: "correct horse", disabled: false, select() {} },
    loginEndpoint: { value: "https://gateway.example", disabled: false, select() {} },
    loginRemember: { checked: client, disabled: false },
    loginFormBack: { disabled: false },
  };
  const state = { authRequired: false, authenticated: false, loginView: "form" };
  const runtimeCapabilities = { targetConfiguration: client };
  const frontendRuntime = {
    endpoint: "",
    async configureTarget(value) { this.endpoint = value; return { endpoint: value }; },
  };
  const rememberedDevices = client ? {
    async remember(endpoint, password) { remembered.push({ endpoint, password }); },
  } : null;
  const factory = new Function(
    "BROWSER_PORT", "elements", "api", "showApplication", "restoreDraft",
    "startHttpPolling", "initializeGateway", "showLoginPreservingView", "runtimeCapabilities",
    "frontendRuntime", "rememberedDevices", "renderLoginDevices", "setLoginBusy", "state",
    `${source.slice(start, end)}\nreturn submitLogin;`,
  );
  const submitLogin = factory(
    browserPort,
    elements,
    async (path, options) => {
      calls.push({ path, options });
      if (path === "/api/auth/status") return { required: true, authenticated: false };
      return {};
    },
    () => {},
    () => {},
    () => {},
    async () => {},
    () => {},
    runtimeCapabilities,
    frontendRuntime,
    rememberedDevices,
    () => {},
    (busy) => { elements.loginSubmit.disabled = Boolean(busy); },
    state,
  );
  return { submitLogin, elements, calls, remembered };
}

describe("WebUI login transition", () => {
  test("the shared core makes repeated 401 login transitions idempotent", () => {
    const runtime = loadShowLogin("../src/webui/app.js");
    runtime.showLogin("登录已失效，请重新登录");
    expect(runtime.state.authenticated).toBe(false);
    expect(runtime.elements.app.classList.contains("hidden")).toBe(true);
    expect(runtime.elements.loginScreen.classList.contains("hidden")).toBe(false);
    expect(runtime.elements.loginError.textContent).toBe("登录已失效，请重新登录");
    expect(runtime.counters.terminal).toBe(1);
    expect(runtime.counters.polling).toBe(1);
    expect(runtime.counters.background).toBe(1);
    expect(runtime.counters.phase).toBe(1);
    expect(runtime.counters.focus).toBe(1);

    runtime.elements.loginPassword.value = "a";
    runtime.showLogin("另一个在途请求也返回了 401");
    expect(runtime.elements.loginError.textContent).toBe("另一个在途请求也返回了 401");
    expect(runtime.elements.loginPassword.value).toBe("a");
    expect(runtime.counters.terminal).toBe(1);
    expect(runtime.counters.polling).toBe(1);
    expect(runtime.counters.background).toBe(1);
    expect(runtime.counters.phase).toBe(1);
    expect(runtime.counters.focus).toBe(1);
  });

  test("the shared core submits the browser-visible port with the passkey", async () => {
    const runtime = loadSubmitLogin("../src/webui/app.js");
    await runtime.submitLogin({ preventDefault() {} });
    expect(runtime.calls).toHaveLength(1);
    expect(runtime.calls[0].path).toBe("/api/auth/login");
    expect(JSON.parse(runtime.calls[0].options.body)).toEqual({
      password: "correct horse",
      browser_port: 45182,
    });
    expect(runtime.elements.loginPassword.value).toBe("");
    expect(runtime.elements.loginSubmit.disabled).toBe(false);
  });

  test("the shared client login remembers a remote device only after successful authentication", async () => {
    const runtime = loadSubmitLogin("../src/webui/app.js", 45182, true);
    await runtime.submitLogin({ preventDefault() {} });
    expect(runtime.calls.map((call) => call.path)).toEqual([
      "/api/auth/status", "/api/auth/login",
    ]);
    expect(runtime.remembered).toEqual([
      { endpoint: "https://gateway.example", password: "correct horse" },
    ]);
    expect(runtime.elements.loginPassword.value).toBe("");
    expect(runtime.elements.loginSubmit.disabled).toBe(false);
  });

  test("the shared login assets provide device selection and reduced-motion flow styling", () => {
    const html = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const css = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const app = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(html).toContain('id="login-local-device"');
    expect(html).toContain('id="login-remembered-list"');
    expect(html).toContain('id="login-remote-device"');
    expect(html).toContain('id="login-remember"');
    expect(css).toContain("@keyframes login-flow-primary");
    expect(css).toContain(":root:not(.target-configuration) [data-target-configuration] { display: none !important; }");
    expect(css).toContain("@media (prefers-reduced-motion: reduce)");
    expect(app).toContain('classList.toggle("remembered-device-logins", Boolean(rememberedDevices))');
    expect(app).toContain('device.endpoint === local.endpoint');
  });
});
