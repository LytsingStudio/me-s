"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { reconcileChildren, reconcileNode } = require("../src/webui/transcript.js");

class FakeNode {
  constructor(nodeType, value = "") {
    this.nodeType = nodeType;
    this.tagName = nodeType === 1 ? value.toUpperCase() : undefined;
    this.data = nodeType === 3 || nodeType === 8 ? value : undefined;
    this.childNodes = [];
    this.parentNode = null;
    this.attributeMap = new Map();
    this.scrollLeft = 0;
  }

  get attributes() {
    return [...this.attributeMap].map(([name, value]) => ({ name, value }));
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

  test("keeps me-s and gateway on the shared stable-DOM and gesture paths", () => {
    const single = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const gateway = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    for (const source of [single, gateway]) {
      expect(source).toContain("MeTranscript.reconcileHtmlChildren(markdown, rendered)");
      expect(source).toContain("MeTranscript.reconcileNode(details, replacement)");
      expect(source).toContain('typeof window.PointerEvent === "function"');
      expect(source).toContain('addEventListener("scrollend", finishTranscriptScrolling');
      expect(source).not.toContain("markdown.innerHTML = rendered");
      expect(source).not.toContain("if (forceFull) replaceElementChildren(elements.transcriptContent)");
    }
    const singleIndex = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const gatewayIndex = readFileSync(join(import.meta.dir, "../src/gateway_webui/index.html"), "utf8");
    for (const html of [singleIndex, gatewayIndex]) {
      expect(html.indexOf('/transcript.js')).toBeGreaterThan(html.indexOf('/markdown.js'));
      expect(html.indexOf('/transcript.js')).toBeLessThan(html.indexOf('/app.js'));
    }
  });

  test("updates Compact notices and session text in place in both WebUIs", () => {
    for (const relative of ["../src/webui/app.js", "../src/gateway_webui/app.js"]) {
      const runtime = loadMessageUpdateRuntime(relative);
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
    }
  });
});
