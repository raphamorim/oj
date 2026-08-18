// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import http from "node:http";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname } from "node:path";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";

const pluginsPath = process.argv[2];
const initial = JSON.parse(process.argv[3] ?? "{}");

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";
const env = initial.env ?? { command: "serve", mode: "development" };

// Start mode hosts only configureServer middleware for a framework config oj
// owns (TanStack Start): the framework plugins drive the module graph and SSR
// themselves, so their config lifecycle hooks are tolerated per-plugin instead
// of aborting the load, keeping the editor middleware (dev-server bridge) alive.
const ojStartMode = initial.ojStartMode === true;

const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";

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

const envName = initial.environment?.name ?? "client";
const environment = {
  name: envName,
  mode: initial.environment?.mode ?? env.mode,
  config: withResolvedDefaults(
    deepMerge(initial.config ?? {}, (initial.config?.environments ?? {})[envName] ?? {}),
  ),
};

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
      define: {
        __dirname: JSON.stringify(dirname(configPath)),
        __filename: JSON.stringify(configPath),
      },
    });
    const out = `${appRoot}/.oj-cache/oj-vite-config.mjs`;
    writeFileSync(out, result.outputFiles[0].text);
    mod = await import(pathToFileURL(out).href);
  } else {
    mod = await import(pathToFileURL(configPath).href);
  }
  return typeof mod.default === "function" ? await mod.default(env) : mod.default;
}

const OJ_NATIVE_PLUGIN_NAMES = new Set([
  "vite:react-babel",
  "vite:react-refresh",
  "vite:react-swc",
  "vite:react-swc:resolve-runtime",
]);

let plugins = [];
try {
  let list;
  if (initial.pluginsFormat === "vite") {
    const cfg = await loadViteConfig(pluginsPath);
    list = await Promise.all((cfg?.plugins ?? []).flat(Infinity));
  } else {
    const mod = await import(pathToFileURL(pluginsPath).href);
    list = mod.default ?? mod.plugins ?? [];
  }
  plugins = (Array.isArray(list) ? list : [list]).filter(Boolean);
  plugins = plugins.filter((p) => !OJ_NATIVE_PLUGIN_NAMES.has(p && p.name));
  plugins = plugins.filter((p) => {
    if (p.apply == null) return true;
    try {
      if (typeof p.apply === "function") return !!p.apply(initial.config ?? {}, env);
      return p.apply === env.command;
    } catch (e) {
      if (ojStartMode) return true;
      throw e;
    }
  });
  plugins = plugins.filter((p) => {
    if (typeof p.applyToEnvironment !== "function") return true;
    try {
      return !!p.applyToEnvironment(environment);
    } catch (e) {
      if (ojStartMode) return true;
      throw e;
    }
  });
  const rank = (p) => (p.enforce === "pre" ? -1 : p.enforce === "post" ? 1 : 0);
  plugins.sort((a, b) => rank(a) - rank(b));
  process.stderr.write(
    `${OJ} plugin host: ${plugins.length} plugin(s) active for ${env.command}: ${plugins.map((p) => `${p.name}[${p.enforce ?? "-"}]`).join(",")}\n`,
  );
} catch (e) {
  process.stderr.write(`${OJ} plugin host: failed to load ${pluginsPath}: ${(e && e.stack) || e}\n`);
}

let rpcCounter = 1;
const rpcPending = new Map();
function ctxRpc(method, args) {
  const rpc = rpcCounter++;
  return new Promise((resolve, reject) => {
    rpcPending.set(rpc, { resolve, reject });
    process.stdout.write(JSON.stringify({ rpc, method, args }) + "\n");
  });
}

let emitCounter = 0;
const emitted = [];

const moduleInfoCache = new Map();

const watchedFiles = new Set();
const transformWatchStore = new AsyncLocalStorage();
const seenIds = new Set();

const ctx = {
  environment,
  warn: (m) => process.stderr.write(`oj plugin warn: ${m}\n`),
  error: (m) => {
    throw typeof m === "string" ? new Error(m) : m;
  },
  async resolve(source, importer) {
    const id = await ctxRpc("resolve", [source, importer ?? ""]);
    return id == null ? null : { id };
  },
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
  async load(options) {
    const id = typeof options === "string" ? options : options.id;
    const info = await ctxRpc("moduleInfo", [id]);
    if (info) {
      moduleInfoCache.set(info.id, info);
      seenIds.add(info.id);
    }
    return info;
  },
  getModuleInfo(id) {
    return moduleInfoCache.get(typeof id === "string" ? id : id.id) ?? null;
  },
  addWatchFile(id) {
    if (!id) return;
    watchedFiles.add(String(id));
    const bucket = transformWatchStore.getStore();
    if (bucket) bucket.add(String(id));
  },
  getModuleIds() {
    return seenIds.values();
  },
};

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
    try {
      const partial = await p.config.call(ctx, config, env);
      if (partial) config = deepMerge(config, partial);
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: config(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
  }
  const resolved = withResolvedDefaults(config);
  for (const p of plugins) {
    if (typeof p.configResolved !== "function") continue;
    try {
      await p.configResolved.call(ctx, resolved);
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: configResolved(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
  }
}
await runConfigHooks();

const wsListeners = new Map();
function ojWsSend(event, data) {
  process.stdout.write(JSON.stringify({ ojWs: { event, data: data ?? null } }) + "\n");
}
const wsApi = {
  on(event, cb) {
    let a = wsListeners.get(event);
    if (!a) wsListeners.set(event, (a = new Set()));
    a.add(cb);
  },
  off(event, cb) {
    wsListeners.get(event)?.delete(cb);
  },
  send(a, b) {
    if (typeof a === "string") ojWsSend(a, b);
    else if (a && typeof a === "object" && typeof a.event === "string") ojWsSend(a.event, a.data);
    else ojWsSend(null, a);
  },
};

let middlewarePort = null;
async function setupConfigureServer() {
  const stack = [];
  const middlewares = {
    use(a, b) {
      if (typeof a === "function") stack.push({ path: null, fn: a });
      else stack.push({ path: a, fn: b });
    },
  };
  const noop = () => {};
  const server = {
    config: initial.config ?? {},
    middlewares,
    httpServer: null,
    ws: wsApi,
    hot: wsApi,
    watcher: { on: noop, off: noop, add: noop, unwatch: noop, close: noop, emit: () => true, removeAllListeners: noop },
    moduleGraph: { getModuleById: () => null, getModulesByFile: () => null, invalidateModule: noop, onFileChange: noop },
    restart: async () => {},
    transformRequest: async () => null,
    ssrLoadModule: async () => {
      throw new Error("oj: server.ssrLoadModule is not available in configureServer");
    },
  };
  const post = [];
  for (const p of plugins) {
    if (typeof p.configureServer !== "function") continue;
    let r;
    try {
      r = await p.configureServer(server);
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: configureServer(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
      continue;
    }
    if (typeof r === "function") post.push(r);
  }
  for (const fn of post) {
    try {
      await fn();
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: post configureServer skipped: ${(e && e.message) || e}\n`);
    }
  }
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
if (env.command !== "build") await setupConfigureServer();

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
    const info = { id, code: current, importedIds: [] };
    moduleInfoCache.set(id, info);
    seenIds.add(id);
    for (const p of plugins) {
      if (typeof p.moduleParsed === "function") await p.moduleParsed.call(ctx, info);
    }
    return JSON.stringify({ code: current, watchFiles: [...bucket] });
  });
}

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

async function watchChange(id, event) {
  for (const p of plugins) {
    if (typeof p.watchChange === "function") await p.watchChange.call(ctx, id, { event });
  }
  return null;
}

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

function escapeAttr(v) {
  return String(v).replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}
function renderTag(t) {
  const attrs = Object.entries(t.attrs ?? {})
    .map(([k, v]) => (v === true ? ` ${k}` : v === false || v == null ? "" : ` ${k}="${escapeAttr(v)}"`))
    .join("");
  const inner = t.children ?? "";
  const voidTag = ["meta", "link", "base"].includes(t.tag);
  return voidTag ? `<${t.tag}${attrs}>` : `<${t.tag}${attrs}>${inner}</${t.tag}>`;
}

function injectTags(html, tags) {
  const at = { "head-prepend": [], head: [], "body-prepend": [], body: [] };
  // Vite's default injection point is head-prepend, not head-append.
  for (const t of tags) (at[t.injectTo ?? "head-prepend"] ?? at["head-prepend"]).push(renderTag(t));
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

function htmlHookRank(hook) {
  const order = hook && typeof hook === "object" ? hook.order ?? hook.enforce : undefined;
  return order === "pre" ? -1 : order === "post" ? 1 : 0;
}
async function transformIndexHtml(html) {
  let current = html;
  // Honor per-hook order: 'pre' hooks run first, 'post' last (stable within a rank).
  const entries = [];
  for (const p of plugins) {
    const hook = p.transformIndexHtml;
    const fn = typeof hook === "function" ? hook : hook?.handler ?? hook?.transform;
    if (typeof fn !== "function") continue;
    entries.push({ fn, rank: htmlHookRank(hook) });
  }
  entries.sort((a, b) => a.rank - b.rank);
  for (const { fn } of entries) {
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

async function runLifecycle(hook) {
  for (const p of plugins) {
    if (typeof p[hook] === "function") await p[hook].call(ctx);
  }
  return null;
}

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
  if (hook === "getEmittedFiles") {
    return JSON.stringify(emitted.map(({ fileName, source }) => ({ fileName, source })));
  }
  if (hook === "getWatchFiles") return JSON.stringify([...watchedFiles]);
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
  if (hook === "getPluginCount") return String(plugins.length);
  if (hook === "getMiddlewarePort") return middlewarePort == null ? null : String(middlewarePort);
  if (hook === "wsMessage") {
    const event = args[0];
    const data = args[1] ? JSON.parse(args[1]) : null;
    const a = wsListeners.get(event);
    if (a) {
      const client = { send: (e, d) => wsApi.send(e, d) };
      for (const cb of [...a]) {
        try {
          cb(data, client);
        } catch (e) {
          process.stderr.write(`${OJ} ws.on(${event}) handler failed: ${(e && e.stack) || e}\n`);
        }
      }
    }
    return null;
  }
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
