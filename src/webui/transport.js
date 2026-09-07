(() => {
  "use strict";

  const networkFetch = globalThis.fetch.bind(globalThis);
  const CHUNK_BYTES = 32 * 1024;
  const MAX_RECORD_BYTES = CHUNK_BYTES + 17;
  const MAX_REQUEST_BYTES = 2 * 1024 * 1024;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder("utf-8", { fatal: true });
  const COOKIE_KEY = "me-encrypted-session-v1";
  const failure = () => new Error("安全连接未能完成，请重试");
  const aborted = () => new DOMException("The operation was aborted", "AbortError");
  let modulePromise = null;
  let channelPromise = null;
  let activeChannel = null;
  let cookie = "";
  try { cookie = localStorage.getItem(COOKIE_KEY) || ""; } catch (_) {}
  globalThis.addEventListener?.("storage", (event) => {
    if (event.key === COOKIE_KEY) cookie = event.newValue || "";
  });

  function outerFetch(path, options = {}) {
    return networkFetch(path, { ...options, credentials: "omit", cache: "no-store", redirect: "error", referrerPolicy: "no-referrer" });
  }

  function cryptoModule() {
    if (!modulePromise) {
      modulePromise = outerFetch("/transport.wasm").then(async (response) => {
        if (!response.ok) throw failure();
        return WebAssembly.compile(await response.arrayBuffer());
      }).catch((error) => { modulePromise = null; throw error; });
    }
    return modulePromise;
  }

  async function createChannel() {
    if (typeof globalThis.crypto?.getRandomValues !== "function" || typeof globalThis.WebAssembly?.instantiate !== "function") {
      throw new Error("无法建立安全连接，请使用受支持的浏览器");
    }
    let wasm;
    const module = await cryptoModule();
    const instance = await WebAssembly.instantiate(module, { env: {
      me_random_fill(pointer, length) {
        try {
          globalThis.crypto.getRandomValues(new Uint8Array(wasm.memory.buffer, pointer, length));
          return 0;
        } catch (_) { return 1; }
      },
    } });
    wasm = instance.exports;
    const output = (length) => {
      if (length < 0) throw failure();
      return new Uint8Array(wasm.memory.buffer, wasm.me_output(), length).slice();
    };
    const input = (bytes) => new Uint8Array(wasm.memory.buffer, wasm.me_input(), bytes.length).set(bytes);
    const hello = output(wasm.me_start());
    const response = await outerFetch("/_me/handshake", { method: "POST", headers: { "Content-Type": "application/octet-stream" }, body: hello });
    if (!response.ok || response.headers.get("Content-Type") !== "application/octet-stream") throw new Error("无法建立安全连接，请确认服务版本与地址");
    const reply = new Uint8Array(await response.arrayBuffer());
    if (reply.length !== 64) throw failure();
    input(reply.subarray(16));
    if (wasm.me_finish(48) !== 0) throw failure();
    return {
      id: reply.slice(0, 16),
      next() {
        const number = Number(wasm.me_next_request());
        if (!Number.isInteger(number) || number < 0 || number >= 0xffffffff) throw failure();
        return number;
      },
      seal(number, block, kind, bytes) {
        if (block >= 0xffffffff || bytes.length > CHUNK_BYTES) throw failure();
        input(bytes);
        return output(wasm.me_seal(number, block, kind, bytes.length));
      },
      open(number, block, bytes) {
        if (block >= 0xffffffff || bytes.length > MAX_RECORD_BYTES) throw failure();
        input(bytes);
        return output(wasm.me_open(number, block, bytes.length));
      },
    };
  }

  function channel() {
    if (activeChannel) return Promise.resolve(activeChannel);
    if (!channelPromise) {
      channelPromise = createChannel().then((created) => { activeChannel = created; return created; })
        .finally(() => { channelPromise = null; });
    }
    return channelPromise;
  }

  function waitFor(promise, signal) {
    if (!signal) return promise;
    if (signal.aborted) return Promise.reject(aborted());
    return new Promise((resolve, reject) => {
      const abort = () => reject(aborted());
      signal.addEventListener("abort", abort, { once: true });
      promise.then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
    });
  }

  function apiUrl(input) {
    const url = new URL(typeof input === "string" || input instanceof URL ? input : input.url, document.baseURI);
    if (url.origin !== location.origin || !url.pathname.startsWith("/api/") || url.hash || url.username || url.password) throw failure();
    return `${url.pathname}${url.search}`;
  }

  function packet(current, number, path, method, headers, body) {
    if (body.length > MAX_REQUEST_BYTES) throw new Error("请求内容过大");
    headers = new Headers(headers);
    headers.delete("cookie");
    headers.delete("host");
    headers.delete("content-length");
    if (cookie) headers.set("Cookie", cookie);
    headers.set("Accept-Encoding", typeof DecompressionStream === "function" ? "gzip" : "identity");
    const head = encoder.encode(JSON.stringify({ method, url: path, headers: Array.from(headers.entries()), body_length: body.length }));
    const prefix = new Uint8Array(20);
    prefix.set(current.id);
    new DataView(prefix.buffer).setUint32(16, number);
    const parts = [prefix, current.seal(number, 0, 0, head)];
    let block = 1;
    for (let offset = 0; offset < body.length; offset += CHUNK_BYTES) {
      parts.push(current.seal(number, block++, 1, body.subarray(offset, offset + CHUNK_BYTES)));
    }
    parts.push(current.seal(number, block, 2, new Uint8Array()));
    return new Blob(parts, { type: "application/octet-stream" });
  }

  function responseReader(response, current, number) {
    const reader = response.body.getReader();
    let pending = new Uint8Array();
    let offset = 0;
    let block = 0;
    async function exact(length) {
      const bytes = new Uint8Array(length);
      let written = 0;
      while (written < length) {
        if (offset === pending.length) {
          const next = await reader.read();
          if (next.done) throw failure();
          pending = next.value;
          offset = 0;
          if (!pending.length) continue;
        }
        const size = Math.min(length - written, pending.length - offset);
        bytes.set(pending.subarray(offset, offset + size), written);
        written += size;
        offset += size;
      }
      return bytes;
    }
    return {
      async record() {
        const lengthBytes = await exact(2);
        const length = (lengthBytes[0] << 8) | lengthBytes[1];
        if (length < 17 || length > MAX_RECORD_BYTES) throw failure();
        return current.open(number, block++, await exact(length));
      },
      async end() {
        if (offset !== pending.length) throw failure();
        while (true) {
          const next = await reader.read();
          if (next.done) return;
          if (next.value.length) throw failure();
        }
      },
      cancel(reason) { return reader.cancel(reason); },
    };
  }

  async function decryptResponse(response, current, number) {
    if (response.status !== 200 || response.headers.get("Content-Type") !== "application/octet-stream" || !response.body) throw failure();
    const reader = responseReader(response, current, number);
    let head;
    try {
      const record = await reader.record();
      if (record[0] !== 0) throw failure();
      head = JSON.parse(decoder.decode(record.subarray(1)));
      if (!Number.isInteger(head.status) || head.status < 200 || head.status > 599 || !Array.isArray(head.headers)
          || head.headers.some((entry) => !Array.isArray(entry) || entry.length !== 2 || entry.some((value) => typeof value !== "string"))
          || (head.body_length !== null && (!Number.isSafeInteger(head.body_length) || head.body_length < 0))) throw failure();
    } catch (error) { await reader.cancel(error).catch(() => {}); throw error; }
    const headers = new Headers();
    for (const [name, value] of head.headers) {
      if (name.toLowerCase() === "set-cookie") {
        cookie = value.split(";", 1)[0];
        try { localStorage.setItem(COOKIE_KEY, cookie); } catch (_) {}
      } else {
        headers.append(name, value);
      }
    }
    let received = 0;
    let stream = new ReadableStream({
      async pull(controller) {
        try {
          const record = await reader.record();
          if (record[0] === 1 && record.length > 1) {
            received += record.length - 1;
            if (!Number.isSafeInteger(received) || (head.body_length !== null && received > head.body_length)) throw failure();
            controller.enqueue(record.subarray(1));
          } else if (record[0] === 2 && record.length === 1) {
            if (head.body_length !== null && received !== head.body_length) throw failure();
            await reader.end();
            controller.close();
          } else { throw failure(); }
        } catch (error) {
          await reader.cancel(error).catch(() => {});
          controller.error(error);
        }
      },
      cancel(reason) { return reader.cancel(reason); },
    });
    const encoding = headers.get("Content-Encoding");
    if (encoding) {
      if (encoding !== "gzip" || typeof DecompressionStream !== "function") {
        await stream.cancel();
        throw failure();
      }
      stream = stream.pipeThrough(new DecompressionStream("gzip"));
      headers.delete("Content-Encoding");
      headers.delete("Content-Length");
    }
    headers.delete("Transfer-Encoding");
    if ([204, 205, 304].includes(head.status)) {
      await new Response(stream).arrayBuffer();
      stream = null;
    }
    return new Response(stream, { status: head.status, headers });
  }

  async function encryptedFetch(input, options = {}) {
    const path = apiUrl(input);
    const source = typeof Request === "function" && input instanceof Request ? input : null;
    const signal = options.signal || source?.signal;
    if (signal?.aborted) throw aborted();
    const method = String(options.method || source?.method || "GET").toUpperCase();
    const headers = new Headers(source?.headers);
    new Headers(options.headers).forEach((value, name) => headers.set(name, value));
    let body = options.body ?? (source ? await source.clone().arrayBuffer() : null);
    if (body == null) body = new Uint8Array();
    else if (typeof body === "string" || body instanceof URLSearchParams) body = encoder.encode(String(body));
    else if (body instanceof Blob) {
      if (body.type && !headers.has("Content-Type")) headers.set("Content-Type", body.type);
      body = new Uint8Array(await body.arrayBuffer());
    } else if (body instanceof ArrayBuffer) body = new Uint8Array(body);
    else if (ArrayBuffer.isView(body)) body = new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
    else throw new TypeError("Unsupported request body");
    for (let attempt = 0; attempt < 2; attempt += 1) {
      const current = await waitFor(channel(), signal);
      if (signal?.aborted) throw aborted();
      const number = current.next();
      const response = await outerFetch("/_me/request", {
        method: "POST", body: packet(current, number, path, method, headers, body), signal, keepalive: Boolean(options.keepalive),
      });
      if (response.status === 410 && attempt === 0) {
        await response.body?.cancel();
        if (activeChannel === current) activeChannel = null;
        continue;
      }
      return decryptResponse(response, current, number);
    }
    throw failure();
  }

  function sendBeacon(input, text) {
    if (!activeChannel || typeof text !== "string") return false;
    try {
      const path = apiUrl(input);
      const current = activeChannel;
      const body = packet(current, current.next(), path, "POST", { "Content-Type": "application/json" }, encoder.encode(text));
      void outerFetch("/_me/request", { method: "POST", body, keepalive: true }).then((response) => response.body?.cancel()).catch(() => {});
      return true;
    } catch (_) { return false; }
  }

  async function downloadFile(path, filename, { signal, onProgress } = {}) {
    const response = await encryptedFetch(path, { signal });
    if (!response.ok) { await response.body?.cancel(); throw new Error("无法下载文件"); }
    const reader = response.body.getReader();
    const parts = [];
    let total = 0, lastProgress = 0;
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        if (signal?.aborted) throw aborted();
        parts.push(value);
        total += value.length;
        if (Date.now() - lastProgress >= 100) { onProgress?.(total); lastProgress = Date.now(); }
      }
      if (signal?.aborted) throw aborted();
    } catch (error) { await reader.cancel(error).catch(() => {}); throw error; }
    onProgress?.(total);
    const url = URL.createObjectURL(new Blob(parts, { type: response.headers.get("Content-Type") || "application/octet-stream" }));
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = filename;
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60000);
    return { size: total };
  }

  globalThis.MeEncryptedTransport = Object.freeze({ fetch: encryptedFetch, sendBeacon, downloadFile });
})();
