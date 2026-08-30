(function (root, factory) {
  const api = factory(root);
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  } else {
    root.MeRemoteControl = api;
  }
}(typeof globalThis !== "undefined" ? globalThis : this, function (root) {
  "use strict";

  const FPS_OPTIONS = Object.freeze([1, 3, 5, 10]);
  const SCALE_OPTIONS = Object.freeze([100, 75, 50, 25]);
  const DEFAULT_FPS = 3;
  const DEFAULT_SCALE = 50;
  const KEEPALIVE_MS = 5000;
  const STATUS_REFRESH_MS = 2000;
  const MAX_FRAME_REQUEST_AGE_MS = 1500;
  const MAX_SCREENSHOT_BUSY_RETRIES = 5;
  const MAX_PENDING_INPUT_EVENTS = 256;
  const MAX_INPUT_BATCH_EVENTS = 128;
  const INPUT_FLUSH_MS = 12;
  const PHYSICAL_CODES = new Set([
    "Enter", "NumpadEnter", "Escape", "Space", "Tab", "Backspace", "Delete",
    "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown",
    "ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight",
    "CapsLock", "Minus", "Equal", "BracketLeft", "BracketRight", "Backslash", "Semicolon", "Quote",
    "Comma", "Period", "Slash", "Backquote",
    ...Array.from({ length: 26 }, (_, index) => `Key${String.fromCharCode(65 + index)}`),
    ...Array.from({ length: 10 }, (_, index) => `Digit${index}`),
    ...Array.from({ length: 20 }, (_, index) => `F${index + 1}`),
  ]);

  function isExitShortcut(event) {
    return Boolean(event && event.ctrlKey && event.shiftKey && !event.altKey && !event.metaKey
      && (event.code === "KeyE" || String(event.key || "").toLowerCase() === "e"));
  }

  function positiveInteger(value) {
    const parsed = Number.parseInt(String(value == null ? "" : value), 10);
    return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : null;
  }

  function readFrameMetadata(headers) {
    const sequence = positiveInteger(headers?.get?.("X-Me-Remote-Sequence"));
    const screenWidth = positiveInteger(headers?.get?.("X-Me-Screen-Width"));
    const screenHeight = positiveInteger(headers?.get?.("X-Me-Screen-Height"));
    const frameWidth = positiveInteger(headers?.get?.("X-Me-Frame-Width"));
    const frameHeight = positiveInteger(headers?.get?.("X-Me-Frame-Height"));
    if ([sequence, screenWidth, screenHeight, frameWidth, frameHeight].some((value) => value == null)) {
      throw new Error("远控帧缺少有效的几何信息");
    }
    return { sequence, screenWidth, screenHeight, frameWidth, frameHeight };
  }

  function containedFrameRect(rect, frameWidth, frameHeight) {
    if (!rect || !Number.isFinite(rect.left) || !Number.isFinite(rect.top)
      || rect.width <= 0 || rect.height <= 0 || frameWidth <= 0 || frameHeight <= 0) return null;
    const scale = Math.min(rect.width / frameWidth, rect.height / frameHeight);
    const width = frameWidth * scale;
    const height = frameHeight * scale;
    return {
      left: rect.left + ((rect.width - width) / 2),
      top: rect.top + ((rect.height - height) / 2),
      width,
      height,
    };
  }

  function mapPointerToScreen(clientX, clientY, rect, screenWidth, screenHeight,
    frameWidth = screenWidth, frameHeight = screenHeight) {
    if (!Number.isFinite(clientX) || !Number.isFinite(clientY)
      || screenWidth <= 0 || screenHeight <= 0) return null;
    const frameRect = containedFrameRect(rect, frameWidth, frameHeight);
    if (!frameRect) return null;
    const right = frameRect.left + frameRect.width;
    const bottom = frameRect.top + frameRect.height;
    if (clientX < frameRect.left || clientX > right || clientY < frameRect.top || clientY > bottom) return null;
    const xRatio = (clientX - frameRect.left) / frameRect.width;
    const yRatio = (clientY - frameRect.top) / frameRect.height;
    return {
      x: Math.min(screenWidth - 1, Math.max(0, Math.floor(xRatio * screenWidth))),
      y: Math.min(screenHeight - 1, Math.max(0, Math.floor(yRatio * screenHeight))),
    };
  }

  function normalizeWheelDelta(value, deltaMode) {
    if (!Number.isFinite(value) || value === 0) return 0;
    const divisor = deltaMode === 0 ? 80 : 1;
    const pages = deltaMode === 2 ? value * 3 : value / divisor;
    const rounded = Math.round(pages);
    const nonzero = rounded === 0 ? Math.sign(value) : rounded;
    return Math.max(-10000, Math.min(10000, -nonzero));
  }

  function mouseButtonName(button) {
    if (button === 0) return "left";
    if (button === 1) return "middle";
    if (button === 2) return "right";
    return null;
  }

  function makeError(response, payload) {
    const error = new Error(payload?.error || `HTTP ${response.status}`);
    error.status = response.status;
    error.code = payload?.code || null;
    return error;
  }

  function create(options) {
    const runtime = options?.runtime || root;
    const container = options?.container;
    const request = options?.request;
    const onUnauthorized = options?.onUnauthorized || null;
    const notify = options?.notify || null;
    if (!container || typeof request !== "function") {
      throw new Error("远程控制器缺少容器或请求通道");
    }

    const startButton = container.querySelector("[data-remote-start]");
    const stopButton = container.querySelector("[data-remote-stop]");
    const screenshotButton = container.querySelector("[data-remote-screenshot]");
    const fpsSelect = container.querySelector("[data-remote-fps]");
    const scaleSelect = container.querySelector("[data-remote-scale]");
    const stage = container.querySelector("[data-remote-stage]");
    const image = container.querySelector("[data-remote-image]");
    const keyboardInput = container.querySelector("[data-remote-keyboard]");
    const emptyState = container.querySelector("[data-remote-empty]");
    const statusElement = container.querySelector("[data-remote-status]");
    const frameElement = container.querySelector("[data-remote-frame-count]");
    if (!startButton || !stopButton || !screenshotButton || !fpsSelect || !scaleSelect
      || !stage || !image || !keyboardInput || !emptyState || !statusElement || !frameElement) {
      throw new Error("远程控制器页面结构不完整");
    }

    let supported = null;
    let serverActive = false;
    let owned = false;
    let controllerToken = null;
    let viewActive = false;
    let disposed = false;
    let captured = false;
    let composing = false;
    let fps = FPS_OPTIONS.includes(Number(fpsSelect.value)) ? Number(fpsSelect.value) : DEFAULT_FPS;
    let scale = SCALE_OPTIONS.includes(Number(scaleSelect.value)) ? Number(scaleSelect.value) : DEFAULT_SCALE;
    let frameCount = 0;
    let lastSequence = 0;
    let screenWidth = 0;
    let screenHeight = 0;
    let frameWidth = 0;
    let frameHeight = 0;
    let frameTicker = null;
    let frameAbort = null;
    let frameRequestStartedAt = 0;
    let frameRequestId = 0;
    let displayGeneration = 0;
    let keepaliveTimer = null;
    let statusRefreshTimer = null;
    let imageUrl = null;
    let frameDecodeInFlight = false;
    let pendingFrame = null;
    let pendingInput = [];
    let inputTimer = null;
    let inputInFlight = null;
    let releasePending = false;
    let keepaliveInFlight = false;
    let settingsInFlight = false;
    let settingsPending = false;
    let startInFlight = false;
    let stopInFlight = false;
    let screenshotInFlight = false;
    const pressedCodes = new Set();
    const pressedButtons = new Set();

    fpsSelect.value = String(fps);
    scaleSelect.value = String(scale);

    function setStatus(message, kind = "idle") {
      statusElement.textContent = message;
      statusElement.dataset.state = kind;
    }

    function updateUi() {
      frameElement.textContent = `frame: ${frameCount}`;
      startButton.disabled = supported === false || serverActive || disposed || startInFlight;
      stopButton.disabled = !owned || disposed || stopInFlight;
      screenshotButton.disabled = supported === false || disposed || screenshotInFlight;
      fpsSelect.disabled = disposed;
      scaleSelect.disabled = disposed;
      container.classList.toggle("remote-control-active", owned);
      container.classList.toggle("remote-control-captured", captured);
      if (supported === false) {
        setStatus("当前平台不支持远程控制", "unsupported");
      } else if (owned) {
        setStatus(captured ? "远控中 · 键鼠已捕获" : "远控中 · 点击画面捕获键鼠", captured ? "captured" : "active");
      } else if (serverActive) {
        setStatus("远程控制正在被其他页面使用", "busy");
      } else if (supported === true) {
        setStatus("未开始", "idle");
      } else {
        setStatus("正在检查远程控制状态", "pending");
      }
    }

    function showError(error, announce = false) {
      if (error?.name === "AbortError") return;
      const message = error?.message || "远程控制请求失败";
      setStatus(message, "error");
      if (announce && notify) notify(message, "error");
    }

    async function requestJson(action, body, signal) {
      const response = await request(action, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body || {}),
        signal,
      });
      if (response.status === 401 && onUnauthorized) onUnauthorized();
      const payload = response.status === 204
        ? null
        : await response.json().catch(() => ({ ok: false, error: `HTTP ${response.status}` }));
      if (!response.ok || payload?.ok === false) throw makeError(response, payload);
      return payload;
    }

    function resetDisplayedRound() {
      frameCount = 0;
      lastSequence = 0;
      pendingFrame = null;
      frameElement.textContent = "frame: 0";
    }

    function revokeImageUrl(url) {
      if (!url) return;
      try { runtime.URL?.revokeObjectURL?.(url); } catch (_) {}
    }

    function decodePendingFrame() {
      if (frameDecodeInFlight || !pendingFrame) return;
      const candidate = pendingFrame;
      pendingFrame = null;
      if (candidate.metadata.sequence <= lastSequence) return;
      frameDecodeInFlight = true;
      const { blob, metadata, generation, requireControl } = candidate;
      const url = runtime.URL.createObjectURL(blob);
      const Loader = runtime.Image;
      const finish = () => {
        frameDecodeInFlight = false;
        decodePendingFrame();
      };
      const commit = () => {
        try {
          if (disposed || generation !== displayGeneration
            || (requireControl && (!owned || !viewActive))) {
            revokeImageUrl(url);
            return;
          }
          if (metadata.sequence <= lastSequence) {
            revokeImageUrl(url);
            return;
          }
          const previousUrl = imageUrl;
          imageUrl = url;
          image.src = url;
          image.hidden = false;
          emptyState.hidden = true;
          lastSequence = metadata.sequence;
          screenWidth = metadata.screenWidth;
          screenHeight = metadata.screenHeight;
          frameWidth = metadata.frameWidth;
          frameHeight = metadata.frameHeight;
          frameCount += 1;
          frameElement.textContent = `frame: ${frameCount}`;
          revokeImageUrl(previousUrl);
        } finally {
          finish();
        }
      };
      if (typeof Loader !== "function") {
        commit();
        return;
      }
      const loader = new Loader();
      loader.onload = commit;
      loader.onerror = () => {
        revokeImageUrl(url);
        showError(new Error("远控帧无法显示"));
        finish();
      };
      loader.src = url;
    }

    function displayFrame(blob, metadata, generation, requireControl) {
      if (metadata.sequence <= lastSequence) return;
      if (!pendingFrame || metadata.sequence > pendingFrame.metadata.sequence) {
        pendingFrame = { blob, metadata, generation, requireControl };
      }
      decodePendingFrame();
    }

    async function acceptFrameResponse(response, requestId, generation, requireControl) {
      if (response.status === 401 && onUnauthorized) onUnauthorized();
      if (response.status === 204) return;
      if (!response.ok) {
        const payload = await response.json().catch(() => ({ error: `HTTP ${response.status}` }));
        throw makeError(response, payload);
      }
      const metadata = readFrameMetadata(response.headers);
      if (metadata.sequence <= lastSequence) return;
      const blob = await response.blob();
      if (requestId !== frameRequestId || blob.size === 0) return;
      displayFrame(blob, metadata, generation, requireControl);
    }

    function abortFrameRequest() {
      frameRequestId += 1;
      if (frameAbort) frameAbort.abort();
      frameAbort = null;
      frameRequestStartedAt = 0;
    }

    function framePeriod() {
      return Math.max(50, Math.floor(1000 / fps));
    }

    function stopFrameTicker() {
      if (frameTicker != null) runtime.clearInterval(frameTicker);
      frameTicker = null;
      pendingFrame = null;
      abortFrameRequest();
    }

    function startFrameTicker() {
      stopFrameTicker();
      if (!owned || !viewActive || runtime.document?.hidden) return;
      frameTicker = runtime.setInterval(() => void pollFrame(), framePeriod());
      void pollFrame();
    }

    async function pollFrame() {
      if (disposed || !owned || !controllerToken || !viewActive || runtime.document?.hidden) return;
      const now = Date.now();
      if (frameAbort) {
        if (now - frameRequestStartedAt < MAX_FRAME_REQUEST_AGE_MS) return;
        abortFrameRequest();
      }
      const token = controllerToken;
      const requestId = ++frameRequestId;
      const generation = displayGeneration;
      const abort = new runtime.AbortController();
      frameAbort = abort;
      frameRequestStartedAt = now;
      try {
        const response = await request("frame", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ controller_token: token, after_sequence: lastSequence || null }),
          signal: abort.signal,
        });
        if (requestId !== frameRequestId) return;
        await acceptFrameResponse(response, requestId, generation, true);
      } catch (error) {
        if (error?.name !== "AbortError") {
          if (error?.status === 401 && onUnauthorized) onUnauthorized();
          if (error?.code === "remote_control_not_owned") loseOwnership();
          else showError(error);
        }
      } finally {
        if (requestId === frameRequestId) {
          frameAbort = null;
          frameRequestStartedAt = 0;
        }
      }
    }

    function clearKeepalive() {
      if (keepaliveTimer != null) runtime.clearInterval(keepaliveTimer);
      keepaliveTimer = null;
    }

    function startKeepalive() {
      clearKeepalive();
      if (!owned || !controllerToken) return;
      keepaliveTimer = runtime.setInterval(async () => {
        if (!owned || !controllerToken || disposed || keepaliveInFlight) return;
        const token = controllerToken;
        keepaliveInFlight = true;
        try {
          await requestJson("keepalive", { controller_token: token });
        } catch (error) {
          if (error?.code === "remote_control_not_owned") loseOwnership();
          else if (error?.status !== 401) showError(error);
        } finally {
          keepaliveInFlight = false;
        }
      }, KEEPALIVE_MS);
    }

    function clearStatusRefresh() {
      if (statusRefreshTimer != null) runtime.clearTimeout(statusRefreshTimer);
      statusRefreshTimer = null;
    }

    function scheduleStatusRefresh() {
      clearStatusRefresh();
      if (disposed || !viewActive || owned) return;
      statusRefreshTimer = runtime.setTimeout(() => {
        statusRefreshTimer = null;
        void refreshStatus();
      }, STATUS_REFRESH_MS);
    }

    function loseOwnership() {
      displayGeneration += 1;
      serverActive = false;
      owned = false;
      controllerToken = null;
      captured = false;
      settingsPending = false;
      pressedCodes.clear();
      pressedButtons.clear();
      pendingInput = [];
      releasePending = false;
      stopFrameTicker();
      clearKeepalive();
      updateUi();
      scheduleStatusRefresh();
    }

    async function refreshStatus() {
      clearStatusRefresh();
      try {
        const payload = await requestJson("status", { controller_token: controllerToken });
        supported = Boolean(payload.supported);
        serverActive = Boolean(payload.active);
        if (controllerToken && payload.owned) {
          owned = true;
          fps = FPS_OPTIONS.includes(Number(payload.fps)) ? Number(payload.fps) : fps;
          scale = SCALE_OPTIONS.includes(Number(payload.scale)) ? Number(payload.scale) : scale;
          fpsSelect.value = String(fps);
          scaleSelect.value = String(scale);
          startKeepalive();
          startFrameTicker();
        } else if (controllerToken) {
          loseOwnership();
          serverActive = Boolean(payload.active);
        } else {
          owned = false;
        }
        updateUi();
      } catch (error) {
        showError(error);
      } finally {
        scheduleStatusRefresh();
      }
    }

    async function startControl() {
      if (owned || serverActive || disposed || startInFlight) return;
      clearStatusRefresh();
      startInFlight = true;
      startButton.disabled = true;
      displayGeneration += 1;
      try {
        const payload = await requestJson("start", { fps, scale });
        resetDisplayedRound();
        supported = true;
        serverActive = true;
        owned = true;
        controllerToken = payload.controller_token;
        fps = Number(payload.fps);
        scale = Number(payload.scale);
        fpsSelect.value = String(fps);
        scaleSelect.value = String(scale);
        startKeepalive();
        startFrameTicker();
        startInFlight = false;
        updateUi();
      } catch (error) {
        startInFlight = false;
        if (error?.code === "remote_control_busy") serverActive = true;
        else serverActive = false;
        updateUi();
        showError(error, true);
      }
    }

    function clearLocalInputState() {
      captured = false;
      composing = false;
      pressedCodes.clear();
      pressedButtons.clear();
      pendingInput = [];
      if (inputTimer != null) runtime.clearTimeout(inputTimer);
      inputTimer = null;
      keyboardInput.value = "";
      updateUi();
    }

    async function stopControl(abandonOnFailure = false) {
      if (!owned || !controllerToken || disposed || stopInFlight) return;
      const token = controllerToken;
      stopInFlight = true;
      stopButton.disabled = true;
      displayGeneration += 1;
      stopFrameTicker();
      clearKeepalive();
      releasePending = false;
      clearLocalInputState();
      try {
        await requestJson("stop", { controller_token: token });
        serverActive = false;
        owned = false;
        controllerToken = null;
        stopInFlight = false;
        updateUi();
      } catch (error) {
        stopInFlight = false;
        if (error?.code === "remote_control_not_owned") loseOwnership();
        else if (abandonOnFailure) {
          loseOwnership();
        } else {
          releasePending = true;
          if (!inputInFlight) void flushRelease();
          showError(error, true);
          startKeepalive();
          startFrameTicker();
          stopButton.disabled = !owned || disposed;
        }
      }
    }

    async function screenshot() {
      if (disposed || supported === false || screenshotInFlight) return;
      screenshotInFlight = true;
      screenshotButton.disabled = true;
      if (!owned) {
        displayGeneration += 1;
        resetDisplayedRound();
      }
      abortFrameRequest();
      const requestId = ++frameRequestId;
      const generation = displayGeneration;
      const abort = new runtime.AbortController();
      frameAbort = abort;
      frameRequestStartedAt = Date.now();
      try {
        for (let attempt = 0; attempt <= MAX_SCREENSHOT_BUSY_RETRIES; attempt += 1) {
          const response = await request("screenshot", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ scale }),
            signal: abort.signal,
          });
          if (response.status === 204) {
            if (attempt >= MAX_SCREENSHOT_BUSY_RETRIES) {
              throw new Error("宿主屏幕正在捕获，请稍后重试");
            }
            await new Promise((resolve) => runtime.setTimeout(resolve, 50 * (attempt + 1)));
            if (requestId !== frameRequestId || disposed) return;
            continue;
          }
          await acceptFrameResponse(response, requestId, generation, false);
          supported = true;
          break;
        }
      } catch (error) {
        showError(error, true);
      } finally {
        if (requestId === frameRequestId) {
          frameAbort = null;
          frameRequestStartedAt = 0;
        }
        screenshotInFlight = false;
        screenshotButton.disabled = supported === false || disposed;
        if (owned) startFrameTicker();
      }
    }

    async function updateSettings() {
      fps = Number(fpsSelect.value);
      scale = Number(scaleSelect.value);
      if (!owned || !controllerToken) return;
      settingsPending = true;
      abortFrameRequest();
      if (settingsInFlight) return;
      settingsInFlight = true;
      try {
        while (settingsPending && owned && controllerToken) {
          settingsPending = false;
          const token = controllerToken;
          const requestedFps = fps;
          const requestedScale = scale;
          try {
            await requestJson("settings", {
              controller_token: token,
              fps: requestedFps,
              scale: requestedScale,
            });
          } catch (error) {
            settingsPending = false;
            if (error?.code === "remote_control_not_owned") loseOwnership();
            else {
              showError(error, true);
              await refreshStatus();
            }
            return;
          }
        }
      } finally {
        settingsInFlight = false;
        if (owned) startFrameTicker();
      }
    }

    function scheduleInputPump() {
      if (inputTimer != null || inputInFlight || releasePending || disposed) return;
      inputTimer = runtime.setTimeout(() => {
        inputTimer = null;
        void pumpInput();
      }, INPUT_FLUSH_MS);
    }

    function queueInput(event) {
      if (!captured || !owned || !controllerToken || releasePending) return;
      const lastPending = pendingInput[pendingInput.length - 1];
      if (event.kind === "mouse_move" && lastPending?.kind === "mouse_move") {
        pendingInput[pendingInput.length - 1] = event;
      } else {
        if (pendingInput.length >= MAX_PENDING_INPUT_EVENTS) {
          void exitCapture();
          showError(new Error("输入通道拥塞，已释放键盘和鼠标"), true);
          return;
        }
        pendingInput.push(event);
      }
      scheduleInputPump();
    }

    async function pumpInput() {
      if (inputInFlight || releasePending || !pendingInput.length || !owned || !controllerToken) return;
      const token = controllerToken;
      const events = pendingInput.splice(0, MAX_INPUT_BATCH_EVENTS);
      const operation = requestJson("input", { controller_token: token, events });
      inputInFlight = operation;
      try {
        await operation;
      } catch (error) {
        clearLocalInputState();
        showError(error, true);
        releasePending = true;
      } finally {
        if (inputInFlight === operation) inputInFlight = null;
        if (releasePending) void flushRelease();
        else if (pendingInput.length) scheduleInputPump();
      }
    }

    async function flushRelease() {
      if (!releasePending || inputInFlight || !owned || !controllerToken) return;
      const token = controllerToken;
      try {
        await requestJson("release", { controller_token: token });
      } catch (error) {
        if (error?.code === "remote_control_not_owned") loseOwnership();
        else showError(error);
      } finally {
        releasePending = false;
      }
    }

    async function exitCapture() {
      if (!captured && !pressedCodes.size && !pressedButtons.size && !pendingInput.length) return;
      clearLocalInputState();
      if (!owned || !controllerToken) return;
      releasePending = true;
      if (!inputInFlight) await flushRelease();
    }

    function enterCapture() {
      if (!owned || !viewActive || disposed || releasePending || stopInFlight) return false;
      captured = true;
      updateUi();
      try { keyboardInput.focus({ preventScroll: true }); } catch (_) { keyboardInput.focus(); }
      return true;
    }

    function pointerPosition(event) {
      return mapPointerToScreen(event.clientX, event.clientY, image.getBoundingClientRect(),
        screenWidth, screenHeight, frameWidth, frameHeight);
    }

    function onPointerMove(event) {
      if (!captured || image.hidden) return;
      const position = pointerPosition(event);
      if (position) queueInput({ kind: "mouse_move", ...position });
    }

    function onPointerDown(event) {
      if (!owned || image.hidden) return;
      event.preventDefault();
      const position = pointerPosition(event);
      if (!position || !enterCapture()) return;
      queueInput({ kind: "mouse_move", ...position });
      const button = mouseButtonName(event.button);
      if (!button) return;
      pressedButtons.add(button);
      try { image.setPointerCapture(event.pointerId); } catch (_) {}
      queueInput({ kind: "mouse_down", button });
    }

    function onPointerUp(event) {
      if (!captured) return;
      event.preventDefault();
      const position = pointerPosition(event);
      if (position) queueInput({ kind: "mouse_move", ...position });
      const button = mouseButtonName(event.button);
      if (!button || !pressedButtons.has(button)) return;
      pressedButtons.delete(button);
      queueInput({ kind: "mouse_up", button });
    }

    function onWheel(event) {
      if (!captured) return;
      const deltaX = normalizeWheelDelta(event.deltaX, event.deltaMode);
      const deltaY = normalizeWheelDelta(event.deltaY, event.deltaMode);
      if (!deltaX && !deltaY) return;
      event.preventDefault();
      queueInput({ kind: "mouse_wheel", delta_x: deltaX, delta_y: deltaY });
    }

    function onKeyDown(event) {
      if (!captured) return;
      if (isExitShortcut(event)) {
        event.preventDefault();
        event.stopPropagation();
        void exitCapture();
        return;
      }
      if (event.isComposing || event.keyCode === 229 || event.repeat || !PHYSICAL_CODES.has(event.code)) return;
      event.preventDefault();
      if (pressedCodes.has(event.code)) return;
      pressedCodes.add(event.code);
      queueInput({ kind: "key_down", code: event.code });
    }

    function onKeyUp(event) {
      if (!captured || event.isComposing || !pressedCodes.has(event.code)) return;
      event.preventDefault();
      pressedCodes.delete(event.code);
      queueInput({ kind: "key_up", code: event.code });
    }

    function queueText(text) {
      if (!captured || !text) return;
      queueInput({ kind: "text", text: String(text) });
    }

    function onBeforeInput(event) {
      if (!captured || composing || event.inputType === "insertCompositionText" || !event.data) return;
      event.preventDefault();
      queueText(event.data);
      keyboardInput.value = "";
    }

    function onPaste(event) {
      if (!captured) return;
      const text = event.clipboardData?.getData?.("text/plain") || "";
      if (!text) return;
      event.preventDefault();
      queueText(text);
    }

    function onCompositionEnd(event) {
      composing = false;
      queueText(event.data || keyboardInput.value);
      keyboardInput.value = "";
    }

    function onVisibilityChange() {
      if (runtime.document.hidden) {
        stopFrameTicker();
        void exitCapture();
      } else if (viewActive && owned) {
        startFrameTicker();
      }
    }

    function activate() {
      if (disposed || viewActive) return;
      viewActive = true;
      updateUi();
      void refreshStatus();
    }

    function deactivate() {
      if (!viewActive) return;
      viewActive = false;
      clearStatusRefresh();
      stopFrameTicker();
      if (owned) void stopControl(true);
      else void exitCapture();
    }

    function authenticationLost() {
      viewActive = false;
      clearStatusRefresh();
      loseOwnership();
    }

    function dispose() {
      if (disposed) return;
      deactivate();
      disposed = true;
      clearKeepalive();
      clearStatusRefresh();
      if (inputTimer != null) runtime.clearTimeout(inputTimer);
      inputTimer = null;
      revokeImageUrl(imageUrl);
      imageUrl = null;
      startButton.removeEventListener("click", startControl);
      stopButton.removeEventListener("click", onStopClick);
      screenshotButton.removeEventListener("click", screenshot);
      fpsSelect.removeEventListener("change", updateSettings);
      scaleSelect.removeEventListener("change", updateSettings);
      image.removeEventListener("pointermove", onPointerMove);
      image.removeEventListener("pointerdown", onPointerDown);
      image.removeEventListener("pointerup", onPointerUp);
      image.removeEventListener("pointercancel", exitCapture);
      image.removeEventListener("wheel", onWheel);
      image.removeEventListener("contextmenu", preventContextMenu);
      keyboardInput.removeEventListener("keydown", onKeyDown);
      keyboardInput.removeEventListener("keyup", onKeyUp);
      keyboardInput.removeEventListener("beforeinput", onBeforeInput);
      keyboardInput.removeEventListener("paste", onPaste);
      keyboardInput.removeEventListener("compositionstart", onCompositionStart);
      keyboardInput.removeEventListener("compositionend", onCompositionEnd);
      runtime.removeEventListener?.("blur", onWindowBlur);
      runtime.document?.removeEventListener?.("visibilitychange", onVisibilityChange);
      updateUi();
    }

    function onStopClick() {
      void stopControl();
    }

    function preventContextMenu(event) {
      if (owned) event.preventDefault();
    }

    function onCompositionStart() {
      composing = true;
    }

    function onWindowBlur() {
      void exitCapture();
    }

    startButton.addEventListener("click", startControl);
    stopButton.addEventListener("click", onStopClick);
    screenshotButton.addEventListener("click", screenshot);
    fpsSelect.addEventListener("change", updateSettings);
    scaleSelect.addEventListener("change", updateSettings);
    image.addEventListener("pointermove", onPointerMove);
    image.addEventListener("pointerdown", onPointerDown);
    image.addEventListener("pointerup", onPointerUp);
    image.addEventListener("pointercancel", exitCapture);
    image.addEventListener("wheel", onWheel, { passive: false });
    image.addEventListener("contextmenu", preventContextMenu);
    keyboardInput.addEventListener("keydown", onKeyDown);
    keyboardInput.addEventListener("keyup", onKeyUp);
    keyboardInput.addEventListener("beforeinput", onBeforeInput);
    keyboardInput.addEventListener("paste", onPaste);
    keyboardInput.addEventListener("compositionstart", onCompositionStart);
    keyboardInput.addEventListener("compositionend", onCompositionEnd);
    runtime.addEventListener?.("blur", onWindowBlur);
    runtime.document?.addEventListener?.("visibilitychange", onVisibilityChange);
    updateUi();

    return Object.freeze({
      activate,
      deactivate,
      authenticationLost,
      dispose,
      refreshStatus,
      release: exitCapture,
      snapshot: () => Object.freeze({
        supported,
        active: serverActive,
        owned,
        captured,
        fps,
        scale,
        frameCount,
        lastSequence,
        pendingInput: pendingInput.length,
      }),
    });
  }

  return Object.freeze({
    FPS_OPTIONS,
    SCALE_OPTIONS,
    DEFAULT_FPS,
    DEFAULT_SCALE,
    isExitShortcut,
    mapPointerToScreen,
    normalizeWheelDelta,
    readFrameMetadata,
    create,
  });
}));
