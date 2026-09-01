(() => {
  "use strict";

  const registry = new Map();

  const KNOWN_TOOLS = [
    "SetTitle", "CurrentTime", "Compact",
    "WorkMap.Read", "WorkMap.ReadHistory", "WorkMap.Start", "WorkMap.UpdatePlanState",
    "WorkMap.AddNote", "WorkMap.ChangePlan", "WorkMap.AddPlan", "WorkMap.CloseObjective",
    "WorkMap.AddMemory", "WorkMap.InvalidateMemory",
    "Image.Info", "Image.View",
    "File.Read", "File.ReadBytes", "File.EditBytes", "File.List", "File.Find", "File.Search",
    "File.Stat", "File.MakeDirectory", "File.Create", "File.Edit", "File.Append", "File.Replace",
    "File.Copy", "File.Move", "File.Delete",
    "Terminal.Create", "Terminal.Interact", "Terminal.Status", "Terminal.List", "Terminal.Kill",
    "Desktop.Play",
    "WebBrowser.Create", "WebBrowser.Navigate", "WebBrowser.Click", "WebBrowser.Type",
    "WebBrowser.Press", "WebBrowser.Scroll", "WebBrowser.RequireHumanAction",
    "WebBrowser.Snapshot", "WebBrowser.Pages", "WebBrowser.Back", "WebBrowser.Close",
    "Worker.Ask", "Worker.Wait", "Worker.Stop", "Worker.ClearContext",
    "Agent.Create", "Agent.Wait", "Agent.Ask", "Agent.Stop", "Agent.ClearContext", "Agent.Kill",
  ];

  function objectValue(value) {
    return value && typeof value === "object" && !Array.isArray(value) ? value : {};
  }

  function safeJson(value) {
    if (value && typeof value === "object") return value;
    if (typeof value !== "string" || !value.trim()) return null;
    try { return JSON.parse(value); } catch { return null; }
  }

  function normalized(value) {
    return String(value ?? "").trim().toLowerCase();
  }

  function resultValue(output) {
    return output?.result?.value ?? null;
  }

  function resultState(output) {
    return normalized(output?.result?.state);
  }

  function resultSucceeded(output) {
    return resultState(output) === "succeeded";
  }

  function resultFailed(output) {
    return output?.result && !resultSucceeded(output);
  }

  function rawResultText(output) {
    const raw = output?.result?.rawDetail;
    if (typeof raw === "string" && raw.trim()) return raw.trim();
    const value = resultValue(output);
    if (typeof value === "string") return value;
    return value == null ? "" : JSON.stringify(value, null, 2);
  }

  function preview(value, limit = 96) {
    const text = String(value ?? "").trim().replace(/\s+/g, " ");
    if (!text) return "";
    return text.length > limit ? `${text.slice(0, Math.max(1, limit - 1))}…` : text;
  }

  function quotedPreview(value, limit = 64) {
    const text = preview(value, limit);
    return text ? `“${text}”` : "";
  }

  function formatNumber(value) {
    const number = Number(value);
    return Number.isFinite(number) ? number.toLocaleString("zh-CN") : String(value ?? "");
  }

  function formatBytes(value) {
    const bytes = Number(value);
    if (!Number.isFinite(bytes) || bytes < 0) return String(value ?? "");
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KiB`;
    if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(bytes < 10 * 1024 ** 2 ? 1 : 0)} MiB`;
    return `${(bytes / 1024 ** 3).toFixed(1)} GiB`;
  }

  function countLines(value) {
    const text = String(value ?? "");
    if (!text) return 0;
    return text.split(/\r\n|\r|\n/).length;
  }

  function itemCount(value, unit) {
    const count = Array.isArray(value) ? value.length : Number(value) || 0;
    return `${formatNumber(count)} ${unit}`;
  }

  function compactUrl(value) {
    const text = String(value ?? "");
    try {
      const url = new URL(text);
      return `${url.host}${url.pathname === "/" ? "" : url.pathname}`;
    } catch {
      return preview(text, 100);
    }
  }

  function lineRange(input) {
    const start = input.start_line;
    const end = input.end_line;
    if (start != null && end != null) return `第 ${start}–${end} 行`;
    if (start != null) return `从第 ${start} 行起`;
    if (end != null) return `截至第 ${end} 行`;
    return "全文";
  }

  function labelState(value) {
    const labels = {
      active: "进行中", planned: "待执行", completed: "已完成", cancelled: "已取消",
      superseded: "已取代", working: "执行中", pending: "等待中", interrupted: "已中断",
      failed: "失败", stopped: "已停止", killed: "已结束", cleared: "已清空",
      wait_interrupted: "等待已中断", open: "打开", closed: "关闭", running: "运行中",
      exited: "已退出", succeeded: "成功",
    };
    return labels[normalized(value)] || String(value ?? "");
  }

  function fieldBlock(title, entries) {
    return {
      type: "fields",
      title,
      entries: entries
        .filter((entry) => entry && entry[1] !== undefined && entry[1] !== null && entry[1] !== "")
        .map(([label, value, tone]) => ({ label, value: String(value), tone: tone || "" })),
    };
  }

  function codeBlock(title, content, options = {}) {
    return { type: "code", title, content: String(content ?? ""), ...options };
  }

  function tableBlock(title, columns, rows) {
    return {
      type: "table",
      title,
      columns: columns.map((column) => typeof column === "string" ? { key: column, label: column } : column),
      rows: rows.map((row) => objectValue(row)),
    };
  }

  function listBlock(title, items, ordered = false) {
    return { type: "list", title, items: items.map((item) => String(item)), ordered };
  }

  function noticeBlock(title, text, tone = "info") {
    return { type: "notice", title, text: String(text ?? ""), tone };
  }

  function treeBlock(title, content) {
    return { type: "tree", title, content: String(content ?? "") };
  }

  function terminalBlock(title, content) {
    return { type: "terminal", title, content: String(content ?? "") };
  }

  function define(name, definition) {
    if (registry.has(name)) throw new Error(`duplicate tool presenter: ${name}`);
    registry.set(name, {
      title: definition.title || name,
      icon: definition.icon || "tool",
      summary: definition.summary || (() => ""),
      input: definition.input || ((input) => genericObjectBlocks("参数", input)),
      output: definition.output || ((input, output) => genericOutputBlocks(output)),
    });
  }

  function genericObjectBlocks(title, value) {
    const object = objectValue(value);
    const entries = [];
    const complex = [];
    for (const [key, item] of Object.entries(object)) {
      if (item === null || item === undefined || item === "") continue;
      if (["string", "number", "boolean"].includes(typeof item)) entries.push([humanizeKey(key), item]);
      else complex.push([humanizeKey(key), item]);
    }
    const blocks = [];
    if (entries.length) blocks.push(fieldBlock(title, entries));
    for (const [label, item] of complex) {
      blocks.push(codeBlock(label, JSON.stringify(item, null, 2)));
    }
    if (!blocks.length) blocks.push(noticeBlock(title, "无参数", "muted"));
    return blocks;
  }

  function genericOutputBlocks(output) {
    const value = resultValue(output);
    const blocks = [];
    if (output?.text) blocks.push(codeBlock("输出", output.text));
    if (value && typeof value === "object") blocks.push(...genericObjectBlocks("结果", value));
    else if (value !== null && value !== undefined && value !== "") blocks.push(codeBlock("结果", value));
    if (!blocks.length && output?.result) blocks.push(noticeBlock("结果", resultSucceeded(output) ? "执行成功" : rawResultText(output), resultSucceeded(output) ? "success" : "error"));
    return blocks;
  }

  function humanizeKey(key) {
    return String(key).replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase());
  }

  function unknownPresenter(name) {
    return {
      title: name || "未知工具",
      icon: "tool",
      summary(input) {
        const object = objectValue(input);
        for (const key of ["path", "url", "page_id", "session_id", "query", "name"]) {
          if (object[key] != null) return preview(object[key], 120);
        }
        return preview(JSON.stringify(object), 120);
      },
      input(input) { return genericObjectBlocks("参数", input); },
      output(input, output) { return genericOutputBlocks(output); },
    };
  }

  function failureBlock(output) {
    if (!resultFailed(output)) return null;
    const raw = rawResultText(output);
    const parsed = safeJson(raw);
    const message = parsed?.message || parsed?.error?.message || raw || labelState(resultState(output));
    const code = parsed?.code || parsed?.error?.code || "";
    const tip = parsed?.tip || parsed?.error?.tip || "";
    const parts = [message];
    if (code) parts.push(`错误码：${code}`);
    if (tip) parts.push(tip);
    return noticeBlock("执行失败", parts.join("\n"), "error");
  }

  function describe(name, inputJson, outputJson) {
    const input = objectValue(inputJson);
    const presenter = registry.get(name) || unknownPresenter(name);
    let inputBlocks;
    let outputBlocks = [];
    try { inputBlocks = presenter.input(input); } catch { inputBlocks = genericObjectBlocks("参数", input); }
    if (outputJson !== undefined) {
      try { outputBlocks = presenter.output(input, outputJson) || []; }
      catch { outputBlocks = genericOutputBlocks(outputJson); }
      const failure = failureBlock(outputJson);
      if (failure) outputBlocks.unshift(failure);
    }
    return {
      inputBlocks: Array.isArray(inputBlocks) ? inputBlocks.filter(Boolean) : [],
      outputBlocks: Array.isArray(outputBlocks) ? outputBlocks.filter(Boolean) : [],
      rawInput: input,
      rawOutput: outputJson,
    };
  }

  function summarize(name, inputJson) {
    const input = objectValue(inputJson);
    const presenter = registry.get(name) || unknownPresenter(name);
    let summary = "";
    try { summary = presenter.summary(input); } catch { summary = unknownPresenter(name).summary(input); }
    return {
      icon: presenter.icon,
      title: presenter.title,
      summary: preview(summary, 240),
      known: registry.has(name),
    };
  }

  function present(name, inputJson, outputJson) {
    return { ...summarize(name, inputJson), details: describe(name, inputJson, outputJson) };
  }

  function escapeHtml(value) {
    return String(value ?? "")
      .replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;").replaceAll("'", "&#039;");
  }

  function renderBlock(block) {
    if (!block) return "";
    const title = block.title ? `<div class="tool-block-title">${escapeHtml(block.title)}</div>` : "";
    if (block.type === "fields") {
      const rows = block.entries.map((entry) => `<div class="tool-field"><span class="tool-field-label">${escapeHtml(entry.label)}</span><span class="tool-field-value ${escapeHtml(entry.tone || "")}">${escapeHtml(entry.value)}</span></div>`).join("");
      return `<section class="tool-block tool-fields">${title}<div class="tool-field-grid">${rows}</div></section>`;
    }
    if (block.type === "code" || block.type === "tree" || block.type === "terminal") {
      return `<section class="tool-block tool-${escapeHtml(block.type)}">${title}<pre>${escapeHtml(block.content)}</pre></section>`;
    }
    if (block.type === "table") {
      const head = block.columns.map((column) => `<th>${escapeHtml(column.label)}</th>`).join("");
      const rows = block.rows.map((row) => `<tr>${block.columns.map((column) => `<td>${escapeHtml(row[column.key] ?? "")}</td>`).join("")}</tr>`).join("");
      return `<section class="tool-block tool-table-block">${title}<div class="tool-table-scroll"><table><thead><tr>${head}</tr></thead><tbody>${rows}</tbody></table></div></section>`;
    }
    if (block.type === "list") {
      const tag = block.ordered ? "ol" : "ul";
      return `<section class="tool-block tool-list">${title}<${tag}>${block.items.map((item) => `<li>${escapeHtml(item)}</li>`).join("")}</${tag}></section>`;
    }
    if (block.type === "notice") {
      return `<section class="tool-block tool-notice ${escapeHtml(block.tone || "info")}">${title}<div>${escapeHtml(block.text)}</div></section>`;
    }
    return "";
  }

  function renderDetails(details) {
    const input = details.inputBlocks.map(renderBlock).join("");
    const output = details.outputBlocks.length
      ? `<section class="tool-section tool-output-section" data-reconcile-key="output"><div class="tool-section-title">输出</div>${details.outputBlocks.map(renderBlock).join("")}</section>`
      : "";
    const raw = `<details class="tool-raw" data-reconcile-key="raw" data-reconcile-preserve-open="true"><summary>原始数据</summary><div class="tool-raw-grid"><section><div class="tool-block-title">输入 JSON</div><pre>${escapeHtml(JSON.stringify(details.rawInput, null, 2))}</pre></section>${details.rawOutput === undefined ? "" : `<section><div class="tool-block-title">输出 JSON</div><pre>${escapeHtml(JSON.stringify(details.rawOutput, null, 2))}</pre></section>`}</div></details>`;
    return `<div class="tool-details"><section class="tool-section tool-input-section" data-reconcile-key="input"><div class="tool-section-title">输入</div>${input}</section>${output}${raw}</div>`;
  }

  function outputTip(value) {
    return value?.tip ? noticeBlock("提示", value.tip, "info") : null;
  }

  function linesContent(lines) {
    return Object.entries(objectValue(lines)).map(([line, content]) => {
      const value = typeof content === "string" ? content : JSON.stringify(content);
      return `${String(line).padStart(6, " ")}  ${value}`;
    }).join("\n");
  }

  function fileMetadata(value, extra = []) {
    return fieldBlock("文件信息", [
      ["路径", value?.path], ["大小", value?.size == null ? "" : formatBytes(value.size)],
      ["编码", value?.encoding], ["Hash", value?.hash || value?.previous_hash],
      ["BOM", value?.bom == null ? "" : value.bom ? "有" : "无"], ...extra,
    ]);
  }

  function editInputRows(edits, bytes = false) {
    return (edits || []).map((edit, index) => {
      const operation = edit.operation || (Number(edit.target_length) === 0 ? "insert" : edit.data === "" ? "delete" : "replace");
      const location = bytes
        ? `偏移 ${edit.target_offset ?? 0}，${formatNumber(edit.target_length ?? 0)} 字节`
        : operation === "insert" ? `第 ${edit.before_line} 行前` : `第 ${edit.start_line}–${edit.end_line} 行`;
      const content = bytes ? String(edit.data || "") : Array.isArray(edit.new_lines) ? edit.new_lines.join("\n") : "";
      return {
        index: index + 1,
        operation: ({ replace: "替换", delete: "删除", insert: "插入" })[operation] || operation,
        location,
        amount: content ? bytes ? `${content.trim().split(/\s+/).filter(Boolean).length} 字节` : `${edit.new_lines.length} 行` : "—",
      };
    });
  }

  const fileIcon = "file";

  define("File.Read", {
    title: "读取文件", icon: fileIcon,
    summary: (input) => `${input.path || ""} · ${lineRange(input)}`,
    input: (input) => [fieldBlock("读取范围", [["文件", input.path], ["范围", lineRange(input)], ["编码", input.encoding && input.encoding !== "auto" ? input.encoding : "自动检测"]])],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const actual = value.start_line == null ? "没有返回行" : `第 ${value.start_line}–${value.end_line} 行`;
      const blocks = [
        fieldBlock("读取结果", [["实际范围", actual], ["总行数", formatNumber(value.total_lines)], ["到达文件末尾", value.eof ? "是" : "否"], ["截断", value.truncated ? "是" : "否"]]),
        fileMetadata(value),
      ];
      if (Object.keys(objectValue(value.lines)).length) blocks.push(codeBlock("文件内容", linesContent(value.lines)));
      if (Array.isArray(value.editable_ranges) && value.editable_ranges.length) blocks.push(listBlock("已授权编辑范围", value.editable_ranges.map((range) => `第 ${range.start_line}–${range.end_line} 行`)));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  define("File.ReadBytes", {
    title: "读取字节", icon: fileIcon,
    summary: (input) => `${input.path || ""} · 偏移 ${input.offset ?? 0} · ${formatBytes(input.length ?? 65536)}`,
    input: (input) => [fieldBlock("读取范围", [["文件", input.path], ["起始偏移", input.offset ?? 0], ["请求长度", formatBytes(input.length ?? 65536)]])],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fileMetadata(value, [["返回偏移", value.offset], ["返回长度", formatBytes(value.length)], ["到达文件末尾", value.eof ? "是" : "否"]])];
      if (value.data) blocks.push(codeBlock("十六进制数据", value.data));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  define("File.EditBytes", {
    title: "编辑二进制", icon: fileIcon,
    summary: (input) => `${input.path || ""} · ${itemCount(input.edits, "处修改")}`,
    input: (input) => [
      fieldBlock("目标文件", [["文件", input.path], ["预期 Hash", input.expected_hash]]),
      tableBlock("修改操作", [{ key: "index", label: "#" }, { key: "operation", label: "操作" }, { key: "location", label: "位置" }, { key: "amount", label: "新内容" }], editInputRows(input.edits, true)),
    ],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("编辑结果", [["文件", value.path], ["原大小", formatBytes(value.previous_size)], ["新大小", formatBytes(value.size)], ["原 Hash", value.previous_hash]])];
      if (Array.isArray(value.edit_results)) blocks.push(tableBlock("已提交操作", [
        { key: "index", label: "#" }, { key: "kind", label: "操作" }, { key: "target_offset", label: "偏移" },
        { key: "selected_bytes", label: "原字节" }, { key: "replacement_bytes", label: "新字节" },
      ], value.edit_results));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  define("File.List", {
    title: "浏览目录", icon: "folder",
    summary: (input) => `${input.path || "."} · 深度 ${input.depth ?? 1}`,
    input: (input) => [fieldBlock("浏览选项", [["目录", input.path || "."], ["深度", input.depth ?? 1], ["隐藏项", input.include_hidden ? "包含" : "不包含"], ["返回上限", input.max_entries ?? 1000]])],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const entries = (value.entries || []).map((entry) => ({
        name: entry.name || entry.path || "", type: entry.type || entry.kind || "", size: entry.size == null ? "" : formatBytes(entry.size), modified: entry.modified || entry.modified_ms || "",
      }));
      const blocks = [fieldBlock("结果", [["目录", value.path], ["返回数量", value.returned], ["截断", value.truncated ? "是" : "否"]])];
      if (entries.length) blocks.push(tableBlock("目录内容", [{ key: "name", label: "名称" }, { key: "type", label: "类型" }, { key: "size", label: "大小" }, { key: "modified", label: "修改时间" }], entries));
      else blocks.push(noticeBlock("目录内容", "没有可显示的条目", "muted"));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  define("File.Find", {
    title: "查找路径", icon: "search",
    summary: (input) => `${(input.patterns || []).length === 1 ? input.patterns[0] : `${(input.patterns || []).length} 个模式`} · ${input.path || "."}`,
    input: (input) => [
      fieldBlock("查找范围", [["目录", input.path || "."], ["深度", input.depth ?? "不限"], ["隐藏项", input.include_hidden ? "包含" : "不包含"]]),
      listBlock("匹配模式", input.patterns || []),
      ...(input.exclude?.length ? [listBlock("排除", input.exclude)] : []),
    ],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("结果", [["目录", value.path], ["找到", itemCount(value.returned, "项")], ["截断", value.truncated ? "是" : "否"]])];
      if (value.results?.length) blocks.push(listBlock("路径", value.results));
      else blocks.push(noticeBlock("路径", "没有匹配路径", "muted"));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  function searchMatchesText(matches) {
    return (matches || []).map((match) => {
      const lines = { ...objectValue(match.before), ...objectValue(match.match_text), ...objectValue(match.after) };
      const body = linesContent(lines);
      return `${match.path}:${Object.keys(objectValue(match.match_text))[0] || "?"}:${match.column || 1}\n${body}`;
    }).join("\n\n");
  }

  define("File.Search", {
    title: "搜索文本", icon: "search",
    summary: (input) => `${quotedPreview(input.query)} · ${input.path || "."}${input.regex ? " · 正则" : ""}`,
    input: (input) => [fieldBlock("搜索条件", [["内容", input.query], ["模式", input.regex ? "正则表达式" : "文本"], ["区分大小写", input.case_sensitive === false ? "否" : "是"], ["目录", input.path || "."], ["文件过滤", input.globs?.join(", ")], ["深度", input.depth ?? "不限"], ["上下文", `前 ${input.context_before ?? 0} / 后 ${input.context_after ?? 0} 行`]])],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("结果", [["匹配", itemCount(value.returned, "处")], ["跳过二进制文件", value.skipped_binary], ["截断", value.truncated ? "是" : "否"]])];
      if (value.matches?.length) blocks.push(codeBlock("匹配内容", searchMatchesText(value.matches)));
      else blocks.push(noticeBlock("匹配内容", "没有找到匹配内容", "muted"));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  define("File.Stat", {
    title: "查看文件信息", icon: fileIcon,
    summary: (input) => input.paths?.length === 1 ? input.paths[0] : itemCount(input.paths, "个路径"),
    input: (input) => [listBlock("目标路径", input.paths || [])],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const rows = (value.entries || []).map((entry) => ({
        path: entry.path || "", exists: entry.exists === false ? "不存在" : "存在", type: entry.type || entry.kind || "", size: entry.size == null ? "" : formatBytes(entry.size), hash: entry.hash || "",
      }));
      const blocks = [fieldBlock("结果", [["返回数量", value.returned]])];
      if (rows.length) blocks.push(tableBlock("文件信息", [{ key: "path", label: "路径" }, { key: "exists", label: "状态" }, { key: "type", label: "类型" }, { key: "size", label: "大小" }, { key: "hash", label: "Hash" }], rows));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  define("File.MakeDirectory", {
    title: "新建目录", icon: "folder-add",
    summary: (input) => input.path || "",
    input: (input) => [fieldBlock("目录", [["路径", input.path], ["创建父目录", input.parents ? "是" : "否"]])],
    output: (input, output) => {
      const value = objectValue(resultValue(output));
      return [noticeBlock("结果", value.exists ? `目录已创建：${value.path}` : `未创建目录：${value.path}`, value.exists ? "success" : "warning")];
    },
  });

  function contentInput(input, verb) {
    return [
      fieldBlock("文件", [["路径", input.path], ["编码", input.encoding || "utf-8"], ["BOM", input.bom == null ? "" : input.bom ? "有" : "无"], ["预期 Hash", input.expected_hash], ["内容", `${countLines(input.content)} 行 · ${formatNumber(String(input.content || "").length)} 字符`]]),
      codeBlock(`${verb}内容`, input.content || ""),
    ];
  }

  function basicMutationOutput(output, message) {
    const value = objectValue(resultValue(output));
    const blocks = [noticeBlock("结果", `${message}：${value.path || ""}`, "success"), fileMetadata(value)];
    if (value.appended_bytes != null) blocks.push(fieldBlock("追加信息", [["追加字节数", formatBytes(value.appended_bytes)]]));
    return blocks;
  }

  define("File.Create", {
    title: "创建文件", icon: "file-add",
    summary: (input) => `${input.path || ""} · ${formatNumber(String(input.content || "").length)} 字符`,
    input: (input) => contentInput(input, "文件"),
    output: (input, output) => basicMutationOutput(output, "文件已创建"),
  });

  define("File.Edit", {
    title: "编辑文件", icon: "file-edit",
    summary: (input) => `${input.path || ""} · ${itemCount(input.edits, "处修改")}`,
    input(input) {
      const blocks = [fieldBlock("目标文件", [["文件", input.path], ["编码", input.encoding || "自动检测"]]), tableBlock("修改操作", [{ key: "index", label: "#" }, { key: "operation", label: "操作" }, { key: "location", label: "位置" }, { key: "amount", label: "新内容" }], editInputRows(input.edits))];
      (input.edits || []).forEach((edit, index) => {
        if (Array.isArray(edit.new_lines)) blocks.push(codeBlock(`修改 ${index + 1} 的新内容`, edit.new_lines.join("\n")));
      });
      return blocks;
    },
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [
        noticeBlock("结果", `已原子提交 ${value.edit_results?.length || 0} 处修改`, "success"),
        fieldBlock("文件变化", [["路径", value.path], ["原行数", value.previous_total_lines], ["新行数", value.total_lines], ["原大小", formatBytes(value.previous_size)], ["新大小", formatBytes(value.size)], ["编码", value.encoding]]),
      ];
      if (Array.isArray(value.edit_results)) blocks.push(tableBlock("已提交操作", [
        { key: "index", label: "#" }, { key: "operation", label: "操作" }, { key: "selected_lines", label: "原行数" }, { key: "new_line_count", label: "新行数" }, { key: "replacement_bytes", label: "写入字节" },
      ], value.edit_results));
      if (outputTip(value)) blocks.push(outputTip(value));
      return blocks;
    },
  });

  for (const [name, title, verb] of [
    ["File.Append", "追加文件", "追加"], ["File.Replace", "替换文件", "新文件"],
  ]) {
    define(name, {
      title, icon: "file-edit",
      summary: (input) => `${input.path || ""} · ${countLines(input.content)} 行`,
      input: (input) => contentInput(input, verb),
      output: (input, output) => basicMutationOutput(output, name === "File.Append" ? "内容已追加" : "文件已替换"),
    });
  }

  for (const [name, title, icon, success] of [
    ["File.Copy", "复制文件", "file-copy", "文件已复制"],
    ["File.Move", "移动文件", "file-move", "文件已移动"],
  ]) {
    define(name, {
      title, icon,
      summary: (input) => `${input.path || ""} → ${input.destination || ""}`,
      input: (input) => [fieldBlock("路径", [["来源", input.path], ["目标", input.destination], ["预期 Hash", input.expected_hash]])],
      output(input, output) {
        const value = objectValue(resultValue(output));
        return [noticeBlock("结果", success, "success"), fieldBlock("文件", [["来源", value.path], ["目标", value.destination], ["大小", formatBytes(value.size)], ["Hash", value.hash], ["原 Hash", value.previous_hash]])];
      },
    });
  }

  define("File.Delete", {
    title: "删除文件", icon: "file-delete",
    summary: (input) => input.path || "",
    input: (input) => [fieldBlock("删除目标", [["文件", input.path], ["预期 Hash", input.expected_hash]])],
    output(input, output) {
      const value = objectValue(resultValue(output));
      return [noticeBlock("结果", value.exists === false ? `文件已删除：${value.path}` : `文件仍然存在：${value.path}`, value.exists === false ? "success" : "warning"), fieldBlock("删除记录", [["已删除 Hash", value.deleted_hash]])];
    },
  });

  function terminalActionLabel(action) {
    if (action?.type === "text") {
      return Array.from(String(action.text || ""), (character) => character === "\n" || character === "\r" ? "↵" : character === "\t" ? "⇥" : character).join("");
    }
    if (action?.type === "key") {
      const modifiers = (action.modifiers || []).map((item) => item[0].toUpperCase() + item.slice(1));
      const key = String(action.key || "");
      const label = [...modifiers, key[0]?.toUpperCase() + key.slice(1)].filter(Boolean).join("+");
      return action.repeat > 1 ? `${label} ×${action.repeat}` : label;
    }
    return preview(JSON.stringify(action));
  }

  function terminalUpdatesText(output) {
    const chunks = [];
    for (const update of output?.updates || []) {
      const content = update?.content || update;
      if (content?.kind === "text") {
        if (content.value) chunks.push(content.value);
        continue;
      }
      const terminal = content?.kind === "terminal" ? content.value : content?.value?.rows ? content.value : content;
      if (!terminal?.rows) continue;
      const rows = terminal.rows.map((row) => {
        if (typeof row.text === "string") return row.text;
        let text = "", column = 0;
        for (const run of row.runs || []) {
          if (run.col > column) text += " ".repeat(run.col - column);
          text += run.text || "";
          column = run.col + (run.width || String(run.text || "").length);
        }
        return text.replace(/\s+$/, "");
      });
      chunks.push(rows.join("\n"));
    }
    if (!chunks.length && output?.text) chunks.push(output.text);
    return chunks.filter(Boolean).join("\n");
  }

  define("Terminal.Create", {
    title: "创建终端", icon: "terminal",
    summary: (input) => `${input.cwd || "."} · ${input.width ?? 120}×${input.height ?? 40}`,
    input: (input) => [fieldBlock("终端参数", [["工作目录", input.cwd || "."], ["尺寸", `${input.width ?? 120}×${input.height ?? 40}`], ["等待", `${input.wait_ms ?? 1000} ms`], ["最长等待", `${input.max_wait_ms ?? 10000} ms`], ["输出上限", formatNumber(input.max_output_chars ?? 20000)]])],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("终端会话", [["Session", value.session_id], ["状态", labelState(value.state)], ["Shell", value.shell], ["工作目录", value.cwd], ["尺寸", value.width && value.height ? `${value.width}×${value.height}` : ""]])];
      const terminal = terminalUpdatesText(output);
      if (terminal) blocks.push(terminalBlock("初始终端画面", terminal));
      return blocks;
    },
  });

  define("Terminal.Interact", {
    title: "操作终端", icon: "terminal",
    summary(input) {
      const actions = (input.input || []).map(terminalActionLabel).join(" ");
      return actions ? `${input.session_id || ""} · ${preview(actions, 120)}` : `轮询 · ${input.session_id || ""}`;
    },
    input(input) {
      const actions = (input.input || []).map(terminalActionLabel);
      return [
        fieldBlock("终端", [["Session", input.session_id], ["等待", `${input.wait_ms ?? 1000} ms`], ["最长等待", `${input.max_wait_ms ?? 10000} ms`], ["输出上限", formatNumber(input.max_output_chars ?? 20000)]]),
        ...(actions.length ? [listBlock("输入动作", actions, true)] : [noticeBlock("输入动作", "仅轮询，不发送输入", "muted")]),
      ];
    },
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [];
      const terminal = terminalUpdatesText(output);
      if (terminal) blocks.push(terminalBlock("本次终端更新", terminal));
      blocks.push(fieldBlock("终端状态", [["Session", value.session_id || input.session_id], ["Sequence", value.sequence], ["状态", labelState(value.state)], ["退出码", value.exit_code], ["截断", value.truncated ? "是" : "否"]]));
      return blocks;
    },
  });

  function terminalStatusBlocks(input, output) {
    const value = objectValue(resultValue(output));
    return [fieldBlock("终端状态", [["Session", value.session_id || input.session_id], ["状态", labelState(value.state)], ["Shell", value.shell], ["工作目录", value.cwd], ["尺寸", value.width && value.height ? `${value.width}×${value.height}` : ""], ["退出码", value.exit_code]])];
  }

  define("Terminal.Status", {
    title: "查看终端状态", icon: "terminal",
    summary: (input) => input.session_id || "",
    input: (input) => [fieldBlock("终端", [["Session", input.session_id]])],
    output: terminalStatusBlocks,
  });

  define("Terminal.List", {
    title: "列出终端会话", icon: "terminal",
    summary: () => "",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const rows = (value.sessions || []).map((session) => ({
        session_id: session.session_id || "", state: labelState(session.state), shell: session.shell || "", cwd: session.cwd || "", size: session.width && session.height ? `${session.width}×${session.height}` : "", exit_code: session.exit_code ?? "",
      }));
      return rows.length ? [tableBlock("终端会话", [
        { key: "session_id", label: "Session" }, { key: "state", label: "状态" }, { key: "shell", label: "Shell" }, { key: "cwd", label: "工作目录" }, { key: "size", label: "尺寸" }, { key: "exit_code", label: "退出码" },
      ], rows)] : [noticeBlock("终端会话", "当前没有终端会话", "muted")];
    },
  });

  define("Terminal.Kill", {
    title: "终止终端", icon: "terminal-stop",
    summary: (input) => input.session_id || "",
    input: (input) => [fieldBlock("终端", [["Session", input.session_id], ["优雅等待", `${input.grace_ms ?? 1000} ms`]])],
    output: terminalStatusBlocks,
  });

  function desktopOperationLabel(operation, index) {
    const prefix = `${index + 1}.`;
    switch (operation?.kind) {
      case "capture": {
        const clip = operation.clip;
        return clip ? `${prefix} 截图 · (${clip.x}, ${clip.y}) ${clip.width}×${clip.height}` : `${prefix} 全屏截图`;
      }
      case "delay": return `${prefix} 等待 ${operation.delay_ms ?? 0} ms`;
      case "mouse_move": return `${prefix} 移动鼠标至 (${operation.x}, ${operation.y})`;
      case "mouse_down": return `${prefix} 按下${operation.button || "left"}鼠标键`;
      case "mouse_up": return `${prefix} 释放${operation.button || "left"}鼠标键`;
      case "mouse_wheel": return `${prefix} 滚轮 x=${operation.delta_x ?? 0}, y=${operation.delta_y ?? 0}`;
      case "key_click": return `${prefix} 按键 ${operation.key || ""}`;
      case "key_down": return `${prefix} 按下键 ${operation.key || ""}`;
      case "key_up": return `${prefix} 释放键 ${operation.key || ""}`;
      case "text_input": return `${prefix} 输入 ${quotedPreview(operation.text, 60)}`;
      default: return `${prefix} ${operation?.kind || "未知操作"}`;
    }
  }

  function desktopClipLabel(clip) {
    return clip ? `(${clip.x}, ${clip.y}) ${clip.width}×${clip.height}` : "全屏";
  }

  define("Desktop.Play", {
    title: "操作桌面", icon: "pointer",
    summary(input) {
      const operations = Array.isArray(input.operations) ? input.operations : [];
      return `${formatNumber(operations.length)} 个桌面操作${operations.some((operation) => operation?.kind === "capture") ? " · 操作后截图" : ""}`;
    },
    input(input) {
      const operations = Array.isArray(input.operations) ? input.operations : [];
      const counts = new Map();
      let delay = 0;
      for (const operation of operations) {
        const kind = operation?.kind || "unknown";
        counts.set(kind, (counts.get(kind) || 0) + 1);
        if (kind === "delay") delay += Number(operation.delay_ms) || 0;
      }
      const capture = operations.find((operation) => operation?.kind === "capture");
      const overview = [...counts.entries()].map(([kind, count]) => `${kind} ×${count}`).join(" · ");
      return [
        fieldBlock("桌面操作", [["操作数量", operations.length], ["操作构成", overview], ["总等待", delay ? `${formatNumber(delay)} ms` : ""], ["最终截图", capture ? desktopClipLabel(capture.clip) : "无"]]),
        ...(operations.length ? [listBlock("执行顺序", operations.map(desktopOperationLabel))] : [noticeBlock("执行顺序", "没有操作", "muted")]),
      ];
    },
    output(input, output) {
      if (resultFailed(output)) return [];
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("执行结果", [["状态", labelState(value.state)], ["操作总数", value.operation_count], ["已完成", value.completed_operations], ["失败位置", value.failed_operation_index != null ? `第 ${value.failed_operation_index + 1} 项` : ""]])];
      const captures = (value.captures || []).map((capture) => ({
        index: capture.operation_index != null ? capture.operation_index + 1 : "",
        path: capture.path || "",
        image_size: capture.width && capture.height ? `${capture.width}×${capture.height}` : "",
        full_size: capture.full_width && capture.full_height ? `${capture.full_width}×${capture.full_height}` : "",
        clip: desktopClipLabel(capture.clip),
      }));
      if (captures.length) blocks.push(tableBlock("桌面截图", [
        { key: "index", label: "操作" }, { key: "path", label: "路径" }, { key: "image_size", label: "图片尺寸" }, { key: "full_size", label: "全屏尺寸" }, { key: "clip", label: "区域" },
      ], captures));
      if (value.auto_released?.length) blocks.push(listBlock("自动释放", value.auto_released));
      if (value.cleanup_errors?.length) blocks.push(listBlock("释放失败", value.cleanup_errors.map((error) => `${error.code || "cleanup_failed"}: ${error.message || ""}`)));
      if (value.error) {
        const parts = [value.error.message || "桌面操作失败", value.error.code ? `错误码：${value.error.code}` : "", value.error.tip || ""].filter(Boolean);
        blocks.push(noticeBlock("桌面操作失败", parts.join("\n"), "error"));
      }
      return blocks;
    },
  });

  function browserInput(input, extras = []) {
    return [fieldBlock("页面", [["Page", input.page_id], ...extras])];
  }

  define("WebBrowser.Create", {
    title: "新建浏览器页面", icon: "browser",
    summary: () => "",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output(input, output) { const value = objectValue(resultValue(output)); return [fieldBlock("新页面", [["Page", value.page_id], ["初始地址", "about:blank"]])]; },
  });

  define("WebBrowser.Navigate", {
    title: "打开页面", icon: "browser",
    summary: (input) => `${compactUrl(input.url)} · ${input.page_id || ""}`,
    input: (input) => browserInput(input, [["URL", input.url]]),
    output(input, output) { const value = objectValue(resultValue(output)); return [fieldBlock("导航结果", [["Page", value.page_id], ["已提交", value.navigated ? "是" : "否"], ["URL", value.url]])]; },
  });

  define("WebBrowser.Click", {
    title: "点击页面元素", icon: "pointer",
    summary: (input) => `${input.element_id || ""} · ${input.page_id || ""}`,
    input: (input) => browserInput(input, [["元素", input.element_id]]),
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("点击结果", [["Page", value.page_id], ["已点击", value.clicked ? "是" : "否"]])];
      if (value.opened_page_ids?.length) blocks.push(listBlock("新打开页面", value.opened_page_ids));
      return blocks;
    },
  });

  define("WebBrowser.Type", {
    title: "输入页面内容", icon: "keyboard",
    summary: (input) => `${input.element_id || ""} · ${input.mode === "append" ? "追加" : "替换"} · ${quotedPreview(input.content)}`,
    input: (input) => [...browserInput(input, [["元素", input.element_id], ["模式", input.mode === "append" ? "追加" : "替换"], ["内容", `${formatNumber(String(input.content || "").length)} 字符`]]), codeBlock("输入内容", input.content || "")],
    output(input, output) { const value = objectValue(resultValue(output)); return [noticeBlock("结果", value.typed ? "内容已输入" : "未能输入内容", value.typed ? "success" : "warning"), fieldBlock("页面", [["Page", value.page_id]])]; },
  });

  define("WebBrowser.Press", {
    title: "按键", icon: "keyboard",
    summary: (input) => `${input.key || ""} · ${input.element_id || input.page_id || ""}`,
    input: (input) => browserInput(input, [["按键", input.key], ["目标元素", input.element_id]]),
    output(input, output) { const value = objectValue(resultValue(output)); return [noticeBlock("结果", value.pressed ? "按键已发送" : "按键未发送", value.pressed ? "success" : "warning"), fieldBlock("页面", [["Page", value.page_id]])]; },
  });

  define("WebBrowser.Scroll", {
    title: "滚动页面", icon: "scroll",
    summary: (input) => input.element_id ? `滚动到 ${input.element_id} · ${input.page_id || ""}` : `${input.page_id || ""} · Δx ${input.delta_x ?? 0} · Δy ${input.delta_y ?? 720}`,
    input: (input) => browserInput(input, input.element_id ? [["目标元素", input.element_id]] : [["横向", input.delta_x ?? 0], ["纵向", input.delta_y ?? 720]]),
    output(input, output) { const value = objectValue(resultValue(output)); return [noticeBlock("结果", value.scrolled ? "页面已滚动" : "页面未滚动", value.scrolled ? "success" : "warning"), fieldBlock("页面", [["Page", value.page_id]])]; },
  });

  function pageRows(pages) {
    return (pages || []).map((entry) => {
      const page = entry.page || entry;
      return { page_id: page.page_id || entry.page_id || "", title: page.title || "", url: page.url || "", state: labelState(page.state || entry.change) };
    });
  }

  define("WebBrowser.RequireHumanAction", {
    title: "等待用户操作", icon: "person",
    summary: (input) => `${input.page_id || ""} · ${quotedPreview(input.instruction)}`,
    input: (input) => [...browserInput(input), codeBlock("操作说明", input.instruction || "")],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("交接结果", [["状态", labelState(value.state)], ["目标 Page", value.page_id], ["当前活跃 Page", value.active_page_id], ["消息", value.message]])];
      const changes = [...(value.changed_pages || []), ...(value.opened_pages || [])];
      if (changes.length) blocks.push(tableBlock("页面变化", [{ key: "page_id", label: "Page" }, { key: "title", label: "标题" }, { key: "url", label: "URL" }, { key: "state", label: "状态" }], pageRows(changes)));
      if (value.closed_page_ids?.length) blocks.push(listBlock("已关闭页面", value.closed_page_ids));
      return blocks;
    },
  });

  define("WebBrowser.Snapshot", {
    title: "获取页面快照", icon: "snapshot",
    summary: (input) => `${input.page_id || ""} · ${{ text: "文本", screen: "截图", both: "文本与截图" }[input.kind] || input.kind || ""}`,
    input: (input) => browserInput(input, [["类型", { text: "文本", screen: "截图", both: "文本与截图" }[input.kind] || input.kind], ["采样前等待", `${input.wait_ms} ms`]]),
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("页面快照", [["Page", value.page_id], ["标题", value.title], ["URL", value.url], ["文档状态", value.state], ["Snapshot", value.snapshot_id], ["截图路径", value.screen_path], ["事件丢弃数", value.dropped_browser_events]])];
      if (value.accessibility_tree) blocks.push(treeBlock("Accessibility Tree", typeof value.accessibility_tree === "string" ? value.accessibility_tree : JSON.stringify(value.accessibility_tree, null, 2)));
      if (value.browser_events?.length) blocks.push(tableBlock("浏览器事件", [
        { key: "kind", label: "类型" }, { key: "level", label: "级别" }, { key: "status", label: "状态" }, { key: "message", label: "信息" }, { key: "url", label: "URL" },
      ], value.browser_events));
      if (value.dismissed_native_dialogs?.length) blocks.push(codeBlock("已关闭的原生对话框", JSON.stringify(value.dismissed_native_dialogs, null, 2)));
      return blocks;
    },
  });

  define("WebBrowser.Pages", {
    title: "列出浏览器页面", icon: "browser",
    summary: () => "",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output(input, output) {
      const value = objectValue(resultValue(output));
      const blocks = [fieldBlock("浏览器", [["当前活跃 Page", value.active_page_id]])];
      const rows = pageRows(value.pages);
      if (rows.length) blocks.push(tableBlock("页面", [{ key: "page_id", label: "Page" }, { key: "title", label: "标题" }, { key: "url", label: "URL" }, { key: "state", label: "状态" }], rows));
      else blocks.push(noticeBlock("页面", "当前没有打开的页面", "muted"));
      return blocks;
    },
  });

  define("WebBrowser.Back", {
    title: "浏览器后退", icon: "browser-back",
    summary: (input) => input.page_id || "",
    input: (input) => browserInput(input),
    output(input, output) { const value = objectValue(resultValue(output)); return [fieldBlock("后退结果", [["Page", value.page_id], ["已后退", value.navigated ? "是" : "否"], ["URL", value.url]])]; },
  });

  define("WebBrowser.Close", {
    title: "关闭浏览器页面", icon: "browser-close",
    summary: (input) => input.page_id || "",
    input: (input) => browserInput(input),
    output(input, output) { const value = objectValue(resultValue(output)); return [noticeBlock("结果", value.closed ? `页面已关闭：${value.page_id}` : `页面未关闭：${value.page_id}`, value.closed ? "success" : "warning")]; },
  });

  function imageMetadataBlocks(value, includeEvent = false) {
    return [fieldBlock("图片信息", [
      ...(includeEvent ? [["Image Event", value.image_event_id]] : []),
      ["来源", value.image?.source || value.source], ["格式", value.image?.format || value.format], ["MIME", value.image?.mime_type || value.mime_type],
      ["尺寸", (value.image?.width || value.width) && (value.image?.height || value.height) ? `${value.image?.width || value.width}×${value.image?.height || value.height}` : ""],
      ["宽高比", value.image?.aspect_ratio || value.aspect_ratio], ["色彩", value.image?.color_type || value.color_type],
      ["位深", value.image?.bits_per_pixel || value.bits_per_pixel], ["Alpha", (value.image?.has_alpha ?? value.has_alpha) ? "有" : "无"],
      ["大小", formatBytes(value.image?.bytes ?? value.bytes)], ["SHA256", value.image?.sha256 || value.sha256],
    ])];
  }

  define("Image.Info", {
    title: "读取图片信息", icon: "image",
    summary: (input) => preview(input.url, 160),
    input: (input) => [fieldBlock("图片来源", [["URL 或路径", input.url]])],
    output(input, output) { return imageMetadataBlocks(objectValue(resultValue(output))); },
  });

  define("Image.View", {
    title: "查看图片", icon: "image-view",
    summary: (input) => preview(input.url, 160),
    input: (input) => [fieldBlock("图片来源", [["URL 或路径", input.url]])],
    output(input, output) { const value = objectValue(resultValue(output)); return [noticeBlock("结果", "图片已加入会话", "success"), ...imageMetadataBlocks(value, true)]; },
  });

  define("SetTitle", {
    title: "设置会话标题", icon: "title",
    summary: (input) => input.title || "",
    input: (input) => [fieldBlock("标题", [["新标题", input.title]])],
    output: () => [noticeBlock("结果", "会话标题已更新", "success")],
  });

  define("CurrentTime", {
    title: "查询当前时间", icon: "time",
    summary: () => "",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output(input, output) {
      const value = objectValue(resultValue(output));
      return [fieldBlock("当前时间", [
        ["本地时间", value.local_rfc3339],
        ["UTC 时间", value.utc_rfc3339],
        ["UTC 偏移", value.utc_offset],
        ["星期", value.weekday],
        ["Unix 时间（毫秒）", value.unix_timestamp_ms],
      ])];
    },
  });

  define("Compact", {
    title: "压缩上下文", icon: "compact",
    summary: () => "",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output(input, output) { const value = objectValue(resultValue(output)); return [noticeBlock("结果", value.status === "accepted" ? "压缩请求已接受" : "压缩请求已处理", "success")]; },
  });

  function workMapRecords(output) {
    return objectValue(resultValue(output)).records || [];
  }

  function workMapNoteKind(kind) {
    const labels = {
      action: "操作", finding: "发现", decision: "决策", validation: "验证",
      adjustment: "调整", blocker: "阻塞", next: "下一步", note: "笔记",
    };
    return labels[normalized(kind)] || String(kind || "笔记");
  }

  function workMapRecordKind(kind) {
    const labels = { objective: "目标", plan: "计划", note: "笔记", memory: "记忆" };
    return labels[normalized(kind)] || String(kind || "");
  }

  function currentWorkMapBlocks(value) {
    const current = value?.current;
    const memory = value?.memory || {};
    const blocks = [fieldBlock("记忆", [["事实", memory.facts?.length || 0], ["约定", memory.agreements?.length || 0]])];
    if (!current?.objective) {
      blocks.push(noticeBlock("当前状态", "当前没有进行中的目标", "muted"));
      return blocks;
    }
    blocks.push(fieldBlock("目标", [["ID", current.objective.id], ["标题", current.objective.title], ["状态", labelState(current.objective.state)], ["描述", current.objective.description]]));
    const plans = (current.plans || []).map((entry) => entry.plan || entry).map((plan) => ({ order: plan.order, title: plan.title, state: labelState(plan.state), id: plan.id }));
    if (plans.length) blocks.push(tableBlock("计划", [{ key: "order", label: "#" }, { key: "title", label: "标题" }, { key: "state", label: "状态" }, { key: "id", label: "ID" }], plans));
    return blocks;
  }

  define("WorkMap.Read", {
    title: "查看工作图", icon: "workmap",
    summary: () => "当前状态",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output(input, output) { return currentWorkMapBlocks(objectValue(resultValue(output))); },
  });

  define("WorkMap.ReadHistory", {
    title: "查看工作图历史", icon: "workmap-history",
    summary: (input) => input.objective_id || "历史目标",
    input: (input) => [fieldBlock("历史范围", [["目标", input.objective_id || "全部"]])],
    output(input, output) {
      const value = resultValue(output);
      const history = Array.isArray(value) ? value : value?.history;
      if (Array.isArray(history)) return [tableBlock("历史目标", [{ key: "title", label: "标题" }, { key: "state", label: "状态" }, { key: "id", label: "ID" }], history.map((item) => ({ title: item.title || item.objective?.title || "", state: labelState(item.state || item.objective?.state), id: item.id || item.objective?.id || "" })))];
      return genericObjectBlocks("历史目标", value);
    },
  });

  function planDefinitionBlocks(input) {
    const objective = objectValue(input.objective);
    const plans = input.plans || [];
    return [
      fieldBlock("目标", [["标题", objective.title], ["描述", objective.description]]),
      tableBlock("计划", [{ key: "order", label: "#" }, { key: "title", label: "标题" }, { key: "description", label: "描述" }], plans.map((plan, index) => ({ order: index + 1, title: plan.title || "", description: plan.description || "" }))),
    ];
  }

  function recordBlocks(output, title = "变更记录") {
    const records = workMapRecords(output);
    if (!records.length) return currentWorkMapBlocks(objectValue(resultValue(output)));
    return [tableBlock(title, [{ key: "kind", label: "类型" }, { key: "title", label: "标题" }, { key: "state", label: "状态" }, { key: "id", label: "ID" }], records.map((entry) => {
      const record = entry.record || {};
      return { kind: workMapRecordKind(entry.kind), title: record.title || record.content || "", state: labelState(record.state), id: record.id || "" };
    }))];
  }

  define("WorkMap.Start", {
    title: "创建目标", icon: "workmap-add",
    summary: (input) => `${input.objective?.title || ""} · ${itemCount(input.plans, "个计划")}`,
    input: planDefinitionBlocks,
    output(input, output) { return recordBlocks(output, "已创建记录"); },
  });

  define("WorkMap.UpdatePlanState", {
    title: "更新计划", icon: "plan-state",
    summary: (input) => `${({ completed: "完成", cancelled: "取消", superseded: "取代" })[input.state] || input.state || "更新"} · ${input.plan_id || ""}`,
    input: (input) => [fieldBlock("计划", [["ID", input.plan_id], ["新状态", labelState(input.state)], ["结果", input.outcome], ["验证", input.verification], ["原因", input.reason]])],
    output(input, output) { return currentWorkMapBlocks(objectValue(resultValue(output))); },
  });

  define("WorkMap.AddNote", {
    title: "添加笔记", icon: "note-add",
    summary: (input) => `${workMapNoteKind(input.kind || "note")} · ${input.plan_id || ""} · ${quotedPreview(input.content)}`,
    input: (input) => [fieldBlock("笔记", [["计划", input.plan_id], ["类型", workMapNoteKind(input.kind || "note")]]), codeBlock("内容", input.content || "")],
    output(input, output) { return recordBlocks(output, "已添加笔记"); },
  });

  define("WorkMap.ChangePlan", {
    title: "修改计划", icon: "plan-edit",
    summary: (input) => `${input.plan_id || ""}${input.title ? ` · ${input.title}` : ""}`,
    input: (input) => [fieldBlock("修改内容", [["计划", input.plan_id], ["新标题", input.title], ["新描述", input.description], ["清空描述", input.clear_description ? "是" : ""], ["原因", input.reason]])],
    output(input, output) { return recordBlocks(output, "已修改计划"); },
  });

  define("WorkMap.AddPlan", {
    title: "添加计划", icon: "plan-add",
    summary: (input) => `${input.plan?.title || ""}${input.after_plan_id ? ` · 位于 ${input.after_plan_id} 后` : " · 追加"}`,
    input: (input) => [fieldBlock("计划", [["标题", input.plan?.title], ["描述", input.plan?.description], ["插入位置", input.after_plan_id ? `${input.after_plan_id} 后` : "末尾"]])],
    output(input, output) { return recordBlocks(output, "已添加计划"); },
  });

  define("WorkMap.CloseObjective", {
    title: "关闭目标", icon: "workmap-close",
    summary: (input) => `${input.state === "superseded" ? "取代" : "取消"} · ${quotedPreview(input.reason)}`,
    input: (input) => [fieldBlock("关闭方式", [["状态", labelState(input.state)], ["原因", input.reason]])],
    output(input, output) { return recordBlocks(output, "已关闭目标"); },
  });

  define("WorkMap.AddMemory", {
    title: "添加记忆", icon: "memory-add",
    summary: (input) => `${input.kind === "agreement" ? "全局约定" : "全局事实"} · ${quotedPreview(input.content)}`,
    input: (input) => [fieldBlock("记忆", [["类型", input.kind === "agreement" ? "全局约定" : "全局事实"], ["依据", input.basis]]), codeBlock("内容", input.content || "")],
    output(input, output) { return recordBlocks(output, "已添加记忆"); },
  });

  define("WorkMap.InvalidateMemory", {
    title: "更新记忆", icon: "memory-remove",
    summary: (input) => `${input.replacement ? "替换" : "移除"} · ${input.memory_id || ""}`,
    input: (input) => [fieldBlock("原记忆", [["ID", input.memory_id], ["原因", input.reason], ["处理", input.replacement ? "使用新记录替换" : "移除现有记录"]]), ...(input.replacement ? [codeBlock("替换内容", JSON.stringify(input.replacement, null, 2))] : [])],
    output(input, output) { return recordBlocks(output, "记忆已更新"); },
  });

  function workerOutputBlocks(output) {
    const value = objectValue(resultValue(output));
    const blocks = [fieldBlock("状态", [["状态", labelState(value.state)], ["Turn", value.turn_id], ["原因", value.reason], ["错误", value.error], ["Compact 次数", value.compact_count]])];
    if (value.progress?.length) blocks.push(tableBlock("进度", [{ key: "assistant_text", label: "说明" }, { key: "tools", label: "工具" }], value.progress.map((item) => ({ assistant_text: item.assistant_text || "", tools: (item.tool_calls || []).join(", ") }))));
    if (value.final_answer) blocks.push(codeBlock("最终答复", typeof value.final_answer === "string" ? value.final_answer : JSON.stringify(value.final_answer, null, 2)));
    if (value.context_usage) blocks.push(fieldBlock("上下文用量", [["输入", value.context_usage.input_tokens], ["输出", value.context_usage.output_tokens], ["合计", value.context_usage.total_tokens]]));
    return blocks;
  }

  define("Worker.Ask", {
    title: "开始后台操作", icon: "worker",
    summary: (input) => quotedPreview(input.prompt, 120),
    input: (input) => [codeBlock("操作请求", input.prompt || "")],
    output: (input, output) => workerOutputBlocks(output),
  });

  define("Worker.Wait", {
    title: "等待后台操作", icon: "worker-wait",
    summary: (input) => `最长 ${formatNumber(input.max_wait_ms)} ms`,
    input: (input) => [fieldBlock("等待", [["最长等待", `${formatNumber(input.max_wait_ms)} ms`]])],
    output: (input, output) => workerOutputBlocks(output),
  });

  define("Worker.Stop", {
    title: "停止后台操作", icon: "worker-stop",
    summary: () => "",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output: (input, output) => workerOutputBlocks(output),
  });

  define("Worker.ClearContext", {
    title: "清空后台上下文", icon: "worker-clear",
    summary: () => "",
    input: () => [noticeBlock("参数", "无参数", "muted")],
    output: (input, output) => workerOutputBlocks(output),
  });

  function agentInput(input, includePrompt = false) {
    const blocks = [fieldBlock("Agent", [["Session", input.session_id], ["最长等待", input.max_wait_ms == null ? "" : `${formatNumber(input.max_wait_ms)} ms`]])];
    if (includePrompt && input.system_prompt) blocks.push(codeBlock("System Prompt", input.system_prompt));
    if (includePrompt && input.prompt) blocks.push(codeBlock("Prompt", input.prompt));
    return blocks;
  }

  define("Agent.Create", {
    title: "创建子 Agent", icon: "agent",
    summary: (input) => quotedPreview(input.prompt, 120),
    input: (input) => agentInput(input, true),
    output: (input, output) => workerOutputBlocks(output),
  });

  define("Agent.Wait", {
    title: "等待子 Agent", icon: "agent-wait",
    summary: (input) => `${input.session_id || ""} · 最长 ${formatNumber(input.max_wait_ms)} ms`,
    input: (input) => agentInput(input),
    output: (input, output) => workerOutputBlocks(output),
  });

  define("Agent.Ask", {
    title: "继续子 Agent", icon: "agent",
    summary: (input) => `${input.session_id || ""} · ${quotedPreview(input.prompt)}`,
    input: (input) => agentInput(input, true),
    output: (input, output) => workerOutputBlocks(output),
  });

  for (const [name, title, icon] of [
    ["Agent.Stop", "停止子 Agent", "agent-stop"],
    ["Agent.ClearContext", "清空子 Agent 上下文", "agent-clear"],
    ["Agent.Kill", "结束子 Agent", "agent-kill"],
  ]) {
    define(name, {
      title, icon,
      summary: (input) => input.session_id || "",
      input: (input) => agentInput(input),
      output: (input, output) => workerOutputBlocks(output),
    });
  }

  const missing = KNOWN_TOOLS.filter((name) => !registry.has(name));
  if (missing.length) throw new Error(`missing tool presenters: ${missing.join(", ")}`);

  globalThis.MeToolPresenters = Object.freeze({
    KNOWN_TOOLS: Object.freeze([...KNOWN_TOOLS]),
    has: (name) => registry.has(name),
    names: () => [...registry.keys()],
    summarize,
    describe,
    present,
    renderDetails,
    safeJson,
  });
})();
