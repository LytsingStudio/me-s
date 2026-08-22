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
      const previous = properties.map((name) => ({
        name,
        value: style.getPropertyValue(name),
        priority: style.getPropertyPriority(name),
      }));
      // Recreate the scroll layer before the next paint so compositor momentum cannot overwrite follow().
      style.setProperty("overflow", "hidden", "important");
      style.setProperty("-webkit-overflow-scrolling", "auto", "important");
      void viewport.offsetHeight;
      previous.forEach(({ name, value, priority }) => {
        if (value) style.setProperty(name, value, priority);
        else style.removeProperty(name);
      });
      void viewport.offsetHeight;
    });
    const threshold = runtime.threshold ?? 24;
    const settleDelay = runtime.settleDelay ?? 180;
    let following = true;
    let userScrolling = false;
    let forcing = false;
    let frame = null;
    let settleTimer = null;

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
    const applyFollowNow = () => {
      if (!following || (userScrolling && !forcing)) return;
      viewport.scrollTop = viewport.scrollHeight;
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
        following = Boolean(value);
        forcing = false;
        userScrolling = false;
        clearSettling();
        cancelScheduledFollow();
        if (following) scheduleFollow(); else notify();
      },
      beginUserInteraction() {
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

  function sameNodeKind(target, source) {
    if (target.nodeType !== source.nodeType) return false;
    return target.nodeType !== 1 || target.tagName === source.tagName;
  }

  function nodesEqual(target, source) {
    return typeof target.isEqualNode === "function" && target.isEqualNode(source);
  }

  function syncAttributes(target, source) {
    const desired = new Map([...source.attributes].map((attribute) => [attribute.name, attribute.value]));
    [...target.attributes].forEach((attribute) => {
      if (!desired.has(attribute.name)) target.removeAttribute(attribute.name);
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
        .find((candidate) => nodesEqual(candidate, sourceNode));
      if (reusable) {
        target.insertBefore(reusable, current);
        continue;
      }
      const nextSource = incoming[index + 1];
      if (nextSource && nodesEqual(current, nextSource)) {
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
    reconcileNode,
    reconcileChildren,
    reconcileHtmlChildren,
  });
}));
