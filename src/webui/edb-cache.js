(() => {
  "use strict";

  const DB_NAME = "me-edb-cache";
  const DB_VERSION = 1;
  const SESSION_STORE = "sessions";
  const EVENT_STORE = "events";
  const CHANNEL_NAME = "me-edb-cache";

  function sessionKey(scope, agentId) {
    return JSON.stringify([String(scope || ""), String(agentId || "")]);
  }

  function requestResult(request) {
    return new Promise((resolve, reject) => {
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error || new Error("IndexedDB request failed"));
    });
  }

  function transactionDone(transaction) {
    return new Promise((resolve, reject) => {
      transaction.oncomplete = () => resolve();
      transaction.onabort = () => reject(transaction.error || new Error("IndexedDB transaction aborted"));
      transaction.onerror = () => reject(transaction.error || new Error("IndexedDB transaction failed"));
    });
  }

  function cursorValues(source, range = undefined) {
    return new Promise((resolve, reject) => {
      const values = [];
      const request = source.openCursor(range);
      request.onsuccess = () => {
        const cursor = request.result;
        if (!cursor) {
          resolve(values);
          return;
        }
        values.push(cursor.value);
        cursor.continue();
      };
      request.onerror = () => reject(request.error || new Error("IndexedDB cursor failed"));
    });
  }

  function eventRange(key, keyRange = globalThis.IDBKeyRange) {
    return keyRange.bound([key, 0], [key, Number.MAX_SAFE_INTEGER]);
  }

  function eventByteSize(event, Encoder = globalThis.TextEncoder) {
    const json = JSON.stringify(event);
    if (typeof Encoder === "function") return new Encoder().encode(json).byteLength;
    return unescape(encodeURIComponent(json)).length;
  }

  function formatBytes(value) {
    const bytes = Math.max(0, Number(value) || 0);
    if (bytes < 1024) return `${bytes} B`;
    const units = ["KiB", "MiB", "GiB"];
    let amount = bytes / 1024;
    let index = 0;
    while (amount >= 1024 && index < units.length - 1) {
      amount /= 1024;
      index += 1;
    }
    const digits = amount >= 100 ? 0 : amount >= 10 ? 1 : 2;
    return `${amount.toFixed(digits)} ${units[index]}`;
  }

  function cachedSessionTitle(events, fallback = "未命名会话") {
    let title = "";
    for (const event of events || []) {
      if (!event || typeof event !== "object") continue;
      const entry = Object.entries(event)[0];
      if (!entry) continue;
      const [kind, value] = entry;
      if ((kind === "AgentTitleChanged" || kind === "CloneCompleted") && value?.title) {
        title = String(value.title);
      }
    }
    return title || String(fallback || "未命名会话");
  }

  function workspaceName(path) {
    const normalized = String(path || "").replace(/[\\/]+$/, "");
    const parts = normalized.split(/[\\/]/).filter(Boolean);
    return parts[parts.length - 1] || normalized || "工作区";
  }

  class EdbCache {
    constructor(runtime = {}) {
      this.indexedDB = runtime.indexedDB ?? globalThis.indexedDB;
      this.keyRange = runtime.IDBKeyRange ?? globalThis.IDBKeyRange;
      this.Encoder = runtime.TextEncoder ?? globalThis.TextEncoder;
      this.BroadcastChannel = runtime.BroadcastChannel ?? globalThis.BroadcastChannel;
      this.databaseName = runtime.databaseName || DB_NAME;
      this.databasePromise = null;
      this.disabledReason = "";
      this.suppressed = new Set();
      this.pendingWrites = new Map();
      this.writeDrain = null;
      this.channel = null;
    }

    get available() {
      return Boolean(this.indexedDB && this.keyRange) && !this.disabledReason;
    }

    _ensureChannel() {
      if (this.channel || typeof this.BroadcastChannel !== "function") return;
      try {
        this.channel = new this.BroadcastChannel(CHANNEL_NAME);
        this.channel.addEventListener("message", (event) => {
          const key = event?.data?.removed;
          if (typeof key === "string") this.suppressed.add(key);
        });
      } catch (_) {
        this.channel = null;
      }
    }

    async _database() {
      if (!this.available) throw new Error(this.disabledReason || "当前浏览器不支持 IndexedDB");
      if (this.databasePromise) return this.databasePromise;
      this.databasePromise = new Promise((resolve, reject) => {
        const request = this.indexedDB.open(this.databaseName, DB_VERSION);
        request.onupgradeneeded = () => {
          const database = request.result;
          if (!database.objectStoreNames.contains(SESSION_STORE)) {
            const sessions = database.createObjectStore(SESSION_STORE, { keyPath: "key" });
            sessions.createIndex("scope", "scope", { unique: false });
          }
          if (!database.objectStoreNames.contains(EVENT_STORE)) {
            database.createObjectStore(EVENT_STORE, { keyPath: ["sessionKey", "order"] });
          }
        };
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error || new Error("无法打开 IndexedDB"));
        request.onblocked = () => reject(new Error("IndexedDB 升级被其他页面阻止"));
      }).catch((error) => {
        this.disabledReason = error instanceof Error ? error.message : String(error);
        this.databasePromise = null;
        throw error;
      });
      this._ensureChannel();
      return this.databasePromise;
    }

    async _sessionEvents(database, key) {
      const transaction = database.transaction(EVENT_STORE, "readonly");
      const records = await cursorValues(
        transaction.objectStore(EVENT_STORE),
        eventRange(key, this.keyRange),
      );
      return records.sort((left, right) => left.order - right.order);
    }

    async _deleteNow(key) {
      if (!this.available) return;
      const database = await this._database();
      const transaction = database.transaction([SESSION_STORE, EVENT_STORE], "readwrite");
      const done = transactionDone(transaction);
      transaction.objectStore(SESSION_STORE).delete(key);
      transaction.objectStore(EVENT_STORE).delete(eventRange(key, this.keyRange));
      await done;
    }

    async discardSession(key) {
      try {
        await this._deleteNow(key);
      } catch (error) {
        console.warn("Unable to discard invalid EDB cache", error);
      }
    }

    async removeSession(key) {
      this.suppressed.add(key);
      this.pendingWrites.delete(key);
      this._ensureChannel();
      try { this.channel?.postMessage({ removed: key }); } catch (_) {}
      await this._deleteNow(key);
    }

    async _loadMetadata(scope = null) {
      const database = await this._database();
      const transaction = database.transaction(SESSION_STORE, "readonly");
      const store = transaction.objectStore(SESSION_STORE);
      return scope == null
        ? cursorValues(store)
        : cursorValues(store.index("scope"), this.keyRange.only(String(scope)));
    }

    async _loadEntries(scope = null, includeEvents = true) {
      if (!this.available) return [];
      try {
        const database = await this._database();
        const metadata = await this._loadMetadata(scope);
        const entries = [];
        for (const meta of metadata) {
          if (this.suppressed.has(meta.key)) continue;
          if (!includeEvents) {
            entries.push({ ...meta });
            continue;
          }
          const records = await this._sessionEvents(database, meta.key);
          const valid = records.length === meta.eventCount
            && records.every((record, index) => record.order === index)
            && (meta.eventCount === 0 || typeof meta.lastEventHash === "string");
          if (!valid) {
            await this.discardSession(meta.key);
            continue;
          }
          entries.push({ ...meta, events: records.map((record) => record.event) });
        }
        return entries;
      } catch (error) {
        this.disabledReason = error instanceof Error ? error.message : String(error);
        console.warn("Unable to read EDB cache", error);
        return [];
      }
    }

    loadScope(scope) {
      return this._loadEntries(String(scope || ""), true);
    }

    listSessions() {
      return this._loadEntries(null, true);
    }

    saveSession(session) {
      if (!this.available) return;
      const scope = String(session.scope || "");
      const agentId = String(session.agentId || "");
      const key = sessionKey(scope, agentId);
      const events = Array.isArray(session.events) ? session.events : [];
      if (!scope || !agentId || this.suppressed.has(key)) return;
      if (events.length > 0 && typeof session.lastEventHash !== "string") return;
      this.pendingWrites.set(key, {
        key,
        scope,
        agentId,
        mutationRevision: Math.max(0, Number(session.mutationRevision) || 0),
        lastEventHash: events.length ? session.lastEventHash : null,
        events,
        replace: Boolean(session.replace),
      });
      if (!this.writeDrain) {
        this.writeDrain = Promise.resolve().then(() => this._drainWrites());
      }
    }

    async _drainWrites() {
      try {
        while (this.pendingWrites.size) {
          const writes = [...this.pendingWrites.values()];
          this.pendingWrites.clear();
          for (const write of writes) {
            if (this.suppressed.has(write.key)) continue;
            try { await this._saveNow(write); }
            catch (error) { console.warn("Unable to persist EDB cache", error); }
          }
        }
      } finally {
        this.writeDrain = null;
        if (this.pendingWrites.size) this.writeDrain = Promise.resolve().then(() => this._drainWrites());
      }
    }

    async flush() {
      while (this.writeDrain) await this.writeDrain;
    }

    async _saveNow(write) {
      if (!this.available || this.suppressed.has(write.key)) return;
      const database = await this._database();
      const transaction = database.transaction([SESSION_STORE, EVENT_STORE], "readwrite");
      const done = transactionDone(transaction);
      const sessions = transaction.objectStore(SESSION_STORE);
      const eventsStore = transaction.objectStore(EVENT_STORE);
      const existing = await requestResult(sessions.get(write.key));
      if (this.suppressed.has(write.key)) {
        transaction.abort();
        await done.catch(() => {});
        return;
      }

      if (!write.replace && existing
          && existing.mutationRevision === write.mutationRevision
          && existing.eventCount > write.events.length) {
        transaction.abort();
        await done.catch(() => {});
        return;
      }

      const replace = write.replace || !existing
        || existing.mutationRevision !== write.mutationRevision
        || existing.eventCount > write.events.length
        || (existing.eventCount === write.events.length
          && existing.lastEventHash !== write.lastEventHash);
      const start = replace ? 0 : existing.eventCount;
      if (replace) eventsStore.delete(eventRange(write.key, this.keyRange));
      let byteSize = replace ? 0 : Math.max(0, Number(existing.byteSize) || 0);
      for (let order = start; order < write.events.length; order += 1) {
        const event = write.events[order];
        const bytes = eventByteSize(event, this.Encoder);
        byteSize += bytes;
        eventsStore.put({ sessionKey: write.key, order, event, bytes });
      }
      sessions.put({
        key: write.key,
        scope: write.scope,
        agentId: write.agentId,
        mutationRevision: write.mutationRevision,
        lastEventHash: write.lastEventHash,
        eventCount: write.events.length,
        byteSize,
        updatedAt: Date.now(),
      });
      await done;
    }

    async renderManager(container, options = {}) {
      if (!container) return;
      container.textContent = "";
      const documentValue = container.ownerDocument || globalThis.document;
      const section = documentValue.createElement("section");
      section.className = "edb-cache-settings";
      const heading = documentValue.createElement("div");
      heading.className = "edb-cache-heading";
      const copy = documentValue.createElement("div");
      const title = documentValue.createElement("h3");
      title.textContent = "会话缓存";
      const description = documentValue.createElement("p");
      description.textContent = "仅缓存原始 EDB 事件；消息、工具和界面状态每次都会重新生成。";
      copy.append(title, description);
      heading.appendChild(copy);
      section.appendChild(heading);
      container.appendChild(section);

      if (!this.available) {
        const unavailable = documentValue.createElement("div");
        unavailable.className = "edb-cache-empty";
        unavailable.textContent = this.disabledReason || `${options.storageLabel || "当前浏览器"}不支持本地 EDB 缓存。`;
        section.appendChild(unavailable);
        return;
      }

      await this.flush();
      const entries = await this.listSessions();
      if (!entries.length) {
        const empty = documentValue.createElement("div");
        empty.className = "edb-cache-empty";
        empty.textContent = `${options.storageLabel || "当前浏览器"}还没有会话缓存。`;
        section.appendChild(empty);
        return;
      }

      const totalEvents = entries.reduce((sum, entry) => sum + entry.eventCount, 0);
      const totalBytes = entries.reduce((sum, entry) => sum + entry.byteSize, 0);
      const summary = documentValue.createElement("div");
      summary.className = "edb-cache-summary";
      summary.textContent = `${entries.length} 个会话 · ${totalEvents} 个事件 · ${formatBytes(totalBytes)}`;
      section.appendChild(summary);
      const list = documentValue.createElement("div");
      list.className = "edb-cache-list";
      section.appendChild(list);

      entries.sort((left, right) => right.updatedAt - left.updatedAt);
      for (const entry of entries) {
        const resolved = options.resolveLabel?.(entry) || {};
        const row = documentValue.createElement("article");
        row.className = "edb-cache-row";
        const identity = documentValue.createElement("div");
        identity.className = "edb-cache-identity";
        const session = documentValue.createElement("strong");
        session.textContent = resolved.title
          || cachedSessionTitle(entry.events, entry.agentId === "main" ? "主会话" : entry.agentId);
        const workspace = documentValue.createElement("span");
        workspace.textContent = resolved.workspace || workspaceName(entry.scope);
        identity.append(session, workspace);
        const meta = documentValue.createElement("div");
        meta.className = "edb-cache-meta";
        const updated = Number.isFinite(entry.updatedAt)
          ? new Date(entry.updatedAt).toLocaleString() : "";
        meta.textContent = `${entry.eventCount} 个事件 · ${formatBytes(entry.byteSize)}${updated ? ` · ${updated}` : ""}`;
        const remove = documentValue.createElement("button");
        remove.type = "button";
        remove.className = "ghost-button danger edb-cache-remove";
        remove.textContent = "清除";
        remove.setAttribute("aria-label", `清除“${session.textContent}”的本地 EDB 缓存`);
        remove.addEventListener("click", async () => {
          remove.disabled = true;
          try {
            await this.removeSession(entry.key);
            options.onRemoved?.(entry);
            await this.renderManager(container, options);
          } catch (error) {
            remove.disabled = false;
            options.onError?.(error);
          }
        });
        row.append(identity, meta, remove);
        list.appendChild(row);
      }
    }
  }

  globalThis.MeEdbCache = Object.freeze({
    create: (runtime) => new EdbCache(runtime),
    sessionKey,
    eventByteSize,
    cachedSessionTitle,
    workspaceName,
    formatBytes,
  });
})();
