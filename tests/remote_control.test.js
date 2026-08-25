"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const RemoteControl = require("../src/webui/remote-control.js");

const root = join(import.meta.dir, "..");
const read = (path) => readFileSync(join(root, path), "utf8");
const settle = (milliseconds = 30) => new Promise((resolve) => setTimeout(resolve, milliseconds));

class FakeClassList {
  constructor() { this.values = new Set(); }
  toggle(name, force) {
    if (force) this.values.add(name); else this.values.delete(name);
    return force;
  }
  contains(name) { return this.values.has(name); }
}

class FakeElement {
  constructor({ value = "", hidden = false, rect = null } = {}) {
    this.value = value;
    this.hidden = hidden;
    this.rect = rect;
    this.disabled = false;
    this.textContent = "";
    this.dataset = {};
    this.classList = new FakeClassList();
    this.listeners = new Map();
    this.src = "";
  }
  addEventListener(type, handler) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type).add(handler);
  }
  removeEventListener(type, handler) { this.listeners.get(type)?.delete(handler); }
  dispatch(type, event = {}) {
    event.preventDefault ||= () => { event.defaultPrevented = true; };
    event.stopPropagation ||= () => { event.propagationStopped = true; };
    for (const handler of this.listeners.get(type) || []) handler(event);
    return event;
  }
  focus() { this.focused = true; }
  getBoundingClientRect() { return this.rect || { left: 0, top: 0, width: 100, height: 50 }; }
  setPointerCapture(pointerId) { this.pointerId = pointerId; }
}

function jsonResponse(payload, status = 200) {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function frameResponse(sequence, screenWidth = 400, screenHeight = 200, frameWidth = 200, frameHeight = 100) {
  return new Response(new Uint8Array([0xff, 0xd8, sequence & 0xff, 0xff, 0xd9]), {
    status: 200,
    headers: {
      "Content-Type": "image/jpeg",
      "X-Me-Remote-Sequence": String(sequence),
      "X-Me-Screen-Width": String(screenWidth),
      "X-Me-Screen-Height": String(screenHeight),
      "X-Me-Frame-Width": String(frameWidth),
      "X-Me-Frame-Height": String(frameHeight),
    },
  });
}

function fixture(request) {
  const elements = {
    start: new FakeElement(),
    stop: new FakeElement(),
    screenshot: new FakeElement(),
    fps: new FakeElement({ value: "3" }),
    scale: new FakeElement({ value: "50" }),
    stage: new FakeElement(),
    image: new FakeElement({ hidden: true, rect: { left: 0, top: 0, width: 100, height: 50 } }),
    keyboard: new FakeElement(),
    empty: new FakeElement(),
    status: new FakeElement(),
    frame: new FakeElement(),
  };
  const selectors = new Map([
    ["[data-remote-start]", elements.start],
    ["[data-remote-stop]", elements.stop],
    ["[data-remote-screenshot]", elements.screenshot],
    ["[data-remote-fps]", elements.fps],
    ["[data-remote-scale]", elements.scale],
    ["[data-remote-stage]", elements.stage],
    ["[data-remote-image]", elements.image],
    ["[data-remote-keyboard]", elements.keyboard],
    ["[data-remote-empty]", elements.empty],
    ["[data-remote-status]", elements.status],
    ["[data-remote-frame-count]", elements.frame],
  ]);
  const container = {
    classList: new FakeClassList(),
    querySelector(selector) { return selectors.get(selector) || null; },
  };
  let objectUrl = 0;
  const runtimeListeners = new Map();
  const documentListeners = new Map();
  let intervalId = 0;
  const intervals = new Map();
  class FakeImage {
    set src(value) {
      this.value = value;
      queueMicrotask(() => this.onload?.());
    }
  }
  const runtime = {
    AbortController,
    Image: FakeImage,
    URL: {
      createObjectURL() { objectUrl += 1; return `blob:remote-${objectUrl}`; },
      revokeObjectURL() {},
    },
    document: {
      hidden: false,
      addEventListener(type, handler) { documentListeners.set(type, handler); },
      removeEventListener(type, handler) { if (documentListeners.get(type) === handler) documentListeners.delete(type); },
    },
    addEventListener(type, handler) { runtimeListeners.set(type, handler); },
    removeEventListener(type, handler) { if (runtimeListeners.get(type) === handler) runtimeListeners.delete(type); },
    setInterval(callback) { intervalId += 1; intervals.set(intervalId, callback); return intervalId; },
    clearInterval(id) { intervals.delete(id); },
    setTimeout,
    clearTimeout,
  };
  const controller = RemoteControl.create({ runtime, container, request });
  return { controller, elements, container, runtime, intervals };
}

function requestLog(frameSequences = []) {
  const requests = [];
  let active = false;
  return {
    requests,
    async request(action, options) {
      const body = JSON.parse(options.body || "{}");
      requests.push({ action, body, signal: options.signal });
      if (action === "status") return jsonResponse({ ok: true, supported: true, active, owned: false, fps: null, scale: null });
      if (action === "start") { active = true; return jsonResponse({ ok: true, controller_token: "controller-token", fps: body.fps, scale: body.scale }); }
      if (action === "stop") { active = false; return jsonResponse({ ok: true, active: false, released_inputs: 0, cleanup_errors: [] }); }
      if (action === "frame" || action === "screenshot") {
        const sequence = frameSequences.shift();
        return sequence == null ? new Response(null, { status: 204 }) : frameResponse(sequence);
      }
      return jsonResponse({ ok: true, active, released_inputs: 0, cleanup_errors: [] });
    },
  };
}

describe("RemoteControl shared WebUI controller", () => {
  test("fixes the public choices, geometry mapping, wheel direction, and local release shortcut", () => {
    expect(RemoteControl.FPS_OPTIONS).toEqual([1, 3, 5, 10]);
    expect(RemoteControl.SCALE_OPTIONS).toEqual([100, 75, 50, 25]);
    expect(RemoteControl.DEFAULT_FPS).toBe(3);
    expect(RemoteControl.DEFAULT_SCALE).toBe(50);
    expect(RemoteControl.mapPointerToScreen(50, 25, { left: 0, top: 0, width: 100, height: 50 }, 400, 200))
      .toEqual({ x: 200, y: 100 });
    expect(RemoteControl.mapPointerToScreen(100, 50, { left: 0, top: 0, width: 100, height: 50 }, 400, 200))
      .toEqual({ x: 399, y: 199 });
    expect(RemoteControl.normalizeWheelDelta(80, 0)).toBe(-1);
    expect(RemoteControl.normalizeWheelDelta(-2, 1)).toBe(2);
    expect(RemoteControl.isExitShortcut({ ctrlKey: true, shiftKey: true, code: "KeyE" })).toBe(true);
    expect(RemoteControl.isExitShortcut({ ctrlKey: true, shiftKey: false, code: "KeyE" })).toBe(false);
  });

  test("counts only displayed new sequences, resets on start, freezes on stop, and releases input without stopping", async () => {
    const log = requestLog([5, 5, 6]);
    const { controller, elements } = fixture(log.request);
    controller.activate();
    await settle();
    expect(elements.frame.textContent).toBe("frame: 0");
    expect(elements.start.disabled).toBe(false);

    elements.screenshot.dispatch("click");
    await settle();
    expect(controller.snapshot().frameCount).toBe(1);
    expect(controller.snapshot().lastSequence).toBe(5);

    elements.screenshot.dispatch("click");
    await settle();
    expect(controller.snapshot().frameCount).toBe(1);

    elements.start.dispatch("click");
    await settle(50);
    expect(controller.snapshot().owned).toBe(true);
    expect(controller.snapshot().frameCount).toBe(1);
    expect(controller.snapshot().lastSequence).toBe(6);

    elements.image.dispatch("pointerdown", { clientX: 50, clientY: 25, button: 0, pointerId: 7 });
    elements.keyboard.dispatch("keydown", { code: "ControlLeft", key: "Control", ctrlKey: true });
    elements.keyboard.dispatch("keydown", { code: "ShiftLeft", key: "Shift", ctrlKey: true, shiftKey: true });
    await settle(40);
    const releaseEvent = elements.keyboard.dispatch("keydown", {
      code: "KeyE", key: "e", ctrlKey: true, shiftKey: true, altKey: false, metaKey: false,
    });
    await settle(80);
    expect(releaseEvent.defaultPrevented).toBe(true);
    expect(controller.snapshot().captured).toBe(false);
    expect(controller.snapshot().owned).toBe(true);
    const inputEvents = log.requests
      .filter((entry) => entry.action === "input")
      .flatMap((entry) => entry.body.events);
    expect(inputEvents).toContainEqual({ kind: "mouse_move", x: 200, y: 100 });
    expect(inputEvents).toContainEqual({ kind: "mouse_down", button: "left" });
    expect(inputEvents).toContainEqual({ kind: "key_down", code: "ControlLeft" });
    expect(inputEvents).toContainEqual({ kind: "key_down", code: "ShiftLeft" });
    expect(inputEvents.some((event) => event.code === "KeyE")).toBe(false);
    expect(log.requests.some((entry) => entry.action === "release")).toBe(true);
    expect(log.requests.some((entry) => entry.action === "stop")).toBe(false);

    const countBeforeStop = controller.snapshot().frameCount;
    elements.stop.dispatch("click");
    await settle();
    expect(controller.snapshot().owned).toBe(false);
    expect(controller.snapshot().frameCount).toBe(countBeforeStop);
    expect(elements.image.hidden).toBe(false);
    controller.dispose();
  });

  test("preserves the displayed frame when a new control attempt fails", async () => {
    const log = requestLog([9]);
    const request = async (action, options) => {
      if (action === "start") {
        return jsonResponse({
          ok: false,
          code: "remote_control_busy",
          error: "busy",
        }, 409);
      }
      return log.request(action, options);
    };
    const { controller, elements } = fixture(request);
    controller.activate();
    await settle();
    elements.screenshot.dispatch("click");
    await settle();
    expect(controller.snapshot().frameCount).toBe(1);
    expect(controller.snapshot().lastSequence).toBe(9);
    elements.start.dispatch("click");
    await settle();
    expect(controller.snapshot().owned).toBe(false);
    expect(controller.snapshot().frameCount).toBe(1);
    expect(controller.snapshot().lastSequence).toBe(9);
    controller.dispose();
  });

  test("retries transient capture contention without parallel screenshot requests", async () => {
    let attempts = 0;
    let inFlight = 0;
    let maximumInFlight = 0;
    const request = async (action) => {
      if (action === "status") {
        return jsonResponse({ ok: true, supported: true, active: false, owned: false, fps: null, scale: null });
      }
      if (action === "screenshot") {
        attempts += 1;
        inFlight += 1;
        maximumInFlight = Math.max(maximumInFlight, inFlight);
        await settle(5);
        inFlight -= 1;
        if (attempts < 3) {
          return new Response(null, {
            status: 204,
            headers: { "Cache-Control": "no-store" },
          });
        }
        return frameResponse(11);
      }
      return jsonResponse({ ok: true });
    };
    const { controller, elements } = fixture(request);
    controller.activate();
    await settle();
    elements.screenshot.dispatch("click");
    await settle(400);
    expect(attempts).toBe(3);
    expect(maximumInFlight).toBe(1);
    expect(controller.snapshot().frameCount).toBe(1);
    expect(controller.snapshot().lastSequence).toBe(11);
    controller.dispose();
  });

  test("keeps both pages structurally aligned and gateway remote requests on the chat child", () => {
    const directHtml = read("src/webui/index.html");
    const gatewayHtml = read("src/gateway_webui/index.html");
    const expectedOrder = ["data-remote-start", "data-remote-stop", "data-remote-screenshot", "data-remote-fps", "data-remote-scale"];
    for (const html of [directHtml, gatewayHtml]) {
      let cursor = -1;
      for (const marker of expectedOrder) {
        const next = html.indexOf(marker);
        expect(next).toBeGreaterThan(cursor);
        cursor = next;
      }
      expect(html).toContain('<option value="1">1 FPS</option>');
      expect(html).toContain('<option value="3" selected>3 FPS</option>');
      expect(html).toContain('<option value="50" selected>50%</option>');
      expect(html).toContain("frame: 0");
      expect(html).toContain('<script src="/remote-control.js"></script>');
    }
    const directApp = read("src/webui/app.js");
    const gatewayApp = read("src/gateway_webui/app.js");
    expect(directApp).toContain("/api/remote-control/${encodeURIComponent(action)}");
    expect(gatewayApp).toContain('scopedApiPath(`/api/remote-control/${encodeURIComponent(action)}`, "chat")');
    expect(gatewayApp).toContain('path.startsWith("/api/remote-control/")');
  });

  test("does not introduce a video transport, frame base64, or browser persistence", () => {
    const source = read("src/webui/remote-control.js");
    for (const forbidden of ["WebSocket", "EventSource", "RTCPeerConnection", "localStorage", "sessionStorage", "indexedDB", "FileReader", "readAsDataURL"]) {
      expect(source).not.toContain(forbidden);
    }
    expect(source).toContain("response.blob()");
    expect(source).toContain("MAX_PENDING_INPUT_EVENTS = 256");
    expect(source).toContain("MAX_INPUT_BATCH_EVENTS = 128");
    expect(source).toContain("frameAbort.abort()");
    expect(source).not.toContain(".at(");
    expect(source).toContain("keepaliveInFlight");
    expect(source).toContain("settingsPending");
    expect(source).toContain("MAX_SCREENSHOT_BUSY_RETRIES = 5");
  });
});
