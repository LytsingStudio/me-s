"use strict";
const { test, expect } = require("bun:test");
const { readFileSync } = require("node:fs");
const app = readFileSync(new URL("../src/webui/app.js", import.meta.url), "utf8");
const files = readFileSync(new URL("../src/webui/file-manager.js", import.meta.url), "utf8");
const css = readFileSync(new URL("../src/webui/style.css", import.meta.url), "utf8");
function fn(name) {
  const start = app.search(new RegExp(`^(?:async )?function ${name}\\(`, "m"));
  if (start < 0) throw new Error(name);
  const rest = app.slice(start + 1);
  const end = rest.search(/\n(?:async )?function /);
  return app.slice(start, end < 0 ? undefined : start + 1 + end);
}
const uploadGlobal = {};
new Function("globalThis", files)(uploadGlobal);

function harness({ failUpload = false, unknown = false, known = false } = {}) {
  const one = new File([new Uint8Array([1, 2, 3])], "one.png", { type: "image/png", lastModified: 42 });
  const two = new File([new Uint8Array([4, 5])], "two.jpg", { type: "image/jpeg", lastModified: 43 });
  const images = [one, two].map((file, id) => ({ id, file, url: `blob:${id}`, image: null }));
  const store = { promptImages: images, promptSubmissionRevision: 0, inputDraftRevision: 0, projection: { messages: [] } };
  const state = { selectedAgent: "agent-12345678", workspaceId: "work", stores: new Map([["agent-12345678", store]]), drafts: new Map(), draftSync: new Map() };
  const elements = { input: { value: "caption", focus() {} } };
  const calls = [], commands = [], revoked = [];
  let sequence = 0;
  const api = async (path, options, workspace) => {
    const body = JSON.parse(options.body); calls.push({ path, body, workspace });
    if (path.endsWith("/create")) return { upload: { upload_id: `upload-${++sequence}` } };
    if (path.endsWith("/chunk") && failUpload) throw new Error("offline");
    if (path.endsWith("/finish")) return { state: "completed", image: { path: `/host/${body.upload_id}.png` } };
    return {};
  };
  const sendCommand = async (command, workspace) => {
    commands.push({ command, workspace });
    if (unknown || known) { const error = new Error("offline"); error.commandResultKnown = known; throw error; }
    return { receipt: { prompt_submission_revision: 1, input_draft_revision: 2 } };
  };
  const factory = new Function("state", "elements", "currentStore", "agentMeta", "renderComposer", "pauseDraftSyncForSubmission", "api", "sendCommand", "globalThis", "gatewayWorkspaceState", "autoSizeInput", "requestAnimationFrame", "toast", "URL", `
    let promptImageSequence = 2;
    ${fn("addPromptImages")}
    ${fn("promptSubmissionBoundary")}
    ${fn("commandResultIsUnknown")}
    ${fn("cancelPendingPromptSubmission")}
    ${fn("pendingPromptReachedProjection")}
    ${fn("clearPromptImages")}
    ${fn("submitPrompt")}
    return { submitPrompt, pendingPromptReachedProjection, clearPromptImages, addPromptImages };
  `);
  const runtime = factory(state, elements, () => state.stores.get(state.selectedAgent), () => ({ kind: "interactive" }), () => {}, async () => {}, api, sendCommand, uploadGlobal, () => state, () => {}, (fn) => fn(), () => {}, { revokeObjectURL: (url) => revoked.push(url), createObjectURL: (file) => `blob:${file.name}` });
  return { ...runtime, state, store, elements, calls, commands, images, revoked };
}

test("attachments upload through shared chunks before the one ordered message command", async () => {
  const h = harness(); await h.submitPrompt();
  expect(h.commands).toHaveLength(1);
  expect(h.commands[0]).toEqual({ workspace: "work", command: { command: "submit_user_prompt", agent_id: "agent-12345678", content: "caption", image_paths: ["/host/upload-1.png", "/host/upload-2.png"] } });
  expect(h.calls.map((call) => call.path)).toEqual(["/api/files/paste-images/create", "/api/files/uploads/chunk", "/api/files/uploads/finish", "/api/files/paste-images/create", "/api/files/uploads/chunk", "/api/files/uploads/finish"]);
  expect(h.calls[0].body.original).toEqual({ name: "one.png", mime_type: "image/png", last_modified_ms: 42 });
  expect(h.store.promptImages).toBe(h.images);
  expect(h.elements.input.value).toBe("caption");
  expect(h.store.pendingPromptSubmission.status).toBe("accepted");
  await h.submitPrompt(); expect(h.commands).toHaveLength(1);
});

test("an upload failure cancels its upload and retains text and images without submitting", async () => {
  const h = harness({ failUpload: true }); await h.submitPrompt();
  expect(h.commands).toHaveLength(0);
  expect(h.calls.at(-1).path).toBe("/api/files/uploads/cancel");
  expect(h.store.pendingPromptSubmission).toBeNull();
  expect(h.store.promptImages).toHaveLength(2);
  expect(h.elements.input.value).toBe("caption");
});

test("unknown command results stay locked and are never blindly resubmitted", async () => {
  const h = harness({ unknown: true }); await h.submitPrompt();
  expect(h.store.pendingPromptSubmission.status).toBe("confirming");
  await h.submitPrompt(); expect(h.commands).toHaveLength(1);
  expect(h.store.promptImages).toHaveLength(2);
});

test("known command failures restore the draft and reuse already completed uploads", async () => {
  const h = harness({ known: true }); await h.submitPrompt();
  expect(h.store.pendingPromptSubmission).toBeNull();
  const count = h.calls.length; await h.submitPrompt();
  expect(h.calls).toHaveLength(count); expect(h.commands).toHaveLength(2);
  expect(h.store.promptImages).toHaveLength(2);
});

test("image-only confirmation compares ordered paths, not merely the empty text", async () => {
  const h = harness(); h.elements.input.value = ""; await h.submitPrompt();
  const pending = h.store.pendingPromptSubmission;
  h.store.projection.messages = [{ kind: "user", key: "user:2", eventId: 2, content: "", images: [{ path: pending.imagePaths[1] }, { path: pending.imagePaths[0] }] }];
  expect(h.pendingPromptReachedProjection(h.store)).toBe(false);
  h.store.projection.messages[0].images.reverse(); expect(h.pendingPromptReachedProjection(h.store)).toBe(true);
  pending.imagesReady = false; expect(h.pendingPromptReachedProjection(h.store)).toBe(false);
});

test("cleanup releases local previews and cancels a not-yet-submitted attachment send", () => {
  const h = harness(); h.store.pendingPromptSubmission = {};
  h.clearPromptImages(h.store); expect(h.revoked).toEqual(["blob:0", "blob:1"]);
  expect(h.store.promptImages).toHaveLength(0); expect(h.store.pendingPromptSubmission.cancelled).toBe(true);
});

test("the shared transfer keeps bounded chunks and calls progress without opening a file task", async () => {
  const size = 384 * 1024 + 2; const calls = [], progress = [];
  const result = await uploadGlobal.MeFileManager.uploadContent(async (path, body) => { calls.push([path, body]); return { state: "completed" }; }, new File([new Uint8Array(size)], "file.bin"), { upload_id: "one" }, { onProgress: (done) => progress.push(done) });
  expect(progress).toEqual([384 * 1024, size]); expect(calls[1][1].offset).toBe(384 * 1024);
  expect(result.state).toBe("completed");
});

test("user galleries sit outside the text bubble and touch removal is explicitly two-step", () => {
  expect(app).toContain('</button></div>${promptImageGalleryHtml(message)}</div>');
  expect(app).toContain('if (!event.target.closest(".prompt-image-remove")) { tile.classList.toggle("selected"); return; }');
  expect(css).toContain('@media (hover: hover)');
  expect(css).toContain('.prompt-image.selected .prompt-image-remove');
  expect(app).toContain('if (existing?.dataset.items === encoded) return;');
});

test("attachment drafts are separate for each session and workspace", () => {
  const h = harness();
  const first = h.store.promptImages;
  h.state.stores.set("second", {}); h.state.selectedAgent = "second";
  h.addPromptImages([new File(["x"], "second.png", { type: "image/png" })]);
  expect(h.state.stores.get("second").promptImages.map((item) => item.file.name)).toEqual(["second.png"]);
  expect(h.store.promptImages).toBe(first);
  h.state.stores = new Map([["second", {}]]); h.state.workspaceId = "other";
  h.addPromptImages([new File(["x"], "other.png", { type: "image/png" })]);
  expect(h.state.stores.get("second").promptImages[0].file.name).toBe("other.png");
  expect(first.map((item) => item.file.name)).toEqual(["one.png", "two.jpg"]);
});

test("user gallery estimates reserve the correct number of rows before image loading", () => {
  const estimate = new Function("messageIsVisible", "elements", `${fn("estimateTranscriptMessageHeight")}\nreturn estimateTranscriptMessageHeight;`)(() => true, {});
  expect(estimate({ kind: "user", images: [{}, {}] }, 0, 742)).toBe(72 + 10 + 243);
  expect(estimate({ kind: "user", images: [{}, {}] }, 0, 360)).toBe(72 + 10 + 243 * 2 + 12);
  expect(estimate({ kind: "user" }, 0, 360)).toBe(72);
  const config = JSON.parse(readFileSync(new URL("../me-client/src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  expect(config.app.windows[0].dragDropEnabled).toBe(false);
});
