"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

require("../src/webui/edb-cache.js");

function loadDraftRuntime(options = {}) {
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI draft runtime");
  const factory = new Function(
    "document", "performance", "matchMedia", "navigator", "fetch", "Blob", "requestAnimationFrame",
    "setTimeout", "clearTimeout",
    `${source.slice(0, eventBindings)}
    return {
      state, elements, observeInputDraft, saveDraft, beginInputComposition, endInputComposition,
      projectChat, pendingPromptReachedProjection, promptSubmissionBoundary,
      commandResultIsUnknown, cancelPendingPromptSubmission, finishPendingPromptSubmission, sendCommand,
      restoreDraft, flushDraftBeforePageCloses, queueDraftUpdate, runDraftSync, pauseDraftSyncForSubmission,
      autoSizeInput,
    };`);
  let inputHeight = "";
  let mirrorHeight = "";
  let inputScrollHeight = 0;
  let mirrorScrollHeight = 0;
  let realScrollHeightReads = 0;
  let mirrorScrollHeightReads = 0;
  const realHeightWrites = [];
  const mirrorHeightWrites = [];
  const input = {
    value: "",
    get scrollHeight() { realScrollHeightReads += 1; return inputScrollHeight; },
    set scrollHeight(value) { inputScrollHeight = Number(value); },
    style: {
      get height() { return inputHeight; },
      set height(value) { inputHeight = String(value); realHeightWrites.push(inputHeight); },
    },
  };
  const inputMirror = {
    value: "",
    get scrollHeight() { mirrorScrollHeightReads += 1; return mirrorScrollHeight; },
    set scrollHeight(value) { mirrorScrollHeight = Number(value); },
    style: {
      get height() { return mirrorHeight; },
      set height(value) { mirrorHeight = String(value); mirrorHeightWrites.push(mirrorHeight); },
    },
  };
  const pageCloseCalls = [];
  const fetchCalls = [];
  const timers = new Map();
  const animationFrames = new Map();
  let nextTimerId = 1;
  let nextAnimationFrameId = 1;
  const setTimeoutFake = (callback, delay) => {
    const id = nextTimerId++;
    timers.set(id, { callback, delay });
    return id;
  };
  const clearTimeoutFake = (id) => timers.delete(id);
  const requestAnimationFrameFake = (callback) => {
    const id = nextAnimationFrameId++;
    animationFrames.set(id, callback);
    return id;
  };
  const fetchFake = (...args) => {
    pageCloseCalls.push(["fetch", ...args]);
    fetchCalls.push(args);
    if (options.fetch) return options.fetch(...args);
    return Promise.resolve({ ok: true, status: 200, json: async () => ({ ok: true }) });
  };
  const runtime = factory(
    {
      querySelector: (selector) => selector === "#prompt-input" ? input
        : selector === "#prompt-input-mirror" ? inputMirror : null,
      cookie: "",
    },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
    { sendBeacon: (...args) => { pageCloseCalls.push(["beacon", ...args]); return true; } },
    fetchFake,
    undefined,
    requestAnimationFrameFake,
    setTimeoutFake,
    clearTimeoutFake,
  );
  runtime.pageCloseCalls = pageCloseCalls;
  runtime.fetchCalls = fetchCalls;
  runtime.heightWrites = realHeightWrites;
  runtime.mirrorHeightWrites = mirrorHeightWrites;
  runtime.realScrollHeightReads = () => realScrollHeightReads;
  runtime.mirrorScrollHeightReads = () => mirrorScrollHeightReads;
  runtime.pendingTimers = () => [...timers.values()];
  runtime.pendingAnimationFrames = () => animationFrames.size;
  runtime.runNextTimer = () => {
    const next = timers.entries().next();
    if (next.done) return null;
    const [id, entry] = next.value;
    timers.delete(id);
    entry.callback();
    return entry.delay;
  };
  runtime.runNextAnimationFrame = () => {
    const next = animationFrames.entries().next();
    if (next.done) return false;
    const [id, callback] = next.value;
    animationFrames.delete(id);
    callback();
    return true;
  };
  runtime.clearHeightWrites = () => {
    realHeightWrites.length = 0;
    mirrorHeightWrites.length = 0;
  };
  return runtime;
}

function jsonResponse(payload) {
  return { ok: true, status: 200, json: async () => payload };
}

async function flushMicrotasks(turns = 12) {
  for (let index = 0; index < turns; index += 1) await Promise.resolve();
}

function event(kind, id, value = {}) {
  return { [kind]: { id, timestamp_ms: id * 10, ...value } };
}

describe("WebUI authoritative input draft synchronization", () => {
  test("a newer Runtime draft supersedes an in-flight local write", () => {
    const { state, observeInputDraft } = loadDraftRuntime();
    const store = { inputDraftRevision: 10 };
    state.draftSync.set("main", {
      desired: "",
      sent: "old local value",
      sending: true,
      paused: false,
      inFlight: { expectedRevision: 10, content: "older local write" },
      pendingRemote: null,
      waiters: [],
    });

    expect(observeInputDraft({
      id: "main",
      input_draft_revision: 11,
      input_draft: "撤回后恢复的消息",
    }, store)).toBe(true);
    expect(store.inputDraftRevision).toBe(11);
    expect(state.drafts.get("main")).toBe("撤回后恢复的消息");
    expect(state.draftSync.get("main").desired).toBe("撤回后恢复的消息");
    expect(state.draftSync.get("main").sent).toBe("撤回后恢复的消息");
  });

  test("the Runtime echo of an in-flight write never replaces newer local input", () => {
    const { state, elements, observeInputDraft } = loadDraftRuntime();
    const store = { inputDraftRevision: 10 };
    state.selectedAgent = "main";
    state.drafts.set("main", "abcdef");
    elements.input.value = "abcdef";
    state.draftSync.set("main", {
      desired: "abcdef",
      sent: "",
      sending: true,
      paused: false,
      inFlight: { expectedRevision: 10, content: "abc" },
      pendingRemote: null,
      waiters: [],
    });

    expect(observeInputDraft({
      id: "main",
      input_draft_revision: 11,
      input_draft: "abc",
    }, store)).toBe(false);
    expect(store.inputDraftRevision).toBe(11);
    expect(elements.input.value).toBe("abcdef");
    expect(state.drafts.get("main")).toBe("abcdef");
    expect(state.draftSync.get("main").desired).toBe("abcdef");
    expect(state.draftSync.get("main").sent).toBe("abc");
  });

  test("IME composition is kept intact while a remote draft revision arrives", () => {
    const runtime = loadDraftRuntime();
    const { state, elements, observeInputDraft, saveDraft,
      beginInputComposition, endInputComposition } = runtime;
    const store = { inputDraftRevision: 5 };
    state.selectedAgent = "main";
    state.stores.set("main", store);
    state.drafts.set("main", "旧文本");
    elements.input.value = "旧文本";

    beginInputComposition();
    elements.input.value = "完整中文";
    saveDraft();
    expect(observeInputDraft({
      id: "main",
      input_draft_revision: 6,
      input_draft: "另一个页面的文本",
    }, store)).toBe(false);
    expect(elements.input.value).toBe("完整中文");
    expect(store.inputDraftRevision).toBe(5);

    state.draftSync.get("main").paused = true;
    endInputComposition();
    expect(store.inputDraftRevision).toBe(6);
    expect(elements.input.value).toBe("完整中文");
    expect(state.drafts.get("main")).toBe("完整中文");
    expect(state.draftSync.get("main").sent).toBe("另一个页面的文本");
    expect(state.draftSync.get("main").desired).toBe("完整中文");
    expect(state.draftSync.get("main").pendingRemote).toBe(null);
  });

  test("submission pause preserves text typed while the prompt is being accepted", () => {
    const { state, observeInputDraft } = loadDraftRuntime();
    const store = { inputDraftRevision: 4 };
    state.draftSync.set("main", {
      desired: "next message",
      sent: "",
      sending: false,
      paused: true,
      inFlight: null,
      pendingRemote: null,
      waiters: [],
    });

    expect(observeInputDraft({
      id: "main",
      input_draft_revision: 5,
      input_draft: "",
    }, store)).toBe(false);
    expect(store.inputDraftRevision).toBe(4);
    expect(state.draftSync.get("main").desired).toBe("next message");
  });

  test("an unacknowledged local draft survives reconnect state recovery", () => {
    const { state, elements, observeInputDraft } = loadDraftRuntime();
    const store = { inputDraftRevision: 8 };
    state.selectedAgent = "main";
    state.drafts.set("main", "本地尚未确认的输入");
    elements.input.value = "本地尚未确认的输入";
    state.draftSync.set("main", {
      desired: "本地尚未确认的输入",
      sent: "旧服务端草稿",
      sending: false,
      paused: false,
      inFlight: null,
      pendingRemote: null,
      waiters: [],
    });

    expect(observeInputDraft({
      id: "main",
      input_draft_revision: 9,
      input_draft: "另一页面写入的草稿",
    }, store)).toBe(false);
    expect(store.inputDraftRevision).toBe(9);
    expect(elements.input.value).toBe("本地尚未确认的输入");
    expect(state.draftSync.get("main").desired).toBe("本地尚未确认的输入");
    expect(state.draftSync.get("main").sent).toBe("另一页面写入的草稿");
  });

  test("a pending submission keeps its displayed text despite Runtime draft changes", () => {
    const { state, elements, observeInputDraft } = loadDraftRuntime();
    const pending = { content: "已提交消息", displayContent: "  已提交消息  ", afterEventId: 10 };
    const store = { inputDraftRevision: 4, pendingPromptSubmission: pending };
    state.selectedAgent = "main";
    state.drafts.set("main", pending.displayContent);
    elements.input.value = pending.displayContent;

    expect(observeInputDraft({
      id: "main", input_draft_revision: 5, input_draft: "",
    }, store)).toBe(false);
    expect(store.inputDraftRevision).toBe(4);
    expect(state.drafts.get("main")).toBe(pending.displayContent);
    expect(elements.input.value).toBe(pending.displayContent);
  });

  test("only a matching authoritative UserPrompt or FollowUpPrompt after the boundary confirms pending", () => {
    const { projectChat, pendingPromptReachedProjection } = loadDraftRuntime();
    const pending = { content: "相同正文", displayContent: "相同正文", afterEventId: 10 };
    const reached = (events) => pendingPromptReachedProjection({
      pendingPromptSubmission: pending,
      projection: projectChat(events),
    });

    expect(reached([event("UserPrompt", 10, { content: "相同正文" })])).toBe(false);
    expect(reached([event("UserPrompt", 11, { content: "其他正文" })])).toBe(false);
    expect(reached([event("ManagerPrompt", 11, { content: "相同正文" })])).toBe(false);
    expect(reached([event("UserPrompt", 11, { content: "相同正文" })])).toBe(true);
    expect(reached([event("FollowUpPrompt", 12, { content: "相同正文" })])).toBe(true);
  });

  test("submission boundary prefers the authoritative snapshot and safely falls back to local events", () => {
    const { promptSubmissionBoundary } = loadDraftRuntime();
    const store = { events: [event("AssistResponse", 7, { content: "done" })] };
    expect(promptSubmissionBoundary({ last_event_id: 9 }, store)).toBe(9);
    expect(promptSubmissionBoundary({}, store)).toBe(7);
    expect(promptSubmissionBoundary({}, { events: [] })).toBe(-1);
  });

  test("authoritative completion adopts the latest Runtime draft and resumes synchronization", () => {
    const { state, finishPendingPromptSubmission } = loadDraftRuntime();
    const pending = {
      content: "已提交消息", displayContent: "已提交消息", afterEventId: 10, settled: false,
    };
    const store = { inputDraftRevision: 4, pendingPromptSubmission: pending };
    const sync = { desired: "", sent: "", paused: true, pendingRemote: null };
    state.stores.set("main", store);
    state.draftSync.set("main", sync);
    state.snapshot.agents = [{ id: "main", input_draft_revision: 6, input_draft: "下一条草稿" }];

    expect(finishPendingPromptSubmission("main")).toBe(true);
    expect(pending.settled).toBe(true);
    expect(store.pendingPromptSubmission).toBe(null);
    expect(store.inputDraftRevision).toBe(6);
    expect(state.drafts.get("main")).toBe("下一条草稿");
    expect(sync.paused).toBe(false);
    expect(sync.desired).toBe("下一条草稿");
    expect(sync.sent).toBe("下一条草稿");
  });

  test("a deterministic failure restores the submitted text and draft synchronization", () => {
    const { state, cancelPendingPromptSubmission } = loadDraftRuntime();
    const pending = {
      content: "已提交消息", displayContent: "  已提交消息  ", afterEventId: 10, settled: false,
    };
    const store = { inputDraftRevision: 4, pendingPromptSubmission: pending };
    const sync = { desired: "", sent: pending.displayContent, paused: true };
    state.stores.set("main", store);
    state.draftSync.set("main", sync);

    expect(cancelPendingPromptSubmission("main", pending)).toBe(true);
    expect(pending.settled).toBe(true);
    expect(store.pendingPromptSubmission).toBe(null);
    expect(state.drafts.get("main")).toBe(pending.displayContent);
    expect(sync.paused).toBe(false);
    expect(sync.desired).toBe(pending.displayContent);
  });

  test("pending display and page-close suppression stay isolated by Agent", () => {
    const runtime = loadDraftRuntime();
    const { state, elements, restoreDraft, flushDraftBeforePageCloses, pageCloseCalls } = runtime;
    const pending = { content: "主会话消息", displayContent: "  主会话消息  ", afterEventId: 10 };
    state.stores.set("main", { pendingPromptSubmission: pending, inputDraftRevision: 4 });
    state.stores.set("other", { pendingPromptSubmission: null, inputDraftRevision: 2 });
    state.drafts.set("main", "不会显示的旧草稿");
    state.drafts.set("other", "其他会话草稿");
    state.draftSync.set("main", { desired: pending.displayContent, sent: "", paused: true });
    state.draftSync.set("other", { desired: "其他会话草稿", sent: "旧草稿", paused: false });

    state.selectedAgent = "main";
    restoreDraft();
    expect(elements.input.value).toBe(pending.displayContent);

    state.selectedAgent = "other";
    restoreDraft();
    expect(elements.input.value).toBe("其他会话草稿");
    expect(state.stores.get("main").pendingPromptSubmission).toBe(pending);

    state.selectedAgent = "main";
    flushDraftBeforePageCloses();
    expect(pageCloseCalls).toHaveLength(1);
    expect(pageCloseCalls[0][0]).toBe("fetch");
    expect(JSON.parse(pageCloseCalls[0][2].body)).toEqual({
      command: "update_input_draft",
      agent_id: "other",
      expected_revision: 2,
      content: "其他会话草稿",
    });
  });

  test("known local failures unlock while ambiguous HTTP outcomes keep waiting for authority", async () => {
    const { state, sendCommand, commandResultIsUnknown } = loadDraftRuntime();
    state.connected = false;
    let disconnected;
    try {
      await sendCommand({ command: "submit_user_prompt", agent_id: "main", content: "hello" });
    } catch (error) {
      disconnected = error;
    }
    expect(disconnected?.commandResultKnown).toBe(true);
    expect(commandResultIsUnknown(disconnected)).toBe(false);

    const unavailable = new Error("service unavailable");
    unavailable.status = 503;
    expect(commandResultIsUnknown(unavailable)).toBe(true);
    expect(commandResultIsUnknown(new Error("response lost"))).toBe(true);
    const invalidReceipt = new Error("invalid receipt");
    invalidReceipt.commandResultKnown = true;
    expect(commandResultIsUnknown(invalidReceipt)).toBe(false);
  });

  test("batches rapid local input into one accepted latest-value command without an extra sync", async () => {
    const runtime = loadDraftRuntime({
      fetch: () => Promise.resolve(jsonResponse({
        ok: true, receipt: { accepted: true, input_draft_revision: 1 },
      })),
    });
    runtime.state.connected = true;
    runtime.state.connectionPhase = "connected";
    runtime.state.stores.set("main", { inputDraftRevision: 0 });
    runtime.queueDraftUpdate("main", "a");
    runtime.queueDraftUpdate("main", "ab");
    runtime.queueDraftUpdate("main", "abc");

    expect(runtime.pendingTimers().map(({ delay }) => delay)).toEqual([80]);
    expect(runtime.fetchCalls).toHaveLength(0);
    expect(runtime.runNextTimer()).toBe(80);
    await flushMicrotasks();

    const commandCalls = runtime.fetchCalls.filter(([path]) => path === "/api/command");
    expect(commandCalls).toHaveLength(1);
    expect(JSON.parse(commandCalls[0][1].body)).toEqual({
      command: "update_input_draft", agent_id: "main", expected_revision: 0, content: "abc",
    });
    expect(runtime.fetchCalls.some(([path]) => path === "/api/sync")).toBe(false);
    expect(runtime.state.stores.get("main").inputDraftRevision).toBe(1);
  });

  test("keeps one draft request in flight and sends only the newest pending body", async () => {
    let releaseFirst;
    let commandCount = 0;
    const runtime = loadDraftRuntime({
      fetch: (path) => {
        if (path !== "/api/command") throw new Error(`unexpected request: ${path}`);
        commandCount += 1;
        if (commandCount === 1) {
          return new Promise((resolve) => {
            releaseFirst = () => resolve(jsonResponse({
              ok: true, receipt: { accepted: true, input_draft_revision: 1 },
            }));
          });
        }
        return Promise.resolve(jsonResponse({
          ok: true, receipt: { accepted: true, input_draft_revision: 2 },
        }));
      },
    });
    runtime.state.connected = true;
    runtime.state.connectionPhase = "connected";
    runtime.state.stores.set("main", { inputDraftRevision: 0 });
    runtime.queueDraftUpdate("main", "a");
    runtime.runNextTimer();
    expect(commandCount).toBe(1);

    runtime.queueDraftUpdate("main", "ab");
    runtime.queueDraftUpdate("main", "abcdef");
    expect(runtime.pendingTimers()).toHaveLength(0);
    expect(commandCount).toBe(1);
    releaseFirst();
    await flushMicrotasks();

    const bodies = runtime.fetchCalls.map(([, options]) => JSON.parse(options.body));
    expect(bodies.map(({ content }) => content)).toEqual(["a", "abcdef"]);
    expect(runtime.state.stores.get("main").inputDraftRevision).toBe(2);
    expect(runtime.state.draftSync.get("main").sending).toBe(false);
  });

  test("requests authority immediately when a draft revision is rejected", async () => {
    const runtime = loadDraftRuntime({
      fetch: (path) => {
        if (path === "/api/command") {
          return Promise.resolve(jsonResponse({
            ok: true, receipt: { accepted: false, input_draft_revision: 4 },
          }));
        }
        if (path === "/api/sync") return new Promise(() => {});
        throw new Error(`unexpected request: ${path}`);
      },
    });
    runtime.state.connected = true;
    runtime.state.connectionPhase = "connected";
    runtime.state.stores.set("main", { inputDraftRevision: 4, events: [], mutationRevision: 0 });
    runtime.queueDraftUpdate("main", "stale body");
    runtime.runNextTimer();
    await flushMicrotasks();

    expect(runtime.fetchCalls.map(([path]) => path)).toEqual(["/api/command", "/api/sync"]);
    expect(runtime.state.draftSync.get("main").sent).toBe("stale body");
  });

  test("composition and submission pause clear deferred draft timers", async () => {
    const runtime = loadDraftRuntime();
    runtime.state.connected = true;
    runtime.state.selectedAgent = "main";
    runtime.state.stores.set("main", { inputDraftRevision: 0 });
    runtime.elements.input.value = "完整输入";
    runtime.queueDraftUpdate("main", "完整输入");
    expect(runtime.pendingTimers()).toHaveLength(1);

    runtime.beginInputComposition();
    expect(runtime.pendingTimers()).toHaveLength(0);
    runtime.endInputComposition();
    expect(runtime.pendingTimers()).toHaveLength(1);
    await runtime.pauseDraftSyncForSubmission("main");
    expect(runtime.pendingTimers()).toHaveLength(0);
    expect(runtime.state.draftSync.get("main").paused).toBe(true);
  });

  test("measures only the isolated mirror and commits only real height changes", () => {
    const runtime = loadDraftRuntime();
    runtime.elements.input.value = "short";
    runtime.elements.inputMirror.scrollHeight = 40;
    runtime.autoSizeInput();
    expect(runtime.pendingAnimationFrames()).toBe(1);
    runtime.runNextAnimationFrame();
    expect(runtime.elements.inputMirror.value).toBe("short");
    expect(runtime.realScrollHeightReads()).toBe(0);
    expect(runtime.mirrorScrollHeightReads()).toBe(1);
    expect(runtime.heightWrites).toEqual(["40px"]);
    expect(runtime.mirrorHeightWrites).toEqual(["0px"]);

    runtime.clearHeightWrites();
    runtime.autoSizeInput();
    runtime.runNextAnimationFrame();
    expect(runtime.realScrollHeightReads()).toBe(0);
    expect(runtime.mirrorScrollHeightReads()).toBe(2);
    expect(runtime.heightWrites).toEqual([]);
    expect(runtime.mirrorHeightWrites).toEqual(["0px"]);

    runtime.clearHeightWrites();
    runtime.elements.input.value = "a growing line";
    runtime.elements.inputMirror.scrollHeight = 72;
    runtime.autoSizeInput();
    runtime.runNextAnimationFrame();
    expect(runtime.heightWrites).toEqual(["72px"]);

    runtime.clearHeightWrites();
    runtime.elements.input.value = "";
    runtime.elements.inputMirror.scrollHeight = 28;
    runtime.autoSizeInput();
    runtime.runNextAnimationFrame();
    expect(runtime.heightWrites).toEqual(["29px"]);
    expect(runtime.elements.inputMirror.value).toBe("");
    expect(runtime.realScrollHeightReads()).toBe(0);
  });
});
