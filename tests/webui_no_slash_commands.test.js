"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

const root = join(import.meta.dir, "..");
const read = (path) => readFileSync(join(root, path), "utf8");

const webuis = [
  {
    name: "direct",
    app: read("src/webui/app.js"),
    html: read("src/webui/index.html"),
    style: read("src/webui/style.css"),
  },
  {
    name: "gateway",
    app: read("src/gateway_webui/app.js"),
    html: read("src/gateway_webui/index.html"),
    style: read("src/gateway_webui/style.css"),
  },
];

describe("WebUI without browser slash commands", () => {
  test("both WebUIs omit the slash command menu, state, dispatcher and styles", () => {
    for (const { app, html, style } of webuis) {
      for (const removed of [
        "const COMMANDS",
        "slashIndex",
        "slashMenu",
        "renderSlashMenu",
        "openSlashCommand",
        'content.startsWith("/")',
      ]) {
        expect(app).not.toContain(removed);
      }
      expect(html).not.toContain('id="slash-menu"');
      expect(html).not.toContain("输入 / 查看命令");
      expect(html).toContain('id="prompt-input" rows="1" placeholder="发送消息"');
      expect(style).not.toContain(".slash-menu");
      expect(style).not.toContain(".slash-item");
    }
  });

  test("slash-prefixed text keeps the ordinary prompt path and visual controls remain", () => {
    for (const { app } of webuis) {
      for (const retained of [
        "async function submitPrompt()",
        'command: "submit_user_prompt"',
        "function enterSubmitsPrompt(event)",
        "async function escapeAction()",
        "function openModelDrawer()",
        "function openEffortDrawer()",
        "function openAddAgent()",
        "async function openDeleteAgent",
        'command: "clear_context"',
      ]) {
        expect(app).toContain(retained);
      }

      const keydown = app.match(/elements\.input\.addEventListener\("keydown",[\s\S]*?\n\}\);/);
      expect(keydown).not.toBeNull();
      expect(keydown[0]).toContain("state.composing || event.isComposing || event.keyCode === 229");
      expect(keydown[0]).toContain("enterSubmitsPrompt(event)");
      expect(keydown[0]).toContain('event.key === "Escape"');
      expect(keydown[0]).not.toContain("ArrowUp");
      expect(keydown[0]).not.toContain("ArrowDown");
    }
  });
});
