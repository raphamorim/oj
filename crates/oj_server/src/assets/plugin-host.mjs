// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Persistent plugin host: loads Vite/Rollup-style plugins from the app's
// plugins module and runs their hooks (transform / resolveId / load) against
// oj's pipeline. JSON-lines over stdio with correlation ids (many calls can be
// in flight; a cancelled caller just drops its response).
import http from "node:http";
import { pathToFileURL } from "node:url";
import readline from "node:readline";

const pluginsPath = process.argv[2];
const initial = JSON.parse(process.argv[3] ?? "{}");
const env = initial.env ?? { command: "serve", mode: "development" };

let plugins = [];
try {
  const mod = await import(pathToFileURL(pluginsPath).href);
  const list = mod.default ?? mod.plugins ?? [];
  plugins = (Array.isArray(list) ? list : [list]).filter(Boolean);
  // `apply`: keep plugins active for this command ("serve"/"build" or a fn).
  plugins = plugins.filter((p) => {
    if (p.apply == null) return true;
    if (typeof p.apply === "function") return !!p.apply(initial.config ?? {}, env);
    return p.apply === env.command;
  });
  // `enforce`: pre plugins run first, post last, others keep array order
  // (Array.prototype.sort is stable).
  const rank = (p) => (p.enforce === "pre" ? -1 : p.enforce === "post" ? 1 : 0);
  plugins.sort((a, b) => rank(a) - rank(b));
  process.stderr.write(
    `oj plugin host: ${plugins.length} plugin(s) active for ${env.command}: ${plugins.map((p) => `${p.name}[${p.enforce ?? "-"}]`).join(",")}\n`,
  );
} catch (e) {
  process.stderr.write(`oj plugin host: failed to load ${pluginsPath}: ${(e && e.stack) || e}\n`);
}

// Reverse RPC (Node -> Rust): a plugin's this.resolve asks oj's own resolver
// so plugin resolution matches oj (tsconfig aliases etc.). Correlated by id;
// the reply comes back as an {rpcReply} line on stdin (see the readline loop).
let rpcCounter = 1;
const rpcPending = new Map();
function ctxRpc(method, args) {
  const rpc = rpcCounter++;
  return new Promise((resolve, reject) => {
    rpcPending.set(rpc, { resolve, reject });
    process.stdout.write(JSON.stringify({ rpc, method, args }) + "\n");
  });
}

// Assets a plugin emits via this.emitFile; the Rust build collects them after
// buildEnd (getEmittedFiles) and writes them to the output dir.
let emitCounter = 0;
const emitted = [];

// ModuleInfo cache: this.load populates it (async, via Rust); getModuleInfo
// reads it synchronously — matching Rollup, where getModuleInfo returns info
// for modules already loaded into the graph and null otherwise.
const moduleInfoCache = new Map();

// Files a plugin registered via this.addWatchFile (dev watcher pulls these).
const watchedFiles = new Set();
// Module ids the host has observed (every transform id + this.load-ed id).
// getModuleIds returns this — a subset of Rollup's whole-graph view, honest
// about being "modules the plugin host has seen".
const seenIds = new Set();

// Rollup plugin context. Covers warn/error, this.resolve (async, via oj's
// resolver), and this.emitFile/getFileName (asset form) used by real plugins.
const ctx = {
  warn: (m) => process.stderr.write(`oj plugin warn: ${m}\n`),
  error: (m) => {
    throw typeof m === "string" ? new Error(m) : m;
  },
  // this.resolve(source, importer) -> { id } | null (Rollup shape).
  async resolve(source, importer) {
    const id = await ctxRpc("resolve", [source, importer ?? ""]);
    return id == null ? null : { id };
  },
  // this.emitFile({ type:"asset", name?, fileName?, source }) -> reference id.
  // Assets only; chunk emission isn't supported. fileName defaults to
  // assets/<name> so plugins can predict the output path via getFileName.
  emitFile(file) {
    if (file == null || (file.type && file.type !== "asset")) {
      throw new Error("oj: this.emitFile supports { type: 'asset' } only");
    }
    const fileName = file.fileName ?? `assets/${file.name ?? `asset-${emitCounter}`}`;
    const referenceId = `oj-ref-${emitCounter++}`;
    emitted.push({ referenceId, fileName, source: String(file.source ?? "") });
    return referenceId;
  },
  getFileName(referenceId) {
    const f = emitted.find((e) => e.referenceId === referenceId);
    if (!f) throw new Error(`oj: unknown emit reference ${referenceId}`);
    return f.fileName;
  },
  // this.load({ id }) -> ModuleInfo { id, code, importedIds } (or null). Reads
  // + compiles the module through Rust, then caches it for getModuleInfo.
  async load(options) {
    const id = typeof options === "string" ? options : options.id;
    const info = await ctxRpc("moduleInfo", [id]);
    if (info) {
      moduleInfoCache.set(info.id, info);
      seenIds.add(info.id);
    }
    return info;
  },
  // this.getModuleInfo(id) -> cached ModuleInfo | null. Synchronous (Rollup
  // shape): only modules previously this.load-ed are present.
  getModuleInfo(id) {
    return moduleInfoCache.get(typeof id === "string" ? id : id.id) ?? null;
  },
  // this.addWatchFile(id): watch an extra file; a change forces a dev reload.
  addWatchFile(id) {
    if (id) watchedFiles.add(String(id));
  },
  // this.getModuleIds() -> iterator over observed module ids (Rollup shape).
  getModuleIds() {
    return seenIds.values();
  },
};

// config() / configResolved() handshake, once at startup. Each plugin's
// config(config, env) may return a partial that is deep-merged into the
// resolved config; then every plugin's configResolved(finalConfig) runs so it
// can capture what it needs for later hooks.
function deepMerge(a, b) {
  if (Array.isArray(a) && Array.isArray(b)) return [...a, ...b];
  if (a && b && typeof a === "object" && typeof b === "object") {
    const out = { ...a };
    for (const k of Object.keys(b)) out[k] = k in a ? deepMerge(a[k], b[k]) : b[k];
    return out;
  }
  return b === undefined ? a : b;
}

async function runConfigHooks() {
  let config = initial.config ?? {};
  for (const p of plugins) {
    if (typeof p.config !== "function") continue;
    const partial = await p.config.call(ctx, config, env);
    if (partial) config = deepMerge(config, partial);
  }
  for (const p of plugins) {
    if (typeof p.configResolved === "function") await p.configResolved.call(ctx, config);
  }
}
await runConfigHooks();

// configureServer: plugins add dev-server middleware (Connect-style
// `(req, res, next)`). oj forwards requests it can't serve to this middleware
// HTTP server (see getMiddlewarePort); a middleware that calls next() past the
// end falls through (x-oj-fallthrough header) so oj resumes its own handling.
let middlewarePort = null;
async function setupConfigureServer() {
  const stack = []; // { path, fn }
  const middlewares = {
    use(a, b) {
      if (typeof a === "function") stack.push({ path: null, fn: a });
      else stack.push({ path: a, fn: b });
    },
  };
  // A minimal Vite ViteDevServer shim: enough for middleware-registering
  // plugins. Deep APIs (moduleGraph/ssrLoadModule/ws.send/watcher) are stubbed
  // so plugins that merely reference them in configureServer don't crash.
  const noop = () => {};
  const server = {
    config: initial.config ?? {},
    middlewares,
    httpServer: null,
    ws: { on: noop, off: noop, send: noop },
    watcher: { on: noop, add: noop, close: noop },
    moduleGraph: { getModuleById: () => null, invalidateModule: noop },
    transformRequest: async () => null,
    ssrLoadModule: async () => {
      throw new Error("oj: server.ssrLoadModule is not available in configureServer");
    },
  };
  const post = [];
  for (const p of plugins) {
    if (typeof p.configureServer !== "function") continue;
    const r = await p.configureServer(server);
    if (typeof r === "function") post.push(r);
  }
  for (const fn of post) await fn(); // post hooks run after "internal" middleware
  if (stack.length === 0) return;

  const srv = http.createServer((req, res) => {
    let i = 0;
    const next = (err) => {
      if (err) {
        res.statusCode = 500;
        res.end(String((err && err.stack) || err));
        return;
      }
      while (i < stack.length) {
        const { path, fn } = stack[i++];
        if (path && !req.url.startsWith(path)) continue;
        try {
          return fn(req, res, next);
        } catch (e) {
          return next(e);
        }
      }
      // Nothing handled it: tell oj to resume its own routing.
      res.setHeader("x-oj-fallthrough", "1");
      res.statusCode = 404;
      res.end();
    };
    next();
  });
  await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
  middlewarePort = srv.address().port;
  process.stderr.write(`oj plugin host: configureServer middleware on :${middlewarePort}\n`);
}
await setupConfigureServer();

// transform chains through all plugins (Rollup semantics); returns the final code.
async function transform(code, id) {
  if (id) seenIds.add(id);
  let current = code;
  for (const p of plugins) {
    if (typeof p.transform !== "function") continue;
    const r = await p.transform.call(ctx, current, id);
    if (r == null) continue;
    current = typeof r === "string" ? r : (r.code ?? current);
  }
  return current;
}

// resolveId / load are first-non-null-wins.
async function resolveId(source, importer) {
  for (const p of plugins) {
    if (typeof p.resolveId !== "function") continue;
    const r = await p.resolveId.call(ctx, source, importer || undefined);
    if (r == null) continue;
    return typeof r === "string" ? r : (r.id ?? null);
  }
  return null;
}

async function load(id) {
  for (const p of plugins) {
    if (typeof p.load !== "function") continue;
    const r = await p.load.call(ctx, id);
    if (r == null) continue;
    return typeof r === "string" ? r : (r.code ?? null);
  }
  return null;
}

// handleHotUpdate: plugins customize HMR for a changed file. oj's simplified
// contract — return "full-reload" to force a reload, [] to suppress HMR, or
// undefined to let default HMR proceed. First decisive result wins.
async function handleHotUpdate(file, timestamp) {
  let suppress = false;
  for (const p of plugins) {
    if (typeof p.handleHotUpdate !== "function") continue;
    const r = await p.handleHotUpdate.call(ctx, { file, timestamp: Number(timestamp) });
    if (r === "full-reload") return "full-reload";
    if (Array.isArray(r) && r.length === 0) suppress = true;
  }
  return suppress ? "skip" : null;
}

// Render a Vite tag descriptor { tag, attrs, children } to HTML.
function renderTag(t) {
  const attrs = Object.entries(t.attrs ?? {})
    .map(([k, v]) => (v === true ? ` ${k}` : v === false || v == null ? "" : ` ${k}="${String(v)}"`))
    .join("");
  const inner = t.children ?? "";
  const voidTag = ["meta", "link", "base"].includes(t.tag);
  return voidTag ? `<${t.tag}${attrs}>` : `<${t.tag}${attrs}>${inner}</${t.tag}>`;
}

function injectTags(html, tags) {
  const at = { "head-prepend": [], head: [], "body-prepend": [], body: [] };
  for (const t of tags) (at[t.injectTo ?? "head"] ?? at.head).push(renderTag(t));
  const put = (h, marker, html2, after) => {
    const i = h.indexOf(marker);
    if (i === -1) return h + html2;
    const at2 = after ? i + marker.length : i;
    return h.slice(0, at2) + html2 + h.slice(at2);
  };
  html = put(html, "<head>", at["head-prepend"].join(""), true);
  html = put(html, "</head>", at.head.join(""), false);
  html = put(html, "<body>", at["body-prepend"].join(""), true);
  html = put(html, "</body>", at.body.join(""), false);
  return html;
}

// transformIndexHtml: each plugin may return a new HTML string, an array of tag
// descriptors to inject, or { html, tags }. Chained across plugins.
async function transformIndexHtml(html) {
  let current = html;
  for (const p of plugins) {
    const hook = p.transformIndexHtml;
    const fn = typeof hook === "function" ? hook : hook?.handler ?? hook?.transform;
    if (typeof fn !== "function") continue;
    const r = await fn.call(ctx, current, { path: "/index.html", filename: "index.html" });
    if (r == null) continue;
    if (typeof r === "string") current = r;
    else if (Array.isArray(r)) current = injectTags(current, r);
    else {
      if (typeof r.html === "string") current = r.html;
      if (Array.isArray(r.tags)) current = injectTags(current, r.tags);
    }
  }
  return current;
}

// buildStart / buildEnd: side-effect lifecycle hooks run once per build in
// declaration order. No return value (like Rollup); errors propagate.
async function runLifecycle(hook) {
  for (const p of plugins) {
    if (typeof p[hook] === "function") await p[hook].call(ctx);
  }
  return null;
}

async function run(hook, args) {
  if (hook === "transform") return transform(args[0], args[1]);
  if (hook === "resolveId") return resolveId(args[0], args[1]);
  if (hook === "load") return load(args[0]);
  if (hook === "handleHotUpdate") return handleHotUpdate(args[0], args[1]);
  if (hook === "transformIndexHtml") return transformIndexHtml(args[0]);
  if (hook === "buildStart" || hook === "buildEnd") return runLifecycle(hook);
  // Assets emitted via this.emitFile, as a JSON string for the Rust build.
  if (hook === "getEmittedFiles") {
    return JSON.stringify(emitted.map(({ fileName, source }) => ({ fileName, source })));
  }
  // Files registered via this.addWatchFile, for the dev watcher.
  if (hook === "getWatchFiles") return JSON.stringify([...watchedFiles]);
  // The configureServer middleware port (null if no plugin added middleware).
  if (hook === "getMiddlewarePort") return middlewarePort == null ? null : String(middlewarePort);
  return null;
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  // A reply to a this.resolve (or other ctx) reverse-RPC we sent earlier.
  if (msg.rpcReply != null) {
    const p = rpcPending.get(msg.rpcReply);
    if (p) {
      rpcPending.delete(msg.rpcReply);
      if (msg.error != null) p.reject(new Error(msg.error));
      else p.resolve(msg.result ?? null);
    }
    return;
  }
  const { id, hook, args } = msg;
  try {
    const result = await run(hook, args ?? []);
    process.stdout.write(JSON.stringify({ id, result: result ?? null }) + "\n");
  } catch (e) {
    process.stdout.write(JSON.stringify({ id, error: String((e && e.stack) || e) }) + "\n");
  }
});
