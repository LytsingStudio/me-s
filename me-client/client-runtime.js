(() => {
  "use strict";

  const tauri = globalThis.__TAURI__;
  if (!tauri?.core?.invoke) throw new Error("ME Client native runtime is unavailable");
  const invoke = tauri.core.invoke;
  const browserFetch = globalThis.fetch.bind(globalThis);
  const browserSendBeacon = globalThis.navigator?.sendBeacon?.bind(globalThis.navigator) || null;
  let endpoint = "";
  let activeEdbCache = null;
  let windowReadyPromise = null;
  let titleBarTitleElement = null;
  let maximizeControl = null;
  let currentWindowTitle = "";
  let windowStateRequest = null;
  let windowStateFrame = null;

  const initialDocumentRoot = document.documentElement;
  initialDocumentRoot?.setAttribute?.("data-me-client-startup", "pending");
  if (initialDocumentRoot?.style) initialDocumentRoot.style.visibility = "hidden";

  const NATIVE_BROWSER_SHORTCUT_KEYS = new Set([
    "r", "p", "s", "u", "f", "o", "l", "i", "j", "+", "=", "-", "0",
  ]);
  const NATIVE_TEXT_EDITOR_SELECTOR = "textarea, input:not([type]), input[type='text'], input[type='password'], input[type='search'], input[type='email'], input[type='url'], input[type='tel'], input[type='number'], [contenteditable='true'], [contenteditable='plaintext-only']";
  const NATIVE_RAW_KEY_SURFACE_SELECTOR = ".xterm, .remote-control-keyboard";

  function targetWithin(target, selector) {
    return Boolean(target?.closest?.(selector));
  }

  function nativeBrowserShortcutBlocked(event) {
    const target = event?.target;
    if (targetWithin(target, NATIVE_RAW_KEY_SURFACE_SELECTOR)) return false;
    const key = String(event?.key || "").toLowerCase();
    const textEditor = targetWithin(target, NATIVE_TEXT_EDITOR_SELECTOR);
    if (key === "f5") return true;
    if (key === "contextmenu" || (key === "f10" && event?.shiftKey)) return !textEditor;
    if (event?.altKey && (key === "arrowleft" || key === "arrowright")) return true;
    if (!event?.ctrlKey && !event?.metaKey) return false;
    if (key === "a") return !textEditor;
    if (event.metaKey && (key === "[" || key === "]")) return true;
    return NATIVE_BROWSER_SHORTCUT_KEYS.has(key);
  }

  function stopNativeBrowserAction(event) {
    event.preventDefault();
    event.stopImmediatePropagation?.();
  }

  globalThis.addEventListener?.("keydown", (event) => {
    if (nativeBrowserShortcutBlocked(event)) stopNativeBrowserAction(event);
  }, true);
  document.addEventListener?.("contextmenu", (event) => {
    if (!targetWithin(event.target, NATIVE_TEXT_EDITOR_SELECTOR)) stopNativeBrowserAction(event);
  }, true);

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

  let rememberedDeviceValues = [];

  function rememberedDevice(value) {
    const endpointValue = String(value?.endpoint || "");
    if (!endpointValue || typeof value?.password !== "string") return null;
    return {
      endpoint: endpointValue,
      password: value.password,
      updatedAt: Number(value.updatedAt || 0),
      online: Boolean(value.online),
    };
  }

  function loadRememberedDevices(values) {
    const seen = new Set();
    rememberedDeviceValues = [];
    for (const value of Array.isArray(values) ? values : []) {
      const device = rememberedDevice(value);
      if (!device || seen.has(device.endpoint)) continue;
      seen.add(device.endpoint);
      rememberedDeviceValues.push(device);
    }
  }

  function storeRememberedDevice(value) {
    const device = rememberedDevice(value);
    if (!device) return null;
    rememberedDeviceValues = [
      device,
      ...rememberedDeviceValues.filter((candidate) => candidate.endpoint !== device.endpoint),
    ];
    return { ...device };
  }

  const rememberedDevices = Object.freeze({
    list() {
      return rememberedDeviceValues.map((device) => ({ ...device }));
    },
    async remember(endpointValue, password) {
      const saved = await invoke("remember_device", {
        endpoint: String(endpointValue || ""),
        password: String(password ?? ""),
      });
      return storeRememberedDevice(saved);
    },
    async forget(endpointValue) {
      const normalized = String(endpointValue || "");
      await invoke("forget_device", { endpoint: normalized });
      rememberedDeviceValues = rememberedDeviceValues
        .filter((device) => device.endpoint !== normalized);
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

  function clientPlatform() {
    const identity = `${globalThis.navigator?.platform || ""} ${globalThis.navigator?.userAgent || ""}`;
    if (/Mac|iPhone|iPad|iPod/i.test(identity)) return "macos";
    if (/Win/i.test(identity)) return "windows";
    return "linux";
  }

  function markDragRegion(element) {
    element?.setAttribute?.("data-tauri-drag-region", "");
    return element;
  }

  function createWindowControl(action, label, kind) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `client-window-control client-window-${kind}`;
    button.dataset.windowAction = action;
    button.title = label;
    button.setAttribute("aria-label", label);
    button.innerHTML = '<span aria-hidden="true"></span>';
    button.addEventListener("click", () => { void performWindowAction(action); });
    return button;
  }

  function windowControls(platform) {
    const controls = document.createElement("div");
    controls.className = "client-window-controls";
    controls.setAttribute("role", "group");
    controls.setAttribute("aria-label", "窗口控制");
    if (platform === "macos") {
      controls.append(
        createWindowControl("close", "关闭", "close"),
        createWindowControl("minimize", "最小化", "minimize"),
        createWindowControl("toggle_maximize", "缩放窗口", "maximize"),
      );
    } else {
      controls.append(
        createWindowControl("minimize", "最小化", "minimize"),
        createWindowControl("toggle_maximize", "最大化", "maximize"),
        createWindowControl("close", "关闭", "close"),
      );
    }
    maximizeControl = controls.querySelector?.(".client-window-maximize") || null;
    return controls;
  }

  function installClientTitleBar() {
    if (titleBarTitleElement || !document.body || typeof document.createElement !== "function") return;
    const platform = clientPlatform();
    document.documentElement?.classList?.add?.(`me-client-platform-${platform}`);
    const titleBar = document.createElement("div");
    titleBar.id = "client-titlebar";
    titleBar.className = "client-titlebar";
    const dragHandle = markDragRegion(document.createElement("div"));
    dragHandle.className = "client-window-drag-handle";
    dragHandle.setAttribute("aria-hidden", "true");
    const text = document.createElement("span");
    text.className = "client-titlebar-title";
    text.textContent = currentWindowTitle || "ME Client";
    titleBarTitleElement = text;
    titleBar.append(dragHandle, text, windowControls(platform));
    document.body.prepend(titleBar);
    for (const element of document.querySelectorAll?.("#login-screen, .sidebar-heading, .view-tabs") || []) {
      markDragRegion(element);
    }
    globalThis.addEventListener?.("resize", scheduleWindowStateRefresh);
    void refreshWindowState();
  }

  function applyWindowState(value) {
    const maximized = Boolean(value?.maximized);
    const fullscreen = Boolean(value?.fullscreen);
    document.documentElement?.classList?.toggle?.("me-client-window-maximized", maximized);
    document.documentElement?.classList?.toggle?.("me-client-window-fullscreen", fullscreen);
    maximizeControl?.classList?.toggle?.("restore", maximized);
    if (maximizeControl) {
      const label = maximized ? "还原" : "最大化";
      maximizeControl.title = clientPlatform() === "macos" ? "缩放窗口" : label;
      maximizeControl.setAttribute("aria-label", maximizeControl.title);
    }
    return value;
  }

  function performWindowAction(action) {
    return invoke("client_window_action", { action })
      .then(applyWindowState)
      .catch((error) => {
        if (action !== "close") console.error(`Unable to ${action} client window`, error);
      });
  }

  function refreshWindowState() {
    if (!windowStateRequest) {
      windowStateRequest = invoke("client_window_action", { action: "state" })
        .then(applyWindowState)
        .catch((error) => console.error("Unable to read client window state", error))
        .finally(() => { windowStateRequest = null; });
    }
    return windowStateRequest;
  }

  function scheduleWindowStateRefresh() {
    if (windowStateFrame != null) return;
    const run = () => {
      windowStateFrame = null;
      void refreshWindowState();
    };
    if (typeof globalThis.requestAnimationFrame === "function") {
      windowStateFrame = globalThis.requestAnimationFrame(run);
    } else {
      run();
    }
  }

  function setWindowTitle(value) {
    const title = String(value || "").trim() || "ME Client";
    if (titleBarTitleElement) titleBarTitleElement.textContent = title;
    if (title === currentWindowTitle) return Promise.resolve();
    currentWindowTitle = title;
    return invoke("client_window_action", { action: "set_title", value: title })
      .then(applyWindowState)
      .catch((error) => {
        if (currentWindowTitle === title) currentWindowTitle = "";
        throw error;
      });
  }

  function waitForFirstPaint() {
    if (typeof globalThis.requestAnimationFrame !== "function") return Promise.resolve();
    return new Promise((resolve) => {
      globalThis.requestAnimationFrame(() => globalThis.requestAnimationFrame(resolve));
    });
  }

  function windowReady() {
    installClientTitleBar();
    if (!windowReadyPromise) {
      initialDocumentRoot?.removeAttribute?.("data-me-client-startup");
      if (initialDocumentRoot?.style) initialDocumentRoot.style.visibility = "";
      windowReadyPromise = refreshWindowState()
        .then(() => invoke("client_window_action", { action: "show" }))
        .then(applyWindowState)
        .then((state) => waitForFirstPaint().then(() => state));
    }
    return windowReadyPromise;
  }

  const runtime = {
    capabilities: Object.freeze({
      multipleWorkspaces: true,
      gatewaySettings: true,
      targetConfiguration: true,
      nativeDownload: true,
      dynamicWindowTitle: true,
      pageTitle: "ME Client",
      brandTitle: "ME Client",
      cacheStorageLabel: "ME Client",
      sessionSectionTitle: "聊天",
      newSessionLabel: "新建聊天",
    }),
    get endpoint() { return endpoint; },
    devicePreferences,
    rememberedDevices,
    async initialize() {
      document.documentElement.classList.add("me-client");
      installClientTitleBar();
      const bootstrap = await invoke("client_bootstrap");
      loadDevicePreferences(bootstrap.devicePreferences);
      loadRememberedDevices(bootstrap.rememberedDevices);
      endpoint = String(bootstrap.endpoint || "");
      return {
        endpoint,
        localDevice: bootstrap.localDevice || {
          endpoint: "http://127.0.0.1:38200", online: false, requiresPassword: false,
        },
      };
    },
    windowReady,
    setWindowTitle,
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
