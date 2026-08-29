"use strict";

(function attachFileManager(global) {
  const TERMINAL_STATES = new Set(["completed", "failed", "cancelled"]);
  const UPLOAD_CHUNK_BYTES = 384 * 1024;
  const NAVIGATION_HISTORY_LIMIT = 100;
  const TOUCH_DOUBLE_TAP_MS = 450;
  const TOUCH_TAP_MOVE_PX = 12;

  function create(options) {
    return new FileManager(options);
  }

  class FileManager {
    constructor(options) {
      this.container = options.container;
      this.request = options.request;
      this.downloadUrl = options.downloadUrl;
      this.downloadFile = options.downloadFile || null;
      this.writeClipboard = options.writeClipboard;
      this.onUnauthorized = options.onUnauthorized || (() => {});
      this.notify = options.notify || (() => {});
      this.states = new Map();
      this.identity = null;
      this.state = null;
      this.touchPointer = null;
      this.lastTouchTap = null;
      this.suppressTouchClickPath = null;
      this.ignoreDblClickUntil = 0;
      this.renderShell();
      this.bind();
    }

    attach(identity) {
      if (!identity?.key || !identity?.agentId) return;
      if (this.identity?.key && this.identity.key !== identity.key && this.dialogResolve) this.closeDialog(null);
      if (this.identity?.key !== identity.key) this.resetTouchGesture();
      this.identity = { ...identity };
      let view = this.states.get(identity.key);
      if (!view) {
        view = {
          identity: { ...identity },
          path: identity.defaultPath || null,
          parent: null,
          roots: false,
          entries: [],
          selection: new Set(),
          anchor: null,
          search: "",
          sortKey: "name",
          sortDirection: "asc",
          clipboard: null,
          history: [],
          loading: false,
          error: "",
          job: null,
          upload: null,
          download: null,
          loaded: false,
          pollToken: 0,
        };
        this.states.set(identity.key, view);
      } else if (!view.path && !view.roots && identity.defaultPath) {
        view.path = identity.defaultPath;
      }
      view.history ||= [];
      view.identity = { ...identity };
      this.state = view;
      this.render();
      if (!view.loaded && !view.loading) void this.load(view.path, false);
      else if (view.job && !TERMINAL_STATES.has(view.job.state)) void this.pollJob(view, view.job.operation_id);
    }

    renderShell() {
      this.container.innerHTML = `
        <div class="file-manager-shell">
          <div class="file-manager-toolbar file-manager-navigation">
            ${actionButton("back", "后退")}
            ${actionButton("up", "上一级")}
            ${actionButton("refresh", "刷新")}
            <form class="file-manager-path-form">
              <input class="file-manager-path" type="text" aria-label="当前路径" autocomplete="off" spellcheck="false">
              <button type="submit" class="file-manager-icon-button" title="前往" aria-label="前往">${actionIcon("go")}</button>
            </form>
            <input class="file-manager-search" type="search" placeholder="搜索当前目录" aria-label="搜索当前目录">
          </div>
          <div class="file-manager-toolbar file-manager-actions" role="toolbar" aria-label="文件操作">
            ${actionButton("select-all", "全选")}
            ${actionButton("mkdir", "新建文件夹")}
            ${actionButton("rename", "重命名")}
            ${actionButton("copy-path", "复制绝对路径")}
            ${actionButton("copy", "复制")}
            ${actionButton("cut", "剪切")}
            ${actionButton("paste", "粘贴")}
            ${actionButton("move", "移动")}
            ${actionButton("upload", "上传")}
            ${actionButton("download", "下载")}
            ${actionButton("delete", "永久删除", true)}
            <input class="file-manager-upload-input" type="file" multiple hidden>
            <span class="file-manager-selection" aria-live="polite"></span>
          </div>
          <div class="file-manager-table-wrap">
            <div class="file-manager-header file-manager-grid" role="row">
              <span class="file-manager-check"></span>
              <button type="button" data-file-sort="name">名称</button>
              <button type="button" data-file-sort="kind">类型</button>
              <button type="button" data-file-sort="size_bytes">大小</button>
              <button type="button" data-file-sort="modified_at_ms">修改时间</button>
            </div>
            <div class="file-manager-list" role="listbox" aria-multiselectable="true"></div>
          </div>
          <section class="file-manager-task hidden" aria-live="polite"></section>
          <div class="file-manager-dialog-backdrop hidden">
            <form class="file-manager-dialog" role="dialog" aria-modal="true">
              <header><h2></h2><button type="button" data-dialog-action="close" aria-label="关闭">×</button></header>
              <div class="file-manager-dialog-body"></div>
              <footer><button type="button" data-dialog-action="cancel">取消</button><button type="submit" class="primary-button">确认</button></footer>
            </form>
          </div>
        </div>`;
      this.pathInput = this.container.querySelector(".file-manager-path");
      this.searchInput = this.container.querySelector(".file-manager-search");
      this.list = this.container.querySelector(".file-manager-list");
      this.selectionLabel = this.container.querySelector(".file-manager-selection");
      this.taskPanel = this.container.querySelector(".file-manager-task");
      this.uploadInput = this.container.querySelector(".file-manager-upload-input");
      this.dialogBackdrop = this.container.querySelector(".file-manager-dialog-backdrop");
      this.dialogForm = this.container.querySelector(".file-manager-dialog");
      this.dialogTitle = this.container.querySelector(".file-manager-dialog h2");
      this.dialogBody = this.container.querySelector(".file-manager-dialog-body");
      this.dialogConfirm = this.container.querySelector(".file-manager-dialog footer button[type=submit]");
      this.dialogResolve = null;
    }

    bind() {
      this.container.addEventListener("click", (event) => {
        const action = event.target.closest("[data-file-action]")?.dataset.fileAction;
        if (action) void this.handleAction(action);
        const sort = event.target.closest("[data-file-sort]")?.dataset.fileSort;
        if (sort) this.changeSort(sort);
        const row = event.target.closest(".file-manager-entry");
        if (row && !event.target.closest("button,input")) {
          if (this.suppressTouchClickPath === row.dataset.path) this.suppressTouchClickPath = null;
          else this.selectRow(row.dataset.path, event);
        }
        const dialogAction = event.target.closest("[data-dialog-action]")?.dataset.dialogAction;
        if (dialogAction) this.closeDialog(null);
      });
      this.container.addEventListener("dblclick", (event) => {
        if (Date.now() < this.ignoreDblClickUntil) return;
        const row = event.target.closest(".file-manager-entry");
        if (row?.dataset.navigable === "true") void this.navigate(row.dataset.path, false);
      });
      this.container.addEventListener("pointerdown", (event) => this.handleTouchPointerDown(event));
      this.container.addEventListener("pointermove", (event) => this.handleTouchPointerMove(event));
      this.container.addEventListener("pointerup", (event) => this.handleTouchPointerUp(event));
      this.container.addEventListener("pointercancel", (event) => this.cancelTouchPointer(event));
      this.container.addEventListener("change", (event) => {
        const checkbox = event.target.closest(".file-manager-entry input[type=checkbox]");
        if (checkbox) this.togglePath(checkbox.closest(".file-manager-entry").dataset.path, checkbox.checked);
      });
      this.container.querySelector(".file-manager-path-form").addEventListener("submit", (event) => {
        event.preventDefault();
        const path = this.pathInput.value.trim();
        if (path) void this.navigate(path, false);
      });
      this.searchInput.addEventListener("input", () => {
        if (!this.state) return;
        this.state.search = this.searchInput.value;
        this.renderList();
      });
      this.uploadInput.addEventListener("change", () => {
        const files = [...this.uploadInput.files];
        this.uploadInput.value = "";
        if (files.length) void this.uploadFiles(files);
      });
      this.dialogForm.addEventListener("submit", (event) => {
        event.preventDefault();
        const values = Object.fromEntries(new FormData(this.dialogForm).entries());
        this.closeDialog(values);
      });
    }

    resetTouchGesture() {
      this.touchPointer = null;
      this.lastTouchTap = null;
      this.suppressTouchClickPath = null;
      this.ignoreDblClickUntil = 0;
    }

    handleTouchPointerDown(event) {
      if (event.pointerType !== "touch" || event.isPrimary === false || event.target.closest("button,input")) return;
      const row = event.target.closest(".file-manager-entry");
      if (!row) return;
      this.touchPointer = {
        pointerId: event.pointerId,
        path: row.dataset.path,
        navigable: row.dataset.navigable === "true",
        x: event.clientX,
        y: event.clientY,
        moved: false,
      };
    }

    handleTouchPointerMove(event) {
      const gesture = this.touchPointer;
      if (!gesture || gesture.pointerId !== event.pointerId) return;
      if (Math.hypot(event.clientX - gesture.x, event.clientY - gesture.y) > TOUCH_TAP_MOVE_PX) gesture.moved = true;
    }

    handleTouchPointerUp(event) {
      const gesture = this.touchPointer;
      if (!gesture || gesture.pointerId !== event.pointerId) return;
      this.touchPointer = null;
      if (gesture.moved || Math.hypot(event.clientX - gesture.x, event.clientY - gesture.y) > TOUCH_TAP_MOVE_PX) {
        this.lastTouchTap = null;
        return;
      }
      const now = Date.now();
      const previous = this.lastTouchTap;
      if (previous?.path === gesture.path && now - previous.at <= TOUCH_DOUBLE_TAP_MS) {
        this.lastTouchTap = null;
        if (!gesture.navigable || this.state?.loading) return;
        this.suppressTouchClickPath = gesture.path;
        this.ignoreDblClickUntil = now + TOUCH_DOUBLE_TAP_MS;
        setTimeout(() => {
          if (this.suppressTouchClickPath === gesture.path) this.suppressTouchClickPath = null;
        }, 0);
        void this.navigate(gesture.path, false);
        return;
      }
      this.lastTouchTap = { path: gesture.path, at: now };
    }

    cancelTouchPointer(event) {
      if (this.touchPointer?.pointerId !== event.pointerId) return;
      this.touchPointer = null;
      this.lastTouchTap = null;
    }

    async call(path, body, identity = this.identity) {
      try {
        return await this.request(path, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        }, identity);
      } catch (error) {
        if (error.status === 401) this.onUnauthorized();
        throw error;
      }
    }

    async navigate(path, roots, view = this.state) {
      if (!view || view.loading) return false;
      return this.load(path, roots, view, true);
    }

    async goBack() {
      const view = this.state;
      if (!view || view.loading || !view.history.length) return;
      const target = view.history[view.history.length - 1];
      if (await this.load(target.path, target.roots, view)) view.history.pop();
      if (this.state === view) this.render();
    }

    async load(path, roots, view = this.state, recordHistory = false) {
      const identity = view?.identity;
      if (!view || !identity?.key) return false;
      const previous = view.loaded ? { path: view.path, roots: view.roots } : null;
      view.loading = true;
      view.error = "";
      if (this.state === view) this.render();
      try {
        const listing = await this.call("/api/files/list", { path: roots ? null : path, roots }, identity);
        const next = { path: listing.path, roots: Boolean(listing.root_selector) };
        if (recordHistory && previous && !sameLocation(previous, next)) {
          view.history.push(previous);
          if (view.history.length > NAVIGATION_HISTORY_LIMIT) view.history.splice(0, view.history.length - NAVIGATION_HISTORY_LIMIT);
        }
        view.path = next.path;
        view.parent = listing.parent;
        view.roots = next.roots;
        view.entries = listing.entries || [];
        view.selection.clear();
        view.anchor = null;
        view.loaded = true;
        return true;
      } catch (error) {
        view.error = error.message;
        this.notify(error.message, "error");
        return false;
      } finally {
        view.loading = false;
        if (this.state === view) this.render();
      }
    }

    visibleEntries() {
      if (!this.state) return [];
      const query = this.state.search.trim().toLocaleLowerCase();
      const entries = query
        ? this.state.entries.filter((entry) => entry.name.toLocaleLowerCase().includes(query))
        : [...this.state.entries];
      const key = this.state.sortKey;
      const direction = this.state.sortDirection === "asc" ? 1 : -1;
      return entries.sort((left, right) => {
        const a = left[key];
        const b = right[key];
        if (a == null && b == null) return left.name.localeCompare(right.name) * direction;
        if (a == null) return 1;
        if (b == null) return -1;
        const compared = typeof a === "number" ? a - b : String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: "base" });
        return (compared || left.name.localeCompare(right.name)) * direction;
      });
    }

    render() {
      if (!this.state) return;
      this.pathInput.value = this.state.roots ? "可用位置" : (this.state.path || "");
      this.pathInput.disabled = this.state.loading || this.state.roots;
      this.searchInput.value = this.state.search;
      this.renderList();
      this.renderSelection();
      this.renderTask();
    }

    renderSelection() {
      if (!this.state) return;
      const selected = this.state.selection.size;
      const clipboard = this.state.clipboard;
      this.selectionLabel.textContent = [
        selected ? `已选择 ${selected} 项` : "",
        clipboard ? `${clipboard.mode === "copy" ? "复制" : "剪切"} ${clipboard.sources.length} 项` : "",
      ].filter(Boolean).join(" · ") || "未选择项目";
      const hasDirectory = Boolean(this.state.path) && !this.state.roots;
      this.setDisabled("back", this.state.loading || !this.state.history.length);
      this.setDisabled("up", this.state.loading || (!this.state.parent && this.state.roots));
      this.setDisabled("refresh", this.state.loading);
      this.setDisabled("select-all", !this.visibleEntries().length || this.state.loading);
      this.setDisabled("mkdir", !hasDirectory || this.state.loading);
      this.setDisabled("rename", selected !== 1 || this.state.loading);
      this.setDisabled("copy-path", selected === 0 || this.state.loading);
      this.setDisabled("copy", selected === 0 || this.state.loading);
      this.setDisabled("cut", selected === 0 || this.state.loading);
      this.setDisabled("paste", !hasDirectory || !clipboard || this.state.loading);
      this.setDisabled("move", selected === 0 || this.state.loading);
      this.setDisabled("upload", !hasDirectory || this.state.loading);
      this.setDisabled("download", selected === 0 || this.state.loading);
      this.setDisabled("delete", selected === 0 || this.state.loading);
      this.list.querySelectorAll(".file-manager-entry").forEach((row) => {
        const rowSelected = this.state.selection.has(row.dataset.path);
        row.classList.toggle("selected", rowSelected);
        row.setAttribute("aria-selected", String(rowSelected));
        const checkbox = row.querySelector('input[type="checkbox"]');
        if (checkbox) checkbox.checked = rowSelected;
      });
    }

    renderList() {
      if (!this.state) return;
      if (this.state.loading && !this.state.loaded) {
        this.list.innerHTML = `<div class="file-manager-empty"><span class="file-manager-spinner"></span>正在读取目录</div>`;
        return;
      }
      if (this.state.error && !this.state.entries.length) {
        this.list.innerHTML = `<div class="file-manager-empty error">${escapeHtml(this.state.error)}</div>`;
        return;
      }
      const entries = this.visibleEntries();
      if (!entries.length) {
        this.list.innerHTML = `<div class="file-manager-empty">${this.state.search ? "没有匹配的项目" : "此位置为空"}</div>`;
        return;
      }
      this.list.innerHTML = entries.map((entry) => {
        const selected = this.state.selection.has(entry.path);
        return `<div class="file-manager-entry file-manager-grid ${selected ? "selected" : ""}" role="option" aria-selected="${selected}" data-path="${escapeAttr(entry.path)}" data-navigable="${entry.navigable}">
          <span class="file-manager-check"><input type="checkbox" ${selected ? "checked" : ""} aria-label="选择 ${escapeAttr(entry.name)}"></span>
          <span class="file-manager-name"><span class="file-kind-icon ${escapeAttr(entry.kind)}" aria-hidden="true">${kindIcon(entry.kind)}</span><span title="${escapeAttr(entry.name)}">${escapeHtml(entry.name)}</span>${entry.readonly ? '<span class="file-manager-badge">只读</span>' : ""}</span>
          <span>${escapeHtml(kindLabel(entry.kind))}</span>
          <span>${formatBytes(entry.size_bytes)}</span>
          <span>${formatDate(entry.modified_at_ms)}</span>
        </div>`;
      }).join("");
      this.container.querySelectorAll("[data-file-sort]").forEach((button) => {
        const active = button.dataset.fileSort === this.state.sortKey;
        const direction = active ? this.state.sortDirection : "";
        const label = button.textContent.trim();
        const directionLabel = direction === "asc" ? "升序" : "降序";
        const nextDirectionLabel = direction === "asc" ? "降序" : "升序";
        const description = active
          ? `${label}，当前${directionLabel}，点击切换为${nextDirectionLabel}`
          : `${label}，点击按升序排列`;
        button.classList.toggle("active", active);
        button.dataset.direction = direction === "asc" ? "↑" : direction === "desc" ? "↓" : "";
        button.setAttribute("aria-label", description);
        button.title = description;
      });
    }

    renderTask() {
      const view = this.state;
      const job = view?.job;
      const transfer = view?.upload || view?.download;
      if (!job && !transfer) {
        this.taskPanel.classList.add("hidden");
        this.taskPanel.innerHTML = "";
        return;
      }
      this.taskPanel.classList.remove("hidden");
      if (transfer) {
        const percent = transfer.total ? Math.min(100, Math.round((transfer.done / transfer.total) * 100)) : 0;
        this.taskPanel.innerHTML = `<div class="file-manager-task-head"><strong>${escapeHtml(transfer.label)}</strong><button type="button" data-file-action="cancel-task">取消</button></div>
          <div class="file-manager-progress"><span style="width:${percent}%"></span></div>
          <div class="file-manager-task-meta">${percent}% · ${formatBytes(transfer.done)} / ${formatBytes(transfer.total)}</div>`;
        return;
      }
      const itemPercent = job.stats?.items ? Math.min(100, Math.round((job.processed_items / job.stats.items) * 100)) : 0;
      const terminal = TERMINAL_STATES.has(job.state);
      const results = (job.results || []).map((result) => `<li class="${escapeAttr(result.status)}"><strong>${escapeHtml(baseName(result.source))}</strong><span>${escapeHtml(result.status === "succeeded" ? "成功" : result.status === "skipped" ? "已跳过" : result.status === "partial" ? "部分完成" : "失败")}</span>${result.error ? `<small>${escapeHtml(result.error)}</small>` : ""}</li>`).join("");
      this.taskPanel.innerHTML = `<div class="file-manager-task-head"><strong>${jobTitle(job.kind)} · ${stateLabel(job.state)}</strong>${job.cancellable ? '<button type="button" data-file-action="cancel-task">取消</button>' : ""}</div>
        <div class="file-manager-progress"><span style="width:${itemPercent}%"></span></div>
        <div class="file-manager-task-meta">${job.processed_items || 0} / ${job.stats?.items || 0} 项 · ${formatBytes(job.processed_bytes)} / ${formatBytes(job.stats?.bytes)}${job.current_path ? ` · ${escapeHtml(baseName(job.current_path))}` : ""}</div>
        ${job.error ? `<p class="file-manager-task-error">${escapeHtml(job.error)}</p>` : ""}
        ${terminal && results ? `<ul class="file-manager-results">${results}</ul>` : ""}`;
    }

    setDisabled(action, disabled) {
      const button = this.container.querySelector(`[data-file-action="${action}"]`);
      if (button) button.disabled = disabled;
    }

    selectRow(path, event) {
      const entries = this.visibleEntries();
      const index = entries.findIndex((entry) => entry.path === path);
      if (event.shiftKey && this.state.anchor) {
        const anchor = entries.findIndex((entry) => entry.path === this.state.anchor);
        if (anchor >= 0 && index >= 0) {
          const [start, end] = anchor < index ? [anchor, index] : [index, anchor];
          if (!(event.metaKey || event.ctrlKey)) this.state.selection.clear();
          entries.slice(start, end + 1).forEach((entry) => this.state.selection.add(entry.path));
        }
      } else if (event.metaKey || event.ctrlKey) {
        if (this.state.selection.has(path)) this.state.selection.delete(path);
        else this.state.selection.add(path);
        this.state.anchor = path;
      } else {
        this.state.selection.clear();
        this.state.selection.add(path);
        this.state.anchor = path;
      }
      this.renderSelection();
    }

    togglePath(path, checked) {
      if (checked) this.state.selection.add(path);
      else this.state.selection.delete(path);
      this.state.anchor = path;
      this.renderSelection();
    }

    changeSort(key) {
      if (this.state.sortKey === key) this.state.sortDirection = this.state.sortDirection === "asc" ? "desc" : "asc";
      else {
        this.state.sortKey = key;
        this.state.sortDirection = "asc";
      }
      this.renderList();
    }

    selectedPaths() {
      return [...this.state.selection];
    }

    async handleAction(action) {
      if (!this.state) return;
      if (action === "back") return this.goBack();
      if (action === "up") return this.state.parent ? this.navigate(this.state.parent, false) : this.navigate(null, true);
      if (action === "refresh") return this.load(this.state.path, this.state.roots);
      if (action === "select-all") {
        const entries = this.visibleEntries();
        const allSelected = entries.length && entries.every((entry) => this.state.selection.has(entry.path));
        entries.forEach((entry) => allSelected ? this.state.selection.delete(entry.path) : this.state.selection.add(entry.path));
        return this.renderSelection();
      }
      if (action === "mkdir") return this.mkdir();
      if (action === "rename") return this.rename();
      if (action === "copy-path") return this.copySelectedPaths();
      if (action === "copy" || action === "cut") {
        this.state.clipboard = { mode: action === "copy" ? "copy" : "move", sources: this.selectedPaths() };
        this.notify(action === "copy" ? "已复制到文件剪贴板" : "已剪切到文件剪贴板");
        return this.renderSelection();
      }
      if (action === "paste") return this.startJob(this.state.clipboard.mode, this.state.clipboard.sources, this.state.path);
      if (action === "move") return this.moveSelected();
      if (action === "delete") return this.startJob("delete", this.selectedPaths(), null);
      if (action === "upload") return this.uploadInput.click();
      if (action === "download") return this.downloadSelected();
      if (action === "cancel-task") return this.cancelTask();
    }

    async copySelectedPaths() {
      const paths = this.selectedPaths();
      if (!paths.length) return;
      try {
        await this.writeClipboard(paths.join(";"));
        this.notify(paths.length === 1 ? "已复制绝对路径" : `已复制 ${paths.length} 个绝对路径`);
      } catch (error) {
        this.notify(error.message, "error");
      }
    }

    async mkdir() {
      const view = this.state;
      const values = await this.openDialog({
        title: "新建文件夹",
        body: '<label>名称<input name="name" required autocomplete="off"></label>',
        confirm: "新建",
      });
      if (!values) return;
      try {
        await this.call("/api/files/mkdir", { parent: view.path, name: values.name }, view.identity);
        await this.load(view.path, false, view);
      } catch (error) { this.notify(error.message, "error"); }
    }

    async rename() {
      const view = this.state;
      const path = this.selectedPaths()[0];
      if (!path) return;
      const values = await this.openDialog({
        title: "重命名",
        body: `<label>新名称<input name="name" required autocomplete="off" value="${escapeAttr(baseName(path))}"></label>`,
        confirm: "重命名",
      });
      if (!values) return;
      try {
        await this.call("/api/files/rename", { path, new_name: values.name }, view.identity);
        await this.load(view.path, false, view);
      } catch (error) { this.notify(error.message, "error"); }
    }

    async moveSelected() {
      const sources = this.selectedPaths();
      const values = await this.openDialog({
        title: "移动所选项目",
        body: '<label>目标目录<input name="destination" required autocomplete="off" placeholder="输入宿主机目录路径"></label>',
        confirm: "继续",
      });
      if (values) await this.startJob("move", sources, values.destination);
    }

    async startJob(kind, sources, destination) {
      if (!sources?.length) return;
      const view = this.state;
      const identity = view.identity;
      try {
        const prepared = await this.call("/api/files/jobs/prepare", { kind, sources, destination }, identity);
        view.job = prepared;
        if (this.state === view) this.renderTask();
        const conflictRows = (prepared.conflicts || []).map((conflict) => `<li><strong>${escapeHtml(baseName(conflict.source))}</strong><span>${escapeHtml(conflict.target_kind === "batch" ? "批次内同名" : `目标已有${kindLabel(conflict.target_kind)}`)}</span></li>`).join("");
        const deleteWarning = kind === "delete" ? '<p class="file-manager-danger-note">这些项目将被永久删除，无法恢复。</p>' : "";
        const conflictControls = kind === "delete" ? "" : `<label>同名项目处理<select name="policy"><option value="skip">跳过</option><option value="keep_both">保留两者</option><option value="replace">替换</option></select></label>${prepared.conflicts?.some((conflict) => conflict.directory_replacement) ? '<label class="file-manager-confirm-check"><input type="checkbox" name="replace_directories" value="yes">我确认替换非空目录会永久删除目标目录原有内容</label>' : ""}`;
        const values = await this.openDialog({
          title: kind === "delete" ? "确认永久删除" : kind === "move" ? "确认移动" : "确认复制",
          body: `${deleteWarning}<div class="file-manager-plan-summary"><strong>${prepared.stats.items} 项</strong><span>${formatBytes(prepared.stats.bytes)}</span><span>${prepared.stats.directories} 个目录</span></div>${conflictRows ? `<ul class="file-manager-conflicts">${conflictRows}</ul>` : ""}${conflictControls}`,
          confirm: kind === "delete" ? "永久删除" : "开始",
          danger: kind === "delete",
        });
        if (!values) {
          view.job = await this.call("/api/files/jobs/cancel", { operation_id: prepared.operation_id }, identity);
          if (this.state === view) this.renderTask();
          return;
        }
        if (values.policy === "replace" && prepared.conflicts?.some((conflict) => conflict.directory_replacement) && values.replace_directories !== "yes") {
          this.notify("替换目录需要单独确认", "error");
          view.job = await this.call("/api/files/jobs/cancel", { operation_id: prepared.operation_id }, identity);
          if (this.state === view) this.renderTask();
          return;
        }
        const policy = kind === "delete" ? "skip" : values.policy;
        view.job = await this.call("/api/files/jobs/confirm", {
          operation_id: prepared.operation_id,
          conflict_policy: policy,
          replace_directories: values.replace_directories === "yes",
        }, identity);
        if (this.state === view) this.renderTask();
        await this.pollJob(view, prepared.operation_id);
      } catch (error) {
        this.notify(error.message, "error");
      }
    }

    async pollJob(view, operationId) {
      if (!view || view.job?.operation_id !== operationId) return;
      const token = ++view.pollToken;
      while (view.pollToken === token && view.job?.operation_id === operationId && !TERMINAL_STATES.has(view.job.state)) {
        await delay(500);
        try {
          const payload = await this.call("/api/files/jobs/status", { operation_id: operationId }, view.identity);
          view.job = payload.jobs[0];
          if (this.state === view) this.renderTask();
        } catch (error) {
          this.notify(error.message, "error");
          return;
        }
      }
      if (TERMINAL_STATES.has(view.job?.state) && view.clipboard) {
        const completedSources = new Set((view.job.results || [])
          .filter((result) => result.status === "succeeded")
          .map((result) => result.source));
        if ((view.job.kind === "move" && view.clipboard.mode === "move") || view.job.kind === "delete") {
          view.clipboard.sources = view.clipboard.sources.filter((source) => !completedSources.has(source));
          if (!view.clipboard.sources.length) view.clipboard = null;
        }
      }
      await this.load(view.path, view.roots, view);
    }

    async uploadFiles(files) {
      const view = this.state;
      for (let index = 0; index < files.length; index += 1) {
        const file = files[index];
        try {
          await this.uploadFile(view, file, `${file.name} · ${index + 1}/${files.length}`);
        } catch (error) {
          this.notify(`${file.name}：${error.message}`, "error");
          if (view.upload?.cancelled) break;
        }
      }
      view.upload = null;
      if (this.state === view) this.renderTask();
      await this.load(view.path, false, view);
    }

    async uploadFile(view, file, label) {
      let created = await this.call("/api/files/uploads/create", {
        destination: view.path,
        name: file.name,
        size_bytes: file.size,
        conflict_policy: null,
      }, view.identity);
      if (created.requires_confirmation) {
        if (this.state !== view) return;
        const values = await this.openDialog({
          title: "处理上传冲突",
          body: `<p>目标目录已存在“${escapeHtml(file.name)}”。</p><label>处理方式<select name="policy"><option value="skip">跳过</option><option value="keep_both">保留两者</option><option value="replace">替换</option></select></label>`,
          confirm: "继续",
        });
        if (!values) return;
        created = await this.call("/api/files/uploads/create", {
          destination: view.path,
          name: file.name,
          size_bytes: file.size,
          conflict_policy: values.policy,
        }, view.identity);
      }
      const upload = created.upload;
      if (!upload || upload.state === "skipped") return;
      view.upload = { id: upload.upload_id, label: `上传 ${label}`, done: 0, total: file.size, cancelled: false };
      if (this.state === view) this.renderTask();
      try {
        let offset = 0;
        while (offset < file.size) {
          if (view.upload.cancelled) throw new Error("上传已取消");
          const buffer = new Uint8Array(await file.slice(offset, offset + UPLOAD_CHUNK_BYTES).arrayBuffer());
          await this.call("/api/files/uploads/chunk", {
            upload_id: upload.upload_id,
            offset,
            data: bytesToBase64(buffer),
          }, view.identity);
          offset += buffer.byteLength;
          view.upload.done = offset;
          if (this.state === view) this.renderTask();
        }
        const finished = await this.call("/api/files/uploads/finish", { upload_id: upload.upload_id }, view.identity);
        if (finished.state !== "completed") throw new Error(finished.error || "上传未完整完成");
      } catch (error) {
        await this.call("/api/files/uploads/cancel", { upload_id: upload.upload_id }, view.identity).catch(() => {});
        throw error;
      }
    }

    async downloadSelected() {
      const view = this.state;
      const identity = view.identity;
      const sources = this.selectedPaths();
      try {
        let download = await this.call("/api/files/downloads/create", { sources }, identity);
        view.download = { id: download.download_id, label: `准备下载 ${download.filename}`, done: 0, total: download.size_bytes || 1, cancelled: false };
        if (this.state === view) this.renderTask();
        while (download.state === "preparing") {
          if (view.download.cancelled) {
            await this.call("/api/files/downloads/cancel", { download_id: download.download_id }, identity).catch(() => {});
            view.download = null;
            if (this.state === view) this.renderTask();
            return;
          }
          await delay(500);
          download = await this.call("/api/files/downloads/status", { download_id: download.download_id }, identity);
        }
        if (download.state !== "ready") throw new Error(download.error || "无法准备下载");
        view.download.done = download.size_bytes || 1;
        view.download.total = download.size_bytes || 1;
        if (this.state === view) this.renderTask();
        if (this.downloadFile) {
          const saved = await this.downloadFile(download, identity);
          this.notify(`已保存到 ${saved.path}`, "success");
        } else {
          const anchor = document.createElement("a");
          anchor.href = this.downloadUrl(download.download_id, identity);
          anchor.download = download.filename;
          anchor.rel = "noopener";
          document.body.append(anchor);
          anchor.click();
          anchor.remove();
        }
        view.download = null;
        if (this.state === view) this.renderTask();
      } catch (error) {
        view.download = null;
        if (this.state === view) this.renderTask();
        this.notify(error.message, "error");
      }
    }

    async cancelTask() {
      const view = this.state;
      if (view.upload) {
        view.upload.cancelled = true;
        return;
      }
      if (view.download) {
        view.download.cancelled = true;
        return;
      }
      if (view.job?.cancellable) {
        try {
          view.pollToken += 1;
          view.job = await this.call("/api/files/jobs/cancel", { operation_id: view.job.operation_id }, view.identity);
          if (this.state === view) this.renderTask();
          await this.load(view.path, view.roots, view);
        } catch (error) { this.notify(error.message, "error"); }
      }
    }

    openDialog({ title, body, confirm, danger = false }) {
      if (this.dialogResolve) this.closeDialog(null);
      this.dialogTitle.textContent = title;
      this.dialogBody.innerHTML = body;
      this.dialogConfirm.textContent = confirm;
      this.dialogConfirm.classList.toggle("danger-button", danger);
      this.dialogBackdrop.classList.remove("hidden");
      const first = this.dialogBody.querySelector("input,select");
      setTimeout(() => first?.focus(), 0);
      return new Promise((resolve) => { this.dialogResolve = resolve; });
    }

    closeDialog(value) {
      if (!this.dialogResolve) return;
      const resolve = this.dialogResolve;
      this.dialogResolve = null;
      this.dialogBackdrop.classList.add("hidden");
      this.dialogBody.innerHTML = "";
      resolve(value);
    }
  }

  function actionButton(action, label, danger = false) {
    return `<button type="button" class="file-manager-icon-button${danger ? " danger" : ""}" data-file-action="${action}" title="${label}" aria-label="${label}">${actionIcon(action)}</button>`;
  }

  function actionIcon(action) {
    const paths = {
      back: '<path d="m15 18-6-6 6-6"/>',
      up: '<path d="m6 11 6-6 6 6M12 5v14"/>',
      refresh: '<path d="M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6"/>',
      go: '<path d="m9 18 6-6-6-6"/>',
      "select-all": '<rect x="4" y="4" width="16" height="16" rx="2"/><path d="m8 12 2.5 2.5L16 9"/>',
      mkdir: '<path d="M3 6.5h7l2 2h9v10H3zM15 12v5M12.5 14.5h5"/>',
      rename: '<path d="m4 20 4.2-1 10-10a2.1 2.1 0 0 0-3-3l-10 10L4 20ZM13.8 7.2l3 3"/>',
      copy: '<rect x="8" y="8" width="11" height="11" rx="2"/><path d="M16 8V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h3"/>',
      "copy-path": '<path d="M10 13a4 4 0 0 0 5.7 0l2-2a4 4 0 0 0-5.7-5.7l-1 1M14 11a4 4 0 0 0-5.7 0l-2 2A4 4 0 0 0 12 18.7l1-1"/>',
      cut: '<circle cx="6" cy="7" r="3"/><circle cx="6" cy="17" r="3"/><path d="m8.5 8.5 10 7M8.5 15.5l10-7"/>',
      paste: '<path d="M9 5H6v16h12V5h-3M9 3h6v4H9z"/>',
      move: '<path d="M3 7h7l2 2h9v10H3zM8 14h8M13 11l3 3-3 3"/>',
      upload: '<path d="M12 16V4m-4 4 4-4 4 4M5 14v6h14v-6"/>',
      download: '<path d="M12 4v12m-4-4 4 4 4-4M5 14v6h14v-6"/>',
      delete: '<path d="M4 7h16M9 7V4h6v3m3 0-1 14H7L6 7m4 4v6m4-6v6"/>',
    };
    return `<svg viewBox="0 0 24 24" aria-hidden="true">${paths[action] || ""}</svg>`;
  }

  function kindIcon(kind) {
    if (kind === "directory" || kind === "root" || kind === "drive") return '<svg viewBox="0 0 24 24"><path d="M3 6.5h7l2 2h9v10H3z"/></svg>';
    if (kind === "symlink") return '<svg viewBox="0 0 24 24"><path d="M10 13a4 4 0 0 0 5.7 0l2-2a4 4 0 0 0-5.7-5.7l-1 1M14 11a4 4 0 0 0-5.7 0l-2 2A4 4 0 0 0 12 18.7l1-1"/></svg>';
    return '<svg viewBox="0 0 24 24"><path d="M6 3h8l4 4v14H6zM14 3v5h5"/></svg>';
  }

  function sameLocation(left, right) {
    return Boolean(left?.roots) === Boolean(right?.roots) && (left?.path || null) === (right?.path || null);
  }

  function kindLabel(kind) {
    return ({ directory: "文件夹", file: "文件", symlink: "符号链接", special: "特殊文件", root: "根目录", drive: "磁盘", batch: "批次冲突" })[kind] || kind || "—";
  }

  function jobTitle(kind) {
    return ({ copy: "复制", move: "移动", delete: "永久删除" })[kind] || "文件任务";
  }

  function stateLabel(state) {
    return ({ planning: "正在规划", awaiting_confirmation: "等待确认", running: "正在执行", completed: "已完成", failed: "存在失败", cancelled: "已取消" })[state] || state;
  }

  function formatBytes(value) {
    if (value == null) return "—";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let size = Number(value);
    let unit = 0;
    while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit += 1; }
    return `${unit ? size.toFixed(size >= 10 ? 1 : 2) : Math.round(size)} ${units[unit]}`;
  }

  function formatDate(value) {
    if (!value) return "—";
    try { return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(value)); }
    catch (_) { return new Date(value).toLocaleString(); }
  }

  function baseName(path) {
    const pieces = String(path || "").replace(/[\\/]+$/, "").split(/[\\/]/);
    return pieces[pieces.length - 1] || path || "";
  }

  function bytesToBase64(bytes) {
    let binary = "";
    for (let offset = 0; offset < bytes.length; offset += 0x8000) {
      binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
    }
    return btoa(binary);
  }

  function delay(ms) { return new Promise((resolve) => setTimeout(resolve, ms)); }
  function escapeHtml(value) { return String(value ?? "").replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[character]); }
  function escapeAttr(value) { return escapeHtml(value).replace(/`/g, "&#96;"); }

  global.MeFileManager = Object.freeze({ create });
})(globalThis);
