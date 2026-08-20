"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function loadDraftRuntime() {
  const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
  const eventBindings = source.indexOf("\nelements.tabs.querySelectorAll");
  if (eventBindings < 0) throw new Error("could not isolate WebUI draft runtime");
  const factory = new Function(
    "document", "performance", "matchMedia", "navigator", "fetch", "Blob", "requestAnimationFrame",
    `${source.slice(0, eventBindings)}
    return {
      state, elements, observeInputDraft, saveDraft, beginInputComposition, endInputComposition,
      projectChat, pendingPromptReachedProjection, promptSubmissionBoundary,
      commandResultIsUnknown, cancelPendingPromptSubmission, finishPendingPromptSubmission, sendCommand,
      restoreDraft, flushDraftBeforePageCloses,
    };`);
  const input = { value: "", style: {}, scrollHeight: 0 };
  const pageCloseCalls = [];
  const runtime = factory(
    { querySelector: (selector) => selector === "#prompt-input" ? input : null },
    { now: () => 0 },
    () => ({ matches: false, addEventListener: () => {} }),
    { sendBeacon: (...args) => { pageCloseCalls.push(["beacon", ...args]); return true; } },
    (...args) => { pageCloseCalls.push(["fetch", ...args]); return Promise.resolve(); },
    undefined,
    () => 1,
  );
  runtime.pageCloseCalls = pageCloseCalls;
  return runtime;
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
});
