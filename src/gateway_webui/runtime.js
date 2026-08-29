(() => {
  "use strict";

  function apiPath(path, workspaceId = "chat") {
    const value = String(path || "");
    const childPath = value === "/api/sync" || value === "/api/snapshot" || value === "/api/command"
      || value.startsWith("/api/deletion-blocker/")
      || value.startsWith("/api/session-terminal/")
      || value.startsWith("/api/remote-control/")
      || value.startsWith("/api/files/");
    if (!childPath) return value;
    return `/api/workspaces/${encodeURIComponent(workspaceId || "chat")}${value.slice(4)}`;
  }

  const runtime = {
    capabilities: Object.freeze({
      multipleWorkspaces: true,
      gatewaySettings: true,
      targetConfiguration: false,
      nativeDownload: false,
      pageTitle: "ME",
      brandTitle: "ME",
      cacheStorageLabel: "当前浏览器",
      sessionSectionTitle: "聊天",
      newSessionLabel: "新建聊天",
    }),
    get endpoint() { return ""; },
    async initialize() {
      document.documentElement.classList.add("me-gateway");
      return { endpoint: "" };
    },
    apiPath,
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
