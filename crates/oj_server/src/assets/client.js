// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const hotModules = new Map();

const customListeners = new Map();
function emit(event, data) {
  const set = customListeners.get(event);
  if (set) for (const cb of [...set]) cb(data);
}

export function createHotContext(ownerId) {
  const id = ownerId.split("?")[0];
  let mod = hotModules.get(id);
  if (mod) {
    mod.acceptCallbacks = [];
    mod.disposeCallbacks = [];
    mod.pruneCallbacks = [];
    if (mod.listeners) for (const [ev, cb] of mod.listeners) customListeners.get(ev)?.delete(cb);
    mod.listeners = [];
  } else {
    mod = {
      acceptCallbacks: [],
      disposeCallbacks: [],
      pruneCallbacks: [],
      data: {},
      listeners: [],
    };
    hotModules.set(id, mod);
  }
  const data = mod.data;
  return {
    data,
    accept(first, second) {
      const cb = typeof first === "function" ? first : second || (() => {});
      mod.acceptCallbacks.push(cb);
    },
    acceptExports(_names, cb) {
      mod.acceptCallbacks.push(cb || (() => {}));
    },
    dispose(cb) {
      mod.disposeCallbacks.push(cb);
    },
    prune(cb) {
      mod.pruneCallbacks.push(cb);
    },
    decline() {},
    invalidate(message) {
      console.warn(`[oj] ${id} invalidated${message ? ": " + message : ""}`);
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "invalidate", path: id }));
      } else {
        location.reload();
      }
    },
    on(event, cb) {
      if (!customListeners.has(event)) customListeners.set(event, new Set());
      customListeners.get(event).add(cb);
      mod.listeners.push([event, cb]);
    },
    off(event, cb) {
      customListeners.get(event)?.delete(cb);
    },
    send(event, payload) {
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "custom", event, data: payload }));
      }
    },
  };
}

const styleTags = new Map();

export function updateStyle(id, css) {
  let tag = styleTags.get(id);
  if (!tag) {
    tag = document.createElement("style");
    tag.setAttribute("data-oj-id", id);
    document.head.appendChild(tag);
    styleTags.set(id, tag);
  }
  tag.textContent = css;
}

let updateChain = Promise.resolve();
function queueUpdate(update) {
  updateChain = updateChain.then(() => applyUpdate(update)).catch(() => {});
  return updateChain;
}

async function applyUpdate(update) {
  const cleanPath = update.path.split("?")[0];
  const mod = hotModules.get(cleanPath);
  if (!mod || mod.acceptCallbacks.length === 0) {
    console.log(`[oj] ${cleanPath} has no accept handler, reloading`);
    location.reload();
    return;
  }
  const accepts = mod.acceptCallbacks.slice();
  const disposes = mod.disposeCallbacks.slice();
  emit("vite:beforeUpdate", { type: "js-update", path: cleanPath });
  try {
    for (const dispose of disposes) await dispose(mod.data);
    const sep = update.path.includes("?") ? "&" : "?";
    const next = await import(update.path + sep + "t=" + update.timestamp);
    for (const accept of accepts) await accept(next);
    clearOverlay();
    emit("vite:afterUpdate", { type: "js-update", path: cleanPath });
    console.log(`[oj] hot updated ${update.path}`);
  } catch (err) {
    emit("vite:error", { err: { message: String(err) } });
    showOverlay(`hot update failed for ${update.path}\n\n${err && err.stack ? err.stack : err}`);
  }
}

let overlayEl = null;
let overlayKeyHandler = null;

function parseError(text) {
  const s = String(text);
  const title = (s.split("\n").find((l) => l.trim()) || "Build error").trim();
  const loc = s.match(/([^\s():]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?/);
  return { title, file: loc && loc[1], line: loc && loc[2], col: loc && loc[3], frame: s };
}

function showOverlay(text) {
  clearOverlay();
  const e = parseError(text);
  overlayEl = document.createElement("div");
  overlayEl.setAttribute("role", "dialog");
  overlayEl.setAttribute("aria-label", "Build error");
  overlayEl.style.cssText =
    "position:fixed;inset:0;z-index:99999;background:rgba(10,10,14,.86);" +
    "display:flex;align-items:flex-start;justify-content:center;padding:6vh 4vw;overflow:auto;" +
    "font:13px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace";
  const card = document.createElement("div");
  card.style.cssText =
    "max-width:920px;width:100%;background:#16161c;border:1px solid #33333f;border-radius:12px;" +
    "box-shadow:0 20px 60px rgba(0,0,0,.5);overflow:hidden;color:#e6e6ea";
  const bar = document.createElement("div");
  bar.style.cssText =
    "display:flex;align-items:center;gap:10px;padding:12px 18px;background:#2a1417;border-bottom:1px solid #4a2028";
  const brand = document.createElement("span");
  brand.style.cssText = "font-weight:700;letter-spacing:.08em;color:#ff6b6b";
  brand.textContent = "oj";
  const label = document.createElement("span");
  label.style.cssText = "color:#ff9a9a;font-weight:600";
  label.textContent = "Build error";
  bar.append(brand, label);
  const body = document.createElement("div");
  body.style.cssText = "padding:18px";
  const title = document.createElement("div");
  title.style.cssText = "color:#ff8a8a;font-weight:600;font-size:14px;white-space:pre-wrap;margin-bottom:10px";
  title.textContent = e.title;
  body.appendChild(title);
  if (e.file) {
    const loc = document.createElement("div");
    loc.style.cssText = "color:#8ab4ff;margin-bottom:12px;font-size:12px";
    loc.textContent = e.file + (e.line ? ":" + e.line + (e.col ? ":" + e.col : "") : "");
    body.appendChild(loc);
  }
  const frame = document.createElement("pre");
  frame.style.cssText =
    "margin:0;white-space:pre-wrap;background:#0d0d12;border:1px solid #26262f;border-radius:8px;" +
    "padding:12px 14px;color:#c9c9d4;max-height:52vh;overflow:auto";
  frame.textContent = e.frame;
  body.appendChild(frame);
  const hint = document.createElement("div");
  hint.style.cssText = "margin-top:12px;color:#6f6f80;font-size:12px";
  hint.textContent = "Fix the file and save to retry · click outside or press Esc to dismiss";
  body.appendChild(hint);
  card.append(bar, body);
  overlayEl.appendChild(card);
  // Dismiss on backdrop click only, so text inside the card stays selectable.
  overlayEl.addEventListener("click", (ev) => { if (ev.target === overlayEl) clearOverlay(); });
  overlayKeyHandler = (ev) => { if (ev.key === "Escape") clearOverlay(); };
  window.addEventListener("keydown", overlayKeyHandler);
  document.body.appendChild(overlayEl);
}

function clearOverlay() {
  if (overlayKeyHandler) {
    window.removeEventListener("keydown", overlayKeyHandler);
    overlayKeyHandler = null;
  }
  if (overlayEl) {
    overlayEl.remove();
    overlayEl = null;
  }
}

function swapCss(update) {
  const links = [...document.querySelectorAll("link[rel=stylesheet]")];
  const link = links.find((l) => new URL(l.href).pathname === update.path);
  if (!link) {
    console.log(`[oj] no <link> for ${update.path}, reloading`);
    location.reload();
    return;
  }
  const next = link.cloneNode();
  next.href = update.path + "?t=" + update.timestamp;
  next.addEventListener("load", () => link.remove());
  link.after(next);
  console.log(`[oj] css updated ${update.path}`);
}

let socket = null;

(function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(proto + "://" + location.host + "/__ws");
  socket = ws;
  ws.addEventListener("message", (event) => {
    let msg;
    try {
      msg = JSON.parse(event.data);
    } catch {
      return;
    }
    if (msg.type === "update") {
      (msg.updates || []).forEach(queueUpdate);
    } else if (msg.type === "css-update") {
      swapCss(msg);
    } else if (msg.type === "full-reload") {
      emit("vite:beforeFullReload", { path: msg.reason });
      console.log("[oj] full reload:", msg.reason || "");
      location.reload();
    } else if (msg.type === "error") {
      emit("vite:error", { err: { message: msg.message } });
      showOverlay(msg.message || "unknown error");
    } else if (msg.type === "custom") {
      emit(msg.event, msg.data);
    }
  });
  ws.addEventListener("open", () => console.log("[oj] dev server connected"));
  ws.addEventListener("close", () => {
    console.log("[oj] dev server disconnected, retrying in 1s…");
    setTimeout(connect, 1000);
  });
})();
