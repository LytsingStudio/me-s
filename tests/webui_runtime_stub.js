"use strict";

function installDirectFrontendRuntime() {
  const cache = {
    loadScope: async () => [],
    discardSession: async () => {},
    saveSession() {},
    renderManager() {},
  };
  globalThis.MeFrontendRuntime = {
    capabilities: {},
    endpoint: "",
    apiPath(path) { return String(path || ""); },
    createEdbCache() { return cache; },
    loadCachedSessions(value, _snapshot, scope) { return value.loadScope(scope); },
    cacheKey(scope, agentId) { return `${scope}::${agentId}`; },
    persistSelection() { return Promise.resolve(); },
    loadGatewayState() { return Promise.resolve({ workspaces: [] }); },
  };
  return globalThis.MeFrontendRuntime;
}

module.exports = { installDirectFrontendRuntime };
