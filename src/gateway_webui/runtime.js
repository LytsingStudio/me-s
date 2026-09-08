(() => {
  "use strict";

  function apiPath(path, workspaceId = "chat") {
    const value = String(path || "");
    const childPath = value === "/api/sync" || value === "/api/snapshot" || value === "/api/command"
      || value.startsWith("/api/deletion-blocker/")
      || value.startsWith("/api/ui-projections/")
      || value.startsWith("/api/images/")
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
      sessionSectionTitle: "聊天",
      newSessionLabel: "新建聊天",
    }),
    get endpoint() { return ""; },
    fetch(...args) { return globalThis.MeEncryptedTransport.fetch(...args); },
    sendBeacon(...args) { return globalThis.MeEncryptedTransport.sendBeacon(...args); },
    downloadFile(...args) { return globalThis.MeEncryptedTransport.downloadFile(...args); },
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
  };

  globalThis.MeFrontendRuntime = Object.freeze(runtime);
})();
