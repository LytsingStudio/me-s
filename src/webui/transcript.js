(function (root, factory) {
  if (typeof module === "object" && module.exports) {
    module.exports = factory();
  } else {
    root.MeTranscript = factory();
  }
}(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  function createTranscriptBottomFollower(viewport, content, onPositionChange, runtime = {}) {
    const requestFrame = runtime.requestFrame || ((callback) => requestAnimationFrame(callback));
    const cancelFrame = runtime.cancelFrame || ((id) => cancelAnimationFrame(id));
    const setDelay = runtime.setDelay || ((callback, delay) => setTimeout(callback, delay));
    const clearDelay = runtime.clearDelay || ((id) => clearTimeout(id));
    const createResizeObserver = runtime.createResizeObserver || ((callback) => {
      if (typeof ResizeObserver === "function") return new ResizeObserver(callback);
      return { observe() {}, disconnect() {} };
    });
    const threshold = runtime.threshold ?? 24;
    const settleDelay = runtime.settleDelay ?? 180;
    let following = true;
    let userScrolling = false;
    let forcing = false;
    let frame = null;
    let settleTimer = null;
    let kineticRestoreFrame = null;
    let kineticStyleSnapshot = null;
    let committedScrollHeight = viewport.scrollHeight;
    let committedClientHeight = viewport.clientHeight;

    const restoreKineticScrollLayer = () => {
      if (kineticRestoreFrame !== null) {
        cancelFrame(kineticRestoreFrame);
        kineticRestoreFrame = null;
      }
      if (kineticStyleSnapshot === null) return;
      const style = viewport.style;
      kineticStyleSnapshot.forEach(({ name, value, priority }) => {
        if (value) style.setProperty(name, value, priority);
        else style.removeProperty(name);
      });
      kineticStyleSnapshot = null;
      void viewport.offsetHeight;
    };
    const interruptKineticScroll = runtime.interruptKineticScroll || (() => {
      const style = viewport.style;
      if (
        !style
        || typeof style.getPropertyValue !== "function"
        || typeof style.getPropertyPriority !== "function"
        || typeof style.setProperty !== "function"
        || typeof style.removeProperty !== "function"
      ) return;
      const properties = ["overflow", "-webkit-overflow-scrolling"];
      if (kineticStyleSnapshot === null) {
        kineticStyleSnapshot = properties.map((name) => ({
          name,
          value: style.getPropertyValue(name),
          priority: style.getPropertyPriority(name),
        }));
      }
      style.setProperty("overflow", "hidden", "important");
      style.setProperty("-webkit-overflow-scrolling", "auto", "important");
      void viewport.offsetHeight;
      if (kineticRestoreFrame !== null) cancelFrame(kineticRestoreFrame);
      // Leave the scroll layer disabled for one painted frame. Restoring it in this
      // same task lets mobile WebKit coalesce both styles and preserve old momentum.
      kineticRestoreFrame = requestFrame(() => {
        kineticRestoreFrame = requestFrame(() => {
          kineticRestoreFrame = null;
          restoreKineticScrollLayer();
          if (forcing) {
            applyFollowNow();
            scheduleFollow();
          }
        });
      });
    });

    const isNearBottom = () => viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight
      <= threshold;
    const notify = () => onPositionChange?.();
    const cancelScheduledFollow = () => {
      if (frame === null) return;
      cancelFrame(frame);
      frame = null;
    };
    const clearSettling = () => {
      if (settleTimer === null) return;
      clearDelay(settleTimer);
      settleTimer = null;
    };
    const scheduleSettling = () => {
      clearSettling();
      settleTimer = setDelay(finishSettling, settleDelay);
    };
    const applyFollowNow = (force = forcing) => {
      if (!following || (userScrolling && !forcing)) return;
      const scrollHeight = viewport.scrollHeight;
      const clientHeight = viewport.clientHeight;
      const shouldWrite = force
        || scrollHeight !== committedScrollHeight
        || clientHeight !== committedClientHeight
        || !isNearBottom();
      committedScrollHeight = scrollHeight;
      committedClientHeight = clientHeight;
      if (shouldWrite) viewport.scrollTop = scrollHeight;
      if (forcing) scheduleSettling();
      notify();
    };
    const scheduleFollow = () => {
      if (!following || (userScrolling && !forcing) || frame !== null) return;
      frame = requestFrame(() => {
        frame = null;
        applyFollowNow();
      });
    };
    function finishSettling() {
      settleTimer = null;
      if (forcing) {
        // A programmatic bottom write can emit scrollend before compositor momentum is finished.
        // Keep the explicit lock until a new real user gesture calls beginUserInteraction().
        if (!isNearBottom()) scheduleFollow();
        notify();
        return;
      }
      if (userScrolling) {
        userScrolling = false;
        following = isNearBottom();
        if (following) scheduleFollow();
      }
      notify();
    }
    const layoutChanged = () => {
      if (following && !userScrolling) scheduleFollow(); else notify();
    };
    const resizeObserver = createResizeObserver(layoutChanged);
    resizeObserver.observe(viewport);
    resizeObserver.observe(content);

    return {
      isFollowing: () => following,
      isNearBottom,
      follow() {
        following = true;
        forcing = true;
        userScrolling = false;
        clearSettling();
        cancelScheduledFollow();
        interruptKineticScroll();
        applyFollowNow();
        scheduleFollow();
      },
      restore(value) {
        restoreKineticScrollLayer();
        following = Boolean(value);
        forcing = false;
        userScrolling = false;
        clearSettling();
        cancelScheduledFollow();
        if (following) scheduleFollow(); else notify();
      },
      beginUserInteraction() {
        restoreKineticScrollLayer();
        forcing = false;
        userScrolling = true;
        clearSettling();
        cancelScheduledFollow();
      },
      endUserInteraction() {
        if (userScrolling) scheduleSettling();
      },
      noteUserInteraction() {
        this.beginUserInteraction();
        this.endUserInteraction();
      },
      noteScroll() {
        if (forcing) {
          applyFollowNow();
          scheduleFollow();
          return;
        }
        if (userScrolling) {
          following = isNearBottom();
          scheduleSettling();
        }
        notify();
      },
      noteScrollEnd() {
        clearSettling();
        finishSettling();
      },
      layoutChanged,
    };
  }

  function createHeightIndex() {
    const values = [];
    const tree = [0];

    const prefix = (count) => {
      let total = 0;
      for (let index = Math.min(count, values.length); index > 0; index -= index & -index) {
        total += tree[index] || 0;
      }
      return total;
    };

    return {
      get length() { return values.length; },
      value(index) { return values[index] || 0; },
      total() { return prefix(values.length); },
      prefix,
      append(value) {
        const normalized = Math.max(0, Number(value) || 0);
        const index = values.length + 1;
        const first = index - (index & -index) + 1;
        tree[index] = prefix(index - 1) - prefix(first - 1) + normalized;
        values.push(normalized);
      },
      update(index, value) {
        if (index < 0 || index >= values.length) return false;
        const normalized = Math.max(0, Number(value) || 0);
        const delta = normalized - values[index];
        if (Math.abs(delta) < 0.25) return false;
        values[index] = normalized;
        for (let position = index + 1; position < tree.length; position += position & -position) {
          tree[position] += delta;
        }
        return true;
      },
      truncate(length) {
        const normalized = Math.max(0, Math.min(Number(length) || 0, values.length));
        values.length = normalized;
        tree.length = normalized + 1;
      },
      indexAt(offset) {
        if (!values.length) return 0;
        const target = Math.max(0, Number(offset) || 0);
        let index = 0;
        let total = 0;
        let bit = 1;
        while ((bit << 1) <= values.length) bit <<= 1;
        for (; bit > 0; bit >>= 1) {
          const next = index + bit;
          if (next <= values.length && total + tree[next] <= target) {
            index = next;
            total += tree[next];
          }
        }
        return Math.min(index, values.length - 1);
      },
    };
  }

  function createVirtualTranscript(viewport, content, options = {}, runtime = {}) {
    const documentRef = content.ownerDocument || document;
    const requestFrame = runtime.requestFrame || ((callback) => requestAnimationFrame(callback));
    const cancelFrame = runtime.cancelFrame || ((id) => cancelAnimationFrame(id));
    const createResizeObserver = runtime.createResizeObserver || ((callback) => {
      if (typeof ResizeObserver === "function") return new ResizeObserver(callback);
      return { observe() {}, disconnect() {} };
    });
    const computedStyle = runtime.getComputedStyle || ((node) => getComputedStyle(node));
    const targetHeight = Math.max(1, options.targetHeight ?? 5000);
    const edgeOverscan = Math.max(1, options.edgeOverscan ?? 900);
    const scopes = new Map();
    const topSpacer = documentRef.createElement("div");
    const windowElement = documentRef.createElement("div");
    const bottomSpacer = documentRef.createElement("div");
    topSpacer.className = "transcript-spacer transcript-spacer-top";
    windowElement.className = "transcript-window";
    bottomSpacer.className = "transcript-spacer transcript-spacer-bottom";
    topSpacer.setAttribute("aria-hidden", "true");
    bottomSpacer.setAttribute("aria-hidden", "true");
    while (content.lastChild) content.lastChild.remove();
    content.append(topSpacer);
    content.append(windowElement);
    content.append(bottomSpacer);
    let activeScopeKey = null;
    let activeScope = null;
    let frame = null;
    let preparedScroll = null;
    let stableAnchor = null;

    const keyFor = (item, index) => String(options.key?.(item, index) ?? index);
    const revisionFor = (item, index) => String(options.revision?.(item, index) ?? "");
    const contextFor = (item, index) => options.context?.(item, index) ?? null;
    const estimateFor = (item, index) => {
      const estimate = Number(options.estimateHeight?.(item, index) ?? 80);
      return Number.isFinite(estimate) ? Math.max(0, estimate) : 80;
    };
    const viewportWidth = () => Math.max(0, Number(viewport.clientWidth || content.clientWidth) || 0);
    const followingNow = () => Boolean(options.isFollowing?.());
    const scopeFor = (scopeKey) => {
      const normalized = String(scopeKey ?? "");
      let scope = scopes.get(normalized);
      if (!scope) {
        scope = {
          items: [], keys: [], revisions: [], previousContexts: [], contextAfter: [],
          heights: createHeightIndex(), measurements: new Map(), start: 0, end: 0, empty: false,
        };
        scopes.set(normalized, scope);
      }
      return [normalized, scope];
    };
    const clearWindow = () => {
      while (windowElement.lastChild) windowElement.lastChild.remove();
    };
    const setSpacerHeight = (spacer, height) => {
      const value = `${Math.max(0, height)}px`;
      if (spacer.style.height !== value) spacer.style.height = value;
    };
    const updateSpacers = () => {
      if (!activeScope || activeScope.empty) {
        setSpacerHeight(topSpacer, 0);
        setSpacerHeight(bottomSpacer, 0);
        return;
      }
      const top = activeScope.heights.prefix(activeScope.start);
      const bottom = activeScope.heights.total() - activeScope.heights.prefix(activeScope.end);
      setSpacerHeight(topSpacer, top);
      setSpacerHeight(bottomSpacer, bottom);
    };
    const truncateScope = (scope, length) => {
      scope.keys.length = length;
      scope.revisions.length = length;
      scope.previousContexts.length = length;
      scope.contextAfter.length = length;
      scope.heights.truncate(length);
    };
    const appendItem = (scope, item, index, previousContext, width) => {
      const key = keyFor(item, index);
      const revision = revisionFor(item, index);
      const estimate = estimateFor(item, index);
      const cached = scope.measurements.get(key);
      const height = estimate === 0 ? 0 : Math.max(estimate, Number(cached?.height) || 0);
      scope.keys.push(key);
      scope.revisions.push(revision);
      scope.previousContexts.push(previousContext);
      const context = contextFor(item, index);
      const nextContext = context === null ? previousContext : context;
      scope.contextAfter.push(nextContext);
      scope.heights.append(height);
      if (cached && cached.width === width && cached.revision === revision && estimate > 0) {
        scope.heights.update(index, cached.height);
      }
      return nextContext;
    };
    const syncItems = (scope, items, changedFrom, force) => {
      const width = viewportWidth();
      const start = force ? 0 : Math.max(
        0, Math.min(Number(changedFrom) || 0, items.length, scope.keys.length),
      );
      let previousContext = start > 0 ? scope.contextAfter[start - 1] ?? null : null;
      for (let index = start; index < items.length; index += 1) {
        const item = items[index];
        const key = keyFor(item, index);
        if (index >= scope.keys.length || scope.keys[index] !== key) {
          truncateScope(scope, index);
          for (let appendIndex = index; appendIndex < items.length; appendIndex += 1) {
            previousContext = appendItem(scope, items[appendIndex], appendIndex, previousContext, width);
          }
          scope.items = items;
          return;
        }
        const estimate = estimateFor(item, index);
        if (estimate === 0) scope.heights.update(index, 0);
        else if (scope.heights.value(index) === 0) scope.heights.update(index, estimate);
        scope.revisions[index] = revisionFor(item, index);
        scope.previousContexts[index] = previousContext;
        const context = contextFor(item, index);
        previousContext = context === null ? previousContext : context;
        scope.contextAfter[index] = previousContext;
      }
      if (scope.keys.length > items.length) truncateScope(scope, items.length);
      scope.items = items;
    };
    const viewportRect = () => viewport.getBoundingClientRect?.() || { top: 0, bottom: viewport.clientHeight || 0 };
    const captureAnchor = () => {
      const bounds = viewportRect();
      for (const node of [...windowElement.children]) {
        if (!node.dataset?.messageKey || typeof node.getBoundingClientRect !== "function") continue;
        const rect = node.getBoundingClientRect();
        if (rect.bottom > bounds.top) return { key: node.dataset.messageKey, offset: rect.top - bounds.top };
      }
      return null;
    };
    const restoreAnchor = (anchor) => {
      if (!anchor) return;
      const bounds = viewportRect();
      const node = [...windowElement.children].find((candidate) => candidate.dataset?.messageKey === anchor.key);
      if (!node || typeof node.getBoundingClientRect !== "function") return;
      const delta = node.getBoundingClientRect().top - bounds.top - anchor.offset;
      if (Math.abs(delta) >= 0.25) viewport.scrollTop += delta;
    };
    const outerHeight = (node) => {
      if (runtime.measureNode) return Math.max(0, Number(runtime.measureNode(node)) || 0);
      const rect = node.getBoundingClientRect?.();
      const style = computedStyle(node);
      const marginTop = Number.parseFloat(style?.marginTop) || 0;
      const marginBottom = Number.parseFloat(style?.marginBottom) || 0;
      return Math.max(0, Number(rect?.height ?? node.offsetHeight) + marginTop + marginBottom || 0);
    };
    const measureWindow = () => {
      if (!activeScope || activeScope.empty) return false;
      const width = viewportWidth();
      let changed = false;
      for (const node of [...windowElement.children]) {
        const index = Number(node.dataset?.messageIndex);
        if (!Number.isInteger(index) || index < activeScope.start || index >= activeScope.end) continue;
        const height = outerHeight(node);
        if (height <= 0) continue;
        if (activeScope.heights.update(index, height)) changed = true;
        activeScope.measurements.set(activeScope.keys[index], {
          height, width, revision: activeScope.revisions[index],
        });
      }
      return changed;
    };
    const desiredRange = (scope, following, scrollTop, forceRange) => {
      const length = scope.items.length;
      if (!length) return [0, 0];
      const total = scope.heights.total();
      if (total <= targetHeight) return [0, length];
      const viewTop = Math.max(0, Number(scrollTop) || 0);
      const viewBottom = viewTop + Math.max(0, Number(viewport.clientHeight) || 0);
      if (!forceRange && !following && scope.end > scope.start) {
        const rangeTop = scope.heights.prefix(scope.start);
        const rangeBottom = scope.heights.prefix(scope.end);
        if (viewTop >= rangeTop + edgeOverscan && viewBottom <= rangeBottom - edgeOverscan) {
          return [scope.start, scope.end];
        }
      }
      const budget = Math.max(targetHeight, viewBottom - viewTop + edgeOverscan * 2);
      const rangeTop = following
        ? Math.max(0, total - budget)
        : Math.max(0, viewTop - Math.max(edgeOverscan, (budget - (viewBottom - viewTop)) / 2));
      const rangeBottom = following ? total : Math.min(total, rangeTop + budget);
      const start = rangeTop <= 0 ? 0 : scope.heights.indexAt(rangeTop);
      const end = rangeBottom >= total ? length : Math.min(length, scope.heights.indexAt(rangeBottom) + 1);
      return [Math.min(start, Math.max(0, length - 1)), Math.max(start + 1, end)];
    };
    const renderWindow = ({ forceRender = false, forceRange = false, following = followingNow(), scrollTop = viewport.scrollTop } = {}) => {
      if (!activeScope) return false;
      if (!activeScope.items.length) {
        const changed = !activeScope.empty || forceRender;
        activeScope.empty = true;
        activeScope.start = 0;
        activeScope.end = 0;
        stableAnchor = null;
        updateSpacers();
        if (changed) options.renderEmpty?.(windowElement);
        return changed;
      }
      activeScope.empty = false;
      const [start, end] = desiredRange(activeScope, following, scrollTop, forceRange);
      const rangeChanged = start !== activeScope.start || end !== activeScope.end;
      if (!rangeChanged && !forceRender) return false;
      const anchor = following ? null : captureAnchor();
      activeScope.start = start;
      activeScope.end = end;
      updateSpacers();
      options.renderRange?.(
        windowElement, activeScope.items, start, end, activeScope.previousContexts[start] ?? null,
      );
      const measured = measureWindow();
      if (measured) updateSpacers();
      restoreAnchor(anchor);
      stableAnchor = following ? null : captureAnchor();
      options.onLayoutChange?.();
      return true;
    };
    const schedule = (callback) => {
      if (frame !== null) return;
      frame = requestFrame(() => {
        frame = null;
        callback();
      });
    };
    const measureAndRefresh = () => {
      if (!activeScope) return;
      const following = followingNow();
      const anchor = following ? null : stableAnchor || captureAnchor();
      const changed = measureWindow();
      if (changed) {
        updateSpacers();
        restoreAnchor(anchor);
        options.onLayoutChange?.();
      }
      renderWindow({ following });
      stableAnchor = following ? null : captureAnchor();
    };
    const resizeObserver = createResizeObserver(() => schedule(measureAndRefresh));
    resizeObserver.observe(viewport);
    resizeObserver.observe(windowElement);

    return {
      windowElement,
      update(items, settings = {}) {
        const [scopeKey, scope] = scopeFor(settings.scopeKey);
        const scopeChanged = activeScopeKey !== scopeKey;
        if (scopeChanged) {
          activeScopeKey = scopeKey;
          activeScope = scope;
          clearWindow();
        }
        syncItems(scope, items || [], settings.changedFrom ?? 0, Boolean(settings.force));
        const prepared = preparedScroll;
        preparedScroll = null;
        const following = prepared ? prepared.following : settings.following ?? followingNow();
        const scrollTop = prepared ? prepared.scrollTop : settings.scrollTop ?? viewport.scrollTop;
        renderWindow({
          forceRender: true,
          forceRange: scopeChanged || Boolean(prepared),
          following,
          scrollTop,
        });
      },
      noteScroll() {
        stableAnchor = followingNow() ? null : captureAnchor();
        schedule(() => renderWindow({ following: followingNow() }));
      },
      layoutChanged() {
        schedule(measureAndRefresh);
      },
      follow() {
        if (!activeScope) return;
        stableAnchor = null;
        renderWindow({ following: true, forceRange: true });
      },
      prepareScroll(scrollTop, following) {
        preparedScroll = { scrollTop: Math.max(0, Number(scrollTop) || 0), following: Boolean(following) };
      },
      inspect() {
        return activeScope ? {
          scopeKey: activeScopeKey, start: activeScope.start, end: activeScope.end,
          totalHeight: activeScope.heights.total(),
          topHeight: activeScope.heights.prefix(activeScope.start),
          bottomHeight: activeScope.heights.total() - activeScope.heights.prefix(activeScope.end),
          materialized: windowElement.children.length,
        } : null;
      },
      destroy() {
        if (frame !== null) cancelFrame(frame);
        frame = null;
        resizeObserver.disconnect();
      },
    };
  }

  function sameNodeKind(target, source) {
    if (target.nodeType !== source.nodeType) return false;
    return target.nodeType !== 1 || target.tagName === source.tagName;
  }

  function nodesEqual(target, source) {
    return typeof target.isEqualNode === "function" && target.isEqualNode(source);
  }

  function reconcileKey(node) {
    if (node?.nodeType !== 1 || typeof node.getAttribute !== "function") return null;
    return node.getAttribute("data-reconcile-key");
  }

  function sameReconcileIdentity(target, source) {
    const key = reconcileKey(target);
    return key !== null && key === reconcileKey(source) && sameNodeKind(target, source);
  }

  function preservesRuntimeAttribute(target, source, name) {
    return name === "open"
      && target.tagName === "DETAILS"
      && target.getAttribute("data-reconcile-preserve-open") !== null
      && source.getAttribute("data-reconcile-preserve-open") !== null;
  }

  function syncAttributes(target, source) {
    const desired = new Map([...source.attributes].map((attribute) => [attribute.name, attribute.value]));
    [...target.attributes].forEach((attribute) => {
      if (!desired.has(attribute.name) && !preservesRuntimeAttribute(target, source, attribute.name)) {
        target.removeAttribute(attribute.name);
      }
    });
    desired.forEach((value, name) => {
      if (target.getAttribute(name) !== value) target.setAttribute(name, value);
    });
  }

  function updateCharacterData(target, value) {
    const current = target.data;
    if (current === value) return;
    let prefix = 0;
    const sharedLength = Math.min(current.length, value.length);
    while (prefix < sharedLength && current[prefix] === value[prefix]) prefix += 1;
    const splitsPrefixPair = prefix > 0
      && prefix < current.length
      && prefix < value.length
      && current.charCodeAt(prefix - 1) >= 0xd800
      && current.charCodeAt(prefix - 1) <= 0xdbff;
    if (splitsPrefixPair) prefix -= 1;
    let suffix = 0;
    while (
      suffix < current.length - prefix
      && suffix < value.length - prefix
      && current[current.length - suffix - 1] === value[value.length - suffix - 1]
    ) suffix += 1;
    const currentSuffixStart = current.length - suffix;
    const valueSuffixStart = value.length - suffix;
    const splitsSuffixPair = suffix > 0
      && (
        (currentSuffixStart > 0
          && current.charCodeAt(currentSuffixStart - 1) >= 0xd800
          && current.charCodeAt(currentSuffixStart - 1) <= 0xdbff)
        || (valueSuffixStart > 0
          && value.charCodeAt(valueSuffixStart - 1) >= 0xd800
          && value.charCodeAt(valueSuffixStart - 1) <= 0xdbff)
      );
    if (splitsSuffixPair) suffix -= 1;
    if (typeof target.replaceData === "function") {
      target.replaceData(
        prefix,
        current.length - prefix - suffix,
        value.slice(prefix, value.length - suffix),
      );
    } else {
      target.data = value;
    }
  }

  function reconcileNode(target, source) {
    if (!sameNodeKind(target, source)) {
      target.replaceWith(source);
      return source;
    }
    if (target.nodeType === 3 || target.nodeType === 8) {
      updateCharacterData(target, source.data);
      return target;
    }
    if (target.nodeType === 1) {
      if (nodesEqual(target, source)) return target;
      syncAttributes(target, source);
      reconcileChildren(target, source);
    }
    return target;
  }

  function reconcileChildren(target, source) {
    const incoming = [...source.childNodes];
    for (let index = 0; index < incoming.length; index += 1) {
      const sourceNode = incoming[index];
      const current = target.childNodes[index];
      if (!current) {
        target.append(sourceNode);
        continue;
      }
      if (nodesEqual(current, sourceNode)) continue;
      const reusable = [...target.childNodes].slice(index + 1)
        .find((candidate) => nodesEqual(candidate, sourceNode)
          || sameReconcileIdentity(candidate, sourceNode));
      if (reusable) {
        target.insertBefore(reusable, current);
        continue;
      }
      const nextSource = incoming[index + 1];
      if (
        nextSource
        && (nodesEqual(current, nextSource) || sameReconcileIdentity(current, nextSource))
      ) {
        target.insertBefore(sourceNode, current);
        continue;
      }
      reconcileNode(current, sourceNode);
    }
    while (target.childNodes.length > incoming.length) target.lastChild.remove();
    return target;
  }

  function captureSelection(container, documentRef) {
    const selection = typeof documentRef.getSelection === "function"
      ? documentRef.getSelection()
      : null;
    if (!selection || selection.rangeCount === 0) return null;
    const { anchorNode, anchorOffset, focusNode, focusOffset } = selection;
    if (!anchorNode || !focusNode || !container.contains(anchorNode) || !container.contains(focusNode)) return null;
    return { selection, anchorNode, anchorOffset, focusNode, focusOffset };
  }

  function boundedSelectionOffset(node, offset) {
    const maximum = node.nodeType === 3 || node.nodeType === 8
      ? node.data.length
      : node.childNodes.length;
    return Math.min(offset, maximum);
  }

  function restoreSelection(container, snapshot, documentRef) {
    if (!snapshot || !container.contains(snapshot.anchorNode) || !container.contains(snapshot.focusNode)) return;
    const anchorOffset = boundedSelectionOffset(snapshot.anchorNode, snapshot.anchorOffset);
    const focusOffset = boundedSelectionOffset(snapshot.focusNode, snapshot.focusOffset);
    try {
      if (typeof snapshot.selection.setBaseAndExtent === "function") {
        snapshot.selection.setBaseAndExtent(
          snapshot.anchorNode,
          anchorOffset,
          snapshot.focusNode,
          focusOffset,
        );
      } else {
        const range = documentRef.createRange();
        range.setStart(snapshot.anchorNode, anchorOffset);
        range.setEnd(snapshot.focusNode, focusOffset);
        snapshot.selection.removeAllRanges();
        snapshot.selection.addRange(range);
      }
    } catch (_) {
      // A browser may invalidate a boundary while an incompatible streamed structure is replaced.
    }
  }

  function reconcileHtmlChildren(container, html, documentRef = container.ownerDocument || document) {
    const selection = captureSelection(container, documentRef);
    const template = documentRef.createElement("template");
    template.innerHTML = html;
    const result = reconcileChildren(container, template.content);
    restoreSelection(container, selection, documentRef);
    return result;
  }

  return Object.freeze({
    createTranscriptBottomFollower,
    createVirtualTranscript,
    reconcileNode,
    reconcileChildren,
    reconcileHtmlChildren,
  });
}));
