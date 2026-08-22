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

function fakeControls(encodedBytes) {
  const buttons = encodedBytes.map((encoded) => ({
    dataset: { sessionTerminalByte: encoded },
    disabled: true,
    closest(selector) { return selector === "[data-session-terminal-byte]" ? this : null; },
  }));
  const listeners = new Map();
  return {
    buttons,
    querySelectorAll(selector) { return selector === "[data-session-terminal-byte]" ? buttons : []; },
    contains(button) { return buttons.includes(button); },
    addEventListener(type, handler) { listeners.set(type, handler); },
    removeEventListener(type, handler) { if (listeners.get(type) === handler) listeners.delete(type); },
    click(button) { listeners.get("click")?.({ target: button }); },
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

  test("starts fresh attachments with a nullable cursor and continues from the returned tail", async () => {
    const requests = [];
    const encode = (value) => SessionTerminal.bytesToBase64(new TextEncoder().encode(value));
    const runtime = fakeRuntime(requests, [
      {
        reset: true,
        cursor: 7,
        tail: 7,
        events: [
          { type: "resize", cols: 80, rows: 24 },
          { type: "output", data: encode("current") },
        ],
      },
      {
        reset: false,
        cursor: 8,
        tail: 8,
        events: [{ type: "output", data: encode(" next") }],
      },
    ]);
    const container = {
      clientWidth: 800,
      clientHeight: 500,
      appendChild(child) { child.isConnected = true; },
    };
    const controller = SessionTerminal.create({
      runtime,
      container,
      request: runtime.request,
    });

    controller.attach({ key: "workspace-a:main", workspaceId: "workspace-a", agentId: "main" });
    await new Promise((resolve) => setTimeout(resolve, 200));

    const reads = requests.filter((request) => request.path.endsWith("/read"));
    expect(JSON.parse(reads[0].options.body)).toEqual({ cursor: null });
    expect(JSON.parse(reads[1].options.body)).toEqual({ cursor: 7 });
    expect(new TextDecoder().decode(new Uint8Array(runtime.terminals[0].output)))
      .toBe("current next");
    controller.dispose();
  });

  test("sends every shortcut byte through the shared keyboard and resize operation chain", async () => {
    const encoded = ["1b", "01", "1a", "18", "03", "16", "13", "04", "0f", "10", "11", "0d"];
    const expected = encoded.map((value) => Number.parseInt(value, 16));
    const requests = [];
    const runtime = fakeRuntime(requests);
    const controls = fakeControls(encoded);
    const container = {
      clientWidth: 800,
      clientHeight: 500,
      appendChild(child) { child.isConnected = true; },
    };
    const controller = SessionTerminal.create({
      runtime,
      container,
      controls,
      request: runtime.request,
    });
    controller.attach({ key: "workspace-a:main", workspaceId: "workspace-a", agentId: "main" });
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(controls.buttons.every((button) => !button.disabled)).toBe(true);

    runtime.terminals[0].dataHandler("K");
    for (const button of controls.buttons) controls.click(button);
    await new Promise((resolve) => setTimeout(resolve, 40));

    const paths = requests.map((request) => request.path);
    const sent = requests
      .filter((request) => request.path.endsWith("/input"))
      .flatMap((request) => [...SessionTerminal.base64ToBytes(JSON.parse(request.options.body).data)]);
    expect(sent).toEqual([0x4b, ...expected]);
    expect(paths.findIndex((path) => path.endsWith("/resize")))
      .toBeLessThan(paths.findIndex((path) => path.endsWith("/input")));
    expect(runtime.terminals[0].focused).toBe(true);
    expect(paths.every((path) => !/(?:close|kill|shutdown|exit)/.test(path))).toBe(true);

    controller.deactivate();
    expect(controls.buttons.every((button) => button.disabled)).toBe(true);
    const sentBefore = sent.length;
    controls.click(controls.buttons[0]);
    await new Promise((resolve) => setTimeout(resolve, 20));
    const sentAfter = requests
      .filter((request) => request.path.endsWith("/input"))
      .flatMap((request) => [...SessionTerminal.base64ToBytes(JSON.parse(request.options.body).data)]).length;
    expect(sentAfter).toBe(sentBefore);
    controller.dispose();
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

  test("renders identical one-line horizontally scrollable shortcut strips", () => {
    const expected = [
      ["ESC", "1b"], ["^A", "01"], ["^Z", "1a"], ["^X", "18"],
      ["^C", "03"], ["^V", "16"], ["^S", "13"], ["^D", "04"],
      ["^O", "0f"], ["^P", "10"], ["^Q", "11"], ["Enter", "0d"],
    ];
    for (const htmlPath of ["src/webui/index.html", "src/gateway_webui/index.html"]) {
      const html = read(htmlPath);
      const controls = [...html.matchAll(/<button type="button" data-session-terminal-byte="([0-9a-f]{2})"[^>]*>([^<]+)<\/button>/g)]
        .map((match) => [match[2], match[1]]);
      expect(controls).toEqual(expected);
      expect(html.indexOf('id="session-terminal-screen"'))
        .toBeLessThan(html.indexOf('id="session-terminal-controls"'));
    }
    for (const stylePath of ["src/webui/style.css", "src/gateway_webui/style.css"]) {
      const styles = read(stylePath);
      expect(styles).toContain(".session-terminal-controls { min-width: 0; flex: 0 0 auto; overflow-x: auto; overflow-y: hidden;");
      expect(styles).toContain("overscroll-behavior-x: contain; scrollbar-width: none; touch-action: pan-x;");
      expect(styles).toContain(".session-terminal-controls::-webkit-scrollbar { display: none; }");
      expect(styles).toContain(".session-terminal-control-strip { display: flex; width: max-content; min-width: 100%; flex-wrap: nowrap;");
      expect(styles).toContain(".session-terminal-control-strip button { min-width: 46px; min-height: 36px; flex: 0 0 auto;");
    }
    for (const appPath of ["src/webui/app.js", "src/gateway_webui/app.js"]) {
      const app = read(appPath);
      expect(app).toContain('sessionTerminalControls: $("#session-terminal-controls")');
      expect(app).toContain("controls: elements.sessionTerminalControls");
    }
    expect(read("src/terminal.rs")).not.toContain("session-terminal-byte");
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
