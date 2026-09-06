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
    constructor(tag) { this.tag = tag; this.children = []; this.dataset = {}; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
  }
  const requests = [], observers = [], revoked = [], timers = new Set();
  let Gallery, nextUrl = 0;
  runInNewContext(source, {
    HTMLElement: Element,
    document: { createElement: (tag) => new Element(tag) },
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
    gallery.connectedCallback();
    return gallery;
  }
  const finish = async (index, mime = "image/jpeg") => {
    requests[index].resolve({ ok: true, headers: { get: () => mime }, blob: async () => ({}) });
    await flush();
  };
  return { gallery, requests, observers, revoked, timers, finish };
}

test("gallery only requests visible JPEG previews and exposes ordered original links", async () => {
  const h = harness();
  const g = h.gallery([item(1), item(2)]);
  await flush();
  expect(h.requests).toHaveLength(0);
  expect(g.children.map((figure) => figure.children[1].children[0].href)).toEqual([item(1).original, item(2).original]);
  const save = g.children[0].children[1].children[0];
  expect(save.download).toBe(item(1).filename);
  let prevented = false;
  await save.onclick({ preventDefault: () => { prevented = true; } });
  expect(prevented).toBe(false);
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
  const save = g.children[0].children[1].children[0];
  await save.onclick({ preventDefault: () => { prevented = true; } });
  expect(prevented).toBe(true);
  expect(calls).toEqual([[valid.original, valid.filename]]);
  expect(save.textContent).toBe("已保存原图");
  expect(h.requests).toHaveLength(0);
  g.disconnectedCallback();
});
