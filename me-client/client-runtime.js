(() => {
  "use strict";

  const tauri = globalThis.__TAURI__;
  if (!tauri?.core?.invoke) throw new Error("ME Client native runtime is unavailable");
  const invoke = tauri.core.invoke;
  const browserFetch = globalThis.fetch.bind(globalThis);
  const browserSendBeacon = globalThis.navigator?.sendBeacon?.bind(globalThis.navigator) || null;
  let endpoint = "";
  let activeEdbCache = null;

  const DEVICE_PREFERENCE_KEYS = new Set(["me-theme", "me-color-mode", "me-send-shortcut"]);
  const devicePreferenceValues = new Map();
  let devicePreferencesReady = false;
  let devicePreferenceWrites = Promise.resolve();

  function loadDevicePreferences(values) {
    devicePreferenceValues.clear();
    for (const [key, value] of Object.entries(values || {})) {
      if (DEVICE_PREFERENCE_KEYS.has(key) && typeof value === "string") {
        devicePreferenceValues.set(key, value);
      }
    }
    devicePreferencesReady = true;
  }

  const devicePreferences = Object.freeze({
    getItem(key) {
      return devicePreferenceValues.get(String(key || "")) ?? null;
    },
    setItem(key, value) {
      const normalizedKey = String(key || "");
      if (!DEVICE_PREFERENCE_KEYS.has(normalizedKey)) return Promise.resolve();
      const normalizedValue = String(value ?? "");
      devicePreferenceValues.set(normalizedKey, normalizedValue);
      if (!devicePreferencesReady) return Promise.resolve();
      devicePreferenceWrites = devicePreferenceWrites
        .then(() => invoke("set_device_preference", { key: normalizedKey, value: normalizedValue }))
        .catch((error) => console.warn("Unable to persist native device preference", error));
      return devicePreferenceWrites;
    },
  });

  function bytesToBase64(bytes) {
    let binary = "";
    const chunkSize = 0x8000;
    for (let offset = 0; offset < bytes.length; offset += chunkSize) {
      binary += String.fromCharCode(...bytes.subarray(offset, offset + chunkSize));
    }
    return btoa(binary);
  }

  function base64ToBytes(value) {
    if (!value) return new Uint8Array();
    const binary = atob(value);
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
    return bytes;
  }

  function apiPath(input) {
    const raw = typeof input === "string" || input instanceof URL ? String(input) : String(input?.url || "");
    const parsed = new URL(raw, document.baseURI);
    if (!parsed.pathname.startsWith("/api/")) return null;
    return `${parsed.pathname}${parsed.search}`;
  }

  function scopedApiPath(path, workspaceId = "chat") {
    const value = String(path || "");
    const childPath = value === "/api/sync" || value === "/api/snapshot" || value === "/api/command"
      || value.startsWith("/api/deletion-blocker/")
      || value.startsWith("/api/session-terminal/")
      || value.startsWith("/api/remote-control/")
      || value.startsWith("/api/files/");
    if (!childPath) return value;
    return `/api/workspaces/${encodeURIComponent(workspaceId || "chat")}${value.slice(4)}`;
  }

  async function requestBody(input, options) {
    const body = options.body ?? (typeof Request === "function" && input instanceof Request ? await input.clone().arrayBuffer() : null);
    if (body == null) return {};
    if (typeof body === "string") return { bodyText: body };
    if (body instanceof URLSearchParams) return { bodyText: body.toString() };
    if (body instanceof Blob) {
      const type = String(body.type || "").toLowerCase();
      if (type.startsWith("text/") || type.includes("json")) return { bodyText: await body.text() };
      return { bodyBase64: bytesToBase64(new Uint8Array(await body.arrayBuffer())) };
    }
    if (body instanceof ArrayBuffer) return { bodyBase64: bytesToBase64(new Uint8Array(body)) };
    if (ArrayBuffer.isView(body)) {
      return { bodyBase64: bytesToBase64(new Uint8Array(body.buffer, body.byteOffset, body.byteLength)) };
    }
    throw new TypeError("Unsupported native request body");
  }

  async function nativeFetch(input, options = {}) {
    const path = apiPath(input);
    if (!path) return browserFetch(input, options);
    if (options.signal?.aborted) throw new DOMException("The operation was aborted", "AbortError");
    if (/^\/api\/(?:sync(?:\?|$)|workspaces\/[^/]+\/sync(?:\?|$))/.test(path)) {
      await activeEdbCache?.flush();
    }
    const source = typeof Request === "function" && input instanceof Request ? input : null;
    const headers = new Headers(source?.headers || {});
    new Headers(options.headers || {}).forEach((value, name) => headers.set(name, value));
    const body = await requestBody(input, options);
    const result = await invoke("gateway_request", {
      request: {
        path,
        method: String(options.method || source?.method || "GET").toUpperCase(),
        headers: Object.fromEntries(headers.entries()),
        ...body,
      },
    });
    if (options.signal?.aborted) throw new DOMException("The operation was aborted", "AbortError");
    const responseBody = result.bodyText == null
      ? base64ToBytes(result.bodyBase64 || "")
      : result.bodyText;
    return new Response(responseBody, {
      status: result.status,
      headers: result.headers,
    });
  }

  function nativeSendBeacon(input, body) {
    const path = apiPath(input);
    if (!path) return browserSendBeacon ? browserSendBeacon(input, body) : false;
    void nativeFetch(path, { method: "POST", body }).catch((error) => {
      console.warn("Unable to submit native beacon", error);
    });
    return true;
  }

  class NativeEdbCache {
    constructor() {
      const common = globalThis.MeEdbCache.create({ indexedDB: {}, IDBKeyRange: {} });
      this.available = true;
      this.disabledReason = "";
      this.renderManager = common.renderManager.bind(this);
      this.pendingWrites = [];
      this.writeDrain = null;
      this.chunkBytes = 1024 * 1024;
    }

    async loadMetadata(edbIds) {
      await this.flush();
      return invoke("cache_load_metadata", {
        edbIds: [...new Set((edbIds || []).filter(Boolean).map(String))],
      });
    }

    async loadSession(metadata) {
      const events = [];
      const totalCount = Number(metadata.eventCount || 0);
      let startOrder = 0;
      while (startOrder < totalCount) {
        const chunk = await invoke("cache_load_chunk", {
          edbId: metadata.edbId,
          startOrder,
          byteLimit: this.chunkBytes,
        });
        if (chunk.edbId !== metadata.edbId
            || chunk.startOrder !== startOrder
            || chunk.nextOrder <= startOrder
            || chunk.nextOrder > totalCount
            || chunk.totalCount !== totalCount
            || chunk.mutationRevision !== metadata.mutationRevision
            || chunk.lastEventHash !== metadata.lastEventHash) {
          throw new Error("Native EDB cache returned an invalid chunk");
        }
        events.push(...chunk.events);
        startOrder = chunk.nextOrder;
        if (Boolean(chunk.done) !== (startOrder === totalCount)) {
          throw new Error("Native EDB cache returned an invalid continuation state");
        }
      }
      return { ...metadata, events };
    }

    async loadSessions(edbIds) {
      const metadata = await this.loadMetadata(edbIds);
      const entries = [];
      for (const entry of metadata) {
        try { entries.push(await this.loadSession(entry)); }
        catch (error) {
          console.warn("Unable to restore native EDB cache", error);
          await this.discardSession(entry.edbId).catch(() => {});
        }
      }
      return entries;
    }

    async listSessions() {
      await this.flush();
      return invoke("cache_list");
    }

    saveSession(session) {
      const edbId = String(session?.edbId || "");
      const delta = session?.delta;
      if (!edbId || !delta) return;
      this.pendingWrites.push({
        edbId,
        startOrder: delta.startOrder,
        eventCount: delta.eventCount,
        expectedEventCount: delta.expectedEventCount,
        expectedMutationRevision: delta.expectedMutationRevision,
        mutationRevision: session.mutationRevision,
        lastEventHash: session.lastEventHash,
        reset: Boolean(delta.reset),
        events: [...(delta.events || [])],
        gatewayLabel: session.gatewayLabel || "",
        workspaceLabel: session.workspaceLabel || "",
        sessionLabel: session.sessionLabel || "",
      });
      if (!this.writeDrain) this.writeDrain = Promise.resolve().then(() => this.drainWrites());
    }

    async drainWrites() {
      try {
        while (this.pendingWrites.length) {
          const session = this.pendingWrites.shift();
          try { await invoke("cache_save_batch", { session }); }
          catch (error) { console.warn("Unable to persist native EDB cache", error); }
        }
      } finally {
        this.writeDrain = null;
        if (this.pendingWrites.length) this.writeDrain = Promise.resolve().then(() => this.drainWrites());
      }
    }

    async flush() {
      while (this.writeDrain) await this.writeDrain;
    }

    async discardSession(edbId) {
      if (!edbId) return;
      const key = String(edbId);
      this.pendingWrites = this.pendingWrites.filter((write) => write.edbId !== key);
      await this.flush();
      await invoke("cache_remove", { edbId: key });
    }

    removeSession(edbId) {
      return this.discardSession(edbId);
    }
  }

  const runtime = {
    capabilities: Object.freeze({
      multipleWorkspaces: true,
      gatewaySettings: true,
      targetConfiguration: true,
      nativeDownload: true,
      pageTitle: "ME Client",
      brandTitle: "ME Client",
      cacheStorageLabel: "ME Client",
      sessionSectionTitle: "聊天",
      newSessionLabel: "新建聊天",
    }),
    get endpoint() { return endpoint; },
    devicePreferences,
    async initialize() {
      document.documentElement.classList.add("me-client");
      const bootstrap = await invoke("client_bootstrap");
      loadDevicePreferences(bootstrap.devicePreferences);
      endpoint = String(bootstrap.endpoint || "");
      return { endpoint };
    },
    async configureTarget(value) {
      const configured = await invoke("configure_target", { endpoint: String(value || "") });
      endpoint = String(configured.endpoint || "");
      return { endpoint };
    },
    apiPath: scopedApiPath,
    loadGatewayState(api) {
      return api("/api/gateway/state");
    },
    persistSelection(api, workspaceId, agentId) {
      return api("/api/gateway/selection", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workspace_id: workspaceId, agent_id: agentId }),
      });
    },
    createEdbCache() {
      if (!activeEdbCache) activeEdbCache = new NativeEdbCache();
      return activeEdbCache;
    },
    async loadCachedSessions(cache, snapshot, scope) {
      const agentsByEdbId = new Map((snapshot.agents || [])
        .filter((agent) => agent.edb_id)
        .map((agent) => [String(agent.edb_id), agent]));
      const entries = await cache.loadSessions([...agentsByEdbId.keys()]);
      return entries.flatMap((entry) => {
        const agent = agentsByEdbId.get(String(entry.edbId || ""));
        return agent ? [{ ...entry, key: entry.edbId, scope, agentId: agent.id }] : [];
      });
    },
    cacheKey(_scope, _agentId, edbId) {
      return String(edbId || "");
    },
    async downloadFile(path, filename) {
      return invoke("download_file", { path, filename: String(filename || "download") });
    },
  };

  globalThis.fetch = nativeFetch;
  if (globalThis.navigator) globalThis.navigator.sendBeacon = nativeSendBeacon;
  globalThis.MeFrontendRuntime = Object.freeze(runtime);
})();
