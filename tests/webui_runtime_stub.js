"use strict";

function installDirectFrontendRuntime() {
  globalThis.MeFrontendRuntime = {
    capabilities: {},
    endpoint: "",
    fetch() { throw new Error("unexpected business request in UI unit test"); },
    sendBeacon() { return false; },
    apiPath(path) { return String(path || ""); },
    persistSelection() { return Promise.resolve(); },
    loadGatewayState() { return Promise.resolve({ workspaces: [] }); },
  };
  return globalThis.MeFrontendRuntime;
}

module.exports = { installDirectFrontendRuntime };
