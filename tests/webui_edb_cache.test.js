"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

require("../src/webui/edb-cache.js");

const cacheModule = globalThis.MeEdbCache;

function event(kind, value = {}) {
  return { [kind]: value };
}

describe("browser-local raw EDB cache", () => {
  test("isolates session identities by authoritative Workspace scope and AgentId", () => {
    expect(cacheModule.sessionKey("/work/a", "main"))
      .toBe(JSON.stringify(["/work/a", "main"]));
    expect(cacheModule.sessionKey("/work/a", "main"))
      .not.toBe(cacheModule.sessionKey("/work/b", "main"));
    expect(cacheModule.sessionKey("/work/a", "agent-2"))
      .not.toBe(cacheModule.sessionKey("/work/a", "main"));
  });

  test("measures the UTF-8 JSON payload without persisting a derived projection", () => {
    const value = event("UserPrompt", { id: 1, content: "你好" });
    expect(cacheModule.eventByteSize(value))
      .toBe(new TextEncoder().encode(JSON.stringify(value)).byteLength);
    expect(cacheModule.eventByteSize(value)).toBeGreaterThan(JSON.stringify(value).length);
  });

  test("derives display labels from raw Events only when settings are rendered", () => {
    const events = [
      event("CloneCompleted", { title: "克隆会话" }),
      event("UserPrompt", { content: "hello" }),
      event("AgentTitleChanged", { title: "最终标题" }),
    ];
    expect(cacheModule.cachedSessionTitle(events)).toBe("最终标题");
    expect(cacheModule.cachedSessionTitle([], "main")).toBe("main");
    expect(cacheModule.workspaceName("/Users/example/workspace/")).toBe("workspace");
    expect(cacheModule.workspaceName("C:\\Users\\example\\project\\")).toBe("project");
  });

  test("formats cache management sizes consistently", () => {
    expect(cacheModule.formatBytes(0)).toBe("0 B");
    expect(cacheModule.formatBytes(1023)).toBe("1023 B");
    expect(cacheModule.formatBytes(1024)).toBe("1.00 KiB");
    expect(cacheModule.formatBytes(10 * 1024)).toBe("10.0 KiB");
    expect(cacheModule.formatBytes(2 * 1024 * 1024)).toBe("2.00 MiB");
  });

  test("degrades safely when IndexedDB is unavailable and still suppresses an explicit clear", async () => {
    const cache = cacheModule.create({
      indexedDB: false,
      IDBKeyRange: false,
      BroadcastChannel: false,
    });
    const key = cacheModule.sessionKey("/workspace", "main");
    expect(cache.available).toBe(false);
    expect(await cache.loadScope("/workspace")).toEqual([]);
    expect(await cache.loadScopeMetadata("/workspace")).toEqual([]);
    expect(await cache.loadSession(key)).toBeNull();
    expect(await cache.listSessions()).toEqual([]);
    cache.saveSession({
      scope: "/workspace",
      agentId: "main",
      mutationRevision: 0,
      lastEventHash: "hash",
      events: [event("UserPrompt", { id: 1 })],
    });
    await cache.flush();
    await cache.removeSession(key);
    expect(cache.suppressed.has(key)).toBe(true);
    expect(cache.pendingWrites.size).toBe(0);
  });

  test("keeps the legacy full write and ordered partial delta queue as separate paths", async () => {
    const runtime = { indexedDB: {}, IDBKeyRange: {}, BroadcastChannel: false };
    const fullCache = cacheModule.create(runtime);
    const fullWrites = [];
    fullCache._saveNow = async (write) => fullWrites.push(write);
    fullCache.saveSession({
      scope: "/workspace", agentId: "main", mutationRevision: 0, lastEventHash: "hash-2",
      events: [event("UserPrompt", { id: 1 }), event("AssistResponse", { id: 2 })],
      delta: { events: [event("must-not-select-delta")], startOrder: 1, eventCount: 2 },
    });
    await fullCache.flush();
    expect(fullWrites).toHaveLength(1);
    expect(fullWrites[0].kind).toBe("full");
    expect(fullWrites[0].events).toHaveLength(2);

    const partialCache = cacheModule.create(runtime);
    const deltaWrites = [];
    partialCache._saveNow = async (write) => deltaWrites.push(write);
    partialCache.saveSession({
      scope: "/workspace", agentId: "main", mutationRevision: 0, lastEventHash: "hash-1",
      deltaOnly: true,
      delta: {
        startOrder: 0, eventCount: 1, expectedEventCount: 0, expectedMutationRevision: 0,
        events: [event("UserPrompt", { id: 1 })],
      },
    });
    partialCache.saveSession({
      scope: "/workspace", agentId: "main", mutationRevision: 0, lastEventHash: "hash-2",
      deltaOnly: true,
      delta: {
        startOrder: 1, eventCount: 2, expectedEventCount: 1, expectedMutationRevision: 0,
        events: [event("AssistResponse", { id: 2 })],
      },
    });
    await partialCache.flush();
    expect(deltaWrites).toHaveLength(1);
    expect(deltaWrites[0]).toMatchObject({
      kind: "delta", startOrder: 0, eventCount: 2, expectedEventCount: 0, reset: false,
    });
    expect(deltaWrites[0].events).toHaveLength(2);
    expect(deltaWrites[0].lastEventHash).toBe("hash-2");
  });

  test("keeps raw-cache persistence and selectable hydration semantics aligned", () => {
    const cacheSource = readFileSync(join(import.meta.dir, "../src/webui/edb-cache.js"), "utf8");
    const sharedSource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const sharedIndex = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const directRuntime = readFileSync(join(import.meta.dir, "../src/webui/runtime.js"), "utf8");
    const gatewayRuntime = readFileSync(join(import.meta.dir, "../src/gateway_webui/runtime.js"), "utf8");

    expect(cacheSource).toContain("const DB_VERSION = 1;");
    expect(cacheSource).toContain('createObjectStore(SESSION_STORE, { keyPath: "key" })');
    expect(cacheSource).toContain('createObjectStore(EVENT_STORE, { keyPath: ["sessionKey", "order"] })');
    expect(cacheSource).toContain("loadScopeMetadata(scope)");
    expect(cacheSource).toContain("async loadSession(key)");
    expect(cacheSource).toContain("return this._loadEntries(null, false)");
    expect(cacheSource).toContain("async _saveFull(write)");
    expect(cacheSource).toContain("async _saveDelta(write)");
    expect(cacheSource).toContain('if (write.kind === "delta") return this._saveDelta(write)');
    expect(cacheSource).toContain("existingCount === write.expectedEventCount");
    expect(cacheSource).toContain("!write.reset && existingMutation === write.mutationRevision");
    expect(cacheSource).toContain("write.startOrder !== existingCount");
    expect(cacheSource).toContain("eventsStore.put({ sessionKey: write.key, order: write.startOrder + index, event, bytes })");
    expect(cacheSource).toContain('new this.BroadcastChannel(CHANNEL_NAME)');
    expect(cacheSource).not.toContain("setInterval(");
    expect(cacheSource).not.toContain("projection:");
    expect(cacheSource).not.toContain("workmap:");

    expect(sharedSource).toContain("cache_metadata_only: !state.edbCacheInitialized");
    expect(sharedSource).toContain('["initial", "reconnecting"].includes(state.connectionPhase)');
    expect(sharedSource).toContain("await hydrateEdbCache(message.snapshot)");
    expect(sharedSource).toContain("loadEdbCacheEntries(snapshot, state.partialLoading)");
    expect(sharedSource).toContain("frontendRuntime.loadCachedSessionMetadata(edbCache, snapshot, scope)");
    expect(sharedSource).toContain("frontendRuntime.loadCachedSession(edbCache, snapshot, scope, agentId)");
    expect(sharedSource).toContain("materialized: !state.partialLoading || family.has(meta.id)");
    expect(sharedSource).toContain("const keepEvents = !state.partialLoading || store.materialized;");
    expect(sharedSource).toContain("deltaOnly: state.partialLoading && !store.materialized");
    expect(sharedSource).toContain("function releaseMaterializedStore(store)");
    expect(sharedSource).toContain("function materializeAgentStore(bucket, meta)");
    expect(sharedSource).toContain("if (!store.materialized || store.materializing) return emptyProjectionChanges()");
    expect(sharedSource).toContain("state.stores.clear()");
    expect(sharedSource).toContain("projection: emptyProjection()");
    expect(sharedSource).toContain("workmap: emptyWorkMap()");
    expect(sharedSource).toContain("loadProgress: createAgentLoadProgress(meta, eventCount, mutationRevision)");
    expect(sharedSource).not.toContain("startupPending: true");

    for (const runtimeSource of [directRuntime, gatewayRuntime]) {
      expect(runtimeSource).toContain("createEdbCache()");
      expect(runtimeSource).toContain("return globalThis.MeEdbCache.create()");
      expect(runtimeSource).toContain("loadCachedSessions(cache, _snapshot, scope)");
      expect(runtimeSource).toContain("loadCachedSessionMetadata(cache, _snapshot, scope)");
      expect(runtimeSource).toContain("return cache.loadScopeMetadata(scope)");
      expect(runtimeSource).toContain("loadCachedSession(cache, _snapshot, scope, agentId)");
      expect(runtimeSource).toContain("return cache.loadSession(globalThis.MeEdbCache.sessionKey(scope, agentId))");
    }
    expect(sharedIndex.indexOf('<script src="/edb-cache.js"></script>')).toBeGreaterThan(-1);
    expect(sharedIndex.indexOf('<script src="/edb-cache.js"></script>'))
      .toBeLessThan(sharedIndex.indexOf('<script src="/app.js"></script>'));
    expect(sharedIndex).toContain('id="login-settings"');
    expect(sharedIndex).toContain('id="open-settings"');
    expect(sharedSource).toContain('id="settings-edb-cache-manager"');
  });
});
