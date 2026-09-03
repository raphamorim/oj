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
      // accept() / accept(cb): self-accepting. accept(dep, cb) / accept([deps], cb):
      // this module is the boundary for updates of those dependencies and the
      // callback receives the new dependency module(s), as in Vite.
      if (first === undefined || typeof first === "function") {
        mod.acceptCallbacks.push({ deps: [id], fn: first || (() => {}) });
        return;
      }
      const deps = (Array.isArray(first) ? first : [first]).map((d) => String(d).split("?")[0]);
      mod.acceptCallbacks.push({ deps, fn: second || (() => {}), single: typeof first === "string" });
    },
    acceptExports(_names, cb) {
      mod.acceptCallbacks.push({ deps: [id], fn: cb || (() => {}) });
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
  const acceptedUrl = update.acceptedPath || update.path;
  const acceptedPath = acceptedUrl.split("?")[0];
  const mod = hotModules.get(cleanPath);
  if (!mod) {
    // The module is in the server's graph but this page never loaded it (a
    // lazy route, a component behind a condition). Nothing to swap; like Vite,
    // ignore it instead of reloading a page that does not run that code.
    console.debug(`[oj] ${cleanPath} is not loaded here, ignoring update`);
    return;
  }
  const isSelf = acceptedPath === cleanPath;
  const accepts = mod.acceptCallbacks.filter((cb) => cb.deps.includes(acceptedPath));
  if (accepts.length === 0) {
    console.log(`[oj] ${cleanPath} has no accept handler for ${acceptedPath}, reloading`);
    location.reload();
    return;
  }
  // Dispose callbacks belong to the module being replaced (the accepted one).
  const disposed = hotModules.get(acceptedPath);
  const disposes = disposed ? disposed.disposeCallbacks.slice() : [];
  emit("vite:beforeUpdate", { type: "update", updates: [update] });
  try {
    for (const dispose of disposes) await dispose(disposed.data);
    const sep = acceptedUrl.includes("?") ? "&" : "?";
    const next = await import(acceptedUrl + sep + "t=" + update.timestamp);
    for (const cb of accepts) {
      if (isSelf || cb.single) await cb.fn(next);
      else await cb.fn(cb.deps.map((d) => (d === acceptedPath ? next : undefined)));
    }
    clearOverlay();
    emit("vite:afterUpdate", { type: "update", updates: [update] });
    console.log(`[oj] hot updated ${update.path}`);
  } catch (err) {
    // Like Vite's warnFailedUpdate: log, but do not raise an overlay of our own.
    // A compile error behind the failed fetch already arrived as the server's
    // error frame (with the file and code frame); replacing that overlay with
    // "failed to fetch" would hide the real cause.
    emit("vite:error", { err: { message: String(err) } });
    if (!(err instanceof Error) || !err.message.includes("fetch")) console.error(err);
    console.error(
      `[oj] Failed to reload ${update.path}. This could be due to syntax errors or importing non-existent modules. (see errors above)`,
    );
  }
}

let overlayEl = null;
let overlayKeyHandler = null;
let isFirstUpdate = true;

// If this is the first update and an error overlay is already showing, the page
// opened with a server compile error and the module script never finished
// loading (one of its nested imports was a 500): the boundaries above the fixed
// file were never registered, so a hot update has nothing to swap into. A full
// reload is the only way to recover, as in Vite's clearOverlayOrReloadOnFirstUpdate.
function clearOverlayOrReloadOnFirstUpdate() {
  if (isFirstUpdate && overlayEl) {
    location.reload();
    return "reload";
  }
  clearOverlay();
  isFirstUpdate = false;
  return "continue";
}

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
  if (link) {
    const next = link.cloneNode();
    next.href = update.path + "?t=" + update.timestamp;
    next.addEventListener("load", () => link.remove());
    link.after(next);
    console.log(`[oj] css updated ${update.path}`);
    return;
  }
  // A stylesheet imported from JS (`import "./index.css"`) lives in a <style> tag
  // that its module wrapper wrote via updateStyle. Re-import the wrapper with a
  // fresh timestamp so it re-runs against the recompiled css and swaps the tag in
  // place, as Vite's client does for a css-update; reloading here would drop all
  // component state on every edit in a Tailwind app.
  if (styleTags.has(update.path)) {
    import(update.path + "?import&t=" + update.timestamp)
      .then(() => console.log(`[oj] css updated ${update.path}`))
      .catch((err) => {
        console.log(`[oj] css re-import failed for ${update.path}, reloading`, err);
        location.reload();
      });
    return;
  }
  console.log(`[oj] no <link> or <style> for ${update.path}, reloading`);
  location.reload();
}

let socket = null;
let hadConnection = false;

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
    if (msg.type === "connected") {
      // Vite-protocol greeting; nothing to do.
    } else if (msg.type === "update") {
      // Vite's UpdatePayload: css-update entries swap stylesheets, js-update
      // entries re-import boundaries.
      if (clearOverlayOrReloadOnFirstUpdate() === "reload") return;
      for (const u of msg.updates || []) {
        if (u.type === "css-update") swapCss(u);
        else queueUpdate(u);
      }
    } else if (msg.type === "css-update") {
      swapCss(msg);
    } else if (msg.type === "full-reload") {
      emit("vite:beforeFullReload", { path: msg.reason });
      console.log("[oj] full reload:", msg.reason || "");
      location.reload();
    } else if (msg.type === "error") {
      // Vite's ErrorPayload carries `err`; older oj frames carried `message`.
      const err = msg.err || { message: msg.message };
      emit("vite:error", { err });
      showOverlay(err.message || "unknown error");
    } else if (msg.type === "custom") {
      emit(msg.event, msg.data);
    }
  });
  ws.addEventListener("open", () => {
    if (hadConnection) {
      // The server went away and came back (config or .env change restarts
      // it): its module graph, defines and caches are new, so the page's
      // modules are stale. Vite reloads here too.
      console.log("[oj] dev server restarted, reloading");
      emit("vite:ws:connect", { webSocket: ws });
      location.reload();
      return;
    }
    hadConnection = true;
    emit("vite:ws:connect", { webSocket: ws });
    console.log("[oj] dev server connected");
  });
  ws.addEventListener("close", () => {
    emit("vite:ws:disconnect", { webSocket: ws });
    console.log("[oj] dev server disconnected, retrying in 1s…");
    setTimeout(connect, 1000);
  });
})();
