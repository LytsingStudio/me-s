"use strict";

const { describe, expect, test } = require("bun:test");
const { existsSync, readFileSync } = require("node:fs");
const { execFileSync } = require("node:child_process");
const { join } = require("node:path");

function loadClientRuntime() {
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
            },
            rememberedDevices: [
              { endpoint: "http://127.0.0.1:38200", password: "local secret", updatedAt: 2, online: true },
              { endpoint: "https://offline.example", password: "old secret", updatedAt: 1, online: false },
            ],
            localDevice: { endpoint: "http://127.0.0.1:38200", online: true, requiresPassword: true },
          };
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
    addEventListener(type, listener) { nativeDocumentListeners.set(type, listener); },
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
  return {
    runtime, sandbox, calls, browserBeacons, edbId, cachedEvents,
    nativeWindowListeners, nativeDocumentListeners,
  };
}

describe("ME Client native adapter", () => {
  test("uses native UTF-8 JSON responses while leaving frontend assets local", async () => {
    const { runtime, sandbox, calls } = loadClientRuntime();
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
    expect(config).toContain('"devtools": false');
  });

  test("persists target-independent UI preferences through the native settings adapter", async () => {
    const { runtime, calls } = loadClientRuntime();
    expect(runtime.devicePreferences.getItem("me-theme")).toBeNull();
    await runtime.initialize();
    expect(runtime.devicePreferences.getItem("me-theme")).toBe("ocean");
    expect(runtime.devicePreferences.getItem("me-color-mode")).toBe("light");
    expect(runtime.devicePreferences.getItem("me-send-shortcut")).toBe("enter");

    await runtime.devicePreferences.setItem("me-theme", "obsidian");
    await runtime.devicePreferences.setItem("me-color-mode", "dark");
    await runtime.devicePreferences.setItem("me-send-shortcut", "modified-enter");
    await runtime.devicePreferences.setItem("gateway.endpoint", "must-not-persist");
    await runtime.configureTarget("https://other-gateway.example");
    expect(runtime.devicePreferences.getItem("me-theme")).toBe("obsidian");
    expect(runtime.devicePreferences.getItem("me-color-mode")).toBe("dark");
    expect(runtime.devicePreferences.getItem("me-send-shortcut")).toBe("modified-enter");
    expect(calls.filter((call) => call.command === "set_device_preference").map((call) => call.payload))
      .toEqual([
        { key: "me-theme", value: "obsidian" },
        { key: "me-color-mode", value: "dark" },
        { key: "me-send-shortcut", value: "modified-enter" },
      ]);

    const clientRuntime = readFileSync(join(import.meta.dir, "../me-client/client-runtime.js"), "utf8");
    const nativeRuntime = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/lib.rs"), "utf8");
    expect(clientRuntime).not.toMatch(/document\.cookie|localStorage|globalThis\.indexedDB/);
    expect(nativeRuntime).toContain('const DEVICE_PREFERENCE_KEYS: [&str; 3]');
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

  test("opens independent macOS processes and waits for shared SQLite writers", () => {
    const nativeRuntime = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/lib.rs"), "utf8");
    const nativeCache = readFileSync(join(import.meta.dir, "../me-client/src-tauri/src/cache.rs"), "utf8");
    expect(nativeRuntime).toContain("tauri::RunEvent::Reopen");
    expect(nativeRuntime).toContain("env::current_exe()");
    expect(nativeRuntime).toContain("Command::new(executable)");
    expect(nativeRuntime).toContain(".build(tauri::generate_context!())");
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

    const release = readFileSync(join(root, "release.sh"), "utf8");
    const linuxDockerfile = readFileSync(join(root, "packaging/linux/Dockerfile"), "utf8");
    const linuxBuilder = readFileSync(join(root, "packaging/linux/build-container.sh"), "utf8");
    const runBuilder = readFileSync(join(root, "packaging/linux/build-run.sh"), "utf8");
    const windowsBuilder = readFileSync(join(root, "packaging/windows/build-installer.sh"), "utf8");
    const verifier = readFileSync(join(root, "scripts/verify-release-artifacts.sh"), "utf8");
    const unixInstall = readFileSync(join(root, "install.sh"), "utf8");
    const windowsInstall = readFileSync(join(root, "install.ps1"), "utf8");
    const updater = readFileSync(join(root, "src/updater.rs"), "utf8");
    const packageAssets = [
      "ME-macos-universal.pkg",
      "ME-windows-x86_64-setup.exe",
      "ME-linux-x86_64.run",
      "ME-linux-arm64.run",
    ];
    expect(existsSync(join(root, ".github/workflows/release.yml"))).toBe(false);
    for (const asset of packageAssets) {
      expect(release).toContain(asset);
      expect(verifier).toContain(asset);
      expect(updater).toContain(asset);
    }
    expect(release).toContain("cargo xwin build");
    expect(release).toContain("packaging/linux/build-container.sh");
    expect(release).toContain("scripts/verify-release-artifacts.sh");
    expect(release).toContain("gh release create");
    expect(release).toContain("RELEASE_BUILDER_NAME=me-s-release");
    expect(release).toContain("--driver docker-container");
    expect(release).toContain('docker buildx rm --force "$RELEASE_BUILDER_NAME"');
    expect(release).toContain("trap cleanup_release_builder_on_exit EXIT");
    expect(release).toContain("create_release_builder");
    expect(linuxBuilder).toContain('BUILDER_ARGS=(--builder "$ME_RELEASE_BUILDER")');
    expect(release).toContain("            cd /");
    expect(release).toContain("colima ssh -- sudo fstrim -v /var/lib/docker");
    expect(release).not.toMatch(/gh (?:workflow|run)|GitHub Actions|release\.yml/);
    expect(linuxDockerfile).toContain("cargo zigbuild");
    expect(linuxDockerfile).toContain("build --no-bundle");
    expect(linuxDockerfile).toContain("bundle --verbose --bundles appimage");
    expect(linuxBuilder).toContain("docker buildx build");
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
