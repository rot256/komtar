const BASE = new URL("./", import.meta.url);
const COMMENTS_URL = new URL("api/comments", BASE);
const STATUS_URL = new URL("api/status", BASE);
const HOST_ID = "komtar";
const MAX_SELECTED_TEXT = 2000;
const MAX_TEXT = 4000;
const MAX_HTML = 8000;

const styles = `
  :host {
    all: initial;
    color-scheme: light;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: 13px;
  }

  *, *::before, *::after { box-sizing: border-box; }

  #highlight {
    position: fixed;
    display: none;
    z-index: 2147483645;
    border: 2px solid #eb5e28;
    background: rgb(235 94 40 / 10%);
    box-shadow: 0 0 0 1px rgb(255 255 255 / 90%);
    pointer-events: none;
  }

  #badge {
    position: fixed;
    right: 14px;
    bottom: 14px;
    z-index: 2147483644;
    padding: 7px 10px;
    border: 1px solid rgb(255 255 255 / 30%);
    border-radius: 999px;
    background: #161616;
    color: #fff;
    box-shadow: 0 3px 12px rgb(0 0 0 / 22%);
    font: 600 12px/1 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    pointer-events: none;
  }

  #toast {
    position: fixed;
    right: 14px;
    bottom: 52px;
    z-index: 2147483644;
    max-width: min(360px, calc(100vw - 28px));
    padding: 9px 11px;
    border-radius: 5px;
    background: #161616;
    color: #fff;
    box-shadow: 0 4px 18px rgb(0 0 0 / 24%);
    font: 13px/1.35 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }

  #toast[hidden] { display: none; }

  dialog {
    position: fixed;
    inset: auto;
    width: min(420px, calc(100vw - 24px));
    margin: 0;
    padding: 0;
    border: 1px solid #444;
    border-radius: 7px;
    background: #f8f7f4;
    color: #161616;
    box-shadow: 0 16px 48px rgb(0 0 0 / 28%);
  }

  dialog::backdrop { background: rgb(0 0 0 / 6%); }
  form { padding: 14px; }

  label {
    display: block;
    margin-bottom: 8px;
    font-weight: 700;
    line-height: 1.3;
  }

  #selection-preview {
    margin: 0 0 10px;
    padding: 8px 10px;
    border-left: 3px solid #eb5e28;
    background: #eeeae2;
  }

  #selection-preview[hidden] { display: none; }

  #selection-label {
    margin-bottom: 4px;
    color: #655f58;
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  #selection-text {
    max-height: 96px;
    margin: 0;
    overflow: auto;
    color: #2b2926;
    font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    white-space: pre-wrap;
  }

  textarea {
    display: block;
    width: 100%;
    min-height: 112px;
    resize: vertical;
    padding: 9px 10px;
    border: 1px solid #888;
    border-radius: 4px;
    background: #fff;
    color: #161616;
    font: 14px/1.4 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }

  textarea:focus { outline: 2px solid #287088; outline-offset: 1px; }

  #status {
    min-height: 18px;
    margin-top: 7px;
    color: #a12c10;
    font-size: 11px;
    line-height: 1.4;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 10px;
  }

  button {
    padding: 7px 11px;
    border: 1px solid #555;
    border-radius: 4px;
    background: #fff;
    color: #161616;
    cursor: pointer;
    font: 600 12px/1 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }

  button[type="submit"] { background: #161616; color: #fff; }
  button:disabled { cursor: wait; opacity: 0.55; }
`;

function truncate(value, length) {
  return value.length <= length ? value : `${value.slice(0, length - 1)}…`;
}

function normalizeText(value) {
  return value.replace(/\s+/g, " ").trim();
}

function roundCssPixel(value) {
  return Math.round(value * 100) / 100;
}

function point(x, y) {
  return { x: roundCssPixel(x), y: roundCssPixel(y) };
}

function selectedTextFor(target) {
  const selection = window.getSelection();
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  try {
    if (!selection.getRangeAt(0).intersectsNode(target)) return null;
  } catch {
    return null;
  }
  const selected = normalizeText(selection.toString());
  return selected ? truncate(selected, MAX_SELECTED_TEXT) : null;
}

function elementText(target) {
  const raw = target instanceof HTMLElement ? target.innerText : target.textContent;
  return truncate(normalizeText(raw ?? ""), MAX_TEXT);
}

function uniqueIdSelector(element) {
  if (!element.id) return null;
  const selector = `#${CSS.escape(element.id)}`;
  return document.querySelectorAll(selector).length === 1 ? selector : null;
}

function cssSelector(target) {
  const parts = [];
  let element = target;
  while (element) {
    const idSelector = uniqueIdSelector(element);
    if (idSelector) {
      parts.unshift(idSelector);
      break;
    }
    let part = element.tagName.toLowerCase();
    const parent = element.parentElement;
    if (parent) {
      const sameTag = Array.from(parent.children).filter(
        (sibling) => sibling.tagName === element.tagName,
      );
      if (sameTag.length > 1) part += `:nth-of-type(${sameTag.indexOf(element) + 1})`;
    }
    parts.unshift(part);
    element = parent;
  }
  return parts.join(" > ");
}

function captureTarget(target, event) {
  const rect = target.getBoundingClientRect();
  return {
    element: target,
    context: {
      page: { url: window.location.href, title: document.title },
      target: {
        selector: cssSelector(target),
        tag: target.tagName.toLowerCase(),
        id: target.id || null,
        classes: Array.from(target.classList),
        selectedText: selectedTextFor(target),
        text: elementText(target),
        html: truncate(target.outerHTML, MAX_HTML),
      },
      pointer: {
        page: point(event.pageX, event.pageY),
        viewport: point(event.clientX, event.clientY),
        target: point(event.clientX - rect.left, event.clientY - rect.top),
        scroll: point(window.scrollX, window.scrollY),
        viewportSize: {
          width: roundCssPixel(window.innerWidth),
          height: roundCssPixel(window.innerHeight),
        },
        targetSize: {
          width: roundCssPixel(rect.width),
          height: roundCssPixel(rect.height),
        },
        devicePixelRatio: roundCssPixel(window.devicePixelRatio),
      },
    },
  };
}

function responseMessage(value, fallback) {
  return value && typeof value === "object" && typeof value.error === "string"
    ? value.error
    : fallback;
}

function install() {
  if (document.getElementById(HOST_ID)) return;
  const host = document.createElement("div");
  host.id = HOST_ID;
  const shadow = host.attachShadow({ mode: "open" });
  shadow.innerHTML = `
    <style>${styles}</style>
    <div id="highlight" aria-hidden="true"></div>
    <div id="badge" role="status" aria-live="polite">0 queued</div>
    <div id="toast" role="status" aria-live="polite" hidden></div>
    <dialog id="komtar-dialog" aria-labelledby="dialog-label">
      <form>
        <label id="dialog-label" for="comment">Suggest Edit:</label>
        <div id="selection-preview" hidden>
          <div id="selection-label">Selected text</div>
          <blockquote id="selection-text"></blockquote>
        </div>
        <textarea id="comment" name="comment" maxlength="10000" required></textarea>
        <div id="status" role="alert"></div>
        <div class="actions">
          <button id="cancel" type="button">Cancel</button>
          <button id="send" type="submit">Queue comment</button>
        </div>
      </form>
    </dialog>
  `;
  document.body.append(host);

  const highlight = shadow.querySelector("#highlight");
  const badge = shadow.querySelector("#badge");
  const toast = shadow.querySelector("#toast");
  const dialog = shadow.querySelector("dialog");
  const form = shadow.querySelector("form");
  const selectionPreview = shadow.querySelector("#selection-preview");
  const selectionText = shadow.querySelector("#selection-text");
  const textarea = shadow.querySelector("textarea");
  const status = shadow.querySelector("#status");
  const cancel = shadow.querySelector("#cancel");
  const send = shadow.querySelector("#send");
  if (
    !highlight || !badge || !toast || !dialog || !form || !selectionPreview ||
    !selectionText || !textarea || !status || !cancel || !send
  ) {
    host.remove();
    return;
  }

  let captured = null;
  let toastTimer;

  const setPending = (pending) => {
    badge.textContent = `${pending} queued`;
    badge.dataset.pending = String(pending);
  };

  const showToast = (message) => {
    toast.textContent = message;
    toast.hidden = false;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.hidden = true; }, 2400);
  };

  const updateHighlight = () => {
    if (!captured || !dialog.open || !captured.element.isConnected) {
      highlight.style.display = "none";
      return;
    }
    const rect = captured.element.getBoundingClientRect();
    highlight.style.display = "block";
    highlight.style.left = `${rect.left}px`;
    highlight.style.top = `${rect.top}px`;
    highlight.style.width = `${rect.width}px`;
    highlight.style.height = `${rect.height}px`;
  };

  const stopTracking = () => {
    window.removeEventListener("resize", updateHighlight);
    window.removeEventListener("scroll", updateHighlight, true);
  };

  const closeDialog = () => {
    if (dialog.open) dialog.close();
    stopTracking();
    highlight.style.display = "none";
    captured = null;
    selectionPreview.hidden = true;
    selectionText.textContent = "";
    status.textContent = "";
    textarea.value = "";
  };

  const placeDialog = (x, y) => {
    const gap = 12;
    const width = dialog.offsetWidth;
    const height = dialog.offsetHeight;
    dialog.style.left = `${Math.max(gap, Math.min(x + gap, window.innerWidth - width - gap))}px`;
    dialog.style.top = `${Math.max(gap, Math.min(y + gap, window.innerHeight - height - gap))}px`;
  };

  const openDialog = (target, event) => {
    if (dialog.open) closeDialog();
    captured = captureTarget(target, event);
    const selectedText = captured.context.target.selectedText;
    selectionPreview.hidden = selectedText === null;
    selectionText.textContent = selectedText ?? "";
    status.textContent = "";
    textarea.value = "";
    dialog.showModal();
    placeDialog(event.clientX, event.clientY);
    updateHighlight();
    window.addEventListener("resize", updateHighlight);
    window.addEventListener("scroll", updateHighlight, true);
    textarea.focus();
  };

  document.addEventListener("contextmenu", (event) => {
    if (event.shiftKey || event.composedPath().includes(host)) return;
    if (!(event.target instanceof Element)) return;
    event.preventDefault();
    openDialog(event.target, event);
  }, true);

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!captured) return;
    const comment = textarea.value.trim();
    if (!comment) {
      status.textContent = "Enter a comment before queueing it.";
      textarea.focus();
      return;
    }

    send.disabled = true;
    cancel.disabled = true;
    status.textContent = "Queueing…";
    const context = captured.context;
    void fetch(COMMENTS_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ comment, ...context }),
    })
      .then(async (response) => {
        const body = await response.json().catch(() => null);
        if (!response.ok) throw new Error(responseMessage(body, `request failed (${response.status})`));
        if (!body || typeof body.pending !== "number") {
          throw new Error("server returned an invalid queue status");
        }
        setPending(body.pending);
        closeDialog();
        showToast(`Comment queued · ${body.pending} pending`);
      })
      .catch((error) => {
        status.textContent = error instanceof Error ? error.message : "Could not queue comment";
        textarea.focus();
      })
      .finally(() => {
        send.disabled = false;
        cancel.disabled = false;
      });
  });

  textarea.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      form.requestSubmit();
    }
  });
  cancel.addEventListener("click", closeDialog);
  dialog.addEventListener("cancel", (event) => {
    event.preventDefault();
    closeDialog();
  });
  dialog.addEventListener("pointerdown", (event) => {
    const rect = dialog.getBoundingClientRect();
    if (
      event.clientX < rect.left || event.clientX > rect.right ||
      event.clientY < rect.top || event.clientY > rect.bottom
    ) closeDialog();
  });
  dialog.addEventListener("close", () => {
    stopTracking();
    highlight.style.display = "none";
  });

  const refreshPending = () => {
    void fetch(STATUS_URL)
      .then(async (response) => {
        if (!response.ok) return;
        const body = await response.json();
        if (typeof body.pending === "number") setPending(body.pending);
      })
      .catch(() => undefined);
  };
  refreshPending();
  setInterval(refreshPending, 750);
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", install, { once: true });
} else {
  install();
}
