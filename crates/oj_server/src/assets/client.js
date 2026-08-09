// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// oj dev client: import.meta.hot contexts, HMR over WebSocket, error overlay.
//
// Served at /@oj/client.js. Every compiled module gets
// `import.meta.hot = createHotContext("/src/X.tsx")` appended by the server,
// and the Fast Refresh glue registers an accept callback through it.

const hotModules = new Map(); // id -> { acceptCallbacks, disposeCallbacks, pruneCallbacks, data, listeners }

// Custom event bus shared across all modules' hot.on() plus built-in
// `vite:*` lifecycle events, so tooling written against Vite's API works.
const customListeners = new Map(); // event -> Set<cb>
function emit(event, data) {
  const set = customListeners.get(event);
  if (set) for (const cb of [...set]) cb(data);
}

export function createHotContext(ownerId) {
  const id = ownerId.split("?")[0];
  // Reuse the existing entry and clear its callbacks IN PLACE (stable object
  // reference), rather than replacing it with a fresh object. A replaced
  // object detaches any in-flight snapshot and, combined with deferred
  // registration, drops a fast second edit. Mirrors Vite's HMRContext.
  let mod = hotModules.get(id);
  if (mod) {
    mod.acceptCallbacks = [];
    mod.disposeCallbacks = [];
    mod.pruneCallbacks = [];
    // Drop this module's previous hot.on listeners so a re-exec doesn't
    // accumulate duplicates.
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
    // accept() | accept(cb) | accept(dep, cb) | accept([deps], cb). We
    // re-execute the whole boundary regardless, so all forms register a
    // callback invoked with the new module namespace.
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
    decline() {}, // back-compat no-op (matches Vite)
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

const styleTags = new Map(); // id -> <style>

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

// Serialize updates: each applyUpdate fully completes (re-import + accept
// callbacks) before the next starts, so a second update can never read a
// module's hot context while the first update is mid-re-import.
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
  // Snapshot before re-import: the new instance re-registers its own context.
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

// --- overlay -------------------------------------------------------------

let overlayEl = null;

function showOverlay(text) {
  clearOverlay();
  overlayEl = document.createElement("div");
  overlayEl.style.cssText =
    "position:fixed;inset:0;z-index:99999;background:rgba(12,12,14,0.92);" +
    "color:#ff8484;font:13px/1.5 ui-monospace,Menlo,monospace;padding:2rem;overflow:auto";
  const pre = document.createElement("pre");
  pre.style.cssText = "white-space:pre-wrap;margin:0";
  pre.textContent = "[oj] " + text + "\n\n(click to dismiss — fix the file and save to retry)";
  overlayEl.appendChild(pre);
  overlayEl.addEventListener("click", clearOverlay);
  document.body.appendChild(overlayEl);
}

function clearOverlay() {
  if (overlayEl) {
    overlayEl.remove();
    overlayEl = null;
  }
}

// --- css hot swap ----------------------------------------------------------

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

// --- websocket -----------------------------------------------------------

let socket = null;

(function connect() {
  const ws = new WebSocket("ws://" + location.host + "/__ws");
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
      // Server-sent custom event -> import.meta.hot.on(event) listeners.
      emit(msg.event, msg.data);
    }
  });
  ws.addEventListener("open", () => console.log("[oj] dev server connected"));
  ws.addEventListener("close", () => {
    console.log("[oj] dev server disconnected, retrying in 1s…");
    setTimeout(connect, 1000);
  });
})();
