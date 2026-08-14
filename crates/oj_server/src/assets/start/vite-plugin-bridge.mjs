// SPDX-License-Identifier: MIT

import { importPkg } from "./resolve-pkg.mjs";
import { existsSync } from "node:fs";
import { join } from "node:path";

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";

const CONFIG_FILES = [
  "vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts",
  "oj.config.ts", "oj.config.js", "oj.config.mjs",
];

const hookHandler = (h) => (typeof h === "function" ? h : typeof h?.handler === "function" ? h.handler : null);
const hookFilter = (h) => (typeof h === "object" && h ? h.filter : undefined);

const ojReimplemented = (name = "") =>
  name.startsWith("vite:") || /^tanstack[-:]/.test(name) || name.startsWith("@tanstack/");

function envAllows(plugin, environment) {
  const f = plugin.applyToEnvironment;
  if (typeof f !== "function") return true;
  try { return f({ name: environment }) !== false; } catch { return true; }
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

function findConfig(app) {
  for (const f of CONFIG_FILES) {
    const p = join(app, f);
    if (existsSync(p)) return p;
  }
  return null;
}

export async function loadPluginContainer(app, { command = "serve", mode = "development", environment = "client" } = {}) {
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
  const plugins = ordered(
    all.filter(
      (p) => (p.resolveId || p.load || p.transform || p.generateBundle) && applyMatches(p, command, mode),
    ),
  );

  const parse = typeof vite.parseAst === "function"
    ? (code, opts) => vite.parseAst(code, opts)
    : () => ({});

  const ctx = {
    environment: { name: environment, mode: command === "build" ? "build" : "dev" },
    meta: { rollupVersion: "4.0.0", watchMode: command !== "build", framework: "oj" },
    warn() {}, info() {}, debug() {},
    error(m) { throw new Error(typeof m === "string" ? m : m?.message ?? String(m)); },
    emitFile() { return "oj-emit-ref"; },
    setAssetSource() {}, getFileName() { return ""; },
    addWatchFile() {}, getWatchFiles() { return []; },
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

  const publicDir = typeof loaded?.config?.publicDir === "string" ? loaded.config.publicDir : null;
  return { resolveId, load, transform, transformUserCode, generateBundle, publicDir, pluginCount: plugins.length };
}

export const __test = { matchOne, idAllowed, applyMatches, ordered, hookHandler, hookFilter, ojReimplemented, envAllows };
