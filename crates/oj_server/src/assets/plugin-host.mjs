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
import { EventEmitter } from "node:events";

const pluginsPath = process.argv[2];
const initial = JSON.parse(process.argv[3] ?? "{}");

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";
// Snapshot after the host's own env tweaks so the config()-hook delta reports
// only plugin mutations, not host bootstrap noise.
const ssrEnvBase = { ...process.env };
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
      // Vite's resolved `build` carries defaults plugins read in configResolved
      // (e.g. UnoCSS resolves `config.build.outDir` and reads
      // `config.build.rollupOptions.output`). User config overrides these.
      build: {
        target: "modules",
        outDir: "dist",
        assetsDir: "assets",
        assetsInlineLimit: 4096,
        cssCodeSplit: true,
        sourcemap: false,
        rollupOptions: {},
        minify: "esbuild",
        reportCompressedSize: true,
        chunkSizeWarningLimit: 500,
      },
      server: { headers: {} },
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
  // Vite resolves `publicDir` to an absolute path (default `<root>/public`);
  // plugins like @crxjs read it directly to locate manifest assets (icons,
  // locales). `false` disables it, matching Vite.
  if (merged.publicDir !== false) {
    const pd = typeof merged.publicDir === "string" && merged.publicDir.length > 0 ? merged.publicDir : "public";
    merged.publicDir = isAbsolute(pd) ? pd : pathResolve(merged.root, pd);
  }
  // Vite's resolved config carries a `logger`; plugins (e.g. the cloudflare
  // plugin's ViteMiniflareLogger) call `config.logger.info/warn/error`.
  if (!merged.logger || typeof merged.logger.info !== "function") {
    const w = (...a) => process.stderr.write(a.map(String).join(" ") + "\n");
    merged.logger = {
      info: () => {}, warn: w, warnOnce: w, error: w,
      clearScreen: () => {}, hasErrorLogged: () => false, hasWarned: false,
    };
  }
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

// A minimal stand-in for Vite's `vite:css-post` plugin. UnoCSS (and similar)
// look up a plugin named "vite:css-post" in `config.plugins` during their build
// hook and call its `transform` with the CSS they generate, to route it into
// the output. oj collects that CSS here and folds it into the build stylesheet
// (see getPluginCss). Only relevant fields are implemented.
const ojPluginCss = [];
const cssPostShim = {
  name: "vite:css-post",
  transform: {
    handler(code, id) {
      if (typeof code === "string" && code.length > 0) {
        ojPluginCss.push({ id: typeof id === "string" ? id : "", css: code });
      }
      return null;
    },
  },
};

const moduleInfoCache = new Map();

// Vite reports a failing hook as `[plugin:name] message` followed by the module
// id with `line:column` and the code frame (pluginContainer's formatError), and
// fails the request/build. Attach the same so the overlay and the terminal say
// which plugin broke on which file instead of silently serving the raw source.
function decoratePluginError(e, plugin, id) {
  const err = e instanceof Error ? e : new Error(typeof e === "string" ? e : (e && e.message) || String(e));
  if (e && typeof e === "object" && !(e instanceof Error)) Object.assign(err, e);
  if (err.ojDecorated) return err;
  const name = (plugin && plugin.name) || "unknown";
  const loc = err.loc && typeof err.loc === "object" ? err.loc : null;
  const line = loc ? loc.line : err.line;
  const column = loc ? loc.column : err.column;
  const file = (loc && loc.file) || err.id || id || "";
  let msg = `[plugin:${name}] ${err.message}`;
  if (file) msg += `\n${file}${line != null ? `:${line}${column != null ? `:${column}` : ""}` : ""}`;
  if (typeof err.frame === "string" && err.frame) msg += `\n${err.frame}`;
  err.message = msg;
  err.plugin = name;
  err.ojDecorated = true;
  err.stack = msg;
  return err;
}

const watchedFiles = new Set();
const transformWatchStore = new AsyncLocalStorage();
// Per-transform map of oj-resolved imports (spec -> id) the Rust side computes and
// passes in, so a plugin's `this.resolve` during transform is a local lookup rather
// than a host round-trip. ctx.resolve consults it first.
const transformResolveStore = new AsyncLocalStorage();
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
    if (source === "/@react-refresh") return { id: "/@oj/refresh-runtime.js" };
    const map = transformResolveStore.getStore();
    if (map && map.has(source)) {
      const id = map.get(source);
      return id == null ? null : { id };
    }
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
  // Vite hands the config hook the user config, whose `plugins` is the flat
  // plugin array; plugins like @crxjs read `config.plugins` to find sibling
  // plugins. Use the apply-filtered active set (not allPlugins) so command-
  // inappropriate plugins are absent, matching Vite's resolved config — e.g.
  // `@crxjs`'s serve-only `crx:hmr` must not appear during a build, or crx calls
  // its `transformCrxManifest` (which reads an unset `config`) and throws.
  // The exposed plugin array: the apply-filtered active set plus the css-post
  // shim. Pinned across the config-hook merges below so deepMerge (which
  // concatenates arrays) can't accumulate it into duplicates.
  const configPlugins = plugins.slice();
  if (!configPlugins.some((p) => p && p.name === "vite:css-post")) {
    configPlugins.push(cssPostShim);
  }
  config.plugins = configPlugins;
  for (const { p, fn } of pluginsWithHook("config")) {
    try {
      const partial = await fn.call(ctx, config, env);
      if (partial) config = deepMerge(config, partial);
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: config(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
    // Keep the plugin array stable regardless of what a config hook returned.
    config.plugins = configPlugins;
  }
  // Vite runs `configEnvironment(name, options, env)` for every environment after
  // the config hooks and merges what it returns into that environment's options.
  const envNames = Object.keys(config.environments ?? {});
  for (const n of ["client", "ssr"]) if (!envNames.includes(n)) envNames.push(n);
  const configEnvHooks = pluginsWithHook("configEnvironment");
  if (configEnvHooks.length) {
    config.environments = { ...(config.environments ?? {}) };
    for (const name of envNames) {
      config.environments[name] = config.environments[name] ?? {};
      const opts = {
        ...env,
        isSsrBuild: name !== "client" && env.command === "build",
        isPreview: false,
        isSsrTargetWebworker: false,
      };
      for (const { p, fn } of configEnvHooks) {
        try {
          const r = await fn.call(ctx, name, config.environments[name], opts);
          if (r) config.environments[name] = deepMerge(config.environments[name], r);
        } catch (e) {
          if (!ojStartMode) throw e;
          process.stderr.write(`${OJ} plugin host: configEnvironment(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
        }
      }
    }
  }
  resolvedConfig = withResolvedDefaults(config);
  resolvedConfig.plugins = configPlugins;
  for (const { p, fn } of pluginsWithHook("configResolved")) {
    try {
      await fn.call(ctx, resolvedConfig);
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
// The ViteDevServer stand-in handed to configureServer; hotUpdate/handleHotUpdate
// contexts carry it as `server` (plugins call server.ws.send / moduleGraph on it).
let devServer = null;
let appVite = null;
async function loadAppVite() {
  if (appVite) return appVite;
  try {
    const root = (resolvedConfig && resolvedConfig.root) || (initial.config && initial.config.root) || process.cwd();
    appVite = await import(createRequire(root + "/package.json").resolve("vite"));
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: could not load app vite: ${(e && e.message) || e}\n`);
  }
  return appVite;
}

// Build real Vite DevEnvironments from the app's installed Vite so plugins that
// use the Environment API (e.g. @cloudflare/vite-plugin, which subclasses
// vite.DevEnvironment) run. oj does not depend on Vite; it loads the app's copy.
async function buildEnvironments(server) {
  const vite = await loadAppVite();
  if (!vite || typeof vite.resolveConfig !== "function") return undefined;
  const root = (resolvedConfig && resolvedConfig.root) || (initial.config && initial.config.root) || process.cwd();
  // Real DevEnvironments need a real resolved Vite config (root, cacheDir,
  // resolve.builtins, per-env dev.createEnvironment factories). Hand-faking it
  // is endless whack-a-mole, so ask the app's Vite to resolve it once.
  let rc;
  try {
    // configFile:undefined lets Vite resolve fresh plugin instances. Reusing
    // oj's instances (configFile:false + plugins) corrupts stateful plugins
    // (e.g. @cloudflare/vite-plugin's virtual:cloudflare/export-types) when
    // their config hooks run a second time, so we accept a second resolve. The
    // environments bind to these fresh instances; oj drives its own instances
    // in configureServer, and the two agree because both resolve from `root`.
    rc = await vite.resolveConfig({ root, configFile: undefined, mode: environment.mode }, "serve", "development", "development");
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: vite.resolveConfig failed: ${(e && e.message) || e}\n`);
    return undefined;
  }
  server.config = rc;
  const environments = {};
  for (const [name, envOpts] of Object.entries(rc.environments || {})) {
    try {
      const factory = envOpts && envOpts.dev && envOpts.dev.createEnvironment;
      environments[name] = factory
        ? await factory(name, rc, { ws: server.ws })
        : new vite.DevEnvironment(name, rc, { hot: true, transport: server.ws });
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: createEnvironment(${name}) failed: ${(e && e.message) || e}\n`);
    }
  }
  for (const [name, ei] of Object.entries(environments)) {
    try { if (ei && typeof ei.init === "function") await ei.init({ watcher: server.watcher }); }
    catch (e) { process.stderr.write(`${OJ} plugin host: env.init(${name}) failed: ${(e && e.message) || e}\n`); }
  }
  return environments;
}

// On a source edit oj POSTs the changed paths to /__oj_invalidate; invalidate the
// affected modules in each DevEnvironment's graph and tell its runner to reload,
// so the plugin's Miniflare/workerd re-fetches changed modules (Vite's watcher
// does this itself; oj drives it since it owns the file watcher).
function invalidateEnvironments(environments, watcher, paths) {
  for (const file of paths) {
    try { watcher.emit("change", file); } catch {}
  }
  if (!environments) return;
  for (const env of Object.values(environments)) {
    if (!env) continue;
    try {
      const mg = env.moduleGraph;
      for (const file of paths) {
        if (mg && typeof mg.getModulesByFile === "function") {
          const mods = mg.getModulesByFile(file);
          if (mods) for (const m of mods) mg.invalidateModule && mg.invalidateModule(m);
        }
        if (mg && typeof mg.onFileChange === "function") mg.onFileChange(file);
      }
      if (env.hot && typeof env.hot.send === "function") env.hot.send({ type: "full-reload" });
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: invalidate failed: ${(e && e.message) || e}\n`);
    }
  }
}

async function setupConfigureServer() {
  const stack = [];
  function runStack(req, res, done) {
    let i = 0;
    const next = (err) => {
      while (i < stack.length) {
        const entry = stack[i++];
        const path = entry.route === "/" ? null : entry.route;
        const fn = entry.handle;
        if (path && !req.url.startsWith(path)) continue;
        if ((fn.length >= 4) !== (err != null)) continue;
        try {
          return err != null ? fn(err, req, res, next) : fn(req, res, next);
        } catch (e) {
          return next(e);
        }
      }
      if (typeof done === "function") return done(err);
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
  }
  // @cloudflare/vite-plugin uses `server.middlewares` both as a callable
  // connect app (its workerd->node bridge) and via `.use()`, so provide both.
  function middlewares(req, res, done) {
    return runStack(req, res, done);
  }
  // connect stack entries are { route, handle } (plugins walk .stack reading
  // handle.name); mirror that shape.
  middlewares.use = (a, b) => {
    if (typeof a === "function") stack.push({ route: "/", handle: a });
    else stack.push({ route: a, handle: b });
  };
  middlewares.stack = stack;
  const noop = () => {};
  const fileWatcher = Object.assign(new EventEmitter(), { add: noop, unwatch: noop, close: noop });
  const server = {
    config: resolvedConfig,
    middlewares,
    httpServer: null,
    ws: wsApi,
    hot: wsApi,
    watcher: fileWatcher,
    moduleGraph: { getModuleById: () => null, getModuleByUrl: async () => null, getModulesByFile: () => null, invalidateModule: noop, onFileChange: noop },
    restart: async () => {},
    close: async () => {},
    transformIndexHtml: async (_url, html) => transformIndexHtml(html),
    transformRequest: async () => null,
    ssrLoadModule: async () => {
      throw new Error("oj: server.ssrLoadModule is not available in configureServer");
    },
  };
  if (plugins.some((p) => p && p.name === "vite-plugin-cloudflare:dev")) {
    try {
      server.environments = await buildEnvironments(server);
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: buildEnvironments failed: ${(e && e.message) || e}\n`);
    }
  }
  devServer = server;
  const post = [];
  for (const { p, fn } of pluginsWithHook("configureServer")) {
    let r;
    try {
      r = await fn.call(ctx, server);
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
    if (req.method === "POST" && req.url === "/__oj_invalidate") {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        let paths = [];
        try { paths = JSON.parse(body || "{}").paths || []; } catch {}
        invalidateEnvironments(server.environments, fileWatcher, paths);
        res.statusCode = 204;
        res.end();
      });
      return;
    }
    middlewares(req, res);
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

async function transform(code, id, resolvedJson) {
  const bucket = new Set();
  const chunkBucket = [];
  let resolveMap = null;
  if (resolvedJson) {
    try {
      resolveMap = new Map(Object.entries(JSON.parse(resolvedJson)));
    } catch {}
  }
  const runLoop = async () => {
    if (id) seenIds.add(id);
    let current = code;
    const maps = [];
    const transformOptions = { ssr: environment && environment.name === "ssr" };
    for (const p of plugins) {
      const handler = hookHandler(p.transform);
      if (!handler || !hookTransformMatches(p.transform, id, current)) continue;
      let r;
      try {
        r = await handler.call(ctx, current, id, transformOptions);
      } catch (e) {
        throw decoratePluginError(e, p, id);
      }
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
    for (const { fn } of pluginsWithHook("moduleParsed")) await fn.call(ctx, info);
    return JSON.stringify({
      code: current,
      watchFiles: [...bucket],
      maps,
      emittedChunks: chunkBucket,
    });
  };
  return transformWatchStore.run(bucket, () =>
    transformEmitStore.run(chunkBucket, () =>
      transformResolveStore.run(resolveMap, runLoop),
    ),
  );
}

async function replayModuleParsed(id) {
  if (id) seenIds.add(id);
  const info = moduleInfoCache.get(id) ?? { id, code: "", importedIds: [] };
  moduleInfoCache.set(id, info);
  for (const { fn } of pluginsWithHook("moduleParsed")) await fn.call(ctx, info);
  return null;
}

const anyModuleParsed = () => pluginsWithHook("moduleParsed").length > 0;

async function watchChange(id, event) {
  for (const { fn } of pluginsWithHook("watchChange")) await fn.call(ctx, id, { event });
  return null;
}

// Any hook can be a bare function or Vite's object form { order, filter, handler }
// (createVirtualModule, import-protection, and others use it). Vite unwraps every
// hook through getHookHandler and sorts the plugins of a hook by its `order`
// (pre / normal / post, stable within a rank); do the same for every hook here.
function hookHandler(hook) {
  if (typeof hook === "function") return hook;
  if (hook && typeof hook.handler === "function") return hook.handler;
  return null;
}
function hookFn(plugin, name) {
  return plugin ? hookHandler(plugin[name]) : null;
}
function hookOrderRank(hook) {
  const o = hook && typeof hook === "object" ? hook.order : undefined;
  return o === "pre" ? -1 : o === "post" ? 1 : 0;
}
function pluginsWithHook(name) {
  const out = [];
  for (const p of plugins) {
    const fn = hookFn(p, name);
    if (fn) out.push({ p, fn, rank: hookOrderRank(p[name]) });
  }
  out.sort((a, b) => a.rank - b.rank);
  return out;
}

// Vite's pluginFilter: a string *id* filter is a picomatch glob (dot:true) joined
// to cwd unless it starts with `**` or is absolute, a RegExp is tested against the
// slash-normalized id (lastIndex reset), and a string *code* filter is a substring.
// The glob dialect covered is what plugins use: `*`, `**`, `?`, `{a,b}`, `[...]`.
const slash = (p) => p.replace(/\\/g, "/");
function globToRegExpSource(glob) {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        i++;
        if (glob[i + 1] === "/") {
          i++;
          re += "(?:.*/)?";
        } else {
          re += ".*";
        }
      } else {
        re += "[^/]*";
      }
    } else if (c === "?") {
      re += "[^/]";
    } else if (c === "{") {
      const j = glob.indexOf("}", i);
      if (j > i) {
        re += "(?:" + glob.slice(i + 1, j).split(",").map(globToRegExpSource).join("|") + ")";
        i = j;
      } else {
        re += "\\{";
      }
    } else if (c === "[") {
      const j = glob.indexOf("]", i);
      if (j > i) {
        let cls = glob.slice(i + 1, j);
        if (cls[0] === "!") cls = "^" + cls.slice(1);
        re += "[" + cls + "]";
        i = j;
      } else {
        re += "\\[";
      }
    } else if (c === "\\" && i + 1 < glob.length) {
      re += "\\" + glob[++i];
    } else {
      re += c.replace(/[.+^$()|]/g, "\\$&");
    }
  }
  return re;
}
const idGlobCache = new Map();
function idPatternToRegExp(pattern) {
  if (pattern instanceof RegExp) return pattern;
  let re = idGlobCache.get(pattern);
  if (!re) {
    const cwd = initial.config?.root ?? process.cwd();
    const glob = pattern.startsWith("**") || isAbsolute(pattern) ? slash(pattern) : slash(join(cwd, pattern));
    re = new RegExp("^" + globToRegExpSource(glob) + "$");
    idGlobCache.set(pattern, re);
  }
  return re;
}
function matchId(pattern, id) {
  if (!(pattern instanceof RegExp) && typeof pattern !== "string") return false;
  const re = idPatternToRegExp(pattern);
  const r = re.test(slash(id));
  re.lastIndex = 0;
  return r;
}
function matchCode(pattern, code) {
  if (pattern instanceof RegExp) {
    const r = pattern.test(code);
    pattern.lastIndex = 0;
    return r;
  }
  return typeof pattern === "string" && code.includes(pattern);
}
function matchList(patterns, value, matcher) {
  if (patterns == null) return false;
  return (Array.isArray(patterns) ? patterns : [patterns]).some((p) => matcher(p, value));
}
function filterMatches(spec, value, matcher) {
  if (spec == null) return true;
  if (spec instanceof RegExp || typeof spec === "string" || Array.isArray(spec)) {
    return matchList(spec, value, matcher);
  }
  if (spec.exclude != null && matchList(spec.exclude, value, matcher)) return false;
  if (spec.include != null) return matchList(spec.include, value, matcher);
  return true;
}
// Rolldown's moduleType for an id, from its extension (what `filter.moduleType`
// compares against); anything unknown is "js".
function moduleTypeOf(id) {
  const clean = String(id).split("?", 1)[0].split("#", 1)[0];
  const m = /\.([a-z0-9]+)$/i.exec(clean);
  const ext = m ? m[1].toLowerCase() : "";
  if (["js", "jsx", "ts", "tsx", "json", "css", "text", "base64", "dataurl", "binary", "empty", "asset"].includes(ext)) {
    return ext;
  }
  if (ext === "mjs" || ext === "cjs") return "js";
  if (ext === "mts" || ext === "cts") return "ts";
  return "js";
}

function hookIdMatches(hook, id) {
  const f = hook && typeof hook === "object" ? hook.filter : null;
  return !f || filterMatches(f.id, id, matchId);
}

function hookTransformMatches(hook, id, code) {
  const f = hook && typeof hook === "object" ? hook.filter : null;
  if (!f) return true;
  if (!filterMatches(f.id, id, matchId)) return false;
  if (f.code != null && !filterMatches(f.code, code, matchCode)) return false;
  if (f.moduleType != null && !filterMatches(f.moduleType, moduleTypeOf(id), (p, v) => p === v)) return false;
  return true;
}

async function resolveId(source, importer) {
  const options = { scan: false, isEntry: false, custom: {}, attributes: {} };
  for (const p of plugins) {
    const handler = hookHandler(p.resolveId);
    if (!handler || !hookIdMatches(p.resolveId, source)) continue;
    let r;
    try {
      r = await handler.call(ctx, source, importer || undefined, options);
    } catch (e) {
      throw decoratePluginError(e, p, importer || source);
    }
    if (r == null) continue;
    return typeof r === "string" ? r : (r.id ?? null);
  }
  return null;
}

async function load(id) {
  for (const p of plugins) {
    const handler = hookHandler(p.load);
    if (!handler || !hookIdMatches(p.load, id)) continue;
    let r;
    try {
      r = await handler.call(ctx, id);
    } catch (e) {
      throw decoratePluginError(e, p, id);
    }
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
  // Vite's HmrContext / HotUpdateOptions both carry `server`; the ubiquitous
  // `handleHotUpdate({ file, server }) { server.ws.send(...) }` idiom needs it.
  const hmrContext = {
    file,
    timestamp: Number(timestamp),
    type: type || "update",
    modules,
    read: () => readModifiedFile(file),
    server: devServer,
  };
  let filtered = null;
  for (const { fn } of hotUpdatePlugins()) {
    const r = await fn.call(ctx, hmrContext);
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

// Vite 6+: `plugin.hotUpdate ?? plugin.handleHotUpdate` (the Environment API hook
// takes precedence, with the same context shape plus this.environment).
function hotUpdatePlugins() {
  const out = [];
  for (const p of plugins) {
    const hook = p.hotUpdate != null ? p.hotUpdate : p.handleHotUpdate;
    const fn = hookHandler(hook);
    if (fn) out.push({ p, fn, rank: hookOrderRank(hook) });
  }
  out.sort((a, b) => a.rank - b.rank);
  return out;
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
  for (const { fn } of pluginsWithHook(hook)) await fn.call(ctx);
  return null;
}

async function runBuildStart() {
  const chunkBucket = [];
  // Rollup passes NormalizedInputOptions; plugins such as @crxjs read
  // `options.input` to decide whether to emit their root chunk, so it must be
  // present. Chunks emitted here are drained to Rust as rolldown build roots.
  const options = {
    input: resolvedConfig?.build?.rollupOptions?.input ?? "index.html",
  };
  await transformEmitStore.run(chunkBucket, async () => {
    for (const { fn } of pluginsWithHook("buildStart")) await fn.call(ctx, options);
  });
  return JSON.stringify({ emittedChunks: chunkBucket });
}

async function generateBundle(bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = environment.config?.build ?? {};
  for (const { p, fn } of pluginsWithHook("generateBundle")) {
    try {
      await fn.call(ctx, outputOptions, bundle, isWrite);
    } catch (e) {
      process.stderr.write(
        `${OJ} plugin host: generateBundle(${p.name ?? "?"}) failed: ${(e && (e.stack || e.message)) || e}\n`,
      );
    }
  }
  return JSON.stringify(bundle);
}

async function renderChunk(code, chunkJson) {
  const chunk = JSON.parse(chunkJson || "{}");
  // Rollup passes NormalizedOutputOptions as the 3rd arg; renderChunk hooks such
  // as UnoCSS's read `options.dir` (the resolved output dir) to key their
  // vite:css-post lookup, so provide it resolved against the root.
  const outDir = resolvedConfig?.build?.outDir ?? "dist";
  const root = resolvedConfig?.root ?? initial.config?.root ?? process.cwd();
  const options = { dir: pathResolve(root, outDir), format: "es" };
  let current = code;
  for (const { fn } of pluginsWithHook("renderChunk")) {
    const r = await fn.call(ctx, current, chunk, options);
    if (r == null) continue;
    current = typeof r === "string" ? r : (r.code ?? current);
  }
  return current;
}

async function writeBundle(bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = environment.config?.build ?? {};
  for (const { fn } of pluginsWithHook("writeBundle")) {
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
  if (hook === "transform") return transform(args[0], args[1], args[2]);
  if (hook === "resolveId") return resolveId(args[0], args[1]);
  if (hook === "load") return load(args[0]);
  if (hook === "handleHotUpdate") return handleHotUpdate(args[0], args[1], args[2], args[3]);
  if (hook === "transformIndexHtml") return transformIndexHtml(args[0]);
  if (hook === "buildStart") return runBuildStart();
  if (hook === "buildEnd" || hook === "renderStart" || hook === "closeBundle") {
    return runLifecycle(hook);
  }
  if (hook === "watchChange") return watchChange(args[0], args[1]);
  if (hook === "getEmittedFiles") {
    return JSON.stringify(emitted.map(({ fileName, source }) => ({ fileName, source })));
  }
  if (hook === "getPluginCss") return JSON.stringify(ojPluginCss);
  if (hook === "getWatchFiles") return JSON.stringify([...watchedFiles]);
  if (hook === "hasModuleParsed") return String(anyModuleParsed());
  if (hook === "replayModuleParsed") return replayModuleParsed(args[0]);
  if (hook === "hasGenerateBundle") {
    return String(pluginsWithHook("generateBundle").length > 0);
  }
  if (hook === "generateBundle") return generateBundle(args[0], args[1] === "true");
  if (hook === "hasRenderChunk") {
    return String(pluginsWithHook("renderChunk").length > 0);
  }
  if (hook === "renderChunk") return renderChunk(args[0], args[1]);
  if (hook === "hasWriteBundle") {
    return String(pluginsWithHook("writeBundle").length > 0);
  }
  if (hook === "writeBundle") return writeBundle(args[0], args[1] === "true");
  if (hook === "getPluginCount") return String(plugins.length);
  if (hook === "getEnvDelta") {
    // config() hooks ran at module init (top-level await), so the diff vs the
    // process-start snapshot is final by the time any stdio hook is answered.
    const delta = {};
    for (const [k, v] of Object.entries(process.env)) {
      if (ssrEnvBase[k] !== v) delta[k] = v;
    }
    return JSON.stringify(delta);
  }
  if (hook === "getDepTransformFilters") {
    // A transform's own `filter.code` include patterns. A dep is only worth
    // transforming when its source matches one of these, so oj gates dep
    // transforms on them (like Vite runs a transform only where its filter
    // matches). Transforms with no code filter are app-scoped here (they no-op
    // on deps), which keeps oj from an RPC per dependency module.
    const pats = [];
    for (const p of plugins) {
      const t = p && p.transform;
      const f = t && typeof t === "object" ? t.filter : null;
      let inc = f && f.code;
      if (inc && typeof inc === "object" && !(inc instanceof RegExp) && !Array.isArray(inc)) {
        inc = inc.include;
      }
      for (const r of Array.isArray(inc) ? inc : inc != null ? [inc] : []) {
        if (r instanceof RegExp) pats.push(r.source);
        else if (typeof r === "string") pats.push(r.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
      }
    }
    return JSON.stringify(pats);
  }
  if (hook === "getHasTransform") {
    return String(pluginsWithHook("transform").length > 0);
  }
  if (hook === "getHasLoad") {
    const has = plugins.some((p) => typeof hookHandler(p.load) === "function");
    return String(has);
  }
  if (hook === "getHmrHooks") {
    const watchChangeHook = pluginsWithHook("watchChange").length > 0;
    const hotUpdateHook = hotUpdatePlugins().length > 0;
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
