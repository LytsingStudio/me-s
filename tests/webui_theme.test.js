"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const theme = require("../src/webui/theme.js");

function fakeRuntime({ stored = {}, systemLight = false } = {}) {
  const attributes = new Map();
  const metaAttributes = new Map();
  const values = new Map(Object.entries(stored));
  const documentElement = {
    style: {},
    setAttribute(name, value) { attributes.set(name, String(value)); },
    getAttribute(name) { return attributes.get(name) ?? null; },
  };
  const meta = {
    setAttribute(name, value) { metaAttributes.set(name, String(value)); },
  };
  const runtime = {
    document: {
      documentElement,
      querySelector(selector) { return selector === 'meta[name="color-scheme"]' ? meta : null; },
    },
    localStorage: {
      getItem(key) { return values.get(key) ?? null; },
      setItem(key, value) { values.set(key, String(value)); },
    },
    matchMedia(query) { return { matches: systemLight && query === "(prefers-color-scheme: light)" }; },
  };
  return { runtime, attributes, metaAttributes, values, documentElement };
}

function fakeButton() {
  const attributes = new Map();
  const listeners = new Map();
  return {
    setAttribute(name, value) { attributes.set(name, String(value)); },
    getAttribute(name) { return attributes.get(name) ?? null; },
    addEventListener(name, listener) { listeners.set(name, listener); },
    click() { listeners.get("click")?.(); },
  };
}

function cssDeclarations(source, selector) {
  const marker = `${selector} {`;
  const start = source.indexOf(marker);
  if (start < 0) throw new Error(`missing CSS block: ${selector}`);
  const end = source.indexOf("\n}", start);
  const declarations = {};
  for (const match of source.slice(start + marker.length, end).matchAll(/(--[a-z0-9-]+):\s*(#[0-9a-f]{6});/gi)) {
    declarations[match[1]] = match[2].toLowerCase();
  }
  return declarations;
}

function relativeLuminance(hex) {
  const channels = [1, 3, 5].map((offset) => Number.parseInt(hex.slice(offset, offset + 2), 16) / 255);
  const linear = channels.map((value) => value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4);
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function contrastRatio(first, second) {
  const values = [relativeLuminance(first), relativeLuminance(second)].sort((left, right) => right - left);
  return (values[0] + 0.05) / (values[1] + 0.05);
}

describe("shared WebUI themes", () => {
  test("defines nine stable, named themes", () => {
    expect(theme.THEMES).toEqual([
      { id: "violet", name: "紫曜" },
      { id: "graphite", name: "石墨" },
      { id: "ocean", name: "深海" },
      { id: "forest", name: "松林" },
      { id: "sand", name: "暖砂" },
      { id: "aurora", name: "极光" },
      { id: "sakura", name: "樱雾" },
      { id: "neon", name: "霓虹" },
      { id: "obsidian", name: "曜石黑" },
    ]);
  });

  test("uses the system mode only when no valid preference was saved", () => {
    const first = fakeRuntime({ systemLight: true });
    expect(theme.initialize(first.runtime)).toMatchObject({ theme: { id: "violet" }, mode: "light" });
    expect(first.attributes.get("data-theme")).toBe("violet");
    expect(first.attributes.get("data-mode")).toBe("light");
    expect(first.metaAttributes.get("content")).toBe("light");
    expect(first.documentElement.style.colorScheme).toBe("light");
    expect(first.values.size).toBe(0);

    const saved = fakeRuntime({ stored: { [theme.STORAGE_THEME]: "ocean", [theme.STORAGE_MODE]: "dark" }, systemLight: true });
    expect(theme.initialize(saved.runtime)).toMatchObject({ theme: { id: "ocean" }, mode: "dark" });
  });

  test("cycles themes in order while preserving and persisting the color mode", () => {
    const page = fakeRuntime({ stored: { [theme.STORAGE_THEME]: "violet", [theme.STORAGE_MODE]: "light" } });
    theme.initialize(page.runtime);
    const expectedOrder = ["graphite", "ocean", "forest", "sand", "aurora", "sakura", "neon", "obsidian", "violet"];
    for (const id of expectedOrder) expect(theme.cycle(page.runtime).theme.id).toBe(id);
    expect(page.values.get(theme.STORAGE_THEME)).toBe("violet");
    expect(page.values.get(theme.STORAGE_MODE)).toBe("light");
  });

  test("toggles only the color mode and exposes target-oriented accessible labels", () => {
    const page = fakeRuntime({ stored: { [theme.STORAGE_THEME]: "violet", [theme.STORAGE_MODE]: "dark" } });
    theme.initialize(page.runtime);
    const themeButton = fakeButton();
    const modeButton = fakeButton();
    const announcements = [];
    theme.bindControls(themeButton, modeButton, (message) => announcements.push(message), page.runtime);

    expect(themeButton.getAttribute("title")).toBe("切换主题：石墨");
    expect(themeButton.getAttribute("aria-label")).toContain("当前：紫曜");
    expect(modeButton.getAttribute("title")).toBe("切换到浅色模式");
    themeButton.click();
    expect(page.attributes.get("data-theme")).toBe("graphite");
    expect(page.attributes.get("data-mode")).toBe("dark");
    expect(announcements.at(-1)).toBe("已切换至「石墨」主题");
    modeButton.click();
    expect(page.attributes.get("data-theme")).toBe("graphite");
    expect(page.attributes.get("data-mode")).toBe("light");
    expect(modeButton.getAttribute("title")).toBe("切换到深色模式");
    expect(announcements.at(-1)).toBe("已切换至浅色模式");
  });

  test("loads the shared no-flash runtime and all eighteen palettes in both WebUIs", () => {
    const themeStyles = readFileSync(join(import.meta.dir, "../src/webui/theme.css"), "utf8");
    const singleIndex = readFileSync(join(import.meta.dir, "../src/webui/index.html"), "utf8");
    const gatewayIndex = readFileSync(join(import.meta.dir, "../src/gateway_webui/index.html"), "utf8");
    const singleStyles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const gatewayStyles = readFileSync(join(import.meta.dir, "../src/gateway_webui/style.css"), "utf8");
    const singleApp = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
    const gatewayApp = readFileSync(join(import.meta.dir, "../src/gateway_webui/app.js"), "utf8");
    const singleServer = readFileSync(join(import.meta.dir, "../src/webui.rs"), "utf8");
    const gatewayServer = readFileSync(join(import.meta.dir, "../src/gateway_webui.rs"), "utf8");

    for (const index of [singleIndex, gatewayIndex]) {
      expect(index.indexOf('<script src="/theme.js"></script>')).toBeLessThan(index.indexOf('href="/style.css"'));
      expect(index.indexOf('href="/style.css"')).toBeLessThan(index.indexOf('href="/theme.css"'));
      expect(index).not.toContain('<header class="brand">');
      expect(index).not.toContain('id="connection-label"');
      const sidebarFooterStart = index.indexOf('<footer class="sidebar-footer">');
      const sidebarFooter = index.slice(sidebarFooterStart, index.indexOf("</footer>", sidebarFooterStart));
      expect(sidebarFooter).toContain('class="sidebar-appearance" role="group" aria-label="外观设置"');
      expect(sidebarFooter).toContain('id="theme-cycle" class="theme-control theme-cycle"');
      expect(sidebarFooter).toContain('class="theme-icon theme-icon-shirt"');
      expect(sidebarFooter).toContain('id="theme-mode" class="theme-control theme-mode"');
      expect(sidebarFooter.indexOf('id="theme-cycle"')).toBeLessThan(sidebarFooter.indexOf('id="theme-mode"'));
      expect(sidebarFooter).toContain('class="theme-icon theme-icon-sun"');
      expect(sidebarFooter).toContain('class="theme-icon theme-icon-moon"');
    }
    expect(singleIndex).toContain('<aside class="sidebar">\n      <div class="sidebar-scroll">\n        <div class="sidebar-heading">');
    expect(gatewayIndex).toContain('<aside class="sidebar">\n      <div class="sidebar-scroll">\n        <section class="sidebar-section workspace-section">');
    for (const styles of [singleStyles, gatewayStyles]) {
      expect(styles).not.toContain(".brand {");
      expect(styles).not.toContain(".brand-copy");
      expect(styles).not.toContain(".brand strong");
      expect(styles).not.toContain(".brand span");
      expect(styles).toContain(".brand-mark {");
    }
    for (const app of [singleApp, gatewayApp]) {
      expect(app).toContain("globalThis.MeTheme.bindControls(elements.themeCycle, elements.themeMode");
    }
    for (const server of [singleServer, gatewayServer]) {
      expect(server).toContain('include_str!("webui/theme.js")');
      expect(server).toContain('include_str!("webui/theme.css")');
      expect(server).toContain('(&Method::Get, "/theme.js")');
      expect(server).toContain('(&Method::Get, "/theme.css")');
    }
    for (const palette of theme.THEMES) {
      expect(themeStyles).toContain(`:root[data-theme="${palette.id}"][data-mode="dark"]`);
      expect(themeStyles).toContain(`:root[data-theme="${palette.id}"][data-mode="light"]`);
    }
    expect(themeStyles.match(/:root\[data-theme=/g)).toHaveLength(18);
  });

  test("keeps gateway work and chat in one natural scrolling flow above fixed controls", () => {
    const gatewayIndex = readFileSync(join(import.meta.dir, "../src/gateway_webui/index.html"), "utf8");
    const gatewayStyles = readFileSync(join(import.meta.dir, "../src/gateway_webui/style.css"), "utf8");
    expect(gatewayIndex.indexOf('id="workspace-list"')).toBeLessThan(gatewayIndex.indexOf('class="sidebar-divider"'));
    expect(gatewayIndex.indexOf('class="sidebar-divider"')).toBeLessThan(gatewayIndex.indexOf('class="sidebar-heading chat-heading"'));
    expect(gatewayIndex.indexOf('class="sidebar-heading chat-heading"')).toBeLessThan(gatewayIndex.indexOf('<footer class="sidebar-footer">'));
    expect(gatewayStyles).toContain(".sidebar-scroll { min-height: 0; flex: 1 1 auto; overflow-x: hidden; overflow-y: auto;");
    expect(gatewayStyles).toContain(".workspace-list { display: grid; grid-auto-rows: max-content;");
    expect(gatewayStyles).not.toContain("max-height: 50%");
    expect(gatewayStyles).not.toContain("max-height: 34%");
    expect(gatewayStyles).toContain(".sidebar-footer { display: flex; min-width: 0; flex: 0 0 auto;");
  });

  test("themes common and gateway-only surfaces through semantic tokens", () => {
    const singleStyles = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    const gatewayStyles = readFileSync(join(import.meta.dir, "../src/gateway_webui/style.css"), "utf8");
    for (const styles of [singleStyles, gatewayStyles]) {
      expect(styles).toContain("background: var(--sidebar);");
      expect(styles).toContain("background: var(--terminal-bg);");
      expect(styles).toContain("background: var(--modal-backdrop-bg);");
      expect(styles).toContain("background: linear-gradient(135deg, var(--brand-start), var(--brand-mid), var(--brand-end));");
      expect(styles).toContain(".sidebar-appearance { display: flex;");
      expect(styles).toContain(".theme-control { display: grid;");
      expect(styles).toContain(".theme-cycle:focus-visible, .theme-mode:focus-visible");
    }
    expect(gatewayStyles).toContain("background: linear-gradient(180deg, var(--directory-modal-top) 0%, var(--directory-modal-bottom) 100%);");
    expect(gatewayStyles).toContain("background: var(--directory-table-bg);");
    expect(gatewayStyles).toContain("background: var(--panel); color: var(--text);");
  });

  test("keeps text and primary actions readable in every palette", () => {
    const styles = readFileSync(join(import.meta.dir, "../src/webui/theme.css"), "utf8");
    for (const mode of ["dark", "light"]) {
      const modeTokens = cssDeclarations(styles, `:root[data-mode="${mode}"]`);
      for (const palette of theme.THEMES) {
        const paletteTokens = cssDeclarations(styles, `:root[data-theme="${palette.id}"][data-mode="${mode}"]`);
        const tokens = { ...modeTokens, ...paletteTokens };
        for (const surface of ["--bg", "--panel"]) {
          expect(contrastRatio(tokens["--text"], tokens[surface])).toBeGreaterThanOrEqual(7);
          expect(contrastRatio(tokens["--muted"], tokens[surface])).toBeGreaterThanOrEqual(4.5);
          expect(contrastRatio(tokens["--dim"], tokens[surface])).toBeGreaterThanOrEqual(4.5);
        }
        expect(contrastRatio(tokens["--primary-bg"], "#ffffff")).toBeGreaterThanOrEqual(4.5);
        expect(contrastRatio(tokens["--primary-hover"], "#ffffff")).toBeGreaterThanOrEqual(4.5);
        if (["aurora", "sakura", "neon", "obsidian"].includes(palette.id)) {
          for (const stop of ["--brand-start", "--brand-mid", "--brand-end"]) {
            expect(contrastRatio(tokens[stop], "#ffffff")).toBeGreaterThanOrEqual(4.5);
          }
        }
      }
    }
  });
});
