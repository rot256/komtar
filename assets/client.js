const BASE = new URL("./", import.meta.url);
const COMMENTS_URL = new URL("api/comments", BASE);
const STATUS_URL = new URL("api/status", BASE);
const RELOAD_URL = new URL("api/reload", BASE);
const MESSAGES_URL = new URL("api/messages", BASE);
const LIVE_REVISION = new URL(import.meta.url).searchParams.get("live");
const HOST_ID = "komtar";
const MAX_SELECTED_TEXT = 2000;
const MAX_TEXT = 4000;
const MAX_HTML = 8000;
const DISMISSED_KEY = "komtar-dismissed-v1";
const AGENT_TARGET_PREFIX = "komtar-agent:";

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

  #agent-log { display: contents; }

  #agent-general {
    position: fixed;
    right: 14px;
    bottom: 52px;
    z-index: 2147483643;
    display: flex;
    width: min(360px, calc(100vw - 28px));
    max-height: calc(100vh - 66px);
    flex-direction: column;
    gap: 8px;
    overflow: auto;
    pointer-events: none;
  }

  #agent-anchored {
    position: fixed;
    inset: 0;
    z-index: 2147483643;
    pointer-events: none;
  }

  .agent-message {
    position: relative;
    width: min(340px, calc(100vw - 16px));
    max-height: calc(100vh - 16px);
    padding: 12px 34px 12px 13px;
    border: 1px solid #45413c;
    border-radius: 7px;
    background: #fffdf8;
    color: #1d1b19;
    box-shadow: 0 6px 24px rgb(0 0 0 / 24%);
    font: 13px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    overflow-wrap: anywhere;
    overflow-y: auto;
    pointer-events: auto;
  }

  #agent-anchored .agent-message { position: fixed; }
  .agent-message p { margin: 0 0 8px; }
  .agent-message p:last-child { margin-bottom: 0; }
  .agent-message ul, .agent-message ol { margin: 6px 0; padding-left: 22px; }
  .agent-message pre { overflow: auto; }
  .agent-message code { font: inherit; background: #eeeae2; }
  .agent-message a { color: #075f7a; text-decoration: underline; }
  .agent-message.has-anchor { cursor: pointer; }
  .agent-message.has-anchor:focus-visible {
    outline: 3px solid #287088;
    outline-offset: 2px;
  }

  .agent-dismiss {
    position: absolute;
    top: 5px;
    right: 5px;
    width: 25px;
    height: 25px;
    padding: 0;
    border: 0;
    border-radius: 4px;
    background: transparent;
    color: #504b45;
    font: 18px/1 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }

  .agent-dismiss:hover, .agent-dismiss:focus-visible { background: #eeeae2; }

  .agent-unavailable {
    margin-top: 8px;
    color: #7b3221;
    font-size: 11px;
  }

  .agent-unavailable[hidden] { display: none; }

  #message-highlight {
    position: fixed;
    display: none;
    z-index: 2147483646;
    border: 3px solid #0b7896;
    border-radius: 3px;
    background: rgb(11 120 150 / 12%);
    box-shadow: 0 0 0 3px rgb(255 255 255 / 85%);
    pointer-events: none;
  }

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

  #reload-notice {
    margin-top: 7px;
    color: #655f58;
    font-size: 11px;
    line-height: 1.4;
  }

  #reload-notice[hidden] { display: none; }

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

function captureTarget(target, event, selector = cssSelector(target)) {
  const rect = target.getBoundingClientRect();
  return {
    element: target,
    context: {
      page: { url: window.location.href, title: document.title },
      target: {
        selector,
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

function capturePageTarget() {
  const viewport = point(window.innerWidth / 2, window.innerHeight / 2);
  return {
    element: null,
    context: {
      page: { url: window.location.href, title: document.title },
      target: {
        selector: "",
        tag: "page",
        id: null,
        classes: [],
        selectedText: null,
        text: "",
        html: "",
      },
      pointer: {
        page: point(viewport.x + window.scrollX, viewport.y + window.scrollY),
        viewport,
        target: point(0, 0),
        scroll: point(window.scrollX, window.scrollY),
        viewportSize: {
          width: roundCssPixel(window.innerWidth),
          height: roundCssPixel(window.innerHeight),
        },
        targetSize: { width: 0, height: 0 },
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
    <div id="message-highlight" aria-hidden="true"></div>
    <div id="badge" role="status" aria-live="polite">0 queued</div>
    <div id="toast" role="status" aria-live="polite" hidden></div>
    <div id="agent-log" role="log" aria-live="polite" aria-relevant="additions">
      <div id="agent-general"></div>
      <div id="agent-anchored"></div>
    </div>
    <dialog id="komtar-dialog" aria-labelledby="dialog-label">
      <form>
        <label id="dialog-label" for="comment">Suggest Edit:</label>
        <div id="selection-preview" hidden>
          <div id="selection-label">Selected text</div>
          <blockquote id="selection-text"></blockquote>
        </div>
        <textarea id="comment" name="comment" maxlength="10000" required></textarea>
        <div id="status" role="alert"></div>
        <div id="reload-notice" role="status" hidden>
          Files changed. Reloading when this edit closes.
        </div>
        <div class="actions">
          <button id="cancel" type="button">Cancel</button>
          <button id="send" type="submit">Queue comment</button>
        </div>
      </form>
    </dialog>
  `;
  document.body.append(host);

  const highlight = shadow.querySelector("#highlight");
  const messageHighlight = shadow.querySelector("#message-highlight");
  const badge = shadow.querySelector("#badge");
  const toast = shadow.querySelector("#toast");
  const agentGeneral = shadow.querySelector("#agent-general");
  const agentAnchored = shadow.querySelector("#agent-anchored");
  const dialog = shadow.querySelector("dialog");
  const dialogLabel = shadow.querySelector("#dialog-label");
  const form = shadow.querySelector("form");
  const selectionPreview = shadow.querySelector("#selection-preview");
  const selectionText = shadow.querySelector("#selection-text");
  const textarea = shadow.querySelector("textarea");
  const status = shadow.querySelector("#status");
  const reloadNotice = shadow.querySelector("#reload-notice");
  const cancel = shadow.querySelector("#cancel");
  const send = shadow.querySelector("#send");
  if (
    !highlight || !messageHighlight || !badge || !toast || !agentGeneral ||
    !agentAnchored || !dialog || !dialogLabel || !form || !selectionPreview ||
    !selectionText || !textarea || !status || !reloadNotice || !cancel || !send
  ) {
    host.remove();
    return;
  }

  let captured = null;
  let toastTimer;
  let pendingReload = false;
  let reloadRevision = LIVE_REVISION;
  let placementFrame;
  let messageHighlightTimer;
  const agentMessages = new Map();
  const dismissed = (() => {
    try {
      const stored = JSON.parse(sessionStorage.getItem(DISMISSED_KEY) ?? "[]");
      return new Set(Array.isArray(stored) ? stored.filter((id) => typeof id === "string") : []);
    } catch {
      return new Set();
    }
  })();

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

  const persistDismissed = () => {
    try {
      sessionStorage.setItem(DISMISSED_KEY, JSON.stringify(Array.from(dismissed)));
    } catch {
      // Dismissal remains active for this document if storage is unavailable.
    }
  };

  const targetForSelector = (selector) => {
    if (selector.startsWith(AGENT_TARGET_PREFIX)) {
      const id = selector.slice(AGENT_TARGET_PREFIX.length);
      return agentMessages.get(id)?.node ?? null;
    }
    return document.querySelector(selector);
  };

  const anchorTarget = (anchor) => {
    if (anchor === null) return null;
    try {
      const target = targetForSelector(anchor);
      if (!target || target === host || target.getClientRects().length === 0) return undefined;
      return target;
    } catch {
      return undefined;
    }
  };

  const placeAgentMessages = () => {
    placementFrame = undefined;
    const anchoredGroups = new Map();
    for (const entry of agentMessages.values()) {
      const target = anchorTarget(entry.anchor);
      if (target instanceof Element) {
        entry.unavailable.hidden = true;
        if (entry.node.parentElement !== agentAnchored) agentAnchored.append(entry.node);
        const group = anchoredGroups.get(target) ?? [];
        group.push(entry.node);
        anchoredGroups.set(target, group);
      } else {
        entry.unavailable.hidden = entry.anchor === null;
        if (entry.node.parentElement !== agentGeneral) agentGeneral.append(entry.node);
        entry.node.style.removeProperty("left");
        entry.node.style.removeProperty("top");
      }
    }

    const gap = 8;
    const edge = 8;
    const occupied = Array.from(agentGeneral.children).map((node) => {
      const rect = node.getBoundingClientRect();
      return {
        left: rect.left,
        right: rect.right,
        top: rect.top,
        bottom: rect.bottom,
      };
    });
    const overlapsOccupied = (candidate) => occupied.some((rect) => !(
      candidate.right + gap <= rect.left ||
      candidate.left >= rect.right + gap ||
      candidate.bottom + gap <= rect.top ||
      candidate.top >= rect.bottom + gap
    ));
    const availablePlacement = (rect, width, height, desiredTop) => {
      const maximumLeft = window.innerWidth - width - edge;
      const right = Math.max(edge, Math.min(rect.right + gap, maximumLeft));
      const left = Math.max(edge, Math.min(rect.left - width - gap, maximumLeft));
      const horizontal = rect.right + gap + width <= window.innerWidth - edge
        ? [right, left]
        : [left, right];
      const maximumTop = window.innerHeight - height - edge;
      const vertical = [
        desiredTop,
        ...occupied.flatMap((placed) => [
          placed.bottom + gap,
          placed.top - height - gap,
        ]),
      ].map((top) => Math.max(edge, Math.min(top, maximumTop)));
      const candidates = horizontal.flatMap((candidateLeft, side) =>
        vertical.map((candidateTop) => ({
          left: candidateLeft,
          right: candidateLeft + width,
          top: candidateTop,
          bottom: candidateTop + height,
          score: Math.abs(candidateTop - desiredTop) * 2 + side,
        })))
        .sort((first, second) => first.score - second.score);
      return candidates.find((candidate) => !overlapsOccupied(candidate)) ?? candidates[0];
    };
    for (const [target, nodes] of anchoredGroups) {
      const rect = target.getBoundingClientRect();
      const heights = nodes.map((node) => node.offsetHeight);
      const totalHeight = heights.reduce((total, height) => total + height, 0) +
        gap * Math.max(0, nodes.length - 1);
      let top = Math.max(edge, Math.min(rect.top, window.innerHeight - totalHeight - edge));
      for (const [index, node] of nodes.entries()) {
        const width = node.offsetWidth;
        const height = heights[index] ?? 0;
        const placement = availablePlacement(rect, width, height, top);
        node.style.left = `${placement.left}px`;
        node.style.top = `${placement.top}px`;
        occupied.push(placement);
        top = placement.bottom + gap;
      }
    }
  };

  const scheduleAgentPlacement = () => {
    if (placementFrame !== undefined) return;
    placementFrame = requestAnimationFrame(placeAgentMessages);
  };

  const showMessageHighlight = (target) => {
    const rect = target.getBoundingClientRect();
    messageHighlight.style.display = "block";
    messageHighlight.style.left = `${rect.left}px`;
    messageHighlight.style.top = `${rect.top}px`;
    messageHighlight.style.width = `${rect.width}px`;
    messageHighlight.style.height = `${rect.height}px`;
    if (messageHighlightTimer) clearTimeout(messageHighlightTimer);
    messageHighlightTimer = setTimeout(() => {
      messageHighlight.style.display = "none";
    }, 1800);
  };

  const revealTarget = (selector) => {
    let target;
    try {
      target = targetForSelector(selector);
    } catch {
      target = null;
    }
    if (!target || target.getClientRects().length === 0) {
      showToast("Element is unavailable");
      return;
    }
    target.scrollIntoView({ block: "center", inline: "nearest" });
    requestAnimationFrame(() => showMessageHighlight(target));
  };

  const followElementLink = (link, event) => {
    const href = link.getAttribute("href") ?? "";
    if (!href.toLowerCase().startsWith("komtar:")) return false;
    event.preventDefault();
    let selector;
    try {
      selector = decodeURIComponent(href.slice("komtar:".length));
    } catch {
      showToast("Element link is invalid");
      return true;
    }
    revealTarget(selector);
    return true;
  };

  const addAgentMessage = (data) => {
    if (
      !data || typeof data !== "object" || typeof data.id !== "string" ||
      typeof data.html !== "string" ||
      !(data.anchor === undefined || typeof data.anchor === "string") ||
      dismissed.has(data.id) || agentMessages.has(data.id)
    ) return;

    const node = document.createElement("article");
    node.className = "agent-message";
    node.dataset.messageId = data.id;
    const close = document.createElement("button");
    close.className = "agent-dismiss";
    close.type = "button";
    close.setAttribute("aria-label", "Dismiss agent message");
    close.textContent = "×";
    const body = document.createElement("div");
    body.className = "agent-body";
    body.innerHTML = data.html;
    const unavailable = document.createElement("div");
    unavailable.className = "agent-unavailable";
    unavailable.textContent = "Target unavailable";
    unavailable.hidden = true;
    node.append(close, body, unavailable);
    const anchor = typeof data.anchor === "string" ? data.anchor : null;
    if (anchor !== null) {
      node.classList.add("has-anchor");
      node.tabIndex = 0;
      node.setAttribute("aria-label", "Agent message; activate to show its anchor");
      node.addEventListener("click", (event) => {
        const interactive = event.target instanceof Element
          ? event.target.closest("a, button")
          : null;
        const selection = window.getSelection();
        if (interactive || (selection && !selection.isCollapsed)) return;
        revealTarget(anchor);
      });
      node.addEventListener("keydown", (event) => {
        if (event.target !== node || !["Enter", " "].includes(event.key)) return;
        event.preventDefault();
        revealTarget(anchor);
      });
    }

    for (const link of body.querySelectorAll("a")) {
      const href = link.getAttribute("href") ?? "";
      if (!href.toLowerCase().startsWith("komtar:")) {
        link.target = "_blank";
        link.rel = "noopener noreferrer";
      }
    }
    body.addEventListener("click", (event) => {
      const link = event.target instanceof Element ? event.target.closest("a") : null;
      if (link) followElementLink(link, event);
    });
    close.addEventListener("click", () => {
      dismissed.add(data.id);
      persistDismissed();
      agentMessages.delete(data.id);
      node.remove();
      scheduleAgentPlacement();
    });
    agentMessages.set(data.id, {
      node,
      unavailable,
      anchor,
    });
    scheduleAgentPlacement();
  };

  window.addEventListener("resize", scheduleAgentPlacement);
  window.addEventListener("scroll", scheduleAgentPlacement, true);
  const placementObserver = new MutationObserver(scheduleAgentPlacement);
  placementObserver.observe(document.documentElement, {
    childList: true,
    subtree: true,
    attributes: true,
    characterData: true,
  });

  const messageEvents = new EventSource(MESSAGES_URL);
  messageEvents.addEventListener("message", (event) => {
    try {
      addAgentMessage(JSON.parse(event.data));
    } catch {
      // Ignore malformed events and leave the stream connected.
    }
  });

  const updateHighlight = () => {
    if (
      !captured || !dialog.open || !(captured.element instanceof Element) ||
      !captured.element.isConnected
    ) {
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
    if (pendingReload) window.location.reload();
  };

  const placeDialog = (x, y) => {
    const gap = 12;
    const width = dialog.offsetWidth;
    const height = dialog.offsetHeight;
    dialog.style.left = `${Math.max(gap, Math.min(x + gap, window.innerWidth - width - gap))}px`;
    dialog.style.top = `${Math.max(gap, Math.min(y + gap, window.innerHeight - height - gap))}px`;
  };

  const showDialog = (nextCapture, x, y, label) => {
    if (dialog.open) {
      textarea.focus();
      return;
    }
    captured = nextCapture;
    dialogLabel.textContent = label;
    const selectedText = captured.context.target.selectedText;
    selectionPreview.hidden = selectedText === null;
    selectionText.textContent = selectedText ?? "";
    status.textContent = "";
    reloadNotice.hidden = !pendingReload;
    textarea.value = "";
    dialog.showModal();
    placeDialog(x, y);
    updateHighlight();
    window.addEventListener("resize", updateHighlight);
    window.addEventListener("scroll", updateHighlight, true);
    textarea.focus();
  };

  const openDialog = (
    target,
    event,
    selector = cssSelector(target),
    label = "Suggest Edit:",
  ) => {
    showDialog(captureTarget(target, event, selector), event.clientX, event.clientY, label);
  };

  const openPageDialog = () => {
    showDialog(
      capturePageTarget(),
      window.innerWidth / 2,
      window.innerHeight / 3,
      "Comment:",
    );
  };

  document.addEventListener("contextmenu", (event) => {
    if (event.shiftKey || event.composedPath().includes(host)) return;
    if (!(event.target instanceof Element)) return;
    event.preventDefault();
    openDialog(event.target, event);
  }, true);

  shadow.addEventListener("contextmenu", (event) => {
    if (event.shiftKey) return;
    const target = event.composedPath().find(
      (node) => node instanceof Element && node.classList.contains("agent-message"),
    );
    if (!(target instanceof HTMLElement) || !target.dataset.messageId) return;
    event.preventDefault();
    event.stopPropagation();
    openDialog(
      target,
      event,
      `${AGENT_TARGET_PREFIX}${target.dataset.messageId}`,
      "Comment on agent response:",
    );
  }, true);

  document.addEventListener("keydown", (event) => {
    if (
      event.defaultPrevented || event.isComposing || event.key !== "/" ||
      event.metaKey || event.ctrlKey || event.altKey
    ) return;
    const editing = event.composedPath().some(
      (node) => node instanceof HTMLElement &&
        (node.isContentEditable || node.matches("input, textarea, select")),
    );
    if (editing) return;
    event.preventDefault();
    openPageDialog();
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
    if (event.button !== 0) return;
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

  if (LIVE_REVISION !== null) {
    const reloadEvents = new EventSource(RELOAD_URL);
    reloadEvents.addEventListener("message", (event) => {
      if (!event.data) return;
      if (event.data === reloadRevision) return;
      reloadRevision = event.data;
      if (dialog.open) {
        pendingReload = true;
        reloadNotice.hidden = false;
      } else {
        window.location.reload();
      }
    });
  }
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", install, { once: true });
} else {
  install();
}
