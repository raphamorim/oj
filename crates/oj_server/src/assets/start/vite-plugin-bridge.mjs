// SPDX-License-Identifier: MIT

import { importPkg } from "./resolve-pkg.mjs";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";

const CONFIG_FILES = [
  "vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts",
  "oj.config.ts", "oj.config.js", "oj.config.mjs",
];

const hookHandler = (h) => (typeof h === "function" ? h : typeof h?.handler === "function" ? h.handler : null);
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

function matchOne(pat, id) {
  if (pat instanceof RegExp) return pat.test(id);
  if (typeof pat === "string") return id.includes(pat);
  return false;
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

export function createPluginContainer(vite, allPlugins, { command = "serve", mode = "development", environment = "client" } = {}) {
  const plugins = ordered(
    allPlugins.filter(
      (p) => (p.resolveId || p.load || p.transform || p.renderStart || p.generateBundle)
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
  const ctx = {
    environment: {
      name: environment,
      mode: command === "build" ? "build" : "dev",
      config: { command, consumer, mode: command === "build" ? "production" : "development" },
    },
    meta: { rollupVersion: "4.0.0", watchMode: command !== "build", framework: "oj" },
    warn() {}, info() {}, debug() {},
    error(m) { throw new Error(typeof m === "string" ? m : m?.message ?? String(m)); },
    emitFile() { return "oj-emit-ref"; },
    setAssetSource() {}, getFileName() { return ""; },
    addWatchFile(id) { watchFiles.add(String(id)); }, getWatchFiles() { return [...watchFiles]; },
    getModuleInfo() { return null; }, getModuleIds() { return [][Symbol.iterator](); },
    async resolve() { return null; }, async load() { return null; },
    parse,
  };

  async function resolveId(id, importer) {
    for (const p of plugins) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.resolveId);
      if (!h || !idAllowed(hookFilter(p.resolveId), id)) continue;
      let r;
      try { r = await h.call(ctx, id, importer, { isEntry: false }); } catch { continue; }
      if (r != null) return typeof r === "string" ? r : r.id;
    }
    return null;
  }

  async function load(id) {
    for (const p of plugins) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.load);
      if (!h || !idAllowed(hookFilter(p.load), id)) continue;
      let r;
      try { r = await h.call(ctx, id); } catch { continue; }
      if (r != null) return typeof r === "string" ? r : r.code;
    }
    return null;
  }

  async function transform(code, id) {
    let current = code, changed = false;
    for (const p of plugins) {
      if (!envAllows(p, environment)) continue;
      const h = hookHandler(p.transform);
      if (!h || !idAllowed(hookFilter(p.transform), id)) continue;
      let r;
      try { r = await h.call(ctx, current, id); } catch { continue; }
      const next = r == null ? null : typeof r === "string" ? r : r.code;
      if (next != null) { current = next; changed = true; }
    }
    return changed ? current : null;
  }

  async function transformUserCode(code, id) {
    const ssr = environment === "ssr";
    let current = code, changed = false;
    for (const p of plugins) {
      if (ojReimplemented(p.name) || !envAllows(p, environment)) continue;
      const h = hookHandler(p.transform);
      if (!h || !idAllowed(hookFilter(p.transform), id)) continue;
      let r;
      try { r = await h.call(ctx, current, id, { ssr }); } catch { continue; }
      const next = r == null ? null : typeof r === "string" ? r : r.code;
      if (next != null) { current = next; changed = true; }
    }
    return changed ? current : null;
  }

  async function generateBundle(emit) {
    const genCtx = { ...ctx, emitFile: (f) => (emit(f), "oj-emit-ref") };
    for (const p of plugins) {
      const h = hookHandler(p.generateBundle);
      if (!h) continue;
      const ae = p.applyToEnvironment;
      if (typeof ae === "function") {
        let ok;
        try { ok = ae({ name: environment }); } catch { ok = true; }
        if (ok === false) continue;
      }
      try { await h.call(genCtx, { format: "es" }, {}, false); } catch {}
    }
  }

  async function renderStart(outputOptions, inputOptions) {
    for (const plugin of plugins) {
      if (!envAllows(plugin, environment)) continue;
      const hook = hookHandler(plugin.renderStart);
      if (!hook) continue;
      try { await hook.call(ctx, outputOptions, inputOptions); } catch {}
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
    buildStarted = true;
    await withBuildStartLock(async () => {
      for (const p of plugins) {
        if (ojReimplemented(p.name) || !envAllows(p, environment)) continue;
        const h = hookHandler(p.buildStart);
        if (!h) continue;
        // Degrade gracefully: oj's plugin context is minimal (e.g. a stubbed
        // this.resolve), so some plugins' buildStart throw here though they'd
        // succeed under full Vite. Log and continue rather than abort the whole
        // dev server — a plugin that genuinely needed buildStart will surface as
        // its own load() output being wrong, which is strictly better than one
        // unsupported plugin taking down every other plugin's startup.
        try { await h.call(ctx, {}); }
        catch (e) {
          process.stderr.write(`oj: plugin "${p.name || "?"}" buildStart failed (skipped): ${(e && e.message) || e}\n`);
        }
      }
    });
  }

  return {
    resolveId, load, transform, transformUserCode, buildStart, renderStart, generateBundle,
    pluginCount: plugins.length, watchFiles,
  };
}

export async function loadPluginContainer(app, opts = {}) {
  const { command = "serve", mode = "development" } = opts;
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
  const all = (loaded?.config?.plugins ?? []).flat(Infinity).filter(Boolean);
  const container = createPluginContainer(vite, all, opts);
  const publicDir = typeof loaded?.config?.publicDir === "string" ? loaded.config.publicDir : null;
  const configDependencies = [configFile, ...(loaded?.dependencies ?? [])];
  return { ...container, publicDir, configDependencies };
}

export const __test = { matchOne, idAllowed, applyMatches, ordered, hookHandler, hookFilter, ojReimplemented, envAllows };
