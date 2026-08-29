"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { createVirtualTranscript, reconcileChildren, reconcileNode } = require("../src/webui/transcript.js");
require("../src/webui/edb-cache.js");
const { installDirectFrontendRuntime } = require("./webui_runtime_stub.js");


class FakeNode {
  constructor(nodeType, value = "") {
    this.nodeType = nodeType;
    this.tagName = nodeType === 1 ? value.toUpperCase() : undefined;
    this.data = nodeType === 3 || nodeType === 8 ? value : undefined;
    this.childNodes = [];
    this.parentNode = null;
    this.attributeMap = new Map();
    this.scrollLeft = 0;
    this.style = {};
    this.dataset = {};
    this.className = "";
    this.measuredHeight = 0;
  }

  get attributes() {
    return [...this.attributeMap].map(([name, value]) => ({ name, value }));
  }

  get children() {
    return this.childNodes.filter((node) => node.nodeType === 1);
  }

  get lastElementChild() {
    return this.children[this.children.length - 1] || null;
  }

  get lastChild() {
    return this.childNodes[this.childNodes.length - 1] || null;
  }

  append(node) {
    node.remove();
    node.parentNode = this;
    this.childNodes.push(node);
  }

  insertBefore(node, reference) {
    node.remove();
    const index = reference ? this.childNodes.indexOf(reference) : this.childNodes.length;
    if (index < 0) throw new Error("reference node is not a child");
    node.parentNode = this;
    this.childNodes.splice(index, 0, node);
  }

  replaceWith(node) {
    if (!this.parentNode) return;
    const parent = this.parentNode;
    const index = parent.childNodes.indexOf(this);
    node.remove();
    parent.childNodes[index] = node;
    node.parentNode = parent;
    this.parentNode = null;
  }

  replaceData(offset, count, value) {
    if (!this.characterDataEdits) this.characterDataEdits = [];
    this.characterDataEdits.push({ offset, count, value });
    this.data = this.data.slice(0, offset) + value + this.data.slice(offset + count);
  }

  remove() {
    if (!this.parentNode) return;
    const index = this.parentNode.childNodes.indexOf(this);
    if (index >= 0) this.parentNode.childNodes.splice(index, 1);
    this.parentNode = null;
  }

  getAttribute(name) {
    return this.attributeMap.has(name) ? this.attributeMap.get(name) : null;
  }

  setAttribute(name, value) {
    this.attributeMap.set(name, String(value));
  }

  removeAttribute(name) {
    this.attributeMap.delete(name);
  }

  isEqualNode(other) {
    if (!other || this.nodeType !== other.nodeType || this.tagName !== other.tagName || this.data !== other.data) return false;
    if (JSON.stringify([...this.attributeMap]) !== JSON.stringify([...other.attributeMap])) return false;
    return this.childNodes.length === other.childNodes.length
      && this.childNodes.every((child, index) => child.isEqualNode(other.childNodes[index]));
  }
}

function element(tagName, attributes = {}, ...children) {
  const node = new FakeNode(1, tagName);
  Object.entries(attributes).forEach(([name, value]) => node.setAttribute(name, value));
  children.forEach((child) => node.append(child));
  return node;
}

function text(value) {
  return new FakeNode(3, value);
}

function fragment(...children) {
  const node = new FakeNode(11);
  children.forEach((child) => node.append(child));
  return node;
}

function loadMessageUpdateRuntime(relative) {
  installDirectFrontendRuntime();
  const source = readFileSync(join(import.meta.dir, relative), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error(`could not isolate ${relative}`);
  const replacement = { dataset: {}, classList: { toggle() {} }, addEventListener() {} };
  const factory = new Function("document", "performance", "matchMedia", "MeTranscript", `${source.slice(0, eventBindings)}
    return { state, updateMessageNode, messageRenderRevision };
  `);
  const runtime = factory(
    {
      cookie: "",
      documentElement: { classList: { toggle() {} } },
      querySelector: () => null,
      createElement: () => ({
        set innerHTML(_value) {},
        content: { firstElementChild: replacement },
      }),
    },
    { now: () => 0 },
    () => ({ matches: false, addEventListener() {} }),
    { reconcileHtmlChildren() {} },
  );
  return { ...runtime, replacement };
}

function textMessageNode(kind, content) {
  let replacement = null;
  const node = {
    dataset: { messageVisible: "true", messageKind: kind },
    meRenderRevision: "old",
    querySelector(selector) {
      return selector === `:scope > .${kind}-content` ? content : null;
    },
    replaceWith(value) { replacement = value; },
  };
  return { node, replacement: () => replacement };
}

function virtualHarness(options = {}) {
  const frames = new Map();
  let nextFrame = 1;
  let resizeCallback = null;
  let following = Boolean(options.following);
  const viewport = {
    clientHeight: options.clientHeight ?? 200,
    clientWidth: options.clientWidth ?? 800,
    scrollTop: options.scrollTop ?? 0,
    getBoundingClientRect() { return { top: 0, bottom: this.clientHeight, height: this.clientHeight }; },
  };
  const documentRef = { createElement: (tagName) => element(tagName) };
  const content = element("div");
  content.ownerDocument = documentRef;
  content.clientWidth = viewport.clientWidth;
  let controller = null;
  const renderRange = (container, items, start, end) => {
    const existing = new Map(container.children.map((node) => [node.dataset.messageKey, node]));
    let position = 0;
    for (let index = start; index < end; index += 1) {
      const item = items[index];
      let node = container.children[position];
      if (!node || node.dataset.messageKey !== item.key) {
        node = existing.get(item.key) || element("div");
        container.insertBefore(node, container.children[position] || null);
      }
      node.dataset.messageKey = item.key;
      node.dataset.messageIndex = String(index);
      node.item = item;
      node.getBoundingClientRect = () => {
        const topSpacer = Number.parseFloat(content.children[0].style.height) || 0;
        const before = controller.windowElement.children
          .slice(0, controller.windowElement.children.indexOf(node))
          .reduce((total, candidate) => total + candidate.item.height, 0);
        const top = topSpacer + before - viewport.scrollTop;
        return { top, bottom: top + node.item.height, height: node.item.height };
      };
      position += 1;
    }
    while (container.children.length > position) container.lastElementChild.remove();
  };
  controller = createVirtualTranscript(viewport, content, {
    targetHeight: options.targetHeight ?? 500,
    edgeOverscan: options.edgeOverscan ?? 50,
    key: (item) => item.key,
    revision: (item) => item.revision ?? 0,
    context: (item) => item.kind ?? "message",
    estimateHeight: (item) => item.estimate ?? 100,
    renderRange,
    renderEmpty: (container) => {
      while (container.lastChild) container.lastChild.remove();
      container.append(element("div"));
    },
    isFollowing: () => following,
  }, {
    requestFrame(callback) { const id = nextFrame++; frames.set(id, callback); return id; },
    cancelFrame(id) { frames.delete(id); },
    createResizeObserver(callback) {
      resizeCallback = callback;
      return { observe() {}, disconnect() {} };
    },
    measureNode: (node) => node.item?.height || 0,
  });
  const flushFrames = () => {
    while (frames.size) {
      const callbacks = [...frames.values()];
      frames.clear();
      callbacks.forEach((callback) => callback());
    }
  };
  return {
    viewport, content, controller,
    setFollowing(value) { following = Boolean(value); },
    resize() { resizeCallback?.(); },
    flushFrames,
  };
}

function virtualItems(count, height = 100) {
  return Array.from({ length: count }, (_, index) => ({
    key: `message-${index}`, revision: 1, estimate: 100, height, kind: "message",
  }));
}

describe("shared WebUI transcript reconciliation", () => {
  test("preserves stable Markdown media and scroll containers while text grows", () => {
    const paragraphText = text("Stable ");
    const link = element("a", { href: "https://example.com" }, text("link"));
    const paragraph = element("p", {}, paragraphText, link);
    const image = element("img", { src: "pixel.png", loading: "lazy" });
    const code = element("code", {}, text("0123456789"));
    const pre = element("pre", {}, code);
    pre.scrollLeft = 80;
    const target = fragment(paragraph, image, pre);

    const source = fragment(
      element("p", {}, text("Stable and growing "), element("a", { href: "https://example.com" }, text("link"))),
      element("img", { src: "pixel.png", loading: "lazy" }),
      element("pre", {}, element("code", {}, text("0123456789 more"))),
      element("p", {}, text("New streamed block")),
    );

    reconcileChildren(target, source);
    expect(target.childNodes[0]).toBe(paragraph);
    expect(paragraph.childNodes[0]).toBe(paragraphText);
    expect(paragraph.childNodes[1]).toBe(link);
    expect(target.childNodes[1]).toBe(image);
    expect(target.childNodes[2]).toBe(pre);
    expect(pre.childNodes[0]).toBe(code);
    expect(pre.scrollLeft).toBe(80);
    expect(paragraphText.data).toBe("Stable and growing ");
    expect(paragraphText.characterDataEdits).toEqual([{ offset: 7, count: 0, value: "and growing " }]);
    expect(code.childNodes[0].characterDataEdits).toEqual([{ offset: 10, count: 0, value: " more" }]);
    expect(code.childNodes[0].data).toBe("0123456789 more");
    expect(target.childNodes[3].tagName).toBe("P");
  });

  test("keeps the active block while an unfinished delimiter becomes structured markup", () => {
    const originalText = text("Prefix **bold");
    const paragraph = element("p", {}, originalText);
    const target = fragment(paragraph);
    const source = fragment(element("p", {}, text("Prefix "), element("strong", {}, text("bold"))));

    reconcileChildren(target, source);
    expect(target.childNodes[0]).toBe(paragraph);
    expect(paragraph.childNodes[0]).toBe(originalText);
    expect(originalText.data).toBe("Prefix ");
    expect(paragraph.childNodes[1].tagName).toBe("STRONG");
  });

  test("preserves equal siblings across insertions and removals", () => {
    const first = element("p", {}, text("first"));
    const second = element("p", {}, text("second"));
    const target = fragment(first, second);

    reconcileChildren(target, fragment(
      element("p", {}, text("inserted")),
      element("p", {}, text("first")),
      element("p", {}, text("second")),
    ));
    expect(target.childNodes[1]).toBe(first);
    expect(target.childNodes[2]).toBe(second);

    reconcileChildren(target, fragment(element("p", {}, text("second"))));
    expect(target.childNodes).toHaveLength(1);
    expect(target.childNodes[0]).toBe(second);
  });

  test("keeps keyed disclosure identity and runtime state when output is inserted", () => {
    const input = element("section", { "data-reconcile-key": "input" }, text("input"));
    const rawInput = element("section", {}, text("raw input"));
    rawInput.scrollLeft = 64;
    const raw = element("details", {
      "data-reconcile-key": "raw",
      "data-reconcile-preserve-open": "true",
      open: "",
    }, rawInput);
    const target = fragment(input, raw);
    const source = fragment(
      element("section", { "data-reconcile-key": "input" }, text("input")),
      element("section", { "data-reconcile-key": "output" }, text("output")),
      element("details", {
        "data-reconcile-key": "raw",
        "data-reconcile-preserve-open": "true",
      }, element("section", {}, text("raw input")), element("section", {}, text("raw output"))),
    );

    reconcileChildren(target, source);
    expect(target.childNodes[0]).toBe(input);
    expect(target.childNodes[1].getAttribute("data-reconcile-key")).toBe("output");
    expect(target.childNodes[2]).toBe(raw);
    expect(raw.getAttribute("open")).toBe("");
    expect(raw.childNodes[0]).toBe(rawInput);
    expect(rawInput.scrollLeft).toBe(64);
    expect(raw.childNodes[1].childNodes[0].data).toBe("raw output");
  });

  test("updates attributes without replacing a compatible element", () => {
    const link = element("a", { href: "https://old.example", title: "old" }, text("link"));
    const target = fragment(link);
    reconcileNode(link, element("a", { href: "https://new.example", rel: "noopener" }, text("link")));
    expect(target.childNodes[0]).toBe(link);
    expect(link.getAttribute("href")).toBe("https://new.example");
    expect(link.getAttribute("title")).toBeNull();
    expect(link.getAttribute("rel")).toBe("noopener");
  });

  test("bounds the materialized DOM and represents omitted history with two spacers", () => {
    const subject = virtualHarness();
    const items = virtualItems(100);
    subject.controller.update(items, { scopeKey: "main", force: true, following: false });
    const state = subject.controller.inspect();
    expect(state.start).toBe(0);
    expect(state.end).toBeLessThanOrEqual(6);
    expect(state.materialized).toBeLessThanOrEqual(6);
    expect(state.topHeight).toBe(0);
    expect(state.bottomHeight).toBeGreaterThan(9_000);
    expect(subject.content.children).toHaveLength(3);
    expect(subject.content.children[0].className).toContain("transcript-spacer-top");
    expect(subject.content.children[1]).toBe(subject.controller.windowElement);
    expect(subject.content.children[2].className).toContain("transcript-spacer-bottom");
  });

  test("moves the virtual window both ways and reuses overlapping keyed nodes", () => {
    const subject = virtualHarness();
    const items = virtualItems(100);
    subject.controller.update(items, { scopeKey: "main", force: true, following: false });
    const stable = subject.controller.windowElement.children
      .find((node) => node.dataset.messageKey === "message-4");
    subject.viewport.scrollTop = 450;
    subject.controller.noteScroll();
    subject.flushFrames();
    expect(subject.controller.inspect().start).toBeGreaterThan(0);
    expect(subject.controller.windowElement.children
      .find((node) => node.dataset.messageKey === "message-4")).toBe(stable);
    subject.viewport.scrollTop = 1_400;
    subject.controller.noteScroll();
    subject.flushFrames();
    const lower = subject.controller.inspect();
    expect(lower.start).toBeGreaterThan(10);
    expect(lower.end).toBeLessThan(25);
    subject.viewport.scrollTop = 250;
    subject.controller.noteScroll();
    subject.flushFrames();
    expect(subject.controller.inspect().start).toBeLessThan(5);
  });

  test("preserves the visible message anchor when a measured height above it changes", () => {
    const subject = virtualHarness({ scrollTop: 250 });
    const items = virtualItems(30);
    subject.controller.update(items, { scopeKey: "main", force: true, following: false });
    const anchor = subject.controller.windowElement.children
      .find((node) => node.dataset.messageKey === "message-2");
    expect(anchor.getBoundingClientRect().top).toBe(-50);
    items[1].height = 180;
    subject.resize();
    subject.flushFrames();
    expect(subject.viewport.scrollTop).toBe(330);
    expect(anchor.getBoundingClientRect().top).toBe(-50);
  });

  test("keeps one oversized atomic message intact beyond the soft pixel budget", () => {
    const subject = virtualHarness({ scrollTop: 150 });
    const items = virtualItems(3);
    items[1].estimate = 900;
    items[1].height = 900;
    subject.controller.update(items, { scopeKey: "main", force: true, following: false });
    expect(subject.controller.windowElement.children
      .some((node) => node.dataset.messageKey === "message-1")).toBe(true);
    expect(subject.controller.windowElement.children
      .find((node) => node.dataset.messageKey === "message-1").item.height).toBe(900);
  });

  test("keeps the tail materialized while following but leaves history browsing anchored", () => {
    const tail = virtualHarness({ following: true });
    const tailItems = virtualItems(100);
    tail.controller.update(tailItems, { scopeKey: "main", force: true, following: true });
    const appendedTail = [...tailItems, {
      key: "message-100", revision: 1, estimate: 100, height: 100, kind: "message",
    }];
    tail.controller.update(appendedTail, { scopeKey: "main", changedFrom: 100, following: true });
    expect(tail.controller.inspect().end).toBe(101);
    expect(tail.controller.windowElement.children.at(-1).dataset.messageKey).toBe("message-100");

    const history = virtualHarness({ following: false, scrollTop: 0 });
    const historyItems = virtualItems(100);
    history.controller.update(historyItems, { scopeKey: "main", force: true, following: false });
    const before = history.controller.inspect();
    const appendedHistory = [...historyItems, {
      key: "message-100", revision: 1, estimate: 100, height: 100, kind: "message",
    }];
    history.controller.update(appendedHistory, { scopeKey: "main", changedFrom: 100, following: false });
    const after = history.controller.inspect();
    expect(after.start).toBe(before.start);
    expect(after.end).toBe(before.end);
    expect(history.viewport.scrollTop).toBe(0);
    expect(after.bottomHeight).toBe(before.bottomHeight + 100);
  });

  test("keeps the authoritative shared core on the stable-DOM and gesture paths", () => {
    const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    expect(source).toContain("MeTranscript.reconcileHtmlChildren(markdown, rendered)");
    expect(source).toContain("MeTranscript.reconcileNode(details, replacement)");
    expect(source).toContain('typeof window.PointerEvent === "function"');
    expect(source).toContain('addEventListener("scrollend", finishTranscriptScrolling');
    expect(source).toContain("MeTranscript.createVirtualTranscript(");
    expect(source).toContain("transcriptVirtualizer.noteScroll()");
    expect(source).toContain("renderRange: reconcileTranscript");
    expect(source).not.toContain("markdown.innerHTML = rendered");
    expect(source).not.toContain("if (forceFull) replaceElementChildren(elements.transcriptContent)");
    const index = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    expect(index.indexOf('/transcript.js')).toBeGreaterThan(index.indexOf('/markdown.js'));
    expect(index.indexOf('/transcript.js')).toBeLessThan(index.indexOf('/app.js'));
  });

  test("updates Compact notices and session text in place in the shared core", () => {
    const runtime = loadMessageUpdateRuntime("../src/webui/app.js");
    runtime.state.selectedAgent = "main";
    for (const kind of ["notice", "session"]) {
      const content = { textContent: "旧文本" };
      const stable = textMessageNode(kind, content);
      const message = {
        kind, content: kind === "notice" ? "正在压缩 (1/6) ... ↓ 37" : "新会话状态",
        revision: 7, presentationRevision: 3, timestamp: 10,
      };
      runtime.updateMessageNode(stable.node, message, false, false, 0);
      expect(stable.replacement()).toBeNull();
      expect(stable.node.querySelector(`:scope > .${kind}-content`)).toBe(content);
      expect(content.textContent).toBe(message.content);
      expect(stable.node.meRenderRevision).toBe("7:3:0:0:0:0");
    }

    const incompatible = textMessageNode("notice", null);
    runtime.updateMessageNode(incompatible.node, {
      kind: "notice", content: "结构恢复", revision: 8, presentationRevision: 0, timestamp: 11,
    }, false, false, 0);
    expect(incompatible.replacement()).toBe(runtime.replacement);
  });
});
