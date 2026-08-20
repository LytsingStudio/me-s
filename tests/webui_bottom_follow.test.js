"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function loadBottomFollowRuntime() {
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI bottom-follow runtime");
  const factory = new Function("document", "performance", "matchMedia", `${source.slice(0, eventBindings)}
    return { createTranscriptBottomFollower, beginConfirmedPromptRender };`);
  return factory(
    { querySelector: () => null },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
  );
}

function loadBottomFollowerFactory() {
  return loadBottomFollowRuntime().createTranscriptBottomFollower;
}

function harness() {
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
    },
  };
  const content = {};
  const runtime = {
    threshold: 24,
    requestFrame(callback) { const id = nextId++; frames.set(id, callback); return id; },
    cancelFrame(id) { frames.delete(id); },
    setDelay(callback) { const id = nextId++; timers.set(id, callback); return id; },
    clearDelay(id) { timers.delete(id); },
    createResizeObserver(callback) {
      resizeCallback = callback;
      return { observe() {}, disconnect() {} };
    },
  };
  const flushFrames = () => {
    while (frames.size) {
      const pending = [...frames.values()];
      frames.clear();
      pending.forEach((callback) => callback());
    }
  };
  const flushTimers = () => {
    const pending = [...timers.values()];
    timers.clear();
    pending.forEach((callback) => callback());
  };
  const create = loadBottomFollowerFactory();
  const follower = create(viewport, content, () => {}, runtime);
  return {
    viewport,
    follower,
    resize: () => resizeCallback?.(),
    flushFrames,
    flushTimers,
  };
}

describe("WebUI transcript bottom follower", () => {
  test("keeps the bottom after an external layout block shrinks the transcript viewport", () => {
    const test = harness();
    test.viewport.clientHeight = 260;
    test.resize();
    test.flushFrames();
    expect(test.viewport.scrollTop).toBe(740);
    expect(test.follower.isNearBottom()).toBe(true);
  });

  test("keeps following repeated late measurements after a full replay", () => {
    const test = harness();
    test.viewport.scrollHeight = 1_600;
    test.resize();
    test.flushFrames();
    expect(test.viewport.scrollTop).toBe(1_200);

    test.viewport.scrollHeight = 2_200;
    test.resize();
    test.flushFrames();
    expect(test.viewport.scrollTop).toBe(1_800);
  });

  test("follows the newly selected session through its initial and deferred layouts", () => {
    const test = harness();
    test.viewport.scrollHeight = 3_000;
    test.follower.follow();
    test.resize();
    test.flushFrames();
    expect(test.viewport.scrollTop).toBe(2_600);

    test.viewport.scrollHeight = 3_300;
    test.resize();
    test.flushFrames();
    expect(test.viewport.scrollTop).toBe(2_900);
  });

  test("only a real user scroll away disables following", () => {
    const test = harness();
    test.follower.noteUserInteraction();
    test.flushTimers();
    expect(test.follower.isFollowing()).toBe(true);

    test.follower.noteUserInteraction();
    test.viewport.scrollTop = 300;
    test.follower.noteScroll();
    test.flushTimers();
    expect(test.follower.isFollowing()).toBe(false);

    test.viewport.scrollHeight = 1_500;
    test.resize();
    test.flushFrames();
    expect(test.viewport.scrollTop).toBe(300);

    test.follower.noteUserInteraction();
    test.viewport.scrollTop = test.viewport.scrollHeight;
    test.follower.noteScroll();
    test.flushTimers();
    test.flushFrames();
    expect(test.follower.isFollowing()).toBe(true);
    expect(test.follower.isNearBottom()).toBe(true);
  });

  test("only this page's authoritative pending confirmation forces following", () => {
    const { beginConfirmedPromptRender } = loadBottomFollowRuntime();
    let followCount = 0;
    const follower = { follow() { followCount += 1; } };

    expect(beginConfirmedPromptRender({ promptConfirmed: false }, follower)).toBe(false);
    expect(followCount).toBe(0);
    expect(beginConfirmedPromptRender({ promptConfirmed: true }, follower)).toBe(true);
    expect(followCount).toBe(1);
  });
});
