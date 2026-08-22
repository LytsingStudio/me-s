"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const SessionTerminal = require("../src/webui/session-terminal.js");

const root = join(import.meta.dir, "..");
const read = (path) => readFileSync(join(root, path), "utf8");

function fakeRuntime(requests, readResponses = []) {
  const terminals = [];
  class FakeTerminal {
    constructor(options) {
      this.options = options;
      this.cols = 80;
      this.rows = 24;
      this.output = [];
      this.unicode = { activeVersion: "6" };
      terminals.push(this);
    }
    loadAddon(addon) { addon.terminal = this; addon.activate?.(this); }
    open(host) { this.host = host; }
    onData(handler) { this.dataHandler = handler; }
    onBinary(handler) { this.binaryHandler = handler; }
    write(bytes, done) { this.output.push(...bytes); done?.(); }
    resize(cols, rows) { this.cols = cols; this.rows = rows; }
    reset() { this.output = []; }
    focus() { this.focused = true; }
    dispose() { this.disposed = true; }
  }
  const fitSize = { cols: 100, rows: 30 };
  class FakeFitAddon {
    fit() { this.terminal.resize(fitSize.cols, fitSize.rows); }
  }
  class FakeUnicode11Addon {
    activate() {}
  }
  const documentElement = {};
  const document = {
    documentElement,
    createElement() {
      return {
        className: "", hidden: false, dataset: {}, isConnected: true,
        remove() { this.removed = true; },
      };
    },
  };
  return {
    Terminal: FakeTerminal,
    FitAddon: { FitAddon: FakeFitAddon },
    Unicode11Addon: { Unicode11Addon: FakeUnicode11Addon },
    TextEncoder,
    AbortController,
    document,
    terminals,
    setTimeout,
    clearTimeout,
    requestAnimationFrame: (callback) => setTimeout(callback, 0),
    addEventListener() {},
    removeEventListener() {},
    getComputedStyle() { return { getPropertyValue() { return ""; } }; },
    request: async (path, options, identity) => {
      requests.push({ path, options, identity });
      if (path.endsWith("/read")) {
        const response = readResponses.shift();
        return {
          ok: true,
          shell: "/bin/zsh",
          cwd: "/workspace",
          state: "running",
          exit_code: null,
          error: null,
          drained: false,
          reset: false,
          cursor: 1,
          tail: 1,
          cols: 80,
          rows: 24,
          events: [{ type: "output", data: SessionTerminal.bytesToBase64(new TextEncoder().encode("ready")) }],
          ...(response || {}),
        };
      }
      return { ok: true, state: "running", error: null };
    },
  };
}

describe("SessionTerminal browser transport", () => {
  test("round-trips arbitrary terminal bytes and keeps workspace identity explicit", () => {
    const bytes = new Uint8Array([0, 27, 255, 65, 10]);
    expect([...SessionTerminal.base64ToBytes(SessionTerminal.bytesToBase64(bytes))]).toEqual([...bytes]);
    expect(SessionTerminal.normalizeIdentity({ workspaceId: "workspace-a", agentId: "main" }))
      .toEqual({ key: "workspace-a:main", workspaceId: "workspace-a", agentId: "main" });
    expect(SessionTerminal.normalizeIdentity({ agentId: "main" }))
      .toEqual({ key: "direct:main", workspaceId: null, agentId: "main" });
  });

  test("uses read/input/resize only and deactivation never terminates the host PTY", async () => {
    const requests = [];
    const runtime = fakeRuntime(requests);
    const children = [];
    const container = {
      clientWidth: 800,
      clientHeight: 500,
      appendChild(child) { children.push(child); child.isConnected = true; },
    };
    const controller = SessionTerminal.create({
      runtime,
      container,
      request: runtime.request,
    });
    controller.attach({ key: "workspace-a:main", workspaceId: "workspace-a", agentId: "main" });
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(runtime.terminals[0].unicode.activeVersion).toBe("11");
    runtime.terminals[0].dataHandler("中文");
    await new Promise((resolve) => setTimeout(resolve, 100));
    controller.deactivate();

    const paths = requests.map((request) => request.path);
    expect(paths.some((path) => path.endsWith("/read"))).toBe(true);
    expect(paths.some((path) => path.endsWith("/input"))).toBe(true);
    expect(paths.some((path) => path.endsWith("/resize"))).toBe(true);
    expect(paths.indexOf(paths.find((path) => path.endsWith("/resize"))))
      .toBeLessThan(paths.indexOf(paths.find((path) => path.endsWith("/input"))));
    expect(paths.every((path) => !/(?:close|kill|shutdown|exit)/.test(path))).toBe(true);
    expect(requests.every((request) => request.identity.workspaceId === "workspace-a")).toBe(true);
    expect(children[0].hidden).toBe(true);
    controller.dispose();
  });

  test("reclaims its fitted size after another attachment resizes the PTY", async () => {
    const requests = [];
    const readResponses = [
      { cursor: 1, tail: 1, events: [] },
      { cursor: 2, tail: 2, events: [{ type: "resize", cols: 80, rows: 24 }] },
    ];
    const runtime = fakeRuntime(requests, readResponses);
    const container = {
      clientWidth: 800,
      clientHeight: 500,
      appendChild(child) { child.isConnected = true; },
    };
    const identity = { key: "workspace-a:main", workspaceId: "workspace-a", agentId: "main" };
    const controller = SessionTerminal.create({
      runtime,
      container,
      request: runtime.request,
    });

    controller.attach(identity);
    await new Promise((resolve) => setTimeout(resolve, 220));
    expect(runtime.terminals[0].cols).toBe(80);
    expect(runtime.terminals[0].rows).toBe(24);

    controller.deactivate();
    controller.attach(identity);
    await new Promise((resolve) => setTimeout(resolve, 120));

    const fittedResizes = requests
      .filter((request) => request.path.endsWith("/resize"))
      .map((request) => JSON.parse(request.options.body))
      .filter((size) => size.cols === 100 && size.rows === 30);
    expect(fittedResizes).toHaveLength(2);
    controller.dispose();
  });

  test("keeps the fixed native terminal distinct from dynamic Agent Terminal tool tabs", () => {
    for (const htmlPath of ["src/webui/index.html", "src/gateway_webui/index.html"]) {
      const html = read(htmlPath);
      const chat = html.indexOf('data-view="chat"');
      const workmap = html.indexOf('data-view="workmap"');
      const native = html.indexOf('data-view="session-terminal"');
      const dynamic = html.indexOf('id="terminal-tabs"');
      expect(chat).toBeGreaterThanOrEqual(0);
      expect(chat).toBeLessThan(workmap);
      expect(workmap).toBeLessThan(native);
      expect(native).toBeLessThan(dynamic);
      expect(html).toContain('id="session-terminal-view"');
      expect(html).toContain('id="terminal-view"');
      expect(html).toContain('/xterm.js');
      expect(html).toContain('/xterm-addon-fit.js');
      expect(html).toContain('/xterm-addon-unicode11.js');
      expect(html).toContain('/session-terminal.js');
    }
    expect(read("src/webui/vendor/xterm-addon-unicode11.js")).toContain("Unicode11Addon");
    for (const appPath of ["src/webui/app.js", "src/gateway_webui/app.js"]) {
      const app = read(appPath);
      expect(app).toContain('kind !== "session-terminal"');
      expect(app).toContain('kind === "terminal"');
      expect(app).toContain('Terminal · ${escapeHtml(session.session_id)}');
    }
  });

  test("uses ordinary HTTP polling without browser persistence or lifecycle close calls", () => {
    const source = read("src/webui/session-terminal.js");
    expect(source).not.toContain("new WebSocket");
    expect(source).not.toContain("EventSource");
    expect(source).not.toContain("localStorage");
    expect(source).not.toContain("sessionStorage");
    expect(source).not.toContain("beforeunload");
    expect(source).not.toContain("pagehide");
    expect(source).toContain('terminalPath(session.identity, "read")');
    expect(source).toContain('queueOperation(session, "input"');
    expect(source).toContain('queueOperation(session, "resize"');
    expect(source).toContain("session.operationChain = session.operationChain");
    expect(source).toContain('terminal.unicode.activeVersion = "11"');
  });
});
