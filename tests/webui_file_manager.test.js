"use strict";

const { describe, expect, test } = require("bun:test");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

const root = join(import.meta.dir, "..");
const read = (path) => readFileSync(join(root, path), "utf8");

const controller = read("src/webui/file-manager.js");
const directHtml = read("src/webui/index.html");
const gatewayHtml = read("src/gateway_webui/index.html");
const directApp = read("src/webui/app.js");
const gatewayApp = read("src/gateway_webui/app.js");
const directStyle = read("src/webui/style.css");
const gatewayStyle = read("src/gateway_webui/style.css");
const hostFiles = read("src/host_files.rs");
const gateway = read("src/gateway.rs");

describe("per-session host file manager", () => {
  test("both WebUIs expose the fixed Files tab and load the shared controller before app.js", () => {
    for (const html of [directHtml, gatewayHtml]) {
      expect(html).toContain('data-view="files"');
      expect(html).toContain('id="files-view"');
      expect(html).toContain('id="file-manager"');
      expect(html.indexOf('/file-manager.js')).toBeGreaterThan(0);
      expect(html.indexOf('/file-manager.js')).toBeLessThan(html.indexOf('/app.js'));
    }
  });

  test("both WebUIs present WorkMap as 工作图", () => {
    for (const html of [directHtml, gatewayHtml]) {
      expect(html).toContain('data-view="workmap">工作图</button>');
      expect(html).toContain('<strong>工作图</strong>');
    }
  });

  test("direct and Gateway identities isolate page state without persisting it", () => {
    expect(directApp).toContain('key: `direct:${state.selectedAgent}`');
    expect(gatewayApp).toContain('key: `${state.workspaceId}:${state.selectedAgent}`');
    expect(controller).toContain("this.states = new Map()");
    expect(controller).toContain("identity: { ...identity }");
    expect(controller).not.toContain("localStorage");
    expect(controller).not.toContain("sessionStorage");
    expect(controller).not.toContain("indexedDB");
    expect(controller).not.toContain("MeEdbCache");
  });

  test("Gateway scopes only the formal files protocol to the selected Workspace", () => {
    expect(gatewayApp).toContain('path.startsWith("/api/files/")');
    expect(gatewayApp).toContain('api(path, options, identity.workspaceId)');
    expect(gatewayApp).toContain('scopedApiPath(`/api/files/downloads/');
    expect(gateway).toContain('"files/jobs/prepare"');
    expect(gateway).toContain('"files/uploads/chunk"');
    expect(gateway).toContain('parse_file_download_content_path');
    expect(gateway).toContain("body: Box<dyn Read + Send>");
    expect(gateway).not.toContain('response.bytes().map_err(|_| "工作区响应未能完成")');
  });

  test("navigation uses per-session history, icon controls and guarded touch double-tap", () => {
    expect(controller).toContain("history: []");
    expect(controller).toContain("NAVIGATION_HISTORY_LIMIT = 100");
    expect(controller).toContain('actionButton("back", "后退")');
    expect(controller).not.toContain('data-file-action="roots"');
    expect(controller).toContain('if (action === "back") return this.goBack()');
    expect(controller).toContain("recordHistory && previous && !sameLocation(previous, next)");
    expect(controller).toContain('event.pointerType !== "touch"');
    expect(controller).toContain("TOUCH_DOUBLE_TAP_MS = 450");
    expect(controller).toContain("TOUCH_TAP_MOVE_PX = 12");
    expect(controller).toContain("gesture.moved || Math.hypot");
    expect(controller).toContain("this.suppressTouchClickPath = gesture.path");
    expect(controller).toContain('this.container.addEventListener("dblclick"');
    expect(controller).toContain('if (row?.dataset.navigable === "true") void this.navigate(row.dataset.path, false);');
    const selectStart = controller.indexOf("    selectRow(path, event) {");
    const selectEnd = controller.indexOf("\n    togglePath(path, checked)", selectStart);
    const selectionPath = controller.slice(selectStart, selectEnd);
    expect(selectionPath).toContain("this.renderSelection();");
    expect(selectionPath).not.toContain("this.render();");
    expect(controller).toContain('this.list.querySelectorAll(".file-manager-entry").forEach((row) => {');
    expect(controller).toContain('row.classList.toggle("selected", rowSelected);');
    const directoryClickStart = gatewayApp.indexOf('row.addEventListener("click", () => {');
    const directoryClickEnd = gatewayApp.indexOf('\n    row.addEventListener("dblclick"', directoryClickStart);
    const directoryClick = gatewayApp.slice(directoryClickStart, directoryClickEnd);
    expect(directoryClick).toContain("updateDirectorySelection(directory, list, allEntries);");
    expect(directoryClick).not.toContain("renderDirectoryRows();");
    for (const action of ["select-all", "mkdir", "rename", "copy-path", "copy", "cut", "paste", "move", "upload", "download", "delete"]) {
      expect(controller).toContain(`actionButton("${action}",`);
    }
    for (const style of [directStyle, gatewayStyle]) {
      expect(style).toContain(".file-manager-icon-button svg");
      expect(style).toContain("touch-action: manipulation");
      expect(style).toContain("width: 40px; min-width: 40px; height: 40px");
    }
  });

  test("copy, move and delete submit one top-level server job rather than browser recursion", () => {
    expect(controller).toContain('this.call("/api/files/jobs/prepare", { kind, sources, destination }');
    expect(controller).toContain('conflict_policy: policy');
    expect(controller).toContain('replace_directories: values.replace_directories === "yes"');
    expect(controller).toContain("这些项目将被永久删除，无法恢复");
    expect(controller).toContain("我确认替换非空目录会永久删除目标目录原有内容");
    expect(controller).not.toContain("webkitGetAsEntry");
    expect(controller).not.toContain("readEntries(");
    expect(controller).not.toContain("showDirectoryPicker");
    expect(controller).not.toContain("FileSystemHandle");
  });

  test("selection, logical clipboard, bounded uploads and server-generated downloads stay presentation-side", () => {
    expect(controller).toContain("event.shiftKey");
    expect(controller).toContain("event.metaKey || event.ctrlKey");
    expect(controller).toContain('clipboard = { mode: action === "copy" ? "copy" : "move", sources: this.selectedPaths() }');
    expect(controller).toContain('actionButton("copy-path", "复制绝对路径")');
    expect(controller).toContain('this.setDisabled("copy-path", selected === 0 || this.state.loading);');
    expect(controller).toContain('if (action === "copy-path") return this.copySelectedPaths();');
    expect(controller).toContain('await this.writeClipboard(paths.join(";"));');
    expect(controller).toContain('paths.length === 1 ? "已复制绝对路径" : `已复制 ${paths.length} 个绝对路径`');
    for (const app of [directApp, gatewayApp]) {
      expect(app).toContain("writeClipboard: copyTextToClipboard");
    }
    expect(controller).toContain("const UPLOAD_CHUNK_BYTES = 384 * 1024");
    expect(controller).toContain('this.call("/api/files/uploads/create"');
    expect(controller).toContain('this.call("/api/files/uploads/chunk"');
    expect(controller).toContain('this.call("/api/files/uploads/finish"');
    expect(controller).toContain('this.call("/api/files/uploads/cancel"');
    expect(controller).toContain("const completedSources = new Set");
    expect(controller).toContain('this.call("/api/files/downloads/create"');
    expect(controller).toContain("anchor.href = this.downloadUrl(download.download_id, identity)");
    expect(hostFiles).toContain("prepare_archive(worker_record, sources, temp_path, shutdown)");
    expect(hostFiles).toContain("archive.follow_symlinks(false)");
  });

  test("me-s remains the authority for traversal, conflicts, progress and permanent deletion", () => {
    expect(hostFiles).toContain("fn collect_stats(path: &Path)");
    expect(hostFiles).toContain("fn discover_conflicts(");
    expect(hostFiles).toContain("fn copy_tree(");
    expect(hostFiles).toContain("fn remove_tree(");
    expect(hostFiles).toContain("fs::symlink_metadata");
    expect(hostFiles).toContain("HostFileJobState::AwaitingConfirmation");
    expect(hostFiles).toContain("HostFileJobState::Running");
    expect(hostFiles).toContain("Replacing a directory requires explicit confirmation");
    expect(hostFiles).toContain("Filesystem roots cannot be copied, moved, or deleted");
    expect(hostFiles).toContain("fn commit_temp_path(");
    expect(hostFiles).toContain("filesystem_lock: Arc<Mutex<()>>");
  });
});
