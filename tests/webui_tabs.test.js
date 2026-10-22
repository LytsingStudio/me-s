"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

const source = readFileSync(join(import.meta.dir, "../src/webui/app.js"), "utf8");
const start = source.indexOf("function scrollViewTabs(event)");
const end = source.indexOf("\nelements.tabs.querySelectorAll", start);
const scrollViewTabs = new Function(`${source.slice(start, end)}; return scrollViewTabs;`)();

function wheel(properties = {}, width = 1200) {
  const tabs = { scrollLeft: 300, scrollWidth: width, clientWidth: 600 };
  const event = {
    currentTarget: tabs, deltaX: 0, deltaY: 0, deltaMode: 0, ctrlKey: false,
    prevented: false, preventDefault() { this.prevented = true; }, ...properties,
  };
  scrollViewTabs(event);
  return { tabs, event };
}

describe("shared view tabs", () => {
  test("maps vertical wheel motion to horizontal scrolling in both directions", () => {
    expect(wheel({ deltaY: 80 }).tabs.scrollLeft).toBe(380);
    expect(wheel({ deltaY: -80 }).tabs.scrollLeft).toBe(220);
    expect(wheel({ deltaY: 80 }).event.prevented).toBe(true);
  });

  test("preserves the dominant horizontal trackpad axis without double-applying it", () => {
    expect(wheel({ deltaX: 90, deltaY: 10 }).tabs.scrollLeft).toBe(390);
    expect(wheel({ deltaX: -90, deltaY: 10 }).tabs.scrollLeft).toBe(210);
    expect(wheel({ deltaX: 10, deltaY: 90 }).tabs.scrollLeft).toBe(390);
  });

  test("handles line and page wheel units", () => {
    expect(wheel({ deltaY: 3, deltaMode: 1 }).tabs.scrollLeft).toBe(348);
    expect(wheel({ deltaY: 1, deltaMode: 2 }).tabs.scrollLeft).toBe(900);
  });

  test("leaves zoom and non-overflowing bars alone", () => {
    const zoom = wheel({ deltaY: 80, ctrlKey: true });
    expect(zoom.tabs.scrollLeft).toBe(300);
    expect(zoom.event.prevented).toBe(false);
    const fits = wheel({ deltaY: 80 }, 600);
    expect(fits.tabs.scrollLeft).toBe(300);
    expect(fits.event.prevented).toBe(false);
  });

  test("binds once on the shared bar and hides scrollbars at every viewport size", () => {
    expect(source).toContain('elements.tabs.addEventListener("wheel", scrollViewTabs, { passive: false })');
    const css = readFileSync(join(import.meta.dir, "../src/webui/style.css"), "utf8");
    expect(css).toMatch(/\.view-tabs\s*\{[^}]*min-width: 0[^}]*overflow-x: auto[^}]*scrollbar-width: none/);
    expect(css).toContain(".view-tabs::-webkit-scrollbar { display: none; }");
    expect(css).toMatch(/\.view-tabs::after\s*\{[^}]*flex: 0 0 var\(--view-tabs-end-space, 0px\)[^}]*height: 1px/);
    expect(css).toMatch(/\.view-tabs button\s*\{[^}]*flex: 0 0 auto/);
  });
});
