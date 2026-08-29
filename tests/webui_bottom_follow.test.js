"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { createTranscriptBottomFollower } = require("../src/webui/transcript.js");
require("../src/webui/edb-cache.js");
const { installDirectFrontendRuntime } = require("./webui_runtime_stub.js");


function loadPromptConfirmationRuntime() {
  installDirectFrontendRuntime();
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI prompt-confirmation runtime");
  const factory = new Function("document", "performance", "matchMedia", `${source.slice(0, eventBindings)}
    return { beginConfirmedPromptRender };`);
  return factory(
    { querySelector: () => null, documentElement: { classList: { toggle() {} } } },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
  );
}

function harness(options = {}) {
  const frames = new Map();
  const timers = new Map();
  let nextId = 1;
  let resizeCallback = null;
  const viewport = {
    scrollHeight: 1_000,
    clientHeight: 400,
    _scrollTop: 600,
    get scrollTop() { return this._scrollTop; },
    set scrollTop(value) {
      this._scrollTop = Math.max(0, Math.min(Number(value), this.scrollHeight - this.clientHeight));
      options.onScrollTopChange?.(this._scrollTop);
    },
  };
  const content = {};
  const runtime = {
    threshold: 24,
    settleDelay: 180,
    requestFrame(callback) { const id = nextId++; frames.set(id, callback); return id; },
    cancelFrame(id) { frames.delete(id); },
    setDelay(callback) { const id = nextId++; timers.set(id, callback); return id; },
    clearDelay(id) { timers.delete(id); },
    createResizeObserver(callback) {
      resizeCallback = callback;
      return { observe() {}, disconnect() {} };
    },
    interruptKineticScroll: options.interruptKineticScroll,
  };
  const flushFrame = () => {
    const pending = [...frames.values()];
    frames.clear();
    pending.forEach((callback) => callback());
  };
  const flushFrames = () => {
    while (frames.size) flushFrame();
  };
  const flushTimers = () => {
    const pending = [...timers.values()];
    timers.clear();
    pending.forEach((callback) => callback());
  };
  const follower = createTranscriptBottomFollower(viewport, content, () => {}, runtime);
  return {
    viewport,
    follower,
    resize: () => resizeCallback?.(),
    flushFrames,
    flushFrame,
    flushTimers,
    pendingFrames: () => frames.size,
    pendingTimers: () => timers.size,
  };
}

function scrollAway(test, value = 300) {
  test.follower.beginUserInteraction();
  test.viewport.scrollTop = value;
  test.follower.noteScroll();
  test.follower.endUserInteraction();
  test.flushTimers();
}

describe("WebUI transcript bottom follower", () => {
  test("keeps the bottom after an external layout block shrinks the transcript viewport", () => {
    const subject = harness();
    subject.viewport.clientHeight = 260;
    subject.resize();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(740);
    expect(subject.follower.isNearBottom()).toBe(true);
  });

  test("keeps following repeated late measurements after a full replay", () => {
    const subject = harness();
    subject.viewport.scrollHeight = 1_600;
    subject.resize();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(1_200);

    subject.viewport.scrollHeight = 2_200;
    subject.resize();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(1_800);
  });

  test("follows the newly selected session through its initial and deferred layouts", () => {
    const subject = harness();
    subject.viewport.scrollHeight = 3_000;
    subject.follower.follow();
    subject.resize();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(2_600);

    subject.viewport.scrollHeight = 3_300;
    subject.resize();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(2_900);
  });

  test("only a real user scroll away disables following", () => {
    const subject = harness();
    subject.follower.noteUserInteraction();
    subject.flushTimers();
    expect(subject.follower.isFollowing()).toBe(true);

    scrollAway(subject);
    expect(subject.follower.isFollowing()).toBe(false);

    subject.viewport.scrollHeight = 1_500;
    subject.resize();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(300);

    subject.follower.beginUserInteraction();
    subject.viewport.scrollTop = subject.viewport.scrollHeight;
    subject.follower.noteScroll();
    subject.follower.endUserInteraction();
    subject.flushTimers();
    subject.flushFrames();
    expect(subject.follower.isFollowing()).toBe(true);
    expect(subject.follower.isNearBottom()).toBe(true);
  });

  test("keeps forcing the bottom while kinetic scroll events continue after the button click", () => {
    const subject = harness();
    scrollAway(subject);
    expect(subject.follower.isFollowing()).toBe(false);

    subject.follower.follow();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(600);

    subject.viewport.scrollTop = 360;
    subject.follower.noteScroll();
    expect(subject.pendingFrames()).toBe(1);
    expect(subject.viewport.scrollTop).toBe(600);
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(600);

    subject.viewport.scrollTop = 420;
    subject.follower.noteScroll();
    subject.flushFrames();
    subject.flushTimers();
    expect(subject.viewport.scrollTop).toBe(600);
    expect(subject.follower.isFollowing()).toBe(true);
    expect(subject.follower.isNearBottom()).toBe(true);
  });

  test("interrupts kinetic scrolling before synchronously committing explicit follow", () => {
    const operations = [];
    const subject = harness({
      interruptKineticScroll() { operations.push("interrupt"); },
      onScrollTopChange(value) { operations.push(`scroll:${value}`); },
    });
    scrollAway(subject);
    operations.length = 0;

    subject.follower.follow();

    expect(operations).toEqual(["interrupt", "scroll:600"]);
    expect(subject.viewport.scrollTop).toBe(600);
  });

  test("keeps explicit follow locked through programmatic scrollend and late inertia", () => {
    const subject = harness();
    scrollAway(subject);
    subject.follower.follow();
    subject.flushFrames();

    subject.follower.noteScrollEnd();
    subject.viewport.scrollTop = 360;
    subject.follower.noteScroll();
    expect(subject.viewport.scrollTop).toBe(600);

    subject.flushTimers();
    subject.viewport.scrollTop = 420;
    subject.follower.noteScroll();
    expect(subject.viewport.scrollTop).toBe(600);
  });

  test("keeps the scroll layer disabled across a paint and restores it without leaking styles", () => {
    const operations = [];
    const values = new Map();
    const priorities = new Map();
    const style = {
      getPropertyValue(name) { return values.get(name) || ""; },
      getPropertyPriority(name) { return priorities.get(name) || ""; },
      setProperty(name, value, priority = "") {
        values.set(name, value);
        priorities.set(name, priority);
        operations.push(`set:${name}:${value}:${priority}`);
      },
      removeProperty(name) {
        values.delete(name);
        priorities.delete(name);
        operations.push(`remove:${name}`);
      },
    };
    const subject = harness({
      onScrollTopChange(value) { operations.push(`scroll:${value}`); },
    });
    subject.viewport.style = style;
    Object.defineProperty(subject.viewport, "offsetHeight", {
      get() { operations.push("layout"); return 400; },
    });
    scrollAway(subject);
    operations.length = 0;

    subject.follower.follow();

    expect(operations).toEqual([
      "set:overflow:hidden:important",
      "set:-webkit-overflow-scrolling:auto:important",
      "layout",
      "scroll:600",
    ]);
    expect(style.getPropertyValue("overflow")).toBe("hidden");
    expect(style.getPropertyValue("-webkit-overflow-scrolling")).toBe("auto");

    subject.flushFrame();
    expect(style.getPropertyValue("overflow")).toBe("hidden");
    expect(style.getPropertyValue("-webkit-overflow-scrolling")).toBe("auto");

    subject.flushFrame();
    expect(operations.slice(-4)).toEqual([
      "remove:overflow",
      "remove:-webkit-overflow-scrolling",
      "layout",
      "scroll:600",
    ]);
    expect(style.getPropertyValue("overflow")).toBe("");
    expect(style.getPropertyValue("-webkit-overflow-scrolling")).toBe("");
    subject.flushFrames();

    operations.length = 0;
    subject.follower.follow();
    subject.follower.beginUserInteraction();
    expect(operations.slice(-3)).toEqual([
      "remove:overflow",
      "remove:-webkit-overflow-scrolling",
      "layout",
    ]);
    expect(style.getPropertyValue("overflow")).toBe("");
    expect(style.getPropertyValue("-webkit-overflow-scrolling")).toBe("");
    expect(subject.pendingFrames()).toBe(0);
  });

  test("tracks inertia after pointer release until the scroll really settles", () => {
    const subject = harness();
    subject.follower.beginUserInteraction();
    subject.viewport.scrollTop = 590;
    subject.follower.noteScroll();
    subject.follower.endUserInteraction();

    subject.viewport.scrollTop = 300;
    subject.follower.noteScroll();
    subject.flushTimers();
    subject.flushFrames();
    expect(subject.follower.isFollowing()).toBe(false);
    expect(subject.viewport.scrollTop).toBe(300);
  });

  test("restores following when inertia reaches the bottom after pointer release", () => {
    const subject = harness();
    scrollAway(subject);
    subject.follower.beginUserInteraction();
    subject.viewport.scrollTop = 500;
    subject.follower.noteScroll();
    subject.follower.endUserInteraction();

    subject.viewport.scrollTop = subject.viewport.scrollHeight;
    subject.follower.noteScroll();
    subject.flushTimers();
    subject.flushFrames();
    expect(subject.follower.isFollowing()).toBe(true);
    expect(subject.follower.isNearBottom()).toBe(true);
  });

  test("a new user gesture cancels an explicit forced follow", () => {
    const subject = harness();
    scrollAway(subject);
    subject.follower.follow();
    expect(subject.pendingFrames()).toBe(1);

    subject.follower.beginUserInteraction();
    subject.viewport.scrollTop = 250;
    subject.follower.noteScroll();
    subject.follower.endUserInteraction();
    subject.flushFrames();
    subject.flushTimers();
    expect(subject.viewport.scrollTop).toBe(250);
    expect(subject.follower.isFollowing()).toBe(false);
  });

  test("uses scrollend to finish the active user scroll without waiting for fallback delay", () => {
    const subject = harness();
    subject.follower.beginUserInteraction();
    subject.viewport.scrollTop = 300;
    subject.follower.noteScroll();
    subject.follower.endUserInteraction();
    expect(subject.pendingTimers()).toBe(1);

    subject.follower.noteScrollEnd();
    expect(subject.pendingTimers()).toBe(0);
    expect(subject.follower.isFollowing()).toBe(false);
  });

  test("deduplicates explicit and observed notifications for one layout change", () => {
    let writes = 0;
    const subject = harness({ onScrollTopChange() { writes += 1; } });
    subject.viewport.clientHeight = 260;

    subject.follower.layoutChanged();
    subject.flushFrames();
    subject.resize();
    subject.flushFrames();

    expect(writes).toBe(1);
    expect(subject.viewport.scrollTop).toBe(740);
  });

  test("skips ordinary bottom writes when layout geometry is unchanged", () => {
    let writes = 0;
    const subject = harness({ onScrollTopChange() { writes += 1; } });

    subject.follower.layoutChanged();
    subject.flushFrames();
    subject.resize();
    subject.flushFrames();

    expect(writes).toBe(0);
    expect(subject.follower.isNearBottom()).toBe(true);
  });

  test("explicit follow still writes when layout geometry is unchanged", () => {
    let writes = 0;
    const subject = harness({ onScrollTopChange() { writes += 1; } });

    subject.follower.follow();

    expect(writes).toBe(1);
    expect(subject.viewport.scrollTop).toBe(600);
  });

  test("only this page's authoritative pending confirmation forces following", () => {
    const { beginConfirmedPromptRender } = loadPromptConfirmationRuntime();
    let followCount = 0;
    const follower = { follow() { followCount += 1; } };

    expect(beginConfirmedPromptRender({ promptConfirmed: false }, follower)).toBe(false);
    expect(followCount).toBe(0);
    expect(beginConfirmedPromptRender({ promptConfirmed: true }, follower)).toBe(true);
    expect(followCount).toBe(1);
  });
});
