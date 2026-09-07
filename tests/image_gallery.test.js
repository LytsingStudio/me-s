"use strict";

const { test, expect } = require("bun:test");
const { readFileSync } = require("node:fs");
const { runInNewContext } = require("node:vm");
const source = readFileSync(new URL("../src/webui/image-gallery.js", import.meta.url), "utf8");
const flush = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
const item = (id) => ({
  preview: `/api/images/main/${String(id).repeat(64)}/preview`,
  original: `/api/images/main/${String(id).repeat(64)}/original`,
  filename: `image-${String(id).repeat(12)}.png`, width: 1200, height: 800,
});

function harness(runtime) {
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.dataset = {}; this.inert = false; }
    append(...children) { children.forEach((child) => { child.parent = this; this.children.push(child); }); }
    replaceChildren(...children) { this.children.forEach((child) => { child.parent = null; }); this.children = []; this.append(...children); }
    setAttribute(name, value) { this[name] = value; }
    removeAttribute(name) { delete this[name]; }
    remove() { if (this.parent) this.parent.children = this.parent.children.filter((child) => child !== this); this.parent = null; }
    focus(options) { document.activeElement = this; this.focusOptions = options; }
    get isConnected() { return this.tag === "body" || Boolean(this.parent?.isConnected); }
  }
  const document = { createElement: (tag) => new Element(tag), body: new Element("body"), activeElement: null };
  const requests = [], observers = [], revoked = [], timers = new Set();
  let Gallery, nextUrl = 0;
  runInNewContext(source, {
    HTMLElement: Element,
    document,
    customElements: { define: (_, value) => { Gallery = value; } },
    IntersectionObserver: class {
      constructor(callback) { this.callback = callback; this.targets = []; observers.push(this); }
      observe(target) { this.targets.push(target); }
      unobserve(target) { this.targets = this.targets.filter((item) => item !== target); }
      disconnect() { this.targets = []; }
      reveal(target) { this.callback([{ target, isIntersecting: true }]); }
    },
    fetch: (url, options) => new Promise((resolve, reject) => requests.push({ url, options, resolve, reject })),
    AbortController,
    URL: { createObjectURL: () => `blob:${++nextUrl}`, revokeObjectURL: (url) => revoked.push(url) },
    setTimeout: (callback) => { timers.add(callback); return callback; },
    clearTimeout: (callback) => timers.delete(callback),
    MeFrontendRuntime: runtime,
  });
  function gallery(items) {
    const gallery = new Gallery();
    gallery.dataset.items = JSON.stringify(items);
    document.body.append(gallery);
    gallery.connectedCallback();
    return gallery;
  }
  const finish = async (index, mime = "image/jpeg") => {
    requests[index].resolve({ ok: true, headers: { get: () => mime }, blob: async () => ({}) });
    await flush();
  };
  function open(gallery, index = 0) {
    const opener = gallery.children[index].children[0].children[3];
    opener.onclick();
    const backdrop = document.body.children.find((node) => node.className === "modal-backdrop image-viewer-backdrop");
    const viewer = backdrop.children[0];
    const [title, save, dismiss] = viewer.children[0].children;
    const [image, status, retry] = viewer.children[1].children;
    return { backdrop, viewer, title, save, dismiss, image, status, retry, opener };
  }
  return { gallery, requests, observers, revoked, timers, finish, open, document };
}

test("gallery only requests visible JPEG previews and offers ordered large-image buttons without inline saving", async () => {
  const h = harness();
  const g = h.gallery([item(1), item(2)]);
  await flush();
  expect(h.requests).toHaveLength(0);
  expect(g.children.map((figure) => figure.children[1].children.length)).toEqual([1, 1]);
  expect(g.children.map((figure) => figure.children[1].children[0].href)).toEqual([undefined, undefined]);
  expect(g.children.map((figure) => figure.children[0].children[3]["aria-label"])).toEqual(["查看大图：图片 1", "查看大图：图片 2"]);
  h.observers[0].reveal(g.children[1]);
  await flush();
  expect(h.requests.map((request) => request.url)).toEqual([item(2).preview]);
  await h.finish(0);
  const image = g.children[1].children[0].children[0];
  image.onload();
  expect(image.hidden).toBe(false);
  let stopped = 0;
  g.onclick({ stopPropagation: () => stopped++ });
  g.onkeydown({ stopPropagation: () => stopped++ });
  expect(stopped).toBe(2);
  g.disconnectedCallback();
  expect(h.revoked).toEqual(["blob:1"]);
  expect(h.timers.size).toBe(0);
});

test("global preview queue holds three real requests even when native cancellation settles late", async () => {
  const h = harness();
  const first = h.gallery([item(1), item(2), item(3), item(4)]);
  const second = h.gallery([item(5)]);
  first.children.forEach((figure) => h.observers[0].reveal(figure));
  h.observers[1].reveal(second.children[0]);
  await flush();
  expect(h.requests).toHaveLength(3);
  first.disconnectedCallback();
  expect(h.requests.every((request) => request.options.signal.aborted)).toBe(true);
  await flush();
  expect(h.requests).toHaveLength(3);
  await h.finish(0);
  expect(h.requests).toHaveLength(4);
  expect(h.requests[3].url).toBe(item(5).preview);
  expect(first.children[0].children[0].children[0].src).toBeUndefined();
  await h.finish(1);
  await h.finish(2);
  await h.finish(3);
  expect(h.revoked).toEqual([]);
  second.disconnectedCallback();
  expect(h.revoked).toEqual(["blob:1"]);
});

test("decode failure allows manual retry and revokes the replaced blob before disconnect", async () => {
  const h = harness();
  const g = h.gallery([item(1)]);
  const [image, status, retry] = g.children[0].children[0].children;
  h.observers[0].reveal(g.children[0]);
  await flush();
  await h.finish(0);
  image.onerror();
  expect(retry.hidden).toBe(false);
  expect(status.textContent).toBe("无法加载图片");
  retry.onclick();
  await flush();
  await h.finish(1);
  expect(h.revoked).toEqual(["blob:1"]);
  expect(image.src).toBe("blob:2");
  g.disconnectedCallback();
  expect(h.revoked).toEqual(["blob:1", "blob:2"]);
});

test("invalid preview content and timeout expose retry without an automatic request loop", async () => {
  const h = harness();
  const g = h.gallery([item(1)]);
  h.observers[0].reveal(g.children[0]);
  await flush();
  await h.finish(0, "image/png");
  const retry = g.children[0].children[0].children[2];
  expect(retry.hidden).toBe(false);
  expect(h.requests).toHaveLength(1);
  retry.onclick();
  await flush();
  for (const timeout of h.timers) timeout();
  expect(h.requests[1].options.signal.aborted).toBe(true);
  h.requests[1].reject(new Error("aborted"));
  await flush();
  expect(retry.hidden).toBe(false);
  expect(h.requests).toHaveLength(2);
  g.disconnectedCallback();
});

test("native saving reuses download adapter and unsafe source paths cannot become previews", async () => {
  const calls = [];
  const h = harness({ capabilities: { nativeDownload: true }, downloadFile: async (...args) => calls.push(args) });
  const valid = item(1);
  valid.preview = valid.preview.replace("/api/", "/api/workspaces/work-1/");
  valid.original = valid.original.replace("/api/", "/api/workspaces/work-1/");
  const g = h.gallery([valid, { ...item(2), preview: "https://example.com/private.png" }, { ...item(3), original: "/Users/private.png" }]);
  expect(g.children).toHaveLength(1);
  let prevented = false;
  const { save } = h.open(g);
  await save.onclick({ preventDefault: () => { prevented = true; } });
  expect(prevented).toBe(true);
  expect(calls).toEqual([[valid.original, valid.filename]]);
  expect(save.textContent).toBe("已保存原图");
  expect(h.requests.map((request) => request.url)).toEqual([valid.original]);
  g.disconnectedCallback();
});

test("viewer loads only the selected original, traps focus and closes without scrolling the opener", async () => {
  const h = harness();
  const g = h.gallery([item(1), item(2)]);
  const v = h.open(g, 1);
  expect(h.requests.map((request) => request.url)).toEqual([item(2).original]);
  expect(v.save.href).toBe(item(2).original);
  expect(v.save.download).toBe(item(2).filename);
  expect(v.viewer["aria-modal"]).toBe("true");
  expect(g.inert).toBe(true);
  expect(h.document.activeElement).toBe(v.dismiss);
  expect(v.dismiss.focusOptions).toEqual({ preventScroll: true });
  let prevented = false;
  await v.save.onclick({ preventDefault: () => { prevented = true; } });
  expect(prevented).toBe(false);
  await h.finish(0, "image/png");
  v.image.onload();
  expect(v.image.hidden).toBe(false);
  expect(v.status.hidden).toBe(true);
  v.backdrop.onkeydown({ key: "Tab", stopPropagation() {}, preventDefault() {} });
  expect(h.document.activeElement).toBe(v.save);
  v.backdrop.onkeydown({ key: "Tab", shiftKey: true, stopPropagation() {}, preventDefault() {} });
  expect(h.document.activeElement).toBe(v.dismiss);
  v.backdrop.onkeydown({ key: "Escape", stopPropagation() {}, preventDefault() {} });
  expect(g.inert).toBe(false);
  expect(v.backdrop.isConnected).toBe(false);
  expect(h.document.activeElement).toBe(v.opener);
  expect(v.opener.focusOptions).toEqual({ preventScroll: true });
  expect(v.image.src).toBeUndefined();
  expect(h.revoked).toEqual(["blob:1"]);
  expect(h.timers.size).toBe(0);
});

test("closing or replacing a viewer aborts pending originals and ignores late native replies", async () => {
  const h = harness();
  const g = h.gallery([item(1), item(2)]);
  const first = h.open(g);
  const second = h.open(g, 1);
  expect(first.backdrop.isConnected).toBe(false);
  expect(h.requests[0].options.signal.aborted).toBe(true);
  await h.finish(0, "image/png");
  expect(first.image.src).toBeUndefined();
  expect(second.backdrop.isConnected).toBe(true);
  expect(g.inert).toBe(true);
  g.disconnectedCallback();
  expect(h.requests[1].options.signal.aborted).toBe(true);
  expect(second.backdrop.isConnected).toBe(false);
  expect(g.inert).toBe(false);
  await h.finish(1, "image/png");
  expect(second.image.src).toBeUndefined();
  expect(h.revoked).toEqual([]);
  expect(h.timers.size).toBe(0);
});

test("original errors, timeouts and decode errors are manually retryable; backdrop clicks close", async () => {
  const h = harness();
  const g = h.gallery([item(1)]);
  const v = h.open(g);
  await h.finish(0, "text/html");
  expect(v.status.textContent).toBe("无法加载图片");
  expect(v.retry.hidden).toBe(false);
  expect(h.requests).toHaveLength(1);
  v.retry.onclick();
  for (const timeout of h.timers) timeout();
  expect(h.requests[1].options.signal.aborted).toBe(true);
  await h.finish(1, "image/png");
  expect(v.retry.hidden).toBe(false);
  v.retry.onclick();
  await h.finish(2, "image/png");
  v.image.onerror();
  expect(v.retry.hidden).toBe(false);
  v.retry.onclick();
  await h.finish(3, "image/png");
  expect(h.revoked).toEqual(["blob:1"]);
  v.image.onload();
  v.backdrop.onclick({ target: v.image });
  expect(v.backdrop.isConnected).toBe(true);
  v.backdrop.onclick({ target: v.backdrop });
  expect(v.backdrop.isConnected).toBe(false);
  expect(h.revoked).toEqual(["blob:1", "blob:2"]);
});

test("native save failures can retry and duplicate clicks do not start concurrent downloads", async () => {
  const pending = [];
  const h = harness({ capabilities: { nativeDownload: true }, downloadFile: () => new Promise((resolve, reject) => pending.push({ resolve, reject })) });
  const g = h.gallery([item(1)]);
  const v = h.open(g);
  const event = { preventDefault() {} };
  const first = v.save.onclick(event);
  await v.save.onclick(event);
  expect(pending).toHaveLength(1);
  pending[0].reject(new Error("cancelled"));
  await first;
  expect(v.save.textContent).toBe("保存失败，重试");
  const second = v.save.onclick(event);
  pending[1].resolve();
  await second;
  expect(v.save.textContent).toBe("已保存原图");
  v.dismiss.onclick();
  await h.finish(0);
});
