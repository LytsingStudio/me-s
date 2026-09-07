(() => {
  "use strict";

  const queue = [];
  let active = 0;
  function enqueue(run) {
    queue.push(run);
    drain();
  }
  function drain() {
    while (active < 3 && queue.length) {
      const run = queue.shift();
      active++;
      Promise.resolve().then(run).finally(() => { active--; drain(); });
    }
  }

  let closeViewer = null;
  function showImageViewer(item, index, opener) {
    closeViewer?.();
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop image-viewer-backdrop";
    const viewer = document.createElement("section");
    viewer.className = "image-viewer";
    viewer.setAttribute("role", "dialog");
    viewer.setAttribute("aria-modal", "true");
    viewer.setAttribute("aria-label", `图片 ${index + 1}`);
    const header = document.createElement("header");
    const title = document.createElement("span");
    title.textContent = `图片 ${index + 1} · ${item.width} × ${item.height}`;
    const save = document.createElement("a");
    save.href = item.original;
    save.download = item.filename;
    save.className = "tool-image-save";
    save.textContent = "保存原图";
    const dismiss = document.createElement("button");
    dismiss.type = "button";
    dismiss.className = "image-viewer-close";
    dismiss.setAttribute("aria-label", "关闭大图");
    dismiss.textContent = "×";
    header.append(title, save, dismiss);
    const stage = document.createElement("div");
    stage.className = "image-viewer-stage";
    const image = document.createElement("img");
    image.alt = `图片 ${index + 1}`;
    image.decoding = "async";
    image.hidden = true;
    const status = document.createElement("span");
    status.className = "tool-image-status";
    status.setAttribute("role", "status");
    const retry = document.createElement("button");
    retry.type = "button";
    retry.className = "tool-image-retry";
    retry.textContent = "重新加载";
    retry.hidden = true;
    stage.append(image, status, retry);
    viewer.append(header, stage);
    backdrop.append(viewer);
    const background = [...document.body.children].map((element) => [element, element.inert]);
    for (const [element] of background) element.inert = true;
    document.body.append(backdrop);
    let active = true, loading = false, saving = false;
    let controller = null, timeout = null, url = null;
    const close = () => {
      if (!active) return;
      active = false;
      controller?.abort();
      clearTimeout(timeout);
      image.onload = image.onerror = null;
      image.removeAttribute("src");
      if (url) URL.revokeObjectURL(url);
      backdrop.remove();
      for (const [element, inert] of background) element.inert = inert;
      if (closeViewer === close) closeViewer = null;
      if (opener.isConnected) opener.focus({ preventScroll: true });
    };
    closeViewer = close;
    dismiss.onclick = close;
    backdrop.onclick = (event) => { if (event.target === backdrop) close(); };
    backdrop.onkeydown = (event) => {
      event.stopPropagation();
      if (event.key === "Escape") { event.preventDefault(); close(); }
      if (event.key === "Tab") {
        const controls = retry.hidden ? [save, dismiss] : [save, dismiss, retry];
        const at = controls.indexOf(document.activeElement);
        event.preventDefault();
        controls[(at + (event.shiftKey ? -1 : 1) + controls.length) % controls.length].focus({ preventScroll: true });
      }
    };
    save.onclick = async (event) => {
      const runtime = globalThis.MeFrontendRuntime;
      if (!runtime?.capabilities?.nativeDownload) return;
      event.preventDefault();
      if (saving) return;
      saving = true;
      save.textContent = "正在保存…";
      try {
        await runtime.downloadFile(item.original, item.filename);
        if (active) save.textContent = "已保存原图";
      } catch {
        if (active) save.textContent = "保存失败，重试";
      } finally { saving = false; }
    };
    const failed = () => {
      if (!active) return;
      image.hidden = true;
      status.hidden = false;
      status.textContent = "无法加载图片";
      retry.hidden = false;
    };
    const load = async () => {
      if (loading || !active) return;
      loading = true;
      retry.hidden = true;
      status.hidden = false;
      status.textContent = "正在加载…";
      image.hidden = true;
      image.removeAttribute("src");
      if (url) { URL.revokeObjectURL(url); url = null; }
      controller = new AbortController();
      timeout = setTimeout(() => controller.abort(), 60000);
      try {
        const response = await fetch(item.original, { signal: controller.signal, cache: "no-store" });
        if (!response.ok || !String(response.headers.get("Content-Type")).startsWith("image/")) throw new Error("image unavailable");
        const blob = await response.blob();
        if (!active) return;
        if (controller.signal.aborted) throw new Error("image request timed out");
        url = URL.createObjectURL(blob);
        image.onload = () => { if (active) { image.hidden = false; status.hidden = true; } };
        image.onerror = failed;
        image.src = url;
      } catch { failed(); } finally { clearTimeout(timeout); loading = false; }
    };
    retry.onclick = load;
    dismiss.focus({ preventScroll: true });
    void load();
    return close;
  }

  class MeImageGallery extends HTMLElement {
    connectedCallback() {
      let items;
      try { items = JSON.parse(this.dataset.items || "[]"); } catch { return; }
      if (!Array.isArray(items)) return;
      this.session = { active: true, controllers: new Set(), urls: new Set() };
      this.replaceChildren();
      this.onclick = (event) => event.stopPropagation();
      this.onkeydown = (event) => event.stopPropagation();
      const session = this.session;
      this.observer = typeof IntersectionObserver === "function" ? new IntersectionObserver((entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          this.observer.unobserve(entry.target);
          entry.target.loadPreview();
        }
      }) : null;
      for (const [index, item] of items.slice(0, 16).entries()) {
        if (!this.validPath(item.preview, "preview") || !this.validPath(item.original, "original")) continue;
        const figure = document.createElement("figure");
        const frame = document.createElement("div");
        frame.className = "tool-image-frame";
        const image = document.createElement("img");
        image.alt = `图片 ${index + 1}`;
        image.decoding = "async";
        image.hidden = true;
        const status = document.createElement("span");
        status.className = "tool-image-status";
        status.textContent = "图片预览";
        frame.append(image, status);
        const retry = document.createElement("button");
        retry.type = "button";
        retry.className = "tool-image-retry";
        retry.textContent = "重新加载";
        retry.hidden = true;
        frame.append(retry);
        const open = document.createElement("button");
        open.type = "button";
        open.className = "tool-image-open";
        open.setAttribute("aria-label", `查看大图：图片 ${index + 1}`);
        open.onclick = () => { session.closeViewer = showImageViewer(item, index, open); };
        frame.append(open);
        const caption = document.createElement("figcaption");
        const dimensions = document.createElement("span");
        dimensions.textContent = `${item.width} × ${item.height}`;
        caption.append(dimensions);
        figure.append(frame, caption);
        this.append(figure);

        let loading = false;
        let currentUrl = null;
        figure.loadPreview = () => {
          if (loading || !session.active) return;
          loading = true;
          retry.hidden = true;
          status.textContent = "正在加载…";
          enqueue(async () => {
            if (!session.active) return;
            const controller = new AbortController();
            session.controllers.add(controller);
            const timeout = setTimeout(() => controller.abort(), 15000);
            try {
              const response = await fetch(item.preview, { signal: controller.signal, cache: "no-store" });
              if (!response.ok || !String(response.headers.get("Content-Type")).startsWith("image/jpeg")) throw new Error("preview unavailable");
              const blob = await response.blob();
              if (!session.active || controller.signal.aborted) return;
              if (currentUrl) {
                URL.revokeObjectURL(currentUrl);
                session.urls.delete(currentUrl);
              }
              const url = URL.createObjectURL(blob);
              currentUrl = url;
              session.urls.add(url);
              image.onload = () => { if (session.active) { image.hidden = false; status.hidden = true; } };
              image.onerror = () => { if (session.active) { status.textContent = "无法加载图片"; retry.hidden = false; } };
              image.src = url;
            } catch {
              if (session.active) { status.textContent = "无法加载图片"; retry.hidden = false; }
            } finally {
              clearTimeout(timeout);
              session.controllers.delete(controller);
              loading = false;
            }
          });
        };
        retry.onclick = () => figure.loadPreview();
        if (this.observer) this.observer.observe(figure);
        else figure.loadPreview();
      }
    }

    validPath(path, operation) {
      return typeof path === "string" && new RegExp(`^/api/(?:workspaces/[a-zA-Z0-9_-]+/)?images/[a-zA-Z0-9_-]+/[0-9a-f]{64}/${operation}$`).test(path);
    }

    disconnectedCallback() {
      this.observer?.disconnect();
      const session = this.session;
      if (!session) return;
      session.active = false;
      session.closeViewer?.();
      for (const controller of session.controllers) controller.abort();
      for (const url of session.urls) URL.revokeObjectURL(url);
      session.urls.clear();
    }
  }

  customElements.define("me-image-gallery", MeImageGallery);
})();
