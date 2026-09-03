// SPDX-License-Identifier: MIT

import { importPkg } from "./resolve-pkg.mjs";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";

const CONFIG_FILES = [
  "vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts", "vite.config.cjs", "vite.config.cts",
  "oj.config.ts", "oj.config.js", "oj.config.mjs",
];

const hookHandler = (h) => (typeof h === "function" ? h : typeof h?.handler === "function" ? h.handler : null);
// Vite fails a module whose plugin hook throws, naming the plugin; the loader
// surfaces this as the request's error instead of silently skipping the plugin.
// Framework plugins oj reimplements (vite:*, tanstack*) run here without the
// lifecycle oj replaced (their generators are never initialized), so their
// failures stay skipped as before; only user plugins fail the module.
function pluginError(e, plugin, id) {
  const err = e instanceof Error ? e : new Error((e && e.message) || String(e));
  if (err.ojDecorated) return err;
  const loc = err.loc && typeof err.loc === "object" ? err.loc : null;
  const where = (loc && loc.file) || err.id || id || "";
  const line = loc ? loc.line : err.line;
  err.message = `[plugin:${plugin?.name || "unknown"}] ${err.message}` +
    (where ? `\n${where}${line != null ? `:${line}` : ""}` : "") +
    (typeof err.frame === "string" && err.frame ? `\n${err.frame}` : "");
  err.ojDecorated = true;
  return err;
}
const hookFilter = (h) => (typeof h === "object" && h ? h.filter : undefined);

const ojReimplemented = (name = "") =>
  name.startsWith("vite:") || /^tanstack[-:]/.test(name) || name.startsWith("@tanstack/");

// Vite's Environment passed to applyToEnvironment carries `config.consumer`
// ("client"/"server"); @vitejs/plugin-react reads it directly, so the bare
// `{ name }` oj used to pass threw and (in the client-bundle process, which has
// no rejection guard) killed the build. An async applyToEnvironment can't be
// awaited here, so a thenable is treated as "allowed".
function envConsumer(environment) {
  return environment === "client" ? "client" : "server";
}
function envAllows(plugin, environment) {
  const f = plugin.applyToEnvironment;
  if (typeof f !== "function") return true;
  const env = { name: environment, config: { consumer: envConsumer(environment) } };
  try {
    const r = f(env);
    if (r && typeof r.then === "function") return true;
    return r !== false;
  } catch {
    return true;
  }
}

// Vite's pluginFilter: a string id filter is a picomatch glob joined to cwd (unless
// it starts with `**` or is absolute); a RegExp is tested on the slash-normalized id.
const slash = (p) => p.replace(/\\/g, "/");
function globToRegExpSource(glob) {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        i++;
        if (glob[i + 1] === "/") { i++; re += "(?:.*/)?"; } else re += ".*";
      } else re += "[^/]*";
    } else if (c === "?") re += "[^/]";
    else if (c === "{") {
      const j = glob.indexOf("}", i);
      if (j > i) { re += "(?:" + glob.slice(i + 1, j).split(",").map(globToRegExpSource).join("|") + ")"; i = j; }
      else re += "\\{";
    } else if (c === "[") {
      const j = glob.indexOf("]", i);
      if (j > i) { let cls = glob.slice(i + 1, j); if (cls[0] === "!") cls = "^" + cls.slice(1); re += "[" + cls + "]"; i = j; }
      else re += "\\[";
    } else if (c === "\\" && i + 1 < glob.length) re += "\\" + glob[++i];
    else re += c.replace(/[.+^$()|]/g, "\\$&");
  }
  return re;
}
const idGlobCache = new Map();
function matchOne(pat, id) {
  if (pat instanceof RegExp) { const r = pat.test(slash(id)); pat.lastIndex = 0; return r; }
  if (typeof pat !== "string") return false;
  let re = idGlobCache.get(pat);
  if (!re) {
    const glob = pat.startsWith("**") || pat.startsWith("/") ? slash(pat) : slash(join(process.env.OJ_APP_ROOT ?? process.cwd(), pat));
    re = new RegExp("^" + globToRegExpSource(glob) + "$");
    idGlobCache.set(pat, re);
  }
  return re.test(slash(id));
}
function idAllowed(filter, id) {
  if (!filter) return true;
  const f = filter.id ?? filter;
  const inc = f && typeof f === "object" && !(f instanceof RegExp) && !Array.isArray(f) ? f.include : f;
  const exc = f && typeof f === "object" && !(f instanceof RegExp) && !Array.isArray(f) ? f.exclude : undefined;
  const asArr = (x) => (x == null ? [] : Array.isArray(x) ? x : [x]);
  const incs = asArr(inc), excs = asArr(exc);
  if (excs.some((p) => matchOne(p, id))) return false;
  if (incs.length === 0) return true;
  return incs.some((p) => matchOne(p, id));
}

// Vite's transform `filter.code`: a string is a substring, a RegExp is tested
// (stateful flags reset). Unlike `filter.id`, never a path glob.
function codeAllowed(filter, code) {
  const f = filter?.code;
  if (f == null) return true;
  const one = (pat) => {
    if (pat instanceof RegExp) { const r = pat.test(code); pat.lastIndex = 0; return r; }
    return typeof pat === "string" && code.includes(pat);
  };
  const plain = f instanceof RegExp || typeof f === "string" || Array.isArray(f);
  const asArr = (x) => (x == null ? [] : Array.isArray(x) ? x : [x]);
  const incs = asArr(plain ? f : f.include), excs = asArr(plain ? undefined : f.exclude);
  if (excs.some(one)) return false;
  return incs.length === 0 || incs.some(one);
}

// Vite's getSortedPluginHooks: a hook's own `order` ("pre" | "post") ranks it
// before or after the normal band, stable within a band (so `enforce` order holds).
function byHook(plugins, name) {
  const rank = (p) => { const h = p[name]; return h?.order === "pre" ? -1 : h?.order === "post" ? 1 : 0; };
  return [...plugins].sort((a, b) => rank(a) - rank(b));
}

function applyMatches(plugin, command, mode) {
  const a = plugin.apply;
  if (!a) return true;
  if (typeof a === "function") {
    try { return !!a({}, { command, mode }); } catch { return true; }
  }
  return a === command;
}

function ordered(plugins) {
  const pre = [], normal = [], post = [];
  for (const p of plugins) {
    if (p.enforce === "pre") pre.push(p);
    else if (p.enforce === "post") post.push(p);
    else normal.push(p);
  }
  return [...pre, ...normal, ...post];
}

const HERE = dirname(fileURLToPath(import.meta.url));

async function withBuildStartLock(fn) {
  const lock = join(HERE, "buildstart.lock");
  for (;;) {
    try { mkdirSync(lock); break; }
    catch {
      let pid = 0;
      try { pid = Number(readFileSync(join(lock, "holder"), "utf8")); } catch {}
      let alive = false;
      if (pid) { try { process.kill(pid, 0); alive = true; } catch {} }
      let expired = false;
      try { expired = Date.now() - statSync(lock).mtimeMs > 300_000; } catch {}
      if ((pid && !alive) || expired) { try { rmSync(lock, { recursive: true, force: true }); } catch {} }
      else await new Promise((r) => setTimeout(r, 50));
    }
  }
  try { writeFileSync(join(lock, "holder"), String(process.pid)); } catch {}
  try { return await fn(); }
  finally { try { rmSync(lock, { recursive: true, force: true }); } catch {} }
}

export function findConfig(app) {
  for (const f of CONFIG_FILES) {
    const p = join(app, f);
    if (existsSync(p)) return p;
  }
  return null;
}

export function createPluginContainer(vite, allPlugins, {
  command = "serve", mode = command === "build" ? "production" : "development", environment = "client", config = {},
} = {}) {
  const plugins = ordered(
    allPlugins.filter(
      (p) => (p.buildStart || p.resolveId || p.load || p.transform || p.moduleParsed || p.generateBundle)
        && applyMatches(p, command, mode),
    ),
  );

  const parse = typeof vite?.parseAst === "function"
    ? (code, opts) => vite.parseAst(code, opts)
    : () => ({});

  // Vite exposes this.environment.config with `consumer` ("client" | "server")
  // and `command` on every hook context; plugins branch on them (e.g. return an
  // empty dev manifest when consumer !== "server"). Without config, a plugin
  // reading `this.environment.config.consumer` throws, the hook is swallowed,
  // and oj serves an export-less stub — surfacing downstream as an undefined
  // import. Mirror Vite's shape so those plugins take their intended branch.
  const consumer = environment === "client" ? "client" : "server";
  const watchFiles = new Set();
  const moduleInfo = new Map();
  const resolvedConfig = {
    root: process.cwd(),
    base: "/",
    build: {},
    server: {},
    resolve: {},
    define: {},
    optimizeDeps: {},
    ssr: {},
    env: {},
    experimental: {},
    environments: { client: {}, ssr: {} },
    ...config,
    command,
    mode,
    plugins: allPlugins,
    isProduction: mode === "production",
  };
  const ctx = {
    environment: {
      name: environment,
      mode: command === "build" ? "build" : "dev",
      config: { ...resolvedConfig, consumer },
    },
    meta: { rollupVersion: "4.0.0", watchMode: command !== "build", framework: "oj" },
    warn() {}, info() {}, debug() {},
    error(m) { throw new Error(typeof m === "string" ? m : m?.message ?? String(m)); },
    emitFile() { return "oj-emit-ref"; },
    setAssetSource() {}, getFileName() { return ""; },
    addWatchFile(id) { watchFiles.add(String(id)); }, getWatchFiles() { return [...watchFiles]; },
    getModuleInfo(id) { return moduleInfo.get(id) ?? null; },
    getModuleIds() { return moduleInfo.keys(); },
    async resolve() { return null; },
    async load(options) {
      const id = typeof options === "string" ? options : options.id;
      const code = await load(id);
      return code == null ? null : { id, code };
    },
    parse,
  };

  let initialization;
  function initializePlugins() {
    initialization ??= (async () => {
      for (const plugin of byHook(plugins, "configResolved")) {
        if (ojReimplemented(plugin.name)) continue;
        const hook = hookHandler(plugin.configResolved);
        if (!hook) continue;
        try { await hook.call(ctx, resolvedConfig); }
        catch (error) {
          process.stderr.write(`oj: plugin "${plugin.name || "?"}" configResolved failed (skipped): ${error?.message ?? error}\n`);
        }
      }
    })();
    return initialization;
  }

  function pluginContext(plugin, base = ctx) {
    return Object.assign(Object.create(base), {
      async resolve(source, importer, options = {}) {
        const resolved = await resolveId(source, importer, options.skipSelf === false ? undefined : plugin);
        return resolved == null ? null : { id: resolved };
      },
    });
  }

  async function resolveId(id, importer, skippedPlugin) {
    await initializePlugins();
    for (const p of byHook(plugins, "resolveId")) {
      if (p === skippedPlugin) continue;
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.resolveId);
      if (!h || !idAllowed(hookFilter(p.resolveId), id)) continue;
      let r;
      try { r = await h.call(pluginContext(p), id, importer, { isEntry: false, ssr: environment === "ssr" }); } catch (e) { if (ojReimplemented(p.name)) continue; throw pluginError(e, p, importer || id); }
      if (r != null) return typeof r === "string" ? r : r.id;
    }
    return null;
  }

  async function load(id) {
    await initializePlugins();
    for (const p of byHook(plugins, "load")) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.load);
      if (!h || !idAllowed(hookFilter(p.load), id)) continue;
      let r;
      try { r = await h.call(pluginContext(p), id, { ssr: environment === "ssr" }); } catch (e) { if (ojReimplemented(p.name)) continue; throw pluginError(e, p, id); }
      if (r != null) {
        const code = typeof r === "string" ? r : r.code;
        moduleInfo.set(id, { id, code, importedIds: [], meta: {} });
        return code;
      }
    }
    return null;
  }

  async function moduleParsed(id, code) {
    const info = { id, code, importedIds: [], meta: {} };
    moduleInfo.set(id, info);
    for (const plugin of byHook(plugins, "moduleParsed")) {
      const handler = hookHandler(plugin.moduleParsed);
      if (handler && envAllows(plugin, environment)) await handler.call(ctx, info);
    }
  }

  async function transform(code, id) {
    await initializePlugins();
    moduleInfo.set(id, { id, code, importedIds: [], meta: {} });
    let current = code, changed = false;
    for (const p of byHook(plugins, "transform")) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.transform);
      const filter = hookFilter(p.transform);
      if (!h || !idAllowed(filter, id) || !codeAllowed(filter, current)) continue;
      let r;
      try { r = await h.call(pluginContext(p), current, id, { ssr: environment === "ssr" }); } catch (e) { if (ojReimplemented(p.name)) continue; throw pluginError(e, p, id); }
      const next = r == null ? null : typeof r === "string" ? r : r.code;
      if (next != null) { current = next; changed = true; }
    }
    await moduleParsed(id, current);
    return changed ? current : null;
  }

  async function transformUserCode(code, id) {
    await initializePlugins();
    moduleInfo.set(id, { id, code, importedIds: [], meta: {} });
    const ssr = environment === "ssr";
    let current = code, changed = false;
    for (const p of byHook(plugins, "transform")) {
      if (ojReimplemented(p.name) || !envAllows(p, environment)) continue;
      const h = hookHandler(p.transform);
      const filter = hookFilter(p.transform);
      if (!h || !idAllowed(filter, id) || !codeAllowed(filter, current)) continue;
      let r;
      try { r = await h.call(pluginContext(p), current, id, { ssr }); } catch (e) { throw pluginError(e, p, id); }
      const next = r == null ? null : typeof r === "string" ? r : r.code;
      if (next != null) { current = next; changed = true; }
    }
    await moduleParsed(id, current);
    return changed ? current : null;
  }

  async function generateBundle(emit) {
    await initializePlugins();
    const genCtx = { ...ctx, emitFile: (f) => (emit(f), "oj-emit-ref") };
    for (const p of byHook(plugins, "generateBundle")) {
      const h = hookHandler(p.generateBundle);
      if (!h || !envAllows(p, environment)) continue;
      try { await h.call(pluginContext(p, genCtx), { format: "es" }, {}, false); } catch {}
    }
  }

  // Vite runs buildStart once before any module loads; plugins that compile
  // sources (e.g. i18n message compilers) populate their state here and serve
  // it from load(). oj's SSR loader is a separate process with its own plugin
  // instances, so it must run buildStart itself. Scoped to non-reimplemented
  // (user) plugins, like transformUserCode, so framework plugins oj already
  // handles don't newly execute a hook they never ran under oj before.
  let buildStarted = false;
  async function buildStart() {
    if (buildStarted) return;
    await initializePlugins();
    if (buildStarted) return;
    buildStarted = true;
    await withBuildStartLock(async () => {
      for (const p of byHook(plugins, "buildStart")) {
        if (ojReimplemented(p.name) || !envAllows(p, environment)) continue;
        const h = hookHandler(p.buildStart);
        if (!h) continue;
        // Degrade gracefully: oj's plugin context is minimal (e.g. a stubbed
        // this.resolve), so some plugins' buildStart throw here though they'd
        // succeed under full Vite. Log and continue rather than abort the whole
        // dev server — a plugin that genuinely needed buildStart will surface as
        // its own load() output being wrong, which is strictly better than one
        // unsupported plugin taking down every other plugin's startup.
        try { await h.call(pluginContext(p), {}); }
        catch (e) {
          process.stderr.write(`oj: plugin "${p.name || "?"}" buildStart failed (skipped): ${(e && e.message) || e}\n`);
        }
      }
    });
  }

  return { resolveId, load, transform, transformUserCode, buildStart, generateBundle, pluginCount: plugins.length, watchFiles };
}

function mergeConfigValues(current, update) {
  if (Array.isArray(current) && Array.isArray(update)) return [...current, ...update];
  if (current && update && typeof current === "object" && typeof update === "object") {
    const merged = { ...current };
    for (const key of Object.keys(update)) {
      merged[key] = key in current ? mergeConfigValues(current[key], update[key]) : update[key];
    }
    return merged;
  }
  return update === undefined ? current : update;
}

export async function loadPluginContainer(app, opts = {}) {
  // Vite's default mode follows the command (config hooks see it as env.mode).
  const { command = "serve", mode = command === "build" ? "production" : "development" } = opts;
  const configFile = findConfig(app);
  if (!configFile) return null;
  let vite;
  try { vite = await importPkg(app, "vite", ["@tanstack/react-start"]); } catch { return null; }
  if (typeof vite?.loadConfigFromFile !== "function") return null;
  let loaded;
  try {
    loaded = await vite.loadConfigFromFile({ command, mode }, configFile, app);
  } catch {
    return null;
  }
  // Vite runs every plugin's `config` hook (in order, async, gated by `apply`)
  // and merges the returned partials before resolving; the Start loader has its
  // own plugin instances, so it must do the same for them.
  let config = loaded?.config ?? {};
  const all = (config.plugins ?? []).flat(Infinity).filter(Boolean);
  // Only user plugins: the framework plugins oj reimplements (TanStack, Vite's
  // own, React) never ran hooks under oj, and their config hooks have side
  // effects (the router plugin starts its generator) that fight oj's own.
  for (const plugin of all) {
    if (ojReimplemented(plugin.name)) continue;
    const handler = hookHandler(plugin.config);
    if (!handler || !applyMatches(plugin, command, mode)) continue;
    try {
      const partial = await handler.call(plugin, config, { command, mode });
      if (partial) config = typeof vite.mergeConfig === "function"
        ? vite.mergeConfig(config, partial)
        : mergeConfigValues(config, partial);
    } catch (e) {
      process.stderr.write(`oj: plugin "${plugin.name || "?"}" config hook failed (skipped): ${e?.message ?? e}\n`);
    }
  }
  if (loaded) loaded.config = config;
  const container = createPluginContainer(vite, all, {
    ...opts,
    config: { ...config, root: config.root ?? app },
  });
  const publicDir = config.publicDir === false
    ? false
    : typeof config.publicDir === "string" ? config.publicDir : null;
  const configDependencies = [configFile, ...(loaded?.dependencies ?? [])];
  return { ...container, publicDir, configDependencies };
}

export const __test = { matchOne, idAllowed, codeAllowed, byHook, applyMatches, ordered, hookHandler, hookFilter, ojReimplemented, envAllows };
