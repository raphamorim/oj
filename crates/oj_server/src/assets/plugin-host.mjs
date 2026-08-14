// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Persistent plugin host: loads Vite/Rollup-style plugins from the app's
// plugins module and runs their hooks (transform / resolveId / load) against
// oj's pipeline. JSON-lines over stdio with correlation ids, so many calls can
// be in flight at once.
import http from "node:http";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";

const pluginsPath = process.argv[2];
const initial = JSON.parse(process.argv[3] ?? "{}");

// Suppress Vite's native config-loader "import without a file extension"
// advisories: oj loads the config as tooling, so they are noise here.
process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";
const env = initial.env ?? { command: "serve", mode: "development" };

// The `oj` brand token as a badge: navy foreground on white-background cells,
// so it stays legible on any terminal theme (plain `oj` when piped / NO_COLOR).
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";

// Vite's `configResolved` and later hooks receive a fully-resolved
// `ResolvedConfig`, so real plugins read fields Vite always fills in — e.g.
// @vitejs/plugin-react reads `config.experimental.bundledDev`. oj only has the
// app's user config plus plugin `config()` merges, so fill the standard shape
// UNDER the actual config (user/plugin values win); a missing key must never
// crash a hook (a plugin crash stalls buildStart until the host times out).
function withResolvedDefaults(config) {
  const c = config ?? {};
  return deepMerge(
    {
      command: env.command,
      mode: env.mode,
      root: c.root ?? initial.config?.root ?? process.cwd(),
      base: "/",
      isProduction: env.mode === "production",
      experimental: {},
      build: {},
      server: {},
      define: {},
      resolve: {},
      optimizeDeps: {},
      ssr: {},
      env: {},
    },
    c,
  );
}

// Vite Environment API: the environment this host serves (oj exposes "client"
// for the dev server and the app build). `environment.config` is the base
// config deep-merged with the per-environment overrides from
// `config.environments[name]`, so plugins read the resolved per-env config.
const envName = initial.environment?.name ?? "client";
const environment = {
  name: envName,
  mode: initial.environment?.mode ?? env.mode,
  config: withResolvedDefaults(
    deepMerge(initial.config ?? {}, (initial.config?.environments ?? {})[envName] ?? {}),
  ),
};

// Resolve an app's vite.config to its config object. Preferred path: Vite's
// own `loadConfigFromFile` (a direct dep of any Vite app; handles TS, local
// imports, and `defineConfig`). Fallback: bundle the config graph with the
// app's esbuild (local imports inlined, node_modules external).
async function loadViteConfig(configPath) {
  const appRoot = initial.config?.root ?? process.cwd();
  const req = createRequire(appRoot + "/package.json");
  try {
    const vite = await import(req.resolve("vite"));
    if (typeof vite.loadConfigFromFile === "function") {
      const loaded = await vite.loadConfigFromFile(
        { command: env.command, mode: env.mode },
        configPath,
        appRoot,
      );
      if (loaded && loaded.config) return loaded.config;
    }
  } catch (e) {
    process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} vite.loadConfigFromFile unavailable (${e}); bundling config directly\n`);
  }
  // Fallback for apps without a usable vite install.
  let mod;
  if (/\.(ts|tsx|mts|cts)$/.test(configPath)) {
    const esbuild = await import(req.resolve("esbuild"));
    const result = await esbuild.build({
      entryPoints: [configPath],
      bundle: true,
      platform: "node",
      format: "esm",
      packages: "external",
      write: false,
      sourcemap: false,
      logLevel: "silent",
      absWorkingDir: appRoot,
    });
    const out = `${appRoot}/.oj-cache/oj-vite-config.mjs`;
    writeFileSync(out, result.outputFiles[0].text);
    mod = await import(pathToFileURL(out).href);
  } else {
    mod = await import(pathToFileURL(configPath).href);
  }
  return typeof mod.default === "function" ? await mod.default(env) : mod.default;
}

let plugins = [];
try {
  let list;
  if (initial.pluginsFormat === "vite") {
    // vite.config.*: run its `plugins` array. Config plugins can be nested
    // arrays and promises (async plugins); flatten and await.
    const cfg = await loadViteConfig(pluginsPath);
    list = await Promise.all((cfg?.plugins ?? []).flat(Infinity));
  } else {
    const mod = await import(pathToFileURL(pluginsPath).href);
    list = mod.default ?? mod.plugins ?? [];
  }
  plugins = (Array.isArray(list) ? list : [list]).filter(Boolean);
  // `apply`: keep plugins active for this command ("serve"/"build" or a fn).
  plugins = plugins.filter((p) => {
    if (p.apply == null) return true;
    if (typeof p.apply === "function") return !!p.apply(initial.config ?? {}, env);
    return p.apply === env.command;
  });
  // `applyToEnvironment`: gate a plugin to specific environments (Vite Env API).
  // A plugin absent from this environment is dropped from the pipeline.
  plugins = plugins.filter((p) => {
    if (typeof p.applyToEnvironment !== "function") return true;
    return !!p.applyToEnvironment(environment);
  });
  // `enforce`: pre plugins run first, post last, others keep array order
  // (Array.prototype.sort is stable).
  const rank = (p) => (p.enforce === "pre" ? -1 : p.enforce === "post" ? 1 : 0);
  plugins.sort((a, b) => rank(a) - rank(b));
  process.stderr.write(
    `${OJ} plugin host: ${plugins.length} plugin(s) active for ${env.command}: ${plugins.map((p) => `${p.name}[${p.enforce ?? "-"}]`).join(",")}\n`,
  );
} catch (e) {
  process.stderr.write(`${OJ} plugin host: failed to load ${pluginsPath}: ${(e && e.stack) || e}\n`);
}

// Reverse RPC (Node to Rust): a plugin's this.resolve asks oj's own resolver
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
// reads it synchronously, matching Rollup, where getModuleInfo returns info
// for modules already loaded into the graph and null otherwise.
const moduleInfoCache = new Map();

// Files a plugin registered via this.addWatchFile (dev watcher pulls these).
const watchedFiles = new Set();
// Per-transform attribution of addWatchFile calls: each transform() runs its
// hook chain inside a fresh bucket so oj can cache exactly the files THIS
// module watched (concurrent transforms interleave at awaits, so a plain
// module-scoped var would cross-contaminate -- AsyncLocalStorage scopes it to
// the call chain). Enables re-applying the watch on a warm-cache serve, where
// transform does not re-run.
const transformWatchStore = new AsyncLocalStorage();
// Module ids the host has observed (every transform id + this.load-ed id).
// getModuleIds returns this: a subset of Rollup's whole-graph view, only the
// modules the plugin host has actually seen.
const seenIds = new Set();

// Rollup plugin context. Covers warn/error, this.resolve (async, via oj's
// resolver), and this.emitFile/getFileName (asset form) used by real plugins.
const ctx = {
  // Vite Environment API: hooks read this.environment.name / .config.
  environment,
  warn: (m) => process.stderr.write(`oj plugin warn: ${m}\n`),
  error: (m) => {
    throw typeof m === "string" ? new Error(m) : m;
  },
  // this.resolve(source, importer) returns { id } | null (Rollup shape).
  async resolve(source, importer) {
    const id = await ctxRpc("resolve", [source, importer ?? ""]);
    return id == null ? null : { id };
  },
  // this.emitFile({ type:"asset", name?, fileName?, source }) returns a reference id.
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
  // this.load({ id }) returns ModuleInfo { id, code, importedIds } (or null). Reads
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
  // this.getModuleInfo(id) returns cached ModuleInfo | null. Synchronous (Rollup
  // shape): only modules previously this.load-ed are present.
  getModuleInfo(id) {
    return moduleInfoCache.get(typeof id === "string" ? id : id.id) ?? null;
  },
  // this.addWatchFile(id): watch an extra file; a change forces a dev reload.
  // Also record it in the current transform's bucket (if any) for per-module
  // caching, so the watch survives a warm-cache restart.
  addWatchFile(id) {
    if (!id) return;
    watchedFiles.add(String(id));
    const bucket = transformWatchStore.getStore();
    if (bucket) bucket.add(String(id));
  },
  // this.getModuleIds() returns an iterator over observed module ids (Rollup shape).
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
  const resolved = withResolvedDefaults(config);
  for (const p of plugins) {
    if (typeof p.configResolved === "function") await p.configResolved.call(ctx, resolved);
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
  process.stderr.write(`${OJ} plugin host: configureServer middleware on :${middlewarePort}\n`);
}
// configureServer is a dev-server hook (Vite runs it only in dev). Skipping it
// in `build` also avoids leaving an http.Server that keeps this process alive.
if (env.command !== "build") await setupConfigureServer();

// transform chains through all plugins (Rollup semantics). Returns a JSON
// envelope { code, watchFiles }: the final code plus the files this module's
// hooks registered via this.addWatchFile, so oj can cache + re-apply the watch
// on a warm-cache serve (where transform does not re-run).
async function transform(code, id) {
  const bucket = new Set();
  return transformWatchStore.run(bucket, async () => {
    if (id) seenIds.add(id);
    let current = code;
    for (const p of plugins) {
      if (typeof p.transform !== "function") continue;
      const r = await p.transform.call(ctx, current, id);
      if (r == null) continue;
      current = typeof r === "string" ? r : (r.code ?? current);
    }
    // moduleParsed: the module has been transformed. Fire with a minimal
    // ModuleInfo (id + final code); getModuleInfo caches it for later lookup.
    const info = { id, code: current, importedIds: [] };
    moduleInfoCache.set(id, info);
    seenIds.add(id);
    for (const p of plugins) {
      if (typeof p.moduleParsed === "function") await p.moduleParsed.call(ctx, info);
    }
    return JSON.stringify({ code: current, watchFiles: [...bucket] });
  });
}

// Replay `moduleParsed` for a module served from oj's warm cache (transform did
// not re-run). Fires with a minimal ModuleInfo so graph-tracking plugins see
// the module. Only invoked by oj when a plugin actually defines the hook.
async function replayModuleParsed(id) {
  if (id) seenIds.add(id);
  const info = moduleInfoCache.get(id) ?? { id, code: "", importedIds: [] };
  moduleInfoCache.set(id, info);
  for (const p of plugins) {
    if (typeof p.moduleParsed === "function") await p.moduleParsed.call(ctx, info);
  }
  return null;
}

const anyModuleParsed = () => plugins.some((p) => typeof p.moduleParsed === "function");

// watchChange: a watched file changed (dev). Fire each plugin's hook.
async function watchChange(id, event) {
  for (const p of plugins) {
    if (typeof p.watchChange === "function") await p.watchChange.call(ctx, id, { event });
  }
  return null;
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
// contract: return "full-reload" to force a reload, [] to suppress HMR, or
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

// generateBundle: hand the output bundle (fileName to chunk|asset) to each
// plugin's generateBundle(outputOptions, bundle, isWrite). Plugins may read it,
// mutate chunk.code / asset.source, or this.emitFile new assets. Returns the
// possibly-mutated bundle as JSON.
async function generateBundle(bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = environment.config?.build ?? {};
  for (const p of plugins) {
    const fn = typeof p.generateBundle === "function" ? p.generateBundle : p.generateBundle?.handler;
    if (typeof fn !== "function") continue;
    await fn.call(ctx, outputOptions, bundle, isWrite);
  }
  return JSON.stringify(bundle);
}

// renderChunk: chain the plugins' renderChunk(code, chunk) over one chunk's
// code (Rollup semantics). Each may return a string, { code }, or null.
async function renderChunk(code, chunkJson) {
  const chunk = JSON.parse(chunkJson || "{}");
  let current = code;
  for (const p of plugins) {
    const fn = typeof p.renderChunk === "function" ? p.renderChunk : p.renderChunk?.handler;
    if (typeof fn !== "function") continue;
    const r = await fn.call(ctx, current, chunk);
    if (r == null) continue;
    current = typeof r === "string" ? r : (r.code ?? current);
  }
  return current;
}

// writeBundle: post-write side-effect hook. Files are already on disk, so this
// is read-only (mutations are not written back).
async function writeBundle(bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = environment.config?.build ?? {};
  for (const p of plugins) {
    const fn = typeof p.writeBundle === "function" ? p.writeBundle : p.writeBundle?.handler;
    if (typeof fn !== "function") continue;
    await fn.call(ctx, outputOptions, bundle, isWrite);
  }
  return null;
}

async function run(hook, args) {
  if (hook === "transform") return transform(args[0], args[1]);
  if (hook === "resolveId") return resolveId(args[0], args[1]);
  if (hook === "load") return load(args[0]);
  if (hook === "handleHotUpdate") return handleHotUpdate(args[0], args[1]);
  if (hook === "transformIndexHtml") return transformIndexHtml(args[0]);
  if (hook === "buildStart" || hook === "buildEnd" || hook === "renderStart" || hook === "closeBundle") {
    return runLifecycle(hook);
  }
  if (hook === "watchChange") return watchChange(args[0], args[1]);
  // Assets emitted via this.emitFile, as a JSON string for the Rust build.
  if (hook === "getEmittedFiles") {
    return JSON.stringify(emitted.map(({ fileName, source }) => ({ fileName, source })));
  }
  // Files registered via this.addWatchFile, for the dev watcher.
  if (hook === "getWatchFiles") return JSON.stringify([...watchedFiles]);
  // Whether any plugin defines moduleParsed: oj only pays the per-module replay
  // cost on a warm cache when a plugin actually observes the graph.
  if (hook === "hasModuleParsed") return String(anyModuleParsed());
  if (hook === "replayModuleParsed") return replayModuleParsed(args[0]);
  if (hook === "hasGenerateBundle") {
    return String(plugins.some((p) => typeof (p.generateBundle?.handler ?? p.generateBundle) === "function"));
  }
  if (hook === "generateBundle") return generateBundle(args[0], args[1] === "true");
  if (hook === "hasRenderChunk") {
    return String(plugins.some((p) => typeof (p.renderChunk?.handler ?? p.renderChunk) === "function"));
  }
  if (hook === "renderChunk") return renderChunk(args[0], args[1]);
  if (hook === "hasWriteBundle") {
    return String(plugins.some((p) => typeof (p.writeBundle?.handler ?? p.writeBundle) === "function"));
  }
  if (hook === "writeBundle") return writeBundle(args[0], args[1] === "true");
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
  // A reply to a this.resolve (or other ctx) reverse-RPC sent earlier.
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
