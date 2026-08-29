"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function loadClientRuntime() {
  const source = readFileSync(join(import.meta.dir, "../me-client/client-runtime.js"), "utf8");
  const calls = [];
  const browserBeacons = [];
  const edbId = "a".repeat(64);
  const cachedEvents = [
    { EdbIdGeneration: { edb_id: edbId } },
    { UserPrompt: { content: "cached" } },
  ];
  const metadata = {
    key: edbId,
    edbId,
    mutationRevision: 0,
    lastEventHash: "hash-2",
    eventCount: 2,
    byteSize: 128,
    updatedAt: 1,
  };
  const sandbox = {
    __TAURI__: {
      core: {
        async invoke(command, payload) {
          calls.push({ command, payload });
          if (command === "client_bootstrap") return { endpoint: "http://127.0.0.1:38201" };
          if (command === "configure_target") return { endpoint: "https://gateway.example" };
          if (command === "gateway_request") return {
            status: 200,
            headers: { "content-type": "application/json" },
            bodyText: JSON.stringify({ required: true, authenticated: false }),
            bodyBase64: null,
          };
          if (command === "cache_load_metadata") {
            return payload.edbIds.includes(edbId) ? [{ ...metadata }] : [];
          }
          if (command === "cache_load_chunk") {
            const start = payload.startOrder;
            const events = start < cachedEvents.length ? [cachedEvents[start]] : [];
            const next = start + events.length;
            return {
              edbId,
              startOrder: start,
              nextOrder: next,
              totalCount: cachedEvents.length,
              mutationRevision: 0,
              lastEventHash: "hash-2",
              events,
              done: next === cachedEvents.length,
            };
          }
          if (command === "cache_list") return [{ ...metadata }];
          if (command === "cache_save_batch" || command === "cache_remove") return null;
          if (command === "download_file") return { path: "/Downloads/archive.zip", bytes: 12 };
          throw new Error(`unexpected command ${command}`);
        },
      },
    },
    navigator: {
      sendBeacon(input, body) {
        browserBeacons.push({ input, body });
        return true;
      },
    },
    fetch: async () => new Response("browser"),
    MeEdbCache: {
      create() { return { renderManager() {} }; },
    },
  };
  const documentValue = {
    baseURI: "http://tauri.localhost/",
    documentElement: { classList: { add() {} } },
  };
  const factory = new Function(
    "globalThis", "document", "URL", "Headers", "Request", "Response", "Blob",
    "DOMException", "TextEncoder", "atob", "btoa",
    `${source}\nreturn globalThis.MeFrontendRuntime;`,
  );
  const runtime = factory(
    sandbox, documentValue, URL, Headers, Request, Response, Blob,
    DOMException, TextEncoder, atob, btoa,
  );
  return { runtime, sandbox, calls, browserBeacons, edbId, cachedEvents };
}

describe("ME Client native adapter", () => {
  test("uses native UTF-8 JSON responses while leaving frontend assets local", async () => {
    const { runtime, sandbox, calls } = loadClientRuntime();
    expect(await runtime.initialize()).toEqual({ endpoint: "http://127.0.0.1:38201" });
    expect(await runtime.configureTarget("https://gateway.example")).toEqual({ endpoint: "https://gateway.example" });
    const response = await sandbox.fetch("/api/auth/status", { cache: "no-store" });
    expect(await response.json()).toEqual({ required: true, authenticated: false });
    const request = calls.find((call) => call.command === "gateway_request").payload.request;
    expect(request.path).toBe("/api/auth/status");
    expect(request.bodyBase64).toBeUndefined();
    expect(await (await sandbox.fetch("/theme.js")).text()).toBe("browser");
  });

  test("routes page-close JSON beacons as text through the native transport", async () => {
    const { sandbox, calls, browserBeacons } = loadClientRuntime();
    const content = JSON.stringify({ command: "update_input_draft" });
    const body = new Blob([content], { type: "application/json" });
    expect(sandbox.navigator.sendBeacon("/api/command?workspace_id=chat", body)).toBe(true);
    await new Promise((resolve) => setTimeout(resolve, 0));
    const request = calls.find((call) => call.command === "gateway_request")?.payload.request;
    expect(request.path).toBe("/api/command?workspace_id=chat");
    expect(request.method).toBe("POST");
    expect(request.bodyText).toBe(content);
    expect(request.bodyBase64).toBeUndefined();
    expect(browserBeacons).toHaveLength(0);
    expect(sandbox.navigator.sendBeacon("/local-event", body)).toBe(true);
    expect(browserBeacons).toHaveLength(1);
  });

  test("queues only ordered incremental EDB batches keyed by EDB_ID", async () => {
    const { runtime, calls, edbId } = loadClientRuntime();
    const cache = runtime.createEdbCache();
    cache.saveSession({
      edbId, mutationRevision: 0, lastEventHash: "hash-1",
      gatewayLabel: "Gateway", workspaceLabel: "Workspace", sessionLabel: "Session",
      events: [{ "must-not-cross-native-ipc": true }],
      delta: {
        startOrder: 0, eventCount: 2, expectedEventCount: 0,
        expectedMutationRevision: null, reset: false,
        events: [{ EdbIdGeneration: { edb_id: edbId } }],
      },
    });
    cache.saveSession({
      edbId, mutationRevision: 0, lastEventHash: "hash-2",
      gatewayLabel: "Gateway", workspaceLabel: "Workspace", sessionLabel: "Session",
      events: [{ "must-not-cross-native-ipc": true }, { "must-not-cross-native-ipc": true }],
      delta: {
        startOrder: 1, eventCount: 2, expectedEventCount: 1,
        expectedMutationRevision: 0, reset: false,
        events: [{ UserPrompt: {} }],
      },
    });
    await cache.flush();
    const writes = calls.filter((call) => call.command === "cache_save_batch");
    expect(writes).toHaveLength(2);
    expect(writes.map((write) => write.payload.session.startOrder)).toEqual([0, 1]);
    expect(writes.map((write) => write.payload.session.events.length)).toEqual([1, 1]);
    expect(writes.every((write) => write.payload.session.edbId === edbId)).toBe(true);
    await cache.removeSession(edbId);
    expect(calls.find((call) => call.command === "cache_remove").payload.edbId).toBe(edbId);
  });

  test("loads every requested native cache through bounded startup chunks", async () => {
    const { runtime, calls, edbId, cachedEvents } = loadClientRuntime();
    const cache = runtime.createEdbCache();
    const entries = await runtime.loadCachedSessions(cache, {
      agents: [{ id: "main", edb_id: edbId }],
    }, "/workspace");
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ key: edbId, agentId: "main", scope: "/workspace" });
    expect(entries[0].events).toEqual(cachedEvents);
    expect(calls.find((call) => call.command === "cache_load_metadata").payload.edbIds).toEqual([edbId]);
    const chunks = calls.filter((call) => call.command === "cache_load_chunk");
    expect(chunks).toHaveLength(2);
    expect(chunks.map((call) => call.payload.startOrder)).toEqual([0, 1]);
    expect(chunks.every((call) => call.payload.byteLimit === 1024 * 1024)).toBe(true);
    const listed = await cache.listSessions();
    expect(listed[0].events).toBeUndefined();
  });

  test("assembles the one authoritative frontend core with a client adapter", () => {
    const build = readFileSync(join(import.meta.dir, "../me-client/build-frontend.js"), "utf8");
    const config = readFileSync(join(import.meta.dir, "../me-client/src-tauri/tauri.conf.json"), "utf8");
    const shared = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const directServer = readFileSync(join(import.meta.dir, "../src/webui.rs"), "utf8");
    const gatewayServer = readFileSync(join(import.meta.dir, "../src/gateway_webui.rs"), "utf8");
    expect(build).toContain('resolve(repositoryRoot, "src/webui")');
    expect(build).not.toContain('resolve(repositoryRoot, "src/gateway_webui")');
    expect(build).toContain('[resolve(webuiRoot, "app.js"), "app.js"]');
    expect(build).toContain('[resolve(clientRoot, "client-runtime.js"), "runtime.js"]');
    expect(config).toContain('"withGlobalTauri": true');
    expect(config).toContain('"frontendDist": "../frontend-dist"');
    for (const server of [directServer, gatewayServer]) {
      expect(server).toContain('include_str!("webui/index.html")');
      expect(server).toContain('include_str!("webui/app.js")');
      expect(server).toContain('include_str!("webui/style.css")');
      expect(server).toContain('(&Method::Get, "/runtime.js")');
    }
    expect(shared).toContain("const frontendRuntime = globalThis.MeFrontendRuntime");
    expect(shared).toContain("store.events.push(...events)");
    expect(shared).not.toContain("materializeClientAgentStore");
    expect(shared).not.toContain("releaseClientAgentStore");
  });
});
