// SPDX-License-Identifier: MIT
// A minimal Vite plugin container: loads the app's vite.config plugins and runs
// their resolveId/load hooks so `virtual:*` (and other plugin-owned) specifiers
// resolve in the esbuild client bundle and the SSR loader. This is a FALLBACK
// for ids normal resolution can't handle -- not a full plugin pipeline. It
// faithfully applies the gating that decides whether a hook runs at all:
//   * plugin `apply` ("build" | "serve" | fn) vs the current command
//   * object-form hooks `{ handler, filter, order }` (Vite 6+/Rollup 4), whose
//     `filter.id` must match the id before the handler runs
// Getting these wrong lets a build-only, id-filtered stub swallow every id.
import { importPkg } from "./resolve-pkg.mjs";
import { existsSync } from "node:fs";
import { join } from "node:path";

// oj loads the config as tooling; Vite's native-loader "import without a file
// extension" advisories are noise here. Suppress them as the warning suggests.
process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";

const CONFIG_FILES = [
  "vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts",
  "oj.config.ts", "oj.config.js", "oj.config.mjs",
];

const hookHandler = (h) => (typeof h === "function" ? h : typeof h?.handler === "function" ? h.handler : null);
const hookFilter = (h) => (typeof h === "object" && h ? h.filter : undefined);

// Plugins whose transforms oj reimplements natively (so re-running them here
// would double-transform and clash): React (vite:react-*), everything under
// Vite's own `vite:` namespace (esbuild TS/JSX, glob, define), and TanStack
// Start/Router (server-fns, route code-splitting, manifests). Any other plugin
// is the app's own and its transforms DO need to run on first-party source.
const ojReimplemented = (name = "") =>
  name.startsWith("vite:") || /^tanstack[-:]/.test(name) || name.startsWith("@tanstack/");

// Vite 6+ per-environment gate: a plugin may declare `applyToEnvironment` to
// run only in "client" or "ssr". Ignoring it lets an SSR-only plugin (e.g. one
// that stubs client-only modules) run against the client bundle and corrupt it.
// Undefined = runs everywhere; a throwing gate defaults to "applies" (safe).
function envAllows(plugin, environment) {
  const f = plugin.applyToEnvironment;
  if (typeof f !== "function") return true;
  try { return f({ name: environment }) !== false; } catch { return true; }
}

// Vite/Rollup hook id filter: RegExp | string | {include,exclude} | array of.
// A string is treated as a substring match (close enough for id gating here).
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

// enforce: 'pre' plugins first, then normal, then 'post' (Vite's order).
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

// Load the app's plugins and return { resolveId, load } bound to one command +
// environment. Returns null (no container) if there is no config, no Vite, or
// the config fails to load -- callers then behave as before.
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

  // Vite re-exports Rollup's parser as `parseAst`; give plugins a real
  // `this.parse` (some, e.g. code-stubbing plugins, walk the AST and would emit
  // broken output against a `{}` stub). Fall back to `{}` if unavailable.
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

  // Run the plugins' transform hooks in order, chaining code through each
  // (Rollup/Vite semantics). Returns the transformed code, or null if no plugin
  // touched it. Used for files a plugin owns end-to-end, e.g. `.mdx`.
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

  // Run the app's OWN plugin transforms on first-party source (.ts/.tsx), so
  // plugin-generated code (i18n message accessors, macros, ...) exists in both
  // the SSR module and the client bundle. Deliberately SKIPS the plugins oj
  // reimplements natively -- React (JSX/Fast-Refresh) and TanStack Start/Router
  // (server-fns, route code-splitting, manifests) -- and Vite's own built-ins
  // (esbuild TS/JSX, glob, define). Running those here would double-transform
  // and clash with oj's pipeline (e.g. TanStack's server-fn transform fighting
  // oj's server-fn resolver). The `{ ssr }` option lets plugins branch per
  // environment the way Vite passes it. `try/catch` per plugin degrades safely.
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

  // Run the plugins' generateBundle hooks with a real emitFile, so plugins that
  // publish files (e.g. content-assets emitting /__content/<collection>/<file>)
  // produce their output. `emit` receives Rollup's emitFile arg
  // ({type,fileName,source}) and is expected to write the file. Plugins that
  // read the output `bundle` get an empty one (we don't reconstruct it), so
  // bundle-derived emissions are skipped; asset emissions from a manifest work.
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

// Pure gating helpers exposed for unit tests; not part of the runtime contract.
export const __test = { matchOne, idAllowed, applyMatches, ordered, hookHandler, hookFilter, ojReimplemented, envAllows };
