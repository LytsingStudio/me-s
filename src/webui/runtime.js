(() => {
  "use strict";

  function workspaceName(path) {
    const parts = String(path || "").split(/[\\/]/).filter(Boolean);
    return parts.at(-1) || "当前工作区";
  }

  const runtime = {
    capabilities: Object.freeze({
      multipleWorkspaces: false,
      gatewaySettings: false,
      targetConfiguration: false,
      nativeDownload: false,
      pageTitle: "ME-S",
      brandTitle: "ME-S",
      cacheStorageLabel: "当前浏览器",
      sessionSectionTitle: "会话",
      newSessionLabel: "新建会话",
    }),
    get endpoint() { return ""; },
    async initialize() {
      document.documentElement.classList.add("me-direct");
      return { endpoint: "" };
    },
    apiPath(path) {
      return String(path || "");
    },
    async loadGatewayState(api) {
      const snapshot = await api("/api/snapshot", {}, "chat");
      const path = String(snapshot.environment?.workspace || "");
      return {
        ok: true,
        gateway_root: path,
        workspaces: [{ id: "chat", name: workspaceName(path), path, builtin: true }],
        selected_workspace_id: "chat",
        selected_agent_id: null,
        notices: [],
      };
    },
    persistSelection() {
      return Promise.resolve();
    },
    createEdbCache() {
      return globalThis.MeEdbCache.create();
    },
    loadCachedSessions(cache, _snapshot, scope) {
      return cache.loadScope(scope);
    },
    cacheKey(scope, agentId) {
      return globalThis.MeEdbCache.sessionKey(scope, agentId);
    },
  };

  globalThis.MeFrontendRuntime = Object.freeze(runtime);
})();
