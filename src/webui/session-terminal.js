(function (root, factory) {
  const api = factory(root);
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  } else {
    root.MeSessionTerminal = api;
  }
}(typeof globalThis !== "undefined" ? globalThis : this, function (root) {
  "use strict";

  const IDLE_POLL_MS = 120;
  const ERROR_POLL_MS = 1000;
  const INPUT_FLUSH_MS = 12;
  const RESIZE_FLUSH_MS = 80;
  const MAX_INPUT_BYTES = 64 * 1024;

  function bytesToBase64(bytes) {
    let binary = "";
    const chunk = 0x8000;
    for (let offset = 0; offset < bytes.length; offset += chunk) {
      binary += String.fromCharCode(...bytes.subarray(offset, offset + chunk));
    }
    return root.btoa(binary);
  }

  function base64ToBytes(value) {
    const binary = root.atob(value || "");
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    return bytes;
  }

  function normalizeIdentity(identity) {
    const agentId = String(identity?.agentId || "");
    const workspaceId = identity?.workspaceId == null ? null : String(identity.workspaceId);
    if (!agentId) throw new Error("缺少会话标识");
    const key = identity?.key == null
      ? `${workspaceId == null ? "direct" : workspaceId}:${agentId}`
      : String(identity.key);
    if (!key) throw new Error("缺少终端标识");
    return Object.freeze({ key, agentId, workspaceId });
  }

  function cssValue(style, name, fallback) {
    const value = style?.getPropertyValue?.(name)?.trim();
    return value || fallback;
  }

  function terminalTheme(runtime = root) {
    const document = runtime?.document;
    const style = document?.documentElement && runtime?.getComputedStyle
      ? runtime.getComputedStyle(document.documentElement)
      : null;
    const background = cssValue(style, "--terminal-bg", "#08090c");
    const foreground = cssValue(style, "--text", "#f2f3f5");
    const muted = cssValue(style, "--muted", "#9299a8");
    const accent = cssValue(style, "--accent", "#a896ff");
    return {
      background,
      foreground,
      cursor: accent,
      cursorAccent: background,
      selectionBackground: accent,
      black: background,
      brightBlack: muted,
    };
  }

  function terminalConstructor(runtime) {
    return runtime?.Terminal || null;
  }

  function fitConstructor(runtime) {
    return runtime?.FitAddon?.FitAddon || null;
  }

  function unicode11Constructor(runtime) {
    return runtime?.Unicode11Addon?.Unicode11Addon || null;
  }

  function terminalPath(identity, action) {
    return `/api/session-terminal/${encodeURIComponent(identity.agentId)}/${action}`;
  }

  function writeTerminal(terminal, bytes) {
    return new Promise((resolve) => terminal.write(bytes, resolve));
  }

  function stateLabel(state) {
    switch (state) {
      case "running": return "运行中";
      case "exited": return "已退出";
      case "lost": return "连接已中断";
      case "unavailable": return "不可用";
      default: return "正在连接";
    }
  }

  function create(options) {
    const runtime = options?.runtime || root;
    const container = options?.container;
    const controls = options?.controls || null;
    const statusElement = options?.statusElement || null;
    const shellElement = options?.shellElement || null;
    const request = options?.request;
    const onUnauthorized = options?.onUnauthorized || null;
    const Terminal = terminalConstructor(runtime);
    const FitAddon = fitConstructor(runtime);
    const Unicode11Addon = unicode11Constructor(runtime);
    if (!container || typeof request !== "function") {
      throw new Error("终端控制器缺少容器或请求通道");
    }
    if (!Terminal || !FitAddon || !Unicode11Addon) {
      throw new Error("终端组件未能加载");
    }

    const sessions = new Map();
    const encoder = new runtime.TextEncoder();
    let active = null;
    let generation = 0;
    let resizeObserver = null;
    let themeObserver = null;
    let windowResize = null;

    let controlsClick = null;

    function controlButtons() {
      return controls?.querySelectorAll?.("[data-session-terminal-byte]") || [];
    }

    function updateControls() {
      const disabled = !active || active.state !== "running";
      for (const button of controlButtons()) button.disabled = disabled;
    }

    function setStatus(session, state, error = null, exitCode = null) {
      if (active !== session) return;
      if (shellElement) {
        shellElement.textContent = session.shell || "终端";
        shellElement.title = session.cwd || session.shell || "";
      }
      if (statusElement) {
        let text = stateLabel(state);
        if (state === "exited" && exitCode != null) text += ` · ${exitCode}`;
        statusElement.textContent = error ? `${text} · ${error}` : text;
        statusElement.dataset.state = state || "connecting";
      }
      updateControls();
    }

    function refreshTheme() {
      const theme = terminalTheme(runtime);
      sessions.forEach((session) => {
        session.terminal.options.theme = theme;
      });
    }

    function queueOperation(session, action, body, errorMessage, ignoreConflict = false) {
      session.operationChain = session.operationChain
        .catch(() => {})
        .then(() => request(terminalPath(session.identity, action), {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        }, session.identity))
        .catch((error) => {
          if (error?.status === 401) onUnauthorized?.();
          if (active === session && !(ignoreConflict && error?.status === 409)) {
            setStatus(session, session.state, error?.message || errorMessage);
          }
        });
      return session.operationChain;
    }

    function flushResize(session) {
      runtime.clearTimeout(session.resizeTimer);
      session.resizeTimer = null;
      const size = session.pendingResize;
      session.pendingResize = null;
      if (!size || active !== session || !session.ready || session.state !== "running") return;
      if (size.cols === session.sentCols && size.rows === session.sentRows) return;
      session.sentCols = size.cols;
      session.sentRows = size.rows;
      queueOperation(session, "resize", size, "尺寸同步失败", true);
    }

    function scheduleResize(session) {
      if (active !== session || !session.ready || session.state !== "running") return;
      const cols = session.terminal.cols;
      const rows = session.terminal.rows;
      if (!cols || !rows) return;
      if (session.inputBytes) flushInput(session);
      session.pendingResize = { cols, rows };
      runtime.clearTimeout(session.resizeTimer);
      session.resizeTimer = runtime.setTimeout(() => flushResize(session), RESIZE_FLUSH_MS);
    }

    function fit(session, focus = false) {
      if (active !== session || !session.host.isConnected || container.clientWidth <= 0 || container.clientHeight <= 0) return;
      try {
        session.fitAddon.fit();
        scheduleResize(session);
        if (focus) session.terminal.focus();
      } catch (_) {
        // A hidden or transitioning view can briefly have no measurable cell size.
      }
    }

    function queueInput(session, bytes) {
      if (active !== session || session.state !== "running" || !bytes.length) return;
      if (session.pendingResize) flushResize(session);
      session.inputChunks.push(bytes);
      session.inputBytes += bytes.length;
      if (session.inputBytes >= MAX_INPUT_BYTES) {
        flushInput(session);
      } else if (session.inputTimer == null) {
        session.inputTimer = runtime.setTimeout(() => flushInput(session), INPUT_FLUSH_MS);
      }
    }

    function flushInput(session) {
      runtime.clearTimeout(session.inputTimer);
      session.inputTimer = null;
      if (!session.inputBytes) return;
      const length = Math.min(session.inputBytes, MAX_INPUT_BYTES);
      const bytes = new Uint8Array(length);
      let offset = 0;
      while (session.inputChunks.length && offset < length) {
        const chunk = session.inputChunks.shift();
        const take = Math.min(chunk.length, length - offset);
        bytes.set(chunk.subarray(0, take), offset);
        offset += take;
        if (take < chunk.length) session.inputChunks.unshift(chunk.subarray(take));
      }
      session.inputBytes -= length;
      queueOperation(session, "input", { data: bytesToBase64(bytes) }, "输入失败");
      if (session.inputBytes) session.inputTimer = runtime.setTimeout(() => flushInput(session), 0);
    }

    function createSession(identity) {
      const host = runtime.document.createElement("div");
      host.className = "session-terminal-instance";
      host.hidden = true;
      host.dataset.sessionTerminalKey = identity.key;
      container.appendChild(host);
      const terminal = new Terminal({
        allowProposedApi: true,
        convertEol: false,
        cursorBlink: true,
        cursorStyle: "block",
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace",
        fontSize: 14,
        lineHeight: 1.2,
        rightClickSelectsWord: true,
        screenReaderMode: true,
        scrollback: 10_000,
        theme: terminalTheme(runtime),
      });
      const fitAddon = new FitAddon();
      const unicode11Addon = new Unicode11Addon();
      terminal.loadAddon(fitAddon);
      terminal.loadAddon(unicode11Addon);
      terminal.unicode.activeVersion = "11";
      terminal.open(host);
      const session = {
        identity,
        host,
        terminal,
        fitAddon,
        cursor: null,
        ready: false,
        state: "connecting",
        shell: null,
        cwd: null,
        generation: 0,
        pollTimer: null,
        readAbort: null,
        resizeTimer: null,
        pendingResize: null,
        sentCols: null,
        sentRows: null,
        inputTimer: null,
        inputChunks: [],
        inputBytes: 0,
        operationChain: Promise.resolve(),
      };
      terminal.onData((data) => queueInput(session, encoder.encode(data)));
      terminal.onBinary((data) => {
        const bytes = new Uint8Array(data.length);
        for (let index = 0; index < data.length; index += 1) bytes[index] = data.charCodeAt(index) & 0xff;
        queueInput(session, bytes);
      });
      sessions.set(identity.key, session);
      return session;
    }

    function schedulePoll(session, delay) {
      runtime.clearTimeout(session.pollTimer);
      if (active !== session) return;
      session.pollTimer = runtime.setTimeout(() => poll(session), delay);
    }

    async function applyPayload(session, payload) {
      session.shell = payload.shell || session.shell;
      session.cwd = payload.cwd || session.cwd;
      session.state = payload.state || "running";
      if (payload.reset) session.terminal.reset();
      for (const event of payload.events || []) {
        if (event.type === "resize") {
          if (event.cols > 0 && event.rows > 0) {
            session.sentCols = event.cols;
            session.sentRows = event.rows;
            session.terminal.resize(event.cols, event.rows);
          }
        } else if (event.type === "output") {
          await writeTerminal(session.terminal, base64ToBytes(event.data));
        }
      }
      session.cursor = payload.cursor;
      setStatus(session, session.state, payload.error || null, payload.exit_code);
      if (!session.ready) {
        session.ready = true;
        runtime.requestAnimationFrame(() => fit(session, true));
      }
    }

    async function poll(session) {
      if (active !== session) return;
      const token = session.generation;
      const abort = new runtime.AbortController();
      session.readAbort = abort;
      try {
        const payload = await request(terminalPath(session.identity, "read"), {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ cursor: session.cursor }),
          signal: abort.signal,
        }, session.identity);
        if (active !== session || session.generation !== token) return;
        await applyPayload(session, payload);
        if (active !== session || session.generation !== token) return;
        const backlog = Number(payload.cursor) < Number(payload.tail);
        const complete = session.state !== "running" && Boolean(payload.drained);
        if (backlog) schedulePoll(session, 0);
        else if (!complete) schedulePoll(session, IDLE_POLL_MS);
      } catch (error) {
        if (error?.name === "AbortError" || active !== session || session.generation !== token) return;
        if (error?.status === 401) onUnauthorized?.();
        setStatus(session, session.state, error?.message || "终端连接失败");
        if (error?.status !== 401 && error?.status !== 404) schedulePoll(session, ERROR_POLL_MS);
      } finally {
        if (session.readAbort === abort) session.readAbort = null;
      }
    }

    function deactivate() {
      generation += 1;
      if (!active) {
        updateControls();
        return;
      }
      const session = active;
      active = null;
      updateControls();
      session.generation = generation;
      runtime.clearTimeout(session.pollTimer);
      session.pollTimer = null;
      runtime.clearTimeout(session.resizeTimer);
      session.resizeTimer = null;
      session.pendingResize = null;
      runtime.clearTimeout(session.inputTimer);
      session.inputTimer = null;
      session.readAbort?.abort();
      session.readAbort = null;
      session.host.hidden = true;
    }

    function attach(rawIdentity) {
      const identity = normalizeIdentity(rawIdentity);
      if (active?.identity.key === identity.key) {
        runtime.requestAnimationFrame(() => fit(active, true));
        return active;
      }
      deactivate();
      let session = sessions.get(identity.key);
      if (!session) session = createSession(identity);
      active = session;
      generation += 1;
      session.generation = generation;
      session.host.hidden = false;
      setStatus(session, session.state, null);
      poll(session);
      if (session.inputBytes && session.inputTimer == null) {
        session.inputTimer = runtime.setTimeout(() => flushInput(session), 0);
      }
      if (session.ready) runtime.requestAnimationFrame(() => fit(session, true));
      return session;
    }

    function dispose() {
      deactivate();
      resizeObserver?.disconnect();
      themeObserver?.disconnect();
      if (windowResize) runtime.removeEventListener("resize", windowResize);
      if (controlsClick) controls?.removeEventListener?.("click", controlsClick);
      sessions.forEach((session) => {
        runtime.clearTimeout(session.inputTimer);
        runtime.clearTimeout(session.resizeTimer);
        runtime.clearTimeout(session.pollTimer);
        session.readAbort?.abort();
        session.terminal.dispose();
        session.host.remove();
      });
      sessions.clear();
    }

    if (controls) {
      controlsClick = (event) => {
        const button = event.target.closest?.("[data-session-terminal-byte]");
        if (!button || !controls.contains(button) || button.disabled) return;
        const encoded = button.dataset.sessionTerminalByte;
        if (!/^[0-9a-f]{2}$/i.test(encoded || "")) return;
        const session = active;
        if (!session || session.state !== "running") {
          updateControls();
          return;
        }
        queueInput(session, new Uint8Array([Number.parseInt(encoded, 16)]));
        session.terminal.focus();
      };
      controls.addEventListener("click", controlsClick);
      updateControls();
    }

    if (runtime.ResizeObserver) {
      resizeObserver = new runtime.ResizeObserver(() => {
        if (active) runtime.requestAnimationFrame(() => fit(active));
      });
      resizeObserver.observe(container);
    } else {
      windowResize = () => {
        if (active) runtime.requestAnimationFrame(() => fit(active));
      };
      runtime.addEventListener("resize", windowResize, { passive: true });
    }
    if (runtime.MutationObserver && runtime.document?.documentElement) {
      themeObserver = new runtime.MutationObserver(refreshTheme);
      themeObserver.observe(runtime.document.documentElement, {
        attributes: true,
        attributeFilter: ["data-theme", "data-mode"],
      });
    }

    return { attach, deactivate, refreshTheme, dispose };
  }

  return {
    create,
    bytesToBase64,
    base64ToBytes,
    normalizeIdentity,
    terminalTheme,
  };
}));
