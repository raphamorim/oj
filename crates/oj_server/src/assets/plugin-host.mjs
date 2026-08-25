// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import http from "node:http";
import { createReadStream, existsSync, fstatSync, openSync, statSync, write as fsWrite, writeFileSync } from "node:fs";
import { readFile, stat as fsStat } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, isAbsolute, join, resolve as pathResolve } from "node:path";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";

const pluginsPath = process.argv[2];
const initial = JSON.parse(process.argv[3] ?? "{}");

const ssrEnvBase = { ...process.env };

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";
const env = initial.env ?? { command: "serve", mode: "development" };

// resolve.alias from the app's own vite config (loaded below for its plugins).
// oj applies aliases in its Rust resolver and does not forward them in the
// config it hands the host, so createResolver — which plugins like wyw-in-js use
// to resolve modules during CSS evaluation — would otherwise see no aliases.
let userResolveAlias = null;

// This process hosts untrusted third-party plugin code. A plugin that spawns a
// worker or a floating promise which throws asynchronously would otherwise take
// the whole host down, and with it every other plugin. The host's own request
// handling is synchronous and locally guarded, so a plugin's stray async
// failure is logged and swallowed rather than made fatal.
process.on("uncaughtException", (e) => {
  process.stderr.write(`oj plugin host: uncaught plugin error (ignored): ${(e && (e.stack || e.message)) || e}\n`);
});
process.on("unhandledRejection", (e) => {
  process.stderr.write(`oj plugin host: unhandled plugin rejection (ignored): ${(e && (e.stack || e.message)) || e}\n`);
});

// Start mode hosts only configureServer middleware for a framework config oj
// owns (TanStack Start): the framework plugins drive the module graph and SSR
// themselves, so their config lifecycle hooks are tolerated per-plugin instead
// of aborting the load, keeping the editor middleware (dev-server bridge) alive.
const ojStartMode = initial.ojStartMode === true;

const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";

let ssrBridgeDir = null;
let ssrContainer = null;
let ssrEnvDelta = {};
let ssrResolveReady;
const ssrReady = new Promise((r) => { ssrResolveReady = r; });
if (ojStartMode && initial.ssrBridge && initial.ssrBridge.dir) {
  const dir = initial.ssrBridge.dir;
  try {
    const repFd = openSync(join(dir, "rep.fifo"), "r+");
    const reqFd = openSync(join(dir, "req.fifo"), "r+");
    ssrBridgeDir = dir;
    const writeAll = (buf) => new Promise((resolve) => {
      let off = 0;
      const step = () => fsWrite(repFd, buf, off, buf.length - off, null, (e, n) => {
        if (e) {
          process.stderr.write(`${OJ} ssr bridge write failed: ${e}\n`);
          return resolve();
        }
        off += n;
        off < buf.length ? step() : resolve();
      });
      step();
    });
    let writeChain = Promise.resolve();
    const reply = (obj) => {
      const json = Buffer.from(JSON.stringify(obj));
      const frame = Buffer.allocUnsafe(4 + json.length);
      frame.writeUInt32LE(json.length, 0);
      json.copy(frame, 4);
      writeChain = writeChain.then(() => writeAll(frame));
    };
    const SSR_METHODS = new Set(["resolveId", "load", "transform", "transformUserCode"]);
    const dispatch = async ({ id, method, args }) => {
      await ssrReady;
      try {
        if (method === "__env") return reply({ id, value: ssrEnvDelta });
        if (method === "__heap") {
          return reply({ id, value: (await import("node:v8")).default.getHeapStatistics() });
        }
        if (!ssrContainer || !SSR_METHODS.has(method)) return reply({ id, value: null });
        reply({ id, value: (await ssrContainer[method](...(args ?? []))) ?? null });
      } catch (e) {
        reply({ id, error: String((e && e.stack) || e) });
      }
    };
    let acc = Buffer.alloc(0);
    createReadStream(null, { fd: reqFd, autoClose: false })
      .on("data", (chunk) => {
        acc = acc.length ? Buffer.concat([acc, chunk]) : chunk;
        while (acc.length >= 4) {
          const len = acc.readUInt32LE(0);
          if (acc.length < 4 + len) break;
          let msg = null;
          try { msg = JSON.parse(acc.subarray(4, 4 + len).toString("utf8")); } catch {}
          acc = acc.subarray(4 + len);
          if (msg && msg.id != null) dispatch(msg);
        }
      })
      .on("error", (e) => process.stderr.write(`${OJ} ssr bridge read failed: ${e}\n`));
  } catch (e) {
    process.stderr.write(`${OJ} ssr bridge unavailable: ${(e && e.message) || e}\n`);
    try { writeFileSync(join(dir, "disabled"), "1"); } catch {}
  }
}

// Vite's ResolvedConfig exposes createResolver(): a standalone module resolver
// built from the config's resolve options. Plugins that must resolve modules
// outside a hook context use it (e.g. wyw-in-js/linaria resolves the imports it
// reaches while evaluating `styled`/`css` template literals). It returns
// `(id, importer, aliasOnly, ssr) => Promise<string|undefined>` where the string
// is an absolute path. A faithful-enough subset — alias map + extension probe,
// with a Node fallback for bare specifiers — covers what those plugins need.
const RESOLVE_EXTS = [".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json"];

function aliasEntries(alias) {
  if (!alias) return [];
  if (Array.isArray(alias)) return alias.map((e) => [e.find, e.replacement]);
  return Object.entries(alias);
}

function applyAlias(id, entries) {
  for (const [find, replacement] of entries) {
    if (typeof replacement !== "string") continue;
    if (find instanceof RegExp) {
      if (find.test(id)) return id.replace(find, replacement);
      continue;
    }
    if (typeof find !== "string") continue;
    // oj's config extractor emits directory aliases with a trailing slash
    // (`"@/"` -> "<abs>/src/modules/"); normalize so both `"@"` and `"@/"`
    // forms match `@/foo` and join cleanly.
    const f = find.endsWith("/") ? find.slice(0, -1) : find;
    if (id === f) return replacement;
    if (id.startsWith(f + "/")) {
      const rest = id.slice(f.length + 1);
      const rep = replacement.endsWith("/") ? replacement : replacement + "/";
      return rep + rest;
    }
  }
  return id;
}

function probeFile(p, exts) {
  const st = statSync(p, { throwIfNoEntry: false });
  if (st?.isFile()) return p;
  if (st?.isDirectory()) {
    for (const ext of exts) {
      const idx = join(p, "index" + ext);
      if (existsSync(idx)) return idx;
    }
    return null;
  }
  for (const ext of exts) {
    if (existsSync(p + ext)) return p + ext;
  }
  return null;
}

function makeCreateResolver(config) {
  let entries = aliasEntries(config.resolve?.alias);
  if (entries.length === 0) entries = aliasEntries(userResolveAlias);
  const exts =
    Array.isArray(config.resolve?.extensions) && config.resolve.extensions.length
      ? config.resolve.extensions
      : RESOLVE_EXTS;
  const root = config.root ?? process.cwd();
  return function createResolver() {
    return async (id, importer, _aliasOnly, _ssr) => {
      if (!id) return undefined;
      let spec = id.split("?", 1)[0];
      if (spec.startsWith("\0") || spec.startsWith("/@")) return undefined;
      spec = applyAlias(spec, entries);
      const baseDir = importer
        ? dirname(importer.startsWith("file://") ? fileURLToPath(importer) : importer)
        : root;
      if (spec.startsWith(".")) return probeFile(pathResolve(baseDir, spec), exts) ?? undefined;
      if (isAbsolute(spec)) return probeFile(spec, exts) ?? undefined;
      try {
        return createRequire(join(baseDir, "__oj_resolver__.js")).resolve(spec);
      } catch {
        return undefined;
      }
    };
  };
}

function withResolvedDefaults(config) {
  const c = config ?? {};
  const merged = deepMerge(
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
      // Vite's resolved config always carries these; plugins read them in
      // configResolved/configureServer (e.g. `config.plugins.findIndex`,
      // `config.environments.client`). Absent, those hooks throw and get
      // skipped though they only meant to inspect the shape.
      plugins: [],
      environments: { client: {}, ssr: {} },
    },
    c,
  );
  if (typeof merged.createResolver !== "function") merged.createResolver = makeCreateResolver(merged);
  return merged;
}

const envName = initial.environment?.name ?? "client";
const environment = {
  name: envName,
  mode: initial.environment?.mode ?? env.mode,
  config: withResolvedDefaults(
    deepMerge(initial.config ?? {}, (initial.config?.environments ?? {})[envName] ?? {}),
  ),
};
// Vite tags each environment's resolved config with a `consumer` ("client" or
// "server"); plugins like @vitejs/plugin-react read `env.config.consumer`
// directly in applyToEnvironment, so it must be present or they throw.
environment.config.consumer =
  environment.config.consumer ?? (envName === "client" ? "client" : "server");
// The resolved config (defaults + plugin `config` hooks). configureServer must
// receive this, not the raw initial.config, so plugins reading resolved-only
// fields (experimental, environments, plugins) don't throw. Seeded with the
// resolved environment config until runConfigHooks recomputes it.
let resolvedConfig = environment.config;

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
    const out = join(dirname(fileURLToPath(import.meta.url)), "oj-vite-config.mjs");
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

// Dev-tooling plugins oj cannot host: they drive a full Vite dev server (ws
// error overlay, file watcher, a worker running tsc/eslint) that oj does not
// provide, and they have no effect on the served or built output. Left in, the
// worker they spawn throws asynchronously; skip them outright.
const OJ_UNSUPPORTED_PLUGIN_NAMES = new Set(["vite-plugin-checker"]);

let plugins = [];
let allPlugins = [];
try {
  let list;
  if (initial.pluginsFormat === "vite") {
    const cfg = await loadViteConfig(pluginsPath);
    userResolveAlias = cfg?.resolve?.alias ?? null;
    list = await Promise.all((cfg?.plugins ?? []).flat(Infinity));
  } else {
    const mod = await import(pathToFileURL(pluginsPath).href);
    list = mod.default ?? mod.plugins ?? [];
  }
  plugins = (Array.isArray(list) ? list : [list]).filter(Boolean);
  allPlugins = plugins;
  plugins = plugins.filter((p) => !OJ_NATIVE_PLUGIN_NAMES.has(p && p.name));
  plugins = plugins.filter((p) => {
    if (OJ_UNSUPPORTED_PLUGIN_NAMES.has(p && p.name)) {
      process.stderr.write(
        `${OJ} plugin host: skipping unsupported plugin "${p.name}" (dev-only tooling oj does not host; your app's output is unaffected)\n`,
      );
      return false;
    }
    return true;
  });
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
  const rank = (p) => (p.enforce === "pre" ? -1 : p.enforce === "post" ? 1 : 0);
  plugins.sort((a, b) => rank(a) - rank(b));
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
// Chunks a plugin asks oj to emit via `this.emitFile({ type: "chunk" })`.
// The reference id is minted here and returned synchronously (Rollup's emitFile
// is sync); the chunk is forwarded to rolldown as a build root by the Rust side,
// and its final hashed name is seeded back into `chunkFileNames` before
// generateBundle so `this.getFileName(refId)` resolves.
const chunkFileNames = new Map();
const transformEmitStore = new AsyncLocalStorage();

const moduleInfoCache = new Map();

const watchedFiles = new Set();
const transformWatchStore = new AsyncLocalStorage();
const seenIds = new Set();

// this.parse is synchronous in Rollup, so resolve Vite's parseAst (re-exported
// from Rollup) once at startup; AST-aware plugins call it inside transform.
let viteParseAst = null;
try {
  const _root = initial.config?.root ?? process.cwd();
  const _vite = await import(createRequire(_root + "/package.json").resolve("vite"));
  if (typeof _vite.parseAst === "function") viteParseAst = _vite.parseAst;
} catch {}

const ctx = {
  environment,
  meta: { rollupVersion: "4.0.0", watchMode: true, framework: "oj" },
  parse: (code, opts) => (viteParseAst ? viteParseAst(code, opts) : {}),
  warn: (m) => process.stderr.write(`oj plugin warn: ${m}\n`),
  error: (m) => {
    throw typeof m === "string" ? new Error(m) : m;
  },
  async resolve(source, importer) {
    const id = await ctxRpc("resolve", [source, importer ?? ""]);
    return id == null ? null : { id };
  },
  emitFile(file) {
    if (file == null) {
      throw new Error("oj: this.emitFile requires a file descriptor");
    }
    if (file.type === "chunk") {
      if (!file.id) {
        throw new Error("oj: this.emitFile({ type: 'chunk' }) requires an id");
      }
      const referenceId = `oj-chunk-${emitCounter++}`;
      const rec = {
        referenceId,
        id: file.id,
        name: file.name ?? null,
        fileName: file.fileName ?? null,
      };
      const bucket = transformEmitStore.getStore();
      if (bucket) bucket.push(rec);
      return referenceId;
    }
    if (file.type && file.type !== "asset") {
      throw new Error("oj: this.emitFile supports { type: 'asset' | 'chunk' }");
    }
    const fileName = file.fileName ?? `assets/${file.name ?? `asset-${emitCounter}`}`;
    const referenceId = `oj-ref-${emitCounter++}`;
    emitted.push({ referenceId, fileName, source: String(file.source ?? "") });
    return referenceId;
  },
  getFileName(referenceId) {
    if (chunkFileNames.has(referenceId)) return chunkFileNames.get(referenceId);
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
  resolvedConfig = withResolvedDefaults(config);
  for (const p of plugins) {
    if (typeof p.configResolved !== "function") continue;
    try {
      await p.configResolved.call(ctx, resolvedConfig);
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: configResolved(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
  }
}
await runConfigHooks();

plugins = plugins.filter((p) => {
  if (typeof p.applyToEnvironment !== "function") return true;
  try {
    return !!p.applyToEnvironment(environment);
  } catch (e) {
    process.stderr.write(
      `${OJ} plugin host: applyToEnvironment(${p.name ?? "?"}) threw; keeping the plugin active: ${(e && e.message) || e}\n`,
    );
    return true;
  }
});
process.stderr.write(
  `${OJ} plugin host: ${plugins.length} plugin(s) active for ${env.command}: ${plugins.map((p) => `${p.name}[${p.enforce ?? "-"}]`).join(",")}\n`,
);

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
    config: resolvedConfig,
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
      while (i < stack.length) {
        const { path, fn } = stack[i++];
        if (path && !req.url.startsWith(path)) continue;
        if ((fn.length >= 4) !== (err != null)) continue;
        try {
          return err != null ? fn(err, req, res, next) : fn(req, res, next);
        } catch (e) {
          return next(e);
        }
      }
      if (err != null) {
        res.statusCode = 500;
        res.end(String((err && err.stack) || err));
        return;
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

if (ssrBridgeDir) {
  (async () => {
    try {
      const bridgePath = join(dirname(fileURLToPath(import.meta.url)), "start", "vite-plugin-bridge.mjs");
      const bridge = await import(pathToFileURL(bridgePath).href);
      const c = bridge.createPluginContainer({ parseAst: viteParseAst }, allPlugins, {
        command: env.command,
        mode: env.mode,
        environment: "ssr",
      });
      await c.buildStart();
      ssrContainer = c;
      process.stderr.write(`${OJ} plugin host: ssr container ready (${c.pluginCount} plugin(s), shared config eval)\n`);
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: ssr container failed: ${(e && (e.stack || e.message)) || e}\n`);
    }
    for (const [k, v] of Object.entries(process.env)) {
      if (ssrEnvBase[k] !== v) ssrEnvDelta[k] = v;
    }
    try { writeFileSync(join(ssrBridgeDir, "ready"), "1"); } catch {}
    if (process.env.OJ_BOOT_PHASES) {
      process.stderr.write(`[oj-phase] ${Date.now()} container: bootstrap done\n`);
    }
    ssrResolveReady();
  })();
}

async function transform(code, id) {
  const bucket = new Set();
  const chunkBucket = [];
  return transformWatchStore.run(bucket, () =>
    transformEmitStore.run(chunkBucket, async () => {
      if (id) seenIds.add(id);
      let current = code;
      const maps = [];
      for (const p of plugins) {
        if (typeof p.transform !== "function") continue;
        const r = await p.transform.call(ctx, current, id);
        if (r == null) continue;
        if (typeof r === "string") {
          current = r;
          continue;
        }
        if (r.code != null) current = r.code;
        if (r.map != null) maps.push(typeof r.map === "string" ? r.map : JSON.stringify(r.map));
      }
      const info = { id, code: current, importedIds: [] };
      moduleInfoCache.set(id, info);
      seenIds.add(id);
      for (const p of plugins) {
        if (typeof p.moduleParsed === "function") await p.moduleParsed.call(ctx, info);
      }
      return JSON.stringify({
        code: current,
        watchFiles: [...bucket],
        maps,
        emittedChunks: chunkBucket,
      });
    }),
  );
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

async function readModifiedFile(file) {
  const content = await readFile(file, "utf8");
  if (content) return content;
  const mtime = (await fsStat(file)).mtimeMs;
  for (let n = 0; n < 10; n++) {
    await new Promise((r) => setTimeout(r, 10));
    const newMtime = (await fsStat(file)).mtimeMs;
    if (newMtime !== mtime) break;
  }
  return readFile(file, "utf8");
}

async function handleHotUpdate(file, timestamp, type, modulesJson) {
  let modules;
  try {
    modules = modulesJson ? JSON.parse(modulesJson) : [];
  } catch {
    modules = [];
  }
  const hmrContext = {
    file,
    timestamp: Number(timestamp),
    type: type || "update",
    modules,
    read: () => readModifiedFile(file),
  };
  let filtered = null;
  for (const p of plugins) {
    if (typeof p.handleHotUpdate !== "function") continue;
    const r = await p.handleHotUpdate.call(ctx, hmrContext);
    if (r === "full-reload") return "full-reload";
    if (Array.isArray(r)) {
      hmrContext.modules = r;
      filtered = r
        .map((m) => (typeof m === "string" ? m : m && (m.url ?? m.id)))
        .filter((u) => typeof u === "string" && u.length > 0);
    }
  }
  if (filtered === null) return null;
  if (filtered.length === 0) return "skip";
  return JSON.stringify({ action: "filter", modules: filtered });
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
  if (hook === "seedChunkNames") {
    try {
      const m = JSON.parse(args[0] ?? "{}");
      for (const [k, v] of Object.entries(m)) chunkFileNames.set(k, v);
    } catch {}
    return null;
  }
  if (hook === "transform") return transform(args[0], args[1]);
  if (hook === "resolveId") return resolveId(args[0], args[1]);
  if (hook === "load") return load(args[0]);
  if (hook === "handleHotUpdate") return handleHotUpdate(args[0], args[1], args[2], args[3]);
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
  if (hook === "getHasTransform") {
    const has = plugins.some((p) => {
      const t = p && p.transform;
      const fn = typeof t === "function" ? t : t && t.handler;
      return typeof fn === "function";
    });
    return String(has);
  }
  if (hook === "getHmrHooks") {
    const watchChangeHook = plugins.some((p) => typeof p.watchChange === "function");
    const hotUpdateHook = plugins.some((p) => typeof p.handleHotUpdate === "function");
    return JSON.stringify({ watchChange: watchChangeHook, handleHotUpdate: hotUpdateHook });
  }
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

const hardExit = () => process.kill(process.pid, "SIGKILL");
try {
  if (fstatSync(0, { bigint: true }).isFIFO()) {
    process.stdin.once("end", hardExit);
    process.stdin.once("close", hardExit);
  }
} catch {}

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
