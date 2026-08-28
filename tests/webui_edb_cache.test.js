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

  test("keeps storage, hydration, validation, and settings semantics aligned in both WebUIs", () => {
    const cacheSource = readFileSync(join(import.meta.dir, "../src/webui/edb-cache.js"), "utf8");
    const directSource = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const gatewaySource = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    const directIndex = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const gatewayIndex = readFileSync(join(import.meta.dir, "../src/gateway_webui/index.html"), "utf8");

    expect(cacheSource).toContain('createObjectStore(SESSION_STORE, { keyPath: "key" })');
    expect(cacheSource).toContain('createObjectStore(EVENT_STORE, { keyPath: ["sessionKey", "order"] })');
    expect(cacheSource).toContain("eventsStore.put({ sessionKey: write.key, order, event, bytes })");
    expect(cacheSource).toContain("existing.eventCount > write.events.length");
    expect(cacheSource).toContain('new this.BroadcastChannel(CHANNEL_NAME)');
    expect(cacheSource).not.toContain("setInterval(");
    expect(cacheSource).not.toContain("projection:");
    expect(cacheSource).not.toContain("workmap:");

    for (const source of [directSource, gatewaySource]) {
      expect(source).toContain("cache_metadata_only: !state.edbCacheInitialized");
      expect(source).toContain('["initial", "reconnecting"].includes(state.connectionPhase)');
      expect(source).toContain("await hydrateEdbCache(message.snapshot)");
      expect(source).toContain("createAgentStore(meta, valid ? cached : null)");
      expect(source).toContain("projection: emptyProjection()");
      expect(source).toContain("workmap: emptyWorkMap()");
      expect(source).toContain("if (!agentIds.has(entry.agentId)) void edbCache.discardSession(entry.key)");
      expect(source).toContain("if (payload.reset || payload.events.length > 0) {");
      expect(source).toContain("persistAgentEdb(meta, store, Boolean(payload.reset))");
      const hydrationStart = source.indexOf("async function hydrateEdbCache(snapshot) {");
      const hydrationEnd = source.indexOf("\nfunction persistAgentEdb(", hydrationStart);
      const hydration = source.slice(hydrationStart, hydrationEnd);
      expect(hydration.indexOf("renderAgents();")).toBeGreaterThan(-1);
      expect(hydration.indexOf("renderAgents();"))
        .toBeLessThan(hydration.indexOf("await edbCache.loadScope(scope)"));
      expect(source).toContain("startupPending: true");
      expect(source).toContain("item.disabled = startupLoading");
      expect(source).toContain("deleteButton.disabled = startupLoading");
    }

    expect(gatewaySource).toContain('api("/api/snapshot", {}, workspaceId)');
    expect(gatewaySource).toContain("function gatewayStartupReady()");
    expect(gatewaySource).toContain("state.startupMetadataPending = true");

    for (const index of [directIndex, gatewayIndex]) {
      expect(index.indexOf('<script src="/edb-cache.js"></script>')).toBeGreaterThan(-1);
      expect(index.indexOf('<script src="/edb-cache.js"></script>'))
        .toBeLessThan(index.indexOf('<script src="/app.js"></script>'));
    }
    expect(directIndex).toContain('id="open-settings"');
    expect(gatewaySource).toContain('id="settings-edb-cache-manager"');
  });
});
