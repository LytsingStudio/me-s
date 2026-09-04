"use strict";

const { describe, expect, test } = require("bun:test");
const { existsSync, readFileSync } = require("node:fs");
const { execFileSync } = require("node:child_process");
const { join } = require("node:path");

function loadClientRuntime({ platform = "", userAgent = "" } = {}) {
  const source = readFileSync(join(import.meta.dir, "../me-client/client-runtime.js"), "utf8");
  const calls = [];
  const browserBeacons = [];
  const nativeWindowListeners = new Map();
  const nativeDocumentListeners = new Map();
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
    addEventListener(type, listener) { nativeWindowListeners.set(type, listener); },
    __TAURI__: {
      core: {
        async invoke(command, payload) {
          calls.push({ command, payload });
          if (command === "client_bootstrap") return {
            endpoint: "http://127.0.0.1:38201",
            devicePreferences: {
              "me-theme": "ocean",
              "me-color-mode": "light",
              "me-send-shortcut": "enter",
              "me-window-border-style": "theme",
            },
            rememberedDevices: [
              { endpoint: "http://127.0.0.1:38200", password: "local secret", updatedAt: 2, online: true },
              { endpoint: "https://offline.example", password: "old secret", updatedAt: 1, online: false },
            ],
            localDevice: { endpoint: "http://127.0.0.1:38200", online: true, requiresPassword: true },
          };
          if (command === "client_window_action") return { maximized: false, fullscreen: false };
          if (command === "configure_target") return { endpoint: "https://gateway.example" };
          if (command === "remember_device") return {
            endpoint: payload.endpoint, password: payload.password, updatedAt: 3, online: true,
          };
          if (command === "forget_device") return null;
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
          if (command === "cache_save_batch" || command === "cache_remove"
              || command === "set_device_preference") return null;
          if (command === "download_file") return { path: "/Downloads/archive.zip", bytes: 12 };
          throw new Error(`unexpected command ${command}`);
        },
      },
    },
    navigator: {
      platform,
      userAgent,
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
  const initialViewportContent = "width=device-width,initial-scale=1,viewport-fit=cover,interactive-widget=resizes-content";
  const viewportMeta = {
    content: initialViewportContent,
    getAttribute(name) { return name === "content" ? this.content : null; },
    setAttribute(name, value) { if (name === "content") this.content = String(value); },
  };
  const documentValue = {
    querySelector(selector) { return selector === 'meta[name="viewport"]' ? viewportMeta : null; },
    addEventListener(type, listener) { nativeDocumentListeners.set(type, listener); },
    baseURI: "http://tauri.localhost/",
    documentElement: {
      attributes: {},
      style: { visibility: "" },
      classList: { add() {} },
      setAttribute(name, value) { this.attributes[name] = String(value); },
      removeAttribute(name) { delete this.attributes[name]; },
    },
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
  return {
    runtime, sandbox, calls, browserBeacons, edbId, cachedEvents, documentValue, viewportMeta,
    nativeWindowListeners, nativeDocumentListeners,
  };
}

describe("ME Client native adapter", () => {
  test("uses native UTF-8 JSON responses while leaving frontend assets local", async () => {
    const { runtime, sandbox, calls, viewportMeta } = loadClientRuntime();
    expect(viewportMeta.content).toBe("width=device-width,initial-scale=1,viewport-fit=cover,interactive-widget=resizes-content");
    expect(await runtime.initialize()).toEqual({
      endpoint: "http://127.0.0.1:38201",
      localDevice: { endpoint: "http://127.0.0.1:38200", online: true, requiresPassword: true },
    });
    expect(await runtime.configureTarget("https://gateway.example")).toEqual({ endpoint: "https://gateway.example" });
    const response = await sandbox.fetch("/api/auth/status", { cache: "no-store" });
    expect(await response.json()).toEqual({ required: true, authenticated: false });
    const request = calls.find((call) => call.command === "gateway_request").payload.request;
    expect(request.path).toBe("/api/auth/status");
    expect(request.bodyBase64).toBeUndefined();
    expect(await (await sandbox.fetch("/theme.js")).text()).toBe("browser");
  });


  test("keeps both document and native window hidden until the one-shot readiness boundary", async () => {
    const { runtime, calls, documentValue } = loadClientRuntime();
    expect(documentValue.documentElement.style.visibility).toBe("hidden");
    expect(documentValue.documentElement.attributes["data-me-client-startup"]).toBe("pending");
    await runtime.initialize();
    expect(documentValue.documentElement.style.visibility).toBe("hidden");
    expect(calls.some((call) => call.command === "client_window_action")).toBe(false);
    const first = runtime.windowReady();
    const second = runtime.windowReady();
    expect(first).toBe(second);
    expect(documentValue.documentElement.style.visibility).toBe("");
    expect(documentValue.documentElement.attributes["data-me-client-startup"]).toBeUndefined();
    await Promise.all([first, second]);
    const actions = calls.filter((call) => call.command === "client_window_action");
    expect(actions.map((call) => call.payload.action)).toEqual(["state", "show"]);
    expect(actions.filter((call) => call.payload.action === "show")).toHaveLength(1);

    const source = readFileSync(join(import.meta.dir, "../me-client/client-runtime.js"), "utf8");
    const readyStart = source.indexOf("function windowReady()");
    const readyEnd = source.indexOf("const runtime =", readyStart);
    const readiness = source.slice(readyStart, readyEnd);
    expect(readiness.indexOf('action: "show"')).toBeLessThan(readiness.indexOf("waitForFirstPaint()"));
  });

  test("uses the document readiness gate without desktop window actions on iOS", async () => {
    const { runtime, calls, documentValue, viewportMeta } = loadClientRuntime({
      platform: "iPhone",
      userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 26_1 like Mac OS X)",
    });
    expect(viewportMeta.content).toBe("width=device-width,initial-scale=1,viewport-fit=cover,interactive-widget=resizes-content,maximum-scale=1,user-scalable=no");
    await runtime.initialize();
    await runtime.setWindowTitle("会话一 - ME Client");
    await runtime.windowReady();
    expect(calls.filter((call) => call.command === "client_window_action")).toHaveLength(0);
    expect(documentValue.documentElement.style.visibility).toBe("");
  });

  test("caches dynamic native window titles", async () => {
    const { runtime, calls } = loadClientRuntime();
    await runtime.initialize();
    await runtime.setWindowTitle("会话一 - ME Client");
    await runtime.setWindowTitle("会话一 - ME Client");
    expect(calls.filter((call) => call.command === "client_window_action")).toEqual([
      {
        command: "client_window_action",
        payload: { action: "set_title", value: "会话一 - ME Client" },
      },
    ]);
  });
  test("suppresses browser chrome gestures without breaking native text editing or terminal keys", () => {
    const { nativeWindowListeners, nativeDocumentListeners } = loadClientRuntime();
    const keydown = nativeWindowListeners.get("keydown");
    const contextmenu = nativeDocumentListeners.get("contextmenu");
    const target = (kind) => ({
      closest(selector) {
        if (kind === "editor" && selector.includes("textarea")) return this;
        if (kind === "raw" && selector.includes(".xterm")) return this;
        return null;
      },
    });
    const dispatch = (listener, properties) => {
      const event = {
        prevented: false, stopped: false,
        preventDefault() { this.prevented = true; },
        stopImmediatePropagation() { this.stopped = true; },
        ...properties,
      };
      listener(event);
      return event;
    };

    expect(dispatch(keydown, { key: "F5", target: target("body") }).prevented).toBe(true);
    expect(dispatch(keydown, { key: "a", ctrlKey: true, target: target("body") }).prevented).toBe(true);
    expect(dispatch(keydown, { key: "a", metaKey: true, target: target("editor") }).prevented).toBe(false);
    expect(dispatch(keydown, { key: "r", metaKey: true, target: target("editor") }).prevented).toBe(true);
    expect(dispatch(keydown, { key: "r", ctrlKey: true, target: target("raw") }).prevented).toBe(false);
    expect(dispatch(keydown, { key: "ArrowLeft", altKey: true, target: target("body") }).prevented).toBe(true);
    expect(dispatch(contextmenu, { target: target("body") }).prevented).toBe(true);
    expect(dispatch(contextmenu, { target: target("editor") }).prevented).toBe(false);

    const config = readFileSync(join(import.meta.dir, "../me-client/src-tauri/tauri.conf.json"), "utf8");
    const windowsConfig = readFileSync(join(import.meta.dir, "../me-client/src-tauri/tauri.windows.conf.json"), "utf8");
    expect(config).toContain('"devtools": false');
    expect(config).toContain('"visible": false');
    expect(config).toContain('"decorations": false');
    expect(config).toContain('"transparent": true');
    expect(config).toContain('"shadow": true');
    expect(windowsConfig).toContain('"transparent": true');
    expect(windowsConfig).toContain('"shadow": false');
  });


  test("defines integrated client chrome and real compositor-owned platform window shaping", () => {
    const runtime = readFileSync(join(import.meta.dir, "../me-client/client-runtime.js"), "utf8");
    const css = readFileSync(join(import.meta.dir, "../me-client/client.css"), "utf8");
    const native = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/lib.rs"), "utf8");
    const nativeManifest = readFileSync(join(import.meta.dir, "../me-client/src-tauri/Cargo.toml"), "utf8");
    const runtimeWry = readFileSync(join(import.meta.dir, "../vendor/tauri-runtime-wry/src/lib.rs"), "utf8");
    const shadow = readFileSync(join(import.meta.dir, "../me-client/window-shadow.html"), "utf8");
    const frontendBuild = readFileSync(join(import.meta.dir, "../me-client/build-frontend.js"), "utf8");
    const windowsConfig = readFileSync(join(import.meta.dir, "../me-client/src-tauri/tauri.windows.conf.json"), "utf8");
    const capability = readFileSync(join(import.meta.dir, "../me-client/src-tauri/capabilities/default.json"), "utf8");
    const iosConfig = readFileSync(join(import.meta.dir, "../me-client/src-tauri/tauri.ios.conf.json"), "utf8");
    const iosInfo = readFileSync(join(import.meta.dir, "../me-client/src-tauri/Info.ios.plist"), "utf8");
    const shared = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const sharedCss = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(runtime).toContain('setAttribute?.("data-tauri-drag-region", "")');
    expect(runtime).toContain('document.querySelector?.(".sidebar")');
    expect(runtime).toContain('document.querySelector?.(".workspace-heading")');
    expect(runtime).toContain("markDragRegion(workspaceHeading)");
    expect(runtime).toContain('!element.classList?.contains?.("heading-actions")');
    expect(runtime).not.toContain('markDragRegion(document.querySelector?.(".heading-actions"))');
    expect(runtime).not.toContain('markDragRegion(document.querySelector?.("#create-workspace"))');
    expect(runtime).not.toContain('markDragRegion(document.querySelector?.("#open-workspace"))');
    expect(runtime).toContain('"#login-screen, .view-tabs"');
    expect(runtime).toContain('sidebarDragRegion.className = "client-sidebar-drag-region"');
    expect(runtime).toContain('if (/iPhone|iPad|iPod/i.test(identity)) return "ios"');
    expect(runtime).toContain('if (/Mac/i.test(identity)) return "macos"');
    expect(runtime).toContain('if (/Win/i.test(identity)) return "windows"');
    expect(runtime).toContain('return "linux"');
    expect(runtime).toContain('createWindowControl("toggle_maximize"');
    expect(runtime).toContain('dynamicWindowTitle: true');
    expect(runtime).toContain('if (platform === "ios") return;');
    expect(runtime).toContain('clientPlatform() === "ios"');
    expect(css).toContain("html.me-client-platform-ios {");
    expect(runtime).toContain("maximum-scale=1,user-scalable=no");
    expect(css).toContain("touch-action: pan-x pan-y;");
    expect(css).toContain("html.me-client-platform-ios :is(");
    expect(css).toContain("height: calc(100% + env(safe-area-inset-top) + env(safe-area-inset-bottom));");
    expect(css).toContain("--client-ios-bottom-inset: max(0px, calc(env(safe-area-inset-bottom) - 6px))");
    expect(css).toContain("html.me-client-platform-ios .statusbar {");
    expect(native).toContain("fn initialize_ios_webview");
    expect(native).toContain("root_view.setFrame(ui_window.bounds())");
    expect(native).toContain("webview.setFrame(root_view.bounds())");
    expect(native).toContain("UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight");
    expect(nativeManifest).toContain("objc2-ui-kit");
    expect(iosConfig).toContain('"visible": true');
    expect(iosConfig).toContain('"infoPlist": "Info.ios.plist"');
    expect(iosInfo).toContain("NSLocalNetworkUsageDescription");
    expect(iosInfo).toContain("UIFileSharingEnabled");
    expect(nativeManifest).toContain("target_os = \"ios\"");
    expect(shared).toContain('elements.createWorkspace.addEventListener("click"');
    expect(shared).toContain('elements.openWorkspace.addEventListener("click"');
    expect(capability).toContain('"core:window:allow-start-dragging"');
    expect(css.match(/--client-window-radius: 18px/g)).toHaveLength(1);
    expect(css).not.toContain("--client-window-radius: 8px");
    expect(css).not.toContain("html.me-client-platform-windows {");
    expect(css).toContain("html.me-client,\nhtml.me-client body {\n  background: transparent;\n}");
    expect(css).toContain(".me-client #app {\n  background: var(--bg);\n}");
    expect(css).not.toMatch(/\.me-client\s+#login-screen(?:\s*,[^{}]*)?\s*\{[^}]*background\s*:/s);
    expect(sharedCss).toContain(".login-screen {");
    expect(sharedCss).toContain("radial-gradient(");
    expect(css).toContain("--client-window-outline: color-mix(in srgb, var(--text) 10%, var(--bg))");
    expect(css).toContain('html.me-client[data-window-border-style="theme"] {');
    expect(css).toContain("--client-window-outline: var(--accent);");
    expect(runtime).toContain('windowBorderStyle: clientPlatform() !== "ios"');
    expect(css).toContain("html.me-client body::after");
    expect(css).toContain("border: 1px solid var(--client-window-outline)");
    expect(css).toContain("html.me-client-platform-macos body {");
    expect(css).toContain("html.me-client-window-maximized body::after");
    expect(css).not.toContain(".client-window-drag-handle");
    expect(css).toContain(".me-client-platform-macos .client-sidebar-drag-region {");
    expect(css).toContain("flex: 0 0 var(--client-chrome-height)");
    expect(css).toContain("margin-left: 88px");
    expect(css).toContain(".me-client-platform-windows .view-tabs");
    expect(css).toContain("pointer-events: none;");
    expect(css).toContain("inset: 0;");
    expect(css).not.toContain("backdrop-filter");
    expect(css).toContain(
      "html.me-client-platform-windows .connection-overlay,\n"
      + "html.me-client-platform-windows .modal-backdrop,\n"
      + "html.me-client-platform-windows .drawer-backdrop,\n"
      + "html.me-client-platform-windows .mobile-sidebar-backdrop {\n"
      + "  overflow: hidden;\n"
      + "  border-radius: var(--client-window-radius);\n"
      + "}",
    );
    expect(css).toContain(
      "html.me-client-platform-windows .session-sync-overlay {\n"
      + "  overflow: hidden;\n"
      + "  border-radius: 0 0 var(--client-window-radius) 0;\n"
      + "}",
    );
    expect(css).toContain(
      "@media (orientation: portrait) {\n"
      + "  html.me-client-platform-windows .session-sync-overlay {\n"
      + "    border-bottom-left-radius: var(--client-window-radius);\n"
      + "  }\n"
      + "}",
    );
    expect(css).not.toContain(".me-client-platform-windows .user-message-menu");
    expect(css).not.toContain(".me-client-platform-windows .agent-menu");
    expect(css).not.toContain(".me-client-platform-windows .toast-region");
    expect(sharedCss).not.toContain("--client-window-radius");
    expect(sharedCss).not.toContain("me-client-platform-windows");
    expect(native).toContain('setCornerRadius: corner_radius');
    expect(native).toContain("let corner_radius = if floating { 18.0 } else { 0.0 }");
    expect(native).toContain("setHasShadow: floating");
    expect(native).toContain("invalidateShadow");
    expect(native).toContain("WINDOWS_CORNER_RADIUS_CSS_PX: i32 = 18");
    expect(native).toContain("scaled_windows_px(WINDOWS_CORNER_RADIUS_CSS_PX, dpi)");
    expect(native).toContain("GetDpiForWindow");
    expect(native).toContain("SetWindowSubclass");
    expect(native).toContain("DefSubclassProc");
    expect(native).toContain("WM_NCHITTEST");
    expect(native).toContain("HTNOWHERE");
    expect(native).toContain("HTTOPLEFT");
    expect(native).toContain("HTBOTTOMRIGHT");
    expect(native).toContain("WINDOWS_WINDOW_SQUARE.store(!floating, Ordering::Release)");
    expect(native).toContain("WS_EX_NOREDIRECTIONBITMAP");
    expect(native).toContain("verify_windows_no_redirection(hwnd, \"main window\")");
    expect(native).toContain("WINDOWS_SHADOW_LABEL");
    expect(native).toContain("WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT");
    expect(native).toContain("HTTRANSPARENT");
    expect(native).toContain("SWP_NOACTIVATE | SWP_SHOWWINDOW");
    expect(native).toContain("ShowWindow(shadow_hwnd, SW_HIDE)");
    expect(native).toContain("!state.maximized");
    expect(native).toContain("!state.fullscreen");
    expect(native).not.toContain("SetLayeredWindowAttributes");
    expect(native).not.toContain("UpdateLayeredWindow");
    expect(native).not.toContain("LWA_COLORKEY");
    expect(native).not.toContain("DwmExtendFrameIntoClientArea");
    expect(native).not.toContain("DwmSetWindowAttribute");
    expect(native).not.toContain("CreateRoundRectRgn");
    expect(native).not.toContain("SetWindowRgn");
    expect(nativeManifest).toContain("[patch.crates-io]");
    expect(nativeManifest).toContain('tauri-runtime-wry = { path = "../../vendor/tauri-runtime-wry" }');
    expect(runtimeWry).toContain("let uses_no_redirection_bitmap =");
    expect(runtimeWry).toContain("is_window_transparent && !window_builder.inner.window.decorations");
    expect(runtimeWry).toContain("with_no_redirection_bitmap(true)");
    expect(runtimeWry).toContain("is_window_transparent && !uses_no_redirection_bitmap");
    expect(shadow).toContain("inset: 48px");
    expect(shadow).toContain("border-radius: 18px");
    expect(shadow).toContain("box-shadow:");
    expect(frontendBuild).toContain('"window-shadow.html"');
    expect(windowsConfig).toContain('"transparent": true');
    expect(windowsConfig).toContain('"shadow": false');
    expect(sharedCss).toContain("inset: 46px 0 0");
    expect(sharedCss).toContain("align-items: flex-end");
    expect(sharedCss).toContain("min-height: 46px");
    expect(sharedCss).toContain("height: 38px");
    expect(sharedCss).toContain("align-items: center");
    expect(sharedCss).toContain("justify-content: center");
    expect(sharedCss).toContain("calc(51px + env(safe-area-inset-top))");
    expect(sharedCss).toContain("height: 44px");
    expect(sharedCss).toContain("flex: 0 0 44px");
    expect(shared).toContain("dynamicWindowTitle: false");
    expect(shared).toContain('`${sessionTitle} - ${runtimeCapabilities.pageTitle}`');
  });
  test("uses native app selection boundaries while keeping content copyable", () => {
    const css = readFileSync(join(import.meta.dir, "../me-client/client.css"), "utf8");
    expect(css).toContain("html.me-client body {");
    expect(css).toContain("-webkit-user-select: none;");
    expect(css).toContain("user-select: none;");
    for (const selector of [
      "input,",
      "textarea,",
      "[contenteditable=\"true\"]",
      ".user-message-content,",
      ".message-block.assistant .markdown,",
      ".notice-content,",
      ".session-content,",
      ".tool-details,",
      ".document-view,",
      ".compact-summary-content,",
      ".context-detail-raw,",
      ".message-modal-backdrop .modal > p",
    ]) expect(css).toContain(selector);
    expect(css).toContain("-webkit-user-select: text;");
    expect(css).toContain("user-select: text;");
    expect(css).not.toContain("selectstart");
  });

  test("persists all target-independent UI preferences through the native settings adapter", async () => {
    const { runtime, calls } = loadClientRuntime();
    expect(runtime.devicePreferences.getItem("me-theme")).toBeNull();
    await runtime.initialize();
    expect(runtime.devicePreferences.getItem("me-theme")).toBe("ocean");
    expect(runtime.devicePreferences.getItem("me-color-mode")).toBe("light");
    expect(runtime.devicePreferences.getItem("me-send-shortcut")).toBe("enter");
    expect(runtime.devicePreferences.getItem("me-window-border-style")).toBe("theme");
    expect(runtime.capabilities.windowBorderStyle).toBe(true);

    await runtime.devicePreferences.setItem("me-theme", "obsidian");
    await runtime.devicePreferences.setItem("me-color-mode", "dark");
    await runtime.devicePreferences.setItem("me-send-shortcut", "modified-enter");
    await runtime.devicePreferences.setItem("me-window-border-style", "default");
    await runtime.devicePreferences.setItem("gateway.endpoint", "must-not-persist");
    await runtime.configureTarget("https://other-gateway.example");
    expect(runtime.devicePreferences.getItem("me-theme")).toBe("obsidian");
    expect(runtime.devicePreferences.getItem("me-color-mode")).toBe("dark");
    expect(runtime.devicePreferences.getItem("me-send-shortcut")).toBe("modified-enter");
    expect(runtime.devicePreferences.getItem("me-window-border-style")).toBe("default");
    expect(calls.filter((call) => call.command === "set_device_preference").map((call) => call.payload))
      .toEqual([
        { key: "me-theme", value: "obsidian" },
        { key: "me-color-mode", value: "dark" },
        { key: "me-send-shortcut", value: "modified-enter" },
        { key: "me-window-border-style", value: "default" },
      ]);

    const ios = loadClientRuntime({ platform: "iPhone", userAgent: "Mozilla/5.0 (iPhone)" });
    expect(ios.runtime.capabilities.windowBorderStyle).toBe(false);

    const clientRuntime = readFileSync(join(import.meta.dir, "../me-client/client-runtime.js"), "utf8");
    const nativeRuntime = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/lib.rs"), "utf8");
    expect(clientRuntime).not.toMatch(/document\.cookie|localStorage|globalThis\.indexedDB/);
    expect(clientRuntime).toContain('"me-theme", "me-color-mode", "me-send-shortcut", "me-window-border-style"');
    expect(nativeRuntime).toContain('const DEVICE_PREFERENCE_KEYS: [&str; 4]');
    expect(nativeRuntime).toContain('run_blocking(move || cache.set_setting(&key, &value)).await');
    expect(nativeRuntime).toContain('device_preferences: BTreeMap<String, String>');
  });

  test("restores, updates, and forgets native remembered devices with online state", async () => {
    const { runtime, calls } = loadClientRuntime();
    expect(runtime.rememberedDevices.list()).toEqual([]);
    await runtime.initialize();
    expect(runtime.rememberedDevices.list()).toEqual([
      { endpoint: "http://127.0.0.1:38200", password: "local secret", updatedAt: 2, online: true },
      { endpoint: "https://offline.example", password: "old secret", updatedAt: 1, online: false },
    ]);

    await runtime.rememberedDevices.remember("https://new.example", "new secret");
    expect(runtime.rememberedDevices.list()[0]).toEqual({
      endpoint: "https://new.example", password: "new secret", updatedAt: 3, online: true,
    });
    await runtime.rememberedDevices.forget("https://offline.example");
    expect(runtime.rememberedDevices.list().map((device) => device.endpoint)).toEqual([
      "https://new.example", "http://127.0.0.1:38200",
    ]);
    expect(calls.find((call) => call.command === "remember_device").payload).toEqual({
      endpoint: "https://new.example", password: "new secret",
    });
    expect(calls.find((call) => call.command === "forget_device").payload).toEqual({
      endpoint: "https://offline.example",
    });

    const nativeRuntime = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/lib.rs"), "utf8");
    const nativeCache = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/cache.rs"), "utf8");
    const nativeGateway = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/gateway.rs"), "utf8");
    expect(nativeRuntime).toContain("remembered_devices: Vec<RememberedDeviceStatus>");
    expect(nativeCache).toContain("CREATE TABLE IF NOT EXISTS remembered_devices");
    expect(nativeGateway).toContain("const LOCAL_GATEWAY_FIRST_PORT: u16 = 38200");
    expect(nativeGateway).toContain("const LOCAL_GATEWAY_LAST_PORT: u16 = 38231");
    expect(nativeGateway).toContain("pub async fn online_remembered_devices");
  });

  test("restores the existing macOS window unless New Window is explicitly selected", () => {
    const nativeRuntime = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/lib.rs"), "utf8");
    const nativeCache = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/cache.rs"), "utf8");
    const reopenStart = nativeRuntime.indexOf("if let tauri::RunEvent::Reopen");
    const reopenHandler = nativeRuntime.slice(reopenStart, nativeRuntime.indexOf("\n    });", reopenStart));
    const newWindowStart = nativeRuntime.indexOf("extern \"C-unwind\" fn new_me_client_window");
    const newWindowEnd = nativeRuntime.indexOf("fn install_macos_dock_menu", newWindowStart);
    const newWindowAction = nativeRuntime.slice(newWindowStart, newWindowEnd);
    expect(reopenStart).toBeGreaterThan(-1);
    expect(newWindowStart).toBeGreaterThan(-1);
    expect(newWindowEnd).toBeGreaterThan(newWindowStart);
    expect(reopenHandler).toContain("restore_client_window(_app_handle)");
    expect(reopenHandler).not.toContain("spawn_client_instance()");
    expect(nativeRuntime).toContain("const NEW_CLIENT_WINDOW_LABEL: &str = \"新建窗口\"");
    expect(nativeRuntime).toContain("item.setAction(Some(sel!(newMeClientWindow:)))");
    expect(nativeRuntime).toContain("sel!(applicationDockMenu:)");
    expect(nativeRuntime).toContain("install_macos_dock_menu()?");
    expect(newWindowAction).toContain("spawn_client_instance()");
    const restoreStart = nativeRuntime.indexOf("fn restore_client_window");
    const restoreEnd = nativeRuntime.indexOf("fn spawn_client_instance", restoreStart);
    const restoreAction = nativeRuntime.slice(restoreStart, restoreEnd);
    expect(restoreAction.indexOf("window_revealed")).toBeGreaterThan(-1);
    expect(restoreAction.indexOf("window_revealed")).toBeLessThan(restoreAction.indexOf("app.show()?"));
    const closeStart = nativeRuntime.indexOf('#[cfg(target_os = "macos")]\nfn close_client_window');
    const closeEnd = nativeRuntime.indexOf('#[cfg(target_os = "windows")]', closeStart);
    const closeAction = nativeRuntime.slice(closeStart, closeEnd);
    expect(closeStart).toBeGreaterThan(-1);
    expect(closeAction).toContain("window.hide()");
    expect(closeAction).not.toContain("window.close()");
    expect(nativeRuntime).toContain("state.window_revealed.store(true, Ordering::Release)");
    expect(nativeRuntime).toContain("window.hide()?");
    expect(nativeRuntime).toContain("app.show()?");
    expect(nativeRuntime).toContain("window.unminimize()?");
    expect(nativeRuntime).toContain("window.show()?");
    expect(nativeRuntime).toContain("window.set_focus()?");
    expect(nativeRuntime).toContain("env::current_exe()?");
    expect(nativeRuntime).toContain("Command::new(executable)");
    expect(nativeCache).toContain("const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);");
    expect(nativeCache).toContain(".busy_timeout(DATABASE_BUSY_TIMEOUT)");
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

  test("loads every native cached Session and lists metadata without raw Events", async () => {
    const { runtime, calls, edbId, cachedEvents } = loadClientRuntime();
    const cache = runtime.createEdbCache();
    const snapshot = { agents: [{ id: "main", edb_id: edbId }] };

    const entries = await runtime.loadCachedSessions(cache, snapshot, "/workspace");
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ key: edbId, agentId: "main", scope: "/workspace" });
    expect(entries[0].events).toEqual(cachedEvents);
    expect(calls.filter((call) => call.command === "cache_load_metadata").at(-1).payload.edbIds).toEqual([edbId]);
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
    expect(build).toContain('[resolve(clientRoot, "app-icon.svg"), "app-icon.svg"]');
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
    expect(shared).toContain("frontendRuntime.windowReady?.()");
    expect(shared).not.toContain("materializeClientAgentStore");
    expect(shared).not.toContain("releaseClientAgentStore");
  });

  test("locks one product version and one complete package per release target", () => {
    const root = join(import.meta.dir, "..");
    const productVersion = execFileSync(
      "node", [join(root, "scripts/product-version.cjs"), "--print"],
      { cwd: root, encoding: "utf8" },
    ).trim();
    const packageJson = JSON.parse(readFileSync(join(root, "me-client/package.json"), "utf8"));
    const tauriConfig = JSON.parse(readFileSync(join(root, "me-client/src-tauri/tauri.conf.json"), "utf8"));
    const clientCargo = readFileSync(join(root, "me-client/src-tauri/Cargo.toml"), "utf8");
    const clientLock = readFileSync(join(root, "me-client/src-tauri/Cargo.lock"), "utf8");
    const clientMain = readFileSync(join(root, "me-client/src-tauri/src/main.rs"), "utf8");
    expect(packageJson.version).toBe(productVersion);
    expect(tauriConfig.version).toBe(productVersion);
    expect(clientCargo).toMatch(new RegExp(`^version = "${productVersion.replaceAll(".", "\\.")}"$`, "m"));
    expect(clientLock).toContain(`name = "me-client"\nversion = "${productVersion}"`);
    expect(clientMain).toContain('argument == "version" || argument == "--version"');
    expect(clientMain).toContain('println!("me-client {}", env!("CARGO_PKG_VERSION"))');

    const build = readFileSync(join(root, "build.sh"), "utf8");
    const release = readFileSync(join(root, "release.sh"), "utf8");
    const linuxBuilder = readFileSync(join(root, "packaging/linux/build-container.sh"), "utf8");
    const linuxInitializer = readFileSync(join(root, "packaging/linux/initialize-environment.sh"), "utf8");
    const linuxAppImagePrep = readFileSync(join(root, "packaging/linux/prepare-amd64-appimage-tools.sh"), "utf8");
    const linuxInContainer = readFileSync(join(root, "packaging/linux/build-in-container.sh"), "utf8");
    const runBuilder = readFileSync(join(root, "packaging/linux/build-run.sh"), "utf8");
    const windowsBuilder = readFileSync(join(root, "packaging/windows/build-installer.sh"), "utf8");
    const verifier = readFileSync(join(root, "scripts/verify-release-artifacts.sh"), "utf8");
    const manifest = readFileSync(join(root, "scripts/build-manifest.cjs"), "utf8");
    const buildRs = readFileSync(join(root, "build.rs"), "utf8");
    const unixInstall = readFileSync(join(root, "install.sh"), "utf8");
    const windowsInstall = readFileSync(join(root, "install.ps1"), "utf8");
    const updater = readFileSync(join(root, "src/updater.rs"), "utf8");
    const packageAssets = [
      "ME-macos-universal.pkg",
      "ME-windows-x86_64-setup.exe",
      "ME-linux-x86_64.run",
      "ME-linux-arm64.run",
    ];
    expect(existsSync(join(root, "build.sh"))).toBe(true);
    expect(existsSync(join(root, ".github/workflows/release.yml"))).toBe(false);
    expect(existsSync(join(root, "packaging/linux/Dockerfile"))).toBe(false);
    for (const asset of packageAssets) {
      expect(build).toContain(asset);
      expect(release).toContain(asset);
      expect(verifier).toContain(asset);
      expect(updater).toContain(asset);
    }
    expect(build).toContain("cargo xwin build");
    expect(build).toContain("packaging/linux/build-container.sh");
    expect(build).toContain("scripts/build-manifest.cjs create");
    expect(build).toContain("scripts/verify-release-artifacts.sh");
    expect(build).toContain("dependencies-$HOST_DEPENDENCY_FINGERPRINT.ready");
    expect(build).toContain("normalized_cargo_lock");
    expect(build).toContain("normalized_package_json");
    expect(build).toContain("<product-version>");
    expect(build).not.toContain("cat Cargo.lock me-client/src-tauri/Cargo.lock me-client/package.json");
    expect(build).toContain("BUILD_OFFLINE=1");
    expect(build).toContain("--offline|--online");
    expect(build).toContain('export ME_BUILD_OFFLINE="$BUILD_OFFLINE"');
    expect(build).not.toContain("FORCE_OFFLINE=0");
    expect(build).not.toContain('[[ -f "$HOST_DEPENDENCY_MARKER" ||');
    expect(build).toContain('mv "$STAGING_DIST" "$ROOT_DIR/dist"');
    expect(release).toContain("scripts/build-manifest.cjs verify");
    expect(release).toContain("gh release create");
    expect(release).toContain(".build-cache/bin/7zz");
    expect(release).not.toMatch(/cargo (?:build|xwin|tauri)|docker (?:run|image|volume)|makensis|build-container\.sh/);
    expect(linuxBuilder).toContain("docker image inspect");
    expect(linuxBuilder).toContain("docker commit");
    expect(linuxBuilder).toContain("docker volume create");
    expect(linuxBuilder).toContain("normalized_cargo_lock");
    expect(linuxBuilder).toContain("<product-version>");
    expect(linuxBuilder).toContain("--network=none");
    expect(linuxBuilder).toContain("OFFLINE=${ME_BUILD_OFFLINE:-1}");
    expect(linuxBuilder).toContain("if [[ $OFFLINE == 1 ]]");
    expect(linuxBuilder).toContain("offline Linux cache volume is missing");
    expect(linuxBuilder).not.toContain('[[ -f "$DEPENDENCY_MARKER" ||');
    expect(linuxBuilder).toContain('tar -cf "$WORK/source.tar"');
    expect(linuxBuilder).toContain('--platform "$PLATFORM"');
    expect(linuxBuilder).toContain('--volume "$CARGO_VOLUME:/cache/cargo"');
    expect(linuxBuilder).toContain('PYTHON_VOLUME="me-s-linux-python-${TARGETARCH}-v1"');
    expect(linuxBuilder).toContain('--volume "$PYTHON_VOLUME:/cache/python"');
    expect(linuxBuilder).toContain('"$ROOT_DIR/build.rs"');
    expect(linuxInContainer).toContain('ln -s "$PYTHON_CACHE_DIR" "$SOURCE_DIR/.build/python"');
    expect(buildRs).toContain("rerun-if-env-changed=ME_BUILD_OFFLINE");
    expect(linuxAppImagePrep).toContain("runtime-aarch64");
    expect(linuxAppImagePrep).toContain("7d5d772b7c32f0c84caf0a452a3072a5709027d7eac5856feb89a7a7a8881372");
    expect(linuxAppImagePrep).toContain("--runtime-file /opt/me-linuxdeploy-plugin-appimage/runtime-aarch64");
    expect(linuxBuilder).not.toMatch(/docker (?:image|volume) rm/);
    expect(linuxInitializer).toContain("RUST_VERSION");
    expect(linuxInitializer).toContain("cargo install cargo-zigbuild --version");
    expect(linuxInitializer).toContain("cargo install tauri-cli --version");
    expect(linuxInContainer).toContain("cargo zigbuild");
    expect(linuxInContainer).toContain("cargo tauri build --no-bundle");
    expect(linuxInContainer).toContain("cargo tauri bundle --verbose --bundles appimage");
    expect(linuxInContainer).not.toMatch(/apt-get|curl|cargo install/);
    expect(manifest).toContain("BUILD-MANIFEST.json");
    expect(build + release + linuxBuilder).not.toContain("docker buildx build");
    expect(windowsBuilder).toContain("makensis");
    expect(verifier).toContain("Nullsoft Installer self-extracting archive");
    expect(verifier).toContain("file format coff-x86-64");
    expect(runBuilder).not.toMatch(/\$OUTPUT["']?\s+--(?:version|extract-dir)/);
    expect(release).toContain("ME-linux-arm64.run\\nME-linux-x86_64.run\\nME-macos-universal.pkg\\nME-windows-x86_64-setup.exe\\nSHA256SUMS");
    expect(unixInstall).toContain("ME-macos-universal.pkg");
    expect(windowsInstall).toContain("ME-windows-x86_64-setup.exe");
    for (const source of [release, unixInstall, windowsInstall, updater]) {
      expect(source).not.toMatch(/me-(?:s|gateway)-(?:macos|linux|windows)/);
    }
  });
});
