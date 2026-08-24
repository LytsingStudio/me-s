"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
require("../src/webui/edb-cache.js");

function loadSendShortcutRuntime(cookie = "", location = { protocol: "http:", port: "38199" }) {
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI send shortcut runtime");
  const factory = new Function("document", "performance", "matchMedia", `${source.slice(0, eventBindings)}
    return { state, browserPort, BROWSER_PORT, portScopedCookieName, SEND_SHORTCUT_COOKIE,
      readSendShortcutCookie, sendShortcutHint, sendShortcutPressed };`);
  const input = { value: "", style: {}, scrollHeight: 0 };
  return factory(
    { cookie, location, querySelector: (selector) => selector === "#prompt-input" ? input : null },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
  );
}

function key(overrides = {}) {
  return { key: "Enter", shiftKey: false, altKey: false, ctrlKey: false, metaKey: false, ...overrides };
}

describe("WebUI port-local send shortcut preference", () => {
  test("the default keeps plain Enter multiline and modifiers submit", () => {
    const { state, sendShortcutHint, sendShortcutPressed } = loadSendShortcutRuntime();
    expect(state.sendShortcut).toBe("modified-enter");
    expect(sendShortcutHint()).toBe("Enter 换行 · Shift/Alt+Enter 发送");
    expect(sendShortcutPressed(key(), state.sendShortcut)).toBe(false);
    expect(sendShortcutPressed(key({ shiftKey: true }), state.sendShortcut)).toBe(true);
    expect(sendShortcutPressed(key({ altKey: true }), state.sendShortcut)).toBe(true);
  });

  test("derives a distinct cookie name from the page port", () => {
    const first = loadSendShortcutRuntime("me_send_shortcut_p38199=enter");
    const second = loadSendShortcutRuntime(
      "me_send_shortcut_p38199=enter; me_send_shortcut_p38201=modified-enter",
      { protocol: "http:", port: "38201" },
    );
    expect(first.SEND_SHORTCUT_COOKIE).toBe("me_send_shortcut_p38199");
    expect(first.state.sendShortcut).toBe("enter");
    expect(second.SEND_SHORTCUT_COOKIE).toBe("me_send_shortcut_p38201");
    expect(second.state.sendShortcut).toBe("modified-enter");
    expect(loadSendShortcutRuntime("me_send_shortcut=enter").state.sendShortcut)
      .toBe("modified-enter");
  });

  test("uses protocol defaults when the URL omits its port", () => {
    const runtime = loadSendShortcutRuntime();
    expect(runtime.portScopedCookieName("me_send_shortcut", { protocol: "http:", port: "" }))
      .toBe("me_send_shortcut_p80");
    expect(runtime.portScopedCookieName("me_send_shortcut", { protocol: "https:", port: "" }))
      .toBe("me_send_shortcut_p443");
    expect(runtime.BROWSER_PORT).toBe(38199);
    expect(runtime.browserPort({ protocol: "http:", port: "" })).toBe(80);
    expect(runtime.browserPort({ protocol: "https:", port: "" })).toBe(443);
  });

  test("the cookie can make plain Enter submit and modifiers multiline", () => {
    const runtime = loadSendShortcutRuntime("other=value; me_send_shortcut_p38199=enter; session=opaque");
    expect(runtime.state.sendShortcut).toBe("enter");
    expect(runtime.sendShortcutHint()).toBe("Enter 发送 · Shift/Alt+Enter 换行");
    expect(runtime.sendShortcutPressed(key(), "enter")).toBe(true);
    expect(runtime.sendShortcutPressed(key({ shiftKey: true }), "enter")).toBe(false);
    expect(runtime.sendShortcutPressed(key({ altKey: true }), "enter")).toBe(false);
  });

  test("unknown cookies fall back safely and writes preserve preference attributes", () => {
    const runtime = loadSendShortcutRuntime("me_send_shortcut_p38199=unknown");
    expect(runtime.state.sendShortcut).toBe("modified-enter");
    expect(runtime.readSendShortcutCookie("me_send_shortcut_p38199=%E0%A4%A"))
      .toBe("modified-enter");
    expect(runtime.sendShortcutPressed(key({ ctrlKey: true, shiftKey: true }), "modified-enter")).toBe(false);
    expect(runtime.sendShortcutPressed(key({ metaKey: true }), "enter")).toBe(false);
    expect(runtime.sendShortcutPressed({ ...key(), key: "a" }, "enter")).toBe(false);
    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(source).toContain("Max-Age=31536000; Path=/; SameSite=Lax");
  });
});