"use strict";

const { test, expect } = require("bun:test");
const { readFileSync } = require("node:fs");
const { runInNewContext } = require("node:vm");
const source = readFileSync(new URL("../src/webui/transport.js", import.meta.url), "utf8");
const wasm = readFileSync(new URL("../src/webui/transport.wasm", import.meta.url));

function harness({ handshake = () => new Response(new Uint8Array(64), { headers: { "Content-Type": "application/octet-stream" } }), crypto = globalThis.crypto, wasmRuntime = WebAssembly } = {}) {
  const calls = [];
  const storage = new Map();
  const context = {
    fetch: async (path, options) => {
      calls.push({ path, options });
      if (path === "/transport.wasm") return new Response(wasm);
      if (path === "/_me/handshake") return handshake(options);
      throw new Error(`unexpected outer request ${path}`);
    },
    document: { baseURI: "http://127.0.0.1:65123/" },
    location: { origin: "http://127.0.0.1:65123" },
    localStorage: { getItem: (key) => storage.get(key), setItem: (key, value) => storage.set(key, value) },
    WebAssembly: wasmRuntime, crypto,
    URL, Headers, Request, Response, Blob, URLSearchParams, TextEncoder, TextDecoder,
    DOMException, ReadableStream, DecompressionStream, AbortController,
  };
  runInNewContext(source, context);
  return { calls, transport: context.MeEncryptedTransport };
}

for (const kind of ["invalid reply", "connection failure"]) {
  test(`handshake ${kind} never transmits the password or falls back to a business URL`, async () => {
    const password = "synthetic-password-e546ac99";
    const options = kind === "connection failure" ? { handshake: () => { throw new Error("offline"); } } : {};
    const { calls, transport } = harness(options);
    await expect(transport.fetch("/api/auth/login", { method: "POST", body: JSON.stringify({ password }) })).rejects.toBeDefined();
    expect(calls.map((call) => call.path)).toEqual(["/transport.wasm", "/_me/handshake"]);
    expect(new TextDecoder().decode(calls[1].options.body)).not.toContain(password);
    expect(calls[1].options.body.length).toBe(32);
    for (const call of calls) {
      expect(call.options.credentials).toBe("omit");
      expect(call.options.redirect).toBe("error");
      expect(call.options.referrerPolicy).toBe("no-referrer");
      expect(call.options.headers?.Cookie).toBeUndefined();
    }
  });
}

test("unsupported browser fails before making any request", async () => {
  for (const options of [{ crypto: null }, { wasmRuntime: null }]) {
    const { transport, calls } = harness(options);
    await expect(transport.fetch("/api/auth/login", { method: "POST", body: "private" })).rejects.toThrow("无法建立安全连接");
    expect(calls).toHaveLength(0);
  }
});

test("an already aborted request does not start a handshake", async () => {
  const { transport, calls } = harness();
  const controller = new AbortController();
  controller.abort();
  await expect(transport.fetch("/api/sync", { signal: controller.signal })).rejects.toHaveProperty("name", "AbortError");
  expect(calls).toHaveLength(0);
});

test("unload without an established channel refuses rather than sending plaintext", () => {
  const { transport, calls } = harness();
  expect(transport.sendBeacon("/api/command", "private draft")).toBe(false);
  expect(calls).toHaveLength(0);
});

test("the business adapter refuses foreign origins and non-API paths", async () => {
  const { transport, calls } = harness();
  for (const path of ["http://other.invalid/api/sync", "/app.js", "/api/sync#private", "http://user:password@127.0.0.1:65123/api/sync"]) {
    await expect(transport.fetch(path, { body: "private", method: "POST" })).rejects.toBeDefined();
  }
  expect(calls).toHaveLength(0);
});
