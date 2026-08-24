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
  const counters = { terminal: 0, polling: 0, background: 0, phase: 0, overlay: 0, focus: 0 };
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
    "cancelBackgroundWorkspaceSync", "setConnectionPhase", "hideConnectionOverlay",
    `${source.slice(start, end)}\nreturn showLogin;`,
  );
  const showLogin = factory(
    state,
    elements,
    () => { counters.terminal += 1; },
    () => { counters.polling += 1; },
    () => { counters.background += 1; },
    () => { counters.phase += 1; },
    () => { counters.overlay += 1; },
  );
  return { showLogin, state, elements, counters };
}

function loadSubmitLogin(relative, browserPort = 45182) {
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const start = source.indexOf("async function submitLogin(event)");
  const end = source.indexOf("\nfunction setConnectionPhase", start);
  if (start < 0 || end < 0) throw new Error(`could not isolate submitLogin from ${relative}`);

  const calls = [];
  const elements = {
    loginSubmit: { disabled: false },
    loginError: { textContent: "" },
    loginPassword: { value: "correct horse", select() {} },
  };
  const factory = new Function(
    "BROWSER_PORT", "elements", "api", "showApplication", "restoreDraft",
    "startHttpPolling", "initializeGateway", "showLogin",
    `${source.slice(start, end)}\nreturn submitLogin;`,
  );
  const submitLogin = factory(
    browserPort,
    elements,
    async (path, options) => { calls.push({ path, options }); return {}; },
    () => {},
    () => {},
    () => {},
    async () => {},
    () => {},
  );
  return { submitLogin, elements, calls };
}

describe("WebUI login transition", () => {
  for (const relative of ["../src/webui/app.js", "../src/gateway_webui/app.js"]) {
    test(`${relative} makes repeated 401 login transitions idempotent`, () => {
      const runtime = loadShowLogin(relative);
      runtime.showLogin("登录已失效，请重新登录");
      expect(runtime.state.authenticated).toBe(false);
      expect(runtime.elements.app.classList.contains("hidden")).toBe(true);
      expect(runtime.elements.loginScreen.classList.contains("hidden")).toBe(false);
      expect(runtime.elements.loginError.textContent).toBe("登录已失效，请重新登录");
      expect(runtime.counters.terminal).toBe(1);
      expect(runtime.counters.polling).toBe(1);
      expect(runtime.counters.background).toBe(relative.includes("gateway_webui") ? 1 : 0);
      expect(runtime.counters.phase).toBe(1);
      expect(runtime.counters.overlay).toBe(1);
      expect(runtime.counters.focus).toBe(1);

      runtime.elements.loginPassword.value = "a";
      runtime.showLogin("另一个在途请求也返回了 401");
      expect(runtime.elements.loginError.textContent).toBe("另一个在途请求也返回了 401");
      expect(runtime.elements.loginPassword.value).toBe("a");
      expect(runtime.counters.terminal).toBe(1);
      expect(runtime.counters.polling).toBe(1);
      expect(runtime.counters.background).toBe(relative.includes("gateway_webui") ? 1 : 0);
      expect(runtime.counters.phase).toBe(1);
      expect(runtime.counters.overlay).toBe(1);
      expect(runtime.counters.focus).toBe(1);
    });
  }

  for (const relative of ["../src/webui/app.js", "../src/gateway_webui/app.js"]) {
    test(`${relative} submits the browser-visible port with the passkey`, async () => {
      const runtime = loadSubmitLogin(relative);
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
  }
});
