"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync, existsSync } = require("node:fs");
const { join } = require("node:path");
const vm = require("node:vm");
const root = join(import.meta.dir, "..");
const source = readFileSync(join(root, "src/webui/app.js"), "utf8");
const cleanup = source.slice(source.indexOf("function removeLegacyBrowserCache()"), source.indexOf("async function initializeAuthentication()"));

function runtime(overrides = {}) {
  const preferences = new Map([
    ["me-raw-edb-decoding", "true"], ["me-theme", "obsidian"],
    ["me-color-mode", "dark"], ["me-send-shortcut", "shift-enter"],
    ["me-window-border-style", "theme"], ["unrelated", "preserved"],
  ]);
  const deleted = [], warnings = [], requests = [];
  const context = vm.createContext({
    localStorage: { removeItem(key) { preferences.delete(key); } },
    indexedDB: { deleteDatabase(name) { deleted.push(name); const request = {}; requests.push(request); return request; } },
    console: { warn(...args) { warnings.push(args); } },
    ...overrides,
  });
  vm.runInContext(cleanup, context);
  return { run: () => context.removeLegacyBrowserCache(), preferences, deleted, warnings, requests };
}

describe("legacy frontend cache cleanup", () => {
  test("targets only the known database and preference without reading historical events", () => {
    const r = runtime();
    expect(r.run()).toBeUndefined();
    expect(r.deleted).toEqual(["me-edb-cache"]);
    expect([...r.preferences.keys()]).toEqual(["me-theme", "me-color-mode", "me-send-shortcut", "me-window-border-style", "unrelated"]);
    expect(source).toContain("removeLegacyBrowserCache();");
  });

  test("does not wait for a blocked deletion and retries on later startup", () => {
    const r = runtime();
    expect(r.run()).toBeUndefined();
    r.requests[0].onblocked();
    expect(r.warnings).toHaveLength(1);
    expect(r.run()).toBeUndefined();
    expect(r.deleted).toEqual(["me-edb-cache", "me-edb-cache"]);
  });

  test("unavailable or failed browser storage cannot prevent startup", () => {
    expect(runtime({ localStorage: undefined, indexedDB: undefined }).run()).toBeUndefined();
    const r = runtime({
      localStorage: { removeItem() { throw new Error("storage disabled"); } },
      indexedDB: { deleteDatabase() { throw new Error("storage disabled"); } },
    });
    expect(r.run()).toBeUndefined();
    expect(r.warnings).toHaveLength(1);
    const asynchronous = runtime();
    asynchronous.run();
    asynchronous.requests[0].error = new Error("failed");
    asynchronous.requests[0].onerror();
    expect(asynchronous.warnings).toHaveLength(1);
  });

  test("all products ship the projection-only shared frontend", () => {
    expect(existsSync(join(root, "src/webui/edb-cache.js"))).toBe(false);
    const index = readFileSync(join(root, "src/webui/index.html"), "utf8");
    expect(index).not.toContain("/edb-cache.js");
    for (const filename of ["src/webui/runtime.js", "src/gateway_webui/runtime.js", "me-client/client-runtime.js"]) {
      const adapter = readFileSync(join(root, filename), "utf8");
      for (const legacy of ["createEdbCache", "loadCachedSessions", "cacheKey", "cache_load_", "cache_save_"]) expect(adapter).not.toContain(legacy);
    }
    for (const legacy of ["usesUiProjection", "cache_metadata_only", "cursor_event_hash", "function projectChat(", "function consumeChatEvents(", "settings-edb-cache-manager", "兼容渲染模式", "会话缓存"]) expect(source).not.toContain(legacy);
    expect(source).toContain("ui_projection: true");
  });
});
