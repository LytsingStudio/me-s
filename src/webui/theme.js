(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  } else {
    root.MeTheme = api;
    api.initialize(root);
  }
}(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  const THEMES = Object.freeze([
    Object.freeze({ id: "violet", name: "紫曜" }),
    Object.freeze({ id: "graphite", name: "石墨" }),
    Object.freeze({ id: "ocean", name: "深海" }),
    Object.freeze({ id: "forest", name: "松林" }),
    Object.freeze({ id: "sand", name: "暖砂" }),
  ]);
  const DEFAULT_THEME = THEMES[0].id;
  const STORAGE_THEME = "me-theme";
  const STORAGE_MODE = "me-color-mode";

  function normalizeTheme(value) {
    return THEMES.some((theme) => theme.id === value) ? value : DEFAULT_THEME;
  }

  function normalizeMode(value, fallback = "dark") {
    return value === "light" || value === "dark" ? value : fallback;
  }

  function preferredMode(runtime) {
    try {
      return runtime?.matchMedia?.("(prefers-color-scheme: light)")?.matches ? "light" : "dark";
    } catch (_) {
      return "dark";
    }
  }

  function storage(runtime) {
    try {
      return runtime?.localStorage || null;
    } catch (_) {
      return null;
    }
  }

  function readStored(runtime) {
    const store = storage(runtime);
    let theme = null;
    let mode = null;
    try {
      theme = store?.getItem(STORAGE_THEME);
      mode = store?.getItem(STORAGE_MODE);
    } catch (_) {
      // Browser privacy settings may make localStorage unavailable; the active page still remains usable.
    }
    return {
      theme: normalizeTheme(theme),
      mode: normalizeMode(mode, preferredMode(runtime)),
    };
  }

  function themeById(value) {
    return THEMES.find((theme) => theme.id === normalizeTheme(value));
  }

  function apply(runtime, preference, persist = false) {
    const theme = themeById(preference?.theme);
    const mode = normalizeMode(preference?.mode, preferredMode(runtime));
    const document = runtime?.document;
    const rootElement = document?.documentElement;
    rootElement?.setAttribute?.("data-theme", theme.id);
    rootElement?.setAttribute?.("data-mode", mode);
    if (rootElement?.style) rootElement.style.colorScheme = mode;
    const colorScheme = document?.querySelector?.('meta[name="color-scheme"]');
    colorScheme?.setAttribute?.("content", mode);
    if (persist) {
      const store = storage(runtime);
      try {
        store?.setItem(STORAGE_THEME, theme.id);
        store?.setItem(STORAGE_MODE, mode);
      } catch (_) {
        // Keep the page-level preference even when persistence is unavailable.
      }
    }
    return { theme, mode };
  }

  function initialize(runtime = globalThis) {
    return apply(runtime, readStored(runtime));
  }

  function current(runtime = globalThis) {
    const rootElement = runtime?.document?.documentElement;
    if (!rootElement?.getAttribute) {
      const saved = readStored(runtime);
      return { theme: themeById(saved.theme), mode: saved.mode };
    }
    return {
      theme: themeById(rootElement.getAttribute("data-theme")),
      mode: normalizeMode(rootElement.getAttribute("data-mode"), preferredMode(runtime)),
    };
  }

  function cycle(runtime = globalThis) {
    const active = current(runtime);
    const index = THEMES.findIndex((theme) => theme.id === active.theme.id);
    return apply(runtime, {
      theme: THEMES[(index + 1) % THEMES.length].id,
      mode: active.mode,
    }, true);
  }

  function toggleMode(runtime = globalThis) {
    const active = current(runtime);
    return apply(runtime, {
      theme: active.theme.id,
      mode: active.mode === "dark" ? "light" : "dark",
    }, true);
  }

  function syncControls(themeButton, modeButton, preference) {
    const index = THEMES.findIndex((theme) => theme.id === preference.theme.id);
    const nextTheme = THEMES[(index + 1) % THEMES.length];
    const themeLabel = `切换主题：${nextTheme.name}`;
    themeButton?.setAttribute?.("title", themeLabel);
    themeButton?.setAttribute?.("aria-label", `${themeLabel}（当前：${preference.theme.name}）`);
    themeButton?.setAttribute?.("data-theme-name", preference.theme.name);
    const targetMode = preference.mode === "dark" ? "浅色" : "深色";
    const modeLabel = `切换到${targetMode}模式`;
    modeButton?.setAttribute?.("title", modeLabel);
    modeButton?.setAttribute?.("aria-label", modeLabel);
    modeButton?.setAttribute?.("aria-pressed", preference.mode === "dark" ? "true" : "false");
  }

  function bindControls(themeButton, modeButton, announce, runtime = globalThis) {
    let active = current(runtime);
    syncControls(themeButton, modeButton, active);
    themeButton?.addEventListener?.("click", () => {
      active = cycle(runtime);
      syncControls(themeButton, modeButton, active);
      announce?.(`已切换至「${active.theme.name}」主题`);
    });
    modeButton?.addEventListener?.("click", () => {
      active = toggleMode(runtime);
      syncControls(themeButton, modeButton, active);
      announce?.(`已切换至${active.mode === "dark" ? "深色" : "浅色"}模式`);
    });
    return active;
  }

  return {
    THEMES,
    STORAGE_THEME,
    STORAGE_MODE,
    normalizeTheme,
    normalizeMode,
    readStored,
    apply,
    initialize,
    current,
    cycle,
    toggleMode,
    syncControls,
    bindControls,
  };
}));
