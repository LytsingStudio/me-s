(() => {
  "use strict";
  const escape = (value) => String(value ?? "").replace(/[&<>"']/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[char]);
  const states = { deinit: "未启动", queued: "排队中", running: "运行中", cancelling: "取消中", succeeded: "已完成", failed: "失败", cancelled: "已取消" };
  const percent = (value) => value == null || !Number.isFinite(Number(value)) ? "—" : `${Number(value).toFixed(1)}%`;
  const bytes = (value) => {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    const gib = Number(value) / 1024 ** 3;
    return `${gib.toFixed(1)} GiB`;
  };
  const memory = (value) => `${bytes(value?.used_bytes)} / ${bytes(value?.total_bytes)}`;
  const setHtml = (node, html) => { if (node.innerHTML !== html) node.innerHTML = html; };

  function create({ container, request, onUnauthorized }) {
    container.innerHTML = `<div class="gais-page"><header class="gais-heading"><h2>GAIS</h2><span data-gais-updated></span></header>
      <p class="gais-message" data-gais-message role="status">正在获取…</p>
      <section class="gais-host" data-gais-host></section>
      <section class="gais-tasks"><header><h3>活跃任务</h3><span data-gais-count></span></header>
        <div class="gais-task-list" data-gais-tasks></div></section></div>`;
    const node = (name) => container.querySelector(`[data-gais-${name}]`);
    const rows = new Map();
    let identity = null;
    let generation = 0;
    let timer = null;
    let pending = false;
    let abort = null;
    let haveData = false;
    const current = (epoch) => identity !== null && generation === epoch;

    function render(payload) {
      const host = payload.host || {};
      const metric = (label, value) => `<div class="gais-metric"><span>${escape(label)}</span><strong>${escape(value)}</strong></div>`;
      setHtml(node("host"), `<div class="gais-metrics">${metric("CPU", percent(host.cpu?.utilization_percent))}${metric("内存", memory(host.memory))}${metric("排队任务", host.queued_count ?? "—")}</div>
        <div class="gais-gpus">${(host.gpus || []).map((gpu) => `<div class="gais-gpu"><strong>${escape(gpu.name || gpu.id)}</strong><span>使用率 ${escape(percent(gpu.utilization_percent))}</span><span>${gpu.memory?.kind === "unified" ? "统一内存" : `显存 ${escape(memory(gpu.memory))}`}</span><span>${gpu.power_watts == null ? "" : `${escape(gpu.power_watts)} W`}</span></div>`).join("") || '<p class="settings-help">暂无 GPU 信息</p>'}</div>
        ${host.resource_wait_reason ? `<p class="gais-wait-reason">${escape(host.resource_wait_reason)}</p>` : ""}${host.resource_quarantined ? '<p class="gais-wait-reason">资源暂不可用</p>' : ""}`);
      const tasks = Array.isArray(payload.tasks) ? payload.tasks : [];
      const ids = new Set(tasks.map((task) => task.task));
      for (const [key, row] of rows) if (!ids.has(key)) { row.remove(); rows.delete(key); }
      node("tasks").querySelector(".gais-empty")?.remove();
      for (const task of tasks) {
        let row = rows.get(task.task);
        if (!row) { row = document.createElement("article"); row.className = "gais-task"; rows.set(task.task, row); node("tasks").append(row); }
        const progress = task.progress || {};
        const counters = Object.entries(progress).filter(([key, value]) => key !== "stage" && value != null && typeof value !== "object")
          .map(([key, value]) => `<span>${escape(({ completed: "已完成", total: "总量", current: "当前", step: "步骤", steps: "总步数", percent: "进度" })[key] || key)} ${escape(value)}</span>`).join("");
        setHtml(row, `<div class="gais-task-top"><strong>${escape(task.task)}</strong><span class="gais-task-state">${escape(states[task.state] || task.state)}</span></div>
          <div class="gais-task-meta"><span>${escape(task.model || "")}</span><span>${escape(progress.stage || task.stage || "等待开始")}</span>${task.elapsed_seconds == null ? "" : `<span>${Math.round(Number(task.elapsed_seconds))} 秒</span>`}</div>
          ${counters ? `<div class="gais-progress">${counters}</div>` : ""}${task.resource_wait_reason ? `<p>${escape(task.resource_wait_reason)}</p>` : ""}`);
      }
      if (!tasks.length) node("tasks").innerHTML = '<p class="gais-empty">暂无活跃任务</p>';
      node("count").textContent = String(tasks.length);
      node("message").textContent = "";
      node("message").classList.remove("gais-error");
      node("updated").textContent = `更新于 ${new Date().toLocaleTimeString()}`;
      haveData = true;
    }
    async function poll() {
      if (identity === null || pending) return;
      const epoch = generation;
      pending = true;
      abort = new AbortController();
      try {
        const payload = await request("/api/gais/status", { signal: abort.signal });
        if (current(epoch)) render(payload);
      } catch (error) {
        if (current(epoch) && error.name !== "AbortError") {
          node("message").textContent = `${error.message || "暂时无法获取 GAIS 信息"}${haveData ? " · 当前显示上次结果" : ""}`;
          node("message").classList.add("gais-error");
          if (error.status === 401) onUnauthorized();
        }
      } finally {
        pending = false;
        abort = null;
        if (identity !== null) {
          if (epoch !== generation) void poll();
          else timer = setTimeout(poll, 3000);
        }
      }
    }
    function deactivate() {
      identity = null;
      generation += 1;
      clearTimeout(timer); timer = null;
      abort?.abort();
    }
    function activate(key) {
      if (identity === key) return;
      deactivate();
      identity = key;
      haveData = false;
      node("host").innerHTML = "";
      node("tasks").innerHTML = ""; rows.clear();
      node("count").textContent = ""; node("updated").textContent = "";
      node("message").textContent = "正在获取…";
      node("message").classList.remove("gais-error");
      void poll();
    }
    function refresh() {
      if (identity === null) return;
      const key = identity;
      deactivate(); activate(key);
    }
    return { activate, deactivate, refresh };
  }

  function settingsHtml() {
    return `<section class="settings-section settings-gais-section"><header class="settings-section-header"><div class="settings-section-heading"><h3>生成式 AI 服务</h3></div></header>
      <div class="gais-settings" data-gais-settings><label>URL<input data-gais-url type="url" placeholder="http://127.0.0.1:47589" autocomplete="off" spellcheck="false" disabled></label>
        <label>Token<input data-gais-token type="password" autocomplete="off" spellcheck="false" disabled></label>
        <div class="gais-settings-actions"><span data-gais-result role="status">正在获取…</span><button type="button" class="ghost-button" data-gais-test disabled>测试</button><button type="button" class="primary-button" data-gais-save disabled>保存</button></div>
      </div></section>`;
  }
  async function bindSettings(container, { request, current, onUnauthorized, onSaved, draft, onDraft }) {
    const root = container.querySelector("[data-gais-settings]");
    if (!root) return;
    const url = root.querySelector("[data-gais-url]");
    const token = root.querySelector("[data-gais-token]");
    const result = root.querySelector("[data-gais-result]");
    const controls = [...root.querySelectorAll("input,button")];
    const alive = () => root.isConnected && current();
    const values = () => ({ url: url.value, token: token.value });
    const disabled = (value) => controls.forEach((node) => { node.disabled = value; });
    try {
      const settings = draft || (await request("/api/gais/settings")).settings;
      if (!alive()) return;
      url.value = settings.url || ""; token.value = settings.token || "";
      result.textContent = "";
      onDraft(values()); disabled(false);
    } catch (error) {
      if (!alive()) return;
      result.textContent = error.message;
      disabled(false);
      if (error.status === 401) onUnauthorized();
    }
    for (const input of [url, token]) input.addEventListener("input", () => { onDraft(values()); result.textContent = ""; });
    for (const action of ["test", "save"]) root.querySelector(`[data-gais-${action}]`).addEventListener("click", async () => {
      if (!alive()) return;
      disabled(true); result.textContent = action === "test" ? "正在测试…" : "正在保存…";
      try {
        const payload = await request(action === "test" ? "/api/gais/test" : "/api/gais/settings", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(values()) });
        if (!alive()) return;
        result.textContent = payload.message || (action === "test" ? "连接成功" : "已保存");
        if (action === "save") onSaved();
      } catch (error) {
        if (!alive()) return;
        result.textContent = error.message || "操作失败";
        if (error.status === 401) onUnauthorized();
      } finally { if (alive()) disabled(false); }
    });
  }
  globalThis.MeGais = { create, settingsHtml, bindSettings };
})();
