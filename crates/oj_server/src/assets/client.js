// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// oj dev client: import.meta.hot contexts, HMR over WebSocket, error overlay.
//
// Served at /@oj/client.js. Every compiled module gets
// `import.meta.hot = createHotContext("/src/X.tsx")` appended by the server,
// and the Fast Refresh glue registers an accept callback through it.

const hotModules = new Map(); // id -> { acceptCallbacks: [], disposeCallbacks: [], data: {} }

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
  } else {
    mod = { acceptCallbacks: [], disposeCallbacks: [], data: {} };
    hotModules.set(id, mod);
  }
  const data = mod.data;
  return {
    data,
    accept(callback) {
      mod.acceptCallbacks.push(callback || (() => {}));
    },
    dispose(callback) {
      mod.disposeCallbacks.push(callback);
    },
    invalidate(message) {
      console.warn(`[oj] ${id} invalidated${message ? ": " + message : ""}`);
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "invalidate", path: id }));
      } else {
        location.reload();
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
  try {
    for (const dispose of disposes) await dispose(mod.data);
    const sep = update.path.includes("?") ? "&" : "?";
    const next = await import(update.path + sep + "t=" + update.timestamp);
    for (const accept of accepts) await accept(next);
    clearOverlay();
    console.log(`[oj] hot updated ${update.path}`);
  } catch (err) {
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
      console.log("[oj] full reload:", msg.reason || "");
      location.reload();
    } else if (msg.type === "error") {
      showOverlay(msg.message || "unknown error");
    }
  });
  ws.addEventListener("open", () => console.log("[oj] dev server connected"));
  ws.addEventListener("close", () => {
    console.log("[oj] dev server disconnected, retrying in 1s…");
    setTimeout(connect, 1000);
  });
})();
