// SPDX-License-Identifier: MIT

import { pathToFileURL, fileURLToPath } from "node:url";
import { readFileSync, unlinkSync, statSync } from "node:fs";
import { writeFile, appendFile, rename, mkdir } from "node:fs/promises";
import { createHash } from "node:crypto";
import { resolve as pathResolve, dirname } from "node:path";
import { importPkg, viteEnvDefine, emptyVirtualStub } from "./resolve-pkg.mjs";
import { loadPluginContainerSync } from "./container-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";
import {
  EXTS, isFile, JS_TO_TS, probe, RESERVED, nearestPkgType,
  hasEsmSyntax, isCjsFile, cjsFacade, stripJsonc, readJsonc, rewriteServerFns, substituteAlias,
  parseImportsField, mergeTsConfig,
} from "./loader-util.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const { transformSync } = await importPkg(APP, "rolldown/experimental", ["vite", "@tanstack/react-start"]);
// The plugin container (config eval + buildStart + plugin hooks) lives in a
// worker behind a sync bridge: registerHooks hooks cannot await, and hosting
// it off-thread keeps its bootstrap out of the module-loading critical path.
const container = loadPluginContainerSync(APP, { command: "serve", environment: "ssr" });
const VIRTUAL_SCHEME = "ojvirtual:///";
const CACHE_DIR = pathResolve(APP, ".oj-cache");

const BASE_FILES = [
  "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lockb", "package.json",
  "vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts",
  "oj.config.ts", "oj.config.js", "oj.config.mjs",
];
const LOADER_FILES = ["loader.mjs", "loader-util.mjs", "glob-transform.mjs", "vite-plugin-bridge.mjs", "resolve-pkg.mjs"];
const hashAdd = (h, label, bytes) => { h.update(label); h.update("\0"); h.update(bytes); h.update("\0"); };
function hashBaseInputs(h) {
  hashAdd(h, "node", process.version);
  for (const name of BASE_FILES) {
    try { hashAdd(h, name, readFileSync(pathResolve(APP, name))); } catch {}
  }
  for (const name of LOADER_FILES) {
    try { hashAdd(h, name, readFileSync(pathResolve(HERE, name))); } catch {}
  }
}

// Config evaluation mutates process.env (e.g. VITE_* vars derived from git
// state or .env files) and those values feed the import.meta.env inlining
// below — but the eval now happens in the container worker's env copy. The
// worker reports the delta; to keep warm boots from blocking on config eval,
// the delta is persisted keyed by everything that can change it (config,
// lockfile, loader assets, .env files, git HEAD). A key change means a boot
// that recompiles anyway, so blocking on the worker there costs little.
// Compromise vs the in-worker design: env side effects that are
// nondeterministic for identical inputs are replayed, not recomputed.
function gitStateBytes() {
  try {
    let dir = APP;
    for (let i = 0; i < 12; i++) {
      const dotGit = pathResolve(dir, ".git");
      let gitDir = null;
      try {
        if (statSync(dotGit).isDirectory()) gitDir = dotGit;
        else {
          const m = readFileSync(dotGit, "utf8").match(/^gitdir:\s*(.+)$/m);
          if (m) gitDir = pathResolve(dir, m[1].trim());
        }
      } catch {}
      if (gitDir) {
        const head = readFileSync(pathResolve(gitDir, "HEAD"), "utf8");
        const ref = head.match(/^ref:\s*(.+)$/m);
        let tip = "";
        if (ref) {
          const refPath = ref[1].trim();
          for (const base of [gitDir, (() => { try { return pathResolve(gitDir, readFileSync(pathResolve(gitDir, "commondir"), "utf8").trim()); } catch { return null; } })()]) {
            if (!base) continue;
            try { tip = readFileSync(pathResolve(base, refPath), "utf8"); break; } catch {}
            try { tip = readFileSync(pathResolve(base, "packed-refs"), "utf8"); break; } catch {}
          }
        }
        return head + "\0" + tip;
      }
      const parent = dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
  } catch {}
  return "";
}
const ENV_DELTA_FILE = pathResolve(CACHE_DIR, "ssr-env.json");
const envDelta = (() => {
  if (!container) return {};
  let deltaKey = null;
  if (process.env.OJ_SSR_LOADER_CACHE !== "off") {
    try {
      const h = createHash("sha256");
      hashAdd(h, "v", "oj-ssr-env-delta-v1");
      hashBaseInputs(h);
      for (const name of [".env", ".env.local", ".env.development", ".env.development.local"]) {
        try { hashAdd(h, name, readFileSync(pathResolve(APP, name))); } catch {}
      }
      hashAdd(h, "git", gitStateBytes());
      deltaKey = h.digest("hex");
    } catch {}
  }
  if (deltaKey) {
    try {
      const j = JSON.parse(readFileSync(ENV_DELTA_FILE, "utf8"));
      if (j.key === deltaKey && j.delta && typeof j.delta === "object") return j.delta;
    } catch {}
  }
  let fresh = {};
  try { fresh = container.env() ?? {}; } catch {}
  if (deltaKey) {
    (async () => {
      try {
        await mkdir(CACHE_DIR, { recursive: true });
        await writeFile(ENV_DELTA_FILE, JSON.stringify({ key: deltaKey, delta: fresh }));
      } catch {}
    })();
  }
  return fresh;
})();
const DEFINE = viteEnvDefine({ ssr: true, env: { ...process.env, ...envDelta } });

// Persistent per-module result cache. Same on-disk layout as oj_cache's
// PersistentCache (.oj-cache/<2hex>/<key>.json, corrupt entries self-delete)
// so one prune covers both. Keys are salted with an epoch hash of everything
// non-content that can change a transform result: lockfile, vite/oj config,
// the loader assets themselves (which change with the oj version), and the
// define/env inputs fed to transformSync. Any error degrades to a miss.
const cacheStats = { hits: 0, misses: 0, uncached: 0, rhits: 0, rmisses: 0 };
const EPOCH = (() => {
  if (process.env.OJ_SSR_LOADER_CACHE === "off") return null;
  try {
    const h = createHash("sha256");
    hashAdd(h, "v", "oj-ssr-loader-cache-v1");
    hashBaseInputs(h);
    hashAdd(h, "define", JSON.stringify(DEFINE));
    hashAdd(h, "fnbase", process.env.TSS_SERVER_FN_BASE ?? "");
    return h.digest("hex");
  } catch {
    return null;
  }
})();
function cacheKey(mode, path, source) {
  if (!EPOCH) return null;
  try {
    const h = createHash("sha256");
    for (const part of [EPOCH, mode, path, source]) { h.update(part); h.update("\0"); }
    return h.digest("hex");
  } catch {
    return null;
  }
}
const cachePath = (key) => pathResolve(CACHE_DIR, key.slice(0, 2), `${key}.json`);

// Load pack: an in-memory index over the per-entry load cache, persisted as
// one JSONL file (header line carries the epoch) and read once at init. A
// warm boot serves its ~6k load hits from this Map instead of ~6k per-file
// open/read/parse round trips. The per-entry cache stays authoritative: a key
// missing here falls back to the per-file path below, and such hits are
// re-appended so the pack converges opportunistically.
const LOAD_PACK_FILE = pathResolve(CACHE_DIR, "ssr-load-pack.jsonl");
const loadPack = new Map();
let loadPackFileReady = false;
if (EPOCH) {
  try {
    const lines = readFileSync(LOAD_PACK_FILE, "utf8").split("\n");
    if (JSON.parse(lines[0]).epoch === EPOCH) {
      loadPackFileReady = true;
      for (let i = 1; i < lines.length; i++) {
        if (!lines[i]) continue;
        try { const e = JSON.parse(lines[i]); loadPack.set(e.k, e.c); } catch {}
      }
    }
  } catch {}
}
let packPending = [];
let packFlushChain = Promise.resolve();
function flushLoadPack() {
  const batch = packPending;
  packPending = [];
  if (batch.length === 0) return;
  const body = batch.map((e) => JSON.stringify(e)).join("\n") + "\n";
  const first = !loadPackFileReady;
  loadPackFileReady = true;
  packFlushChain = packFlushChain.then(async () => {
    try {
      await mkdir(CACHE_DIR, { recursive: true });
      if (first) await writeFile(LOAD_PACK_FILE, JSON.stringify({ epoch: EPOCH }) + "\n" + body);
      else await appendFile(LOAD_PACK_FILE, body);
    } catch {}
  });
}
function rememberPack(key, code) {
  if (!EPOCH || loadPack.has(key)) return;
  loadPack.set(key, code);
  packPending.push({ k: key, c: code });
}

function cacheGet(key) {
  if (!key) return null;
  const packed = loadPack.get(key);
  if (packed !== undefined) { cacheStats.hits += 1; return packed; }
  const file = cachePath(key);
  let bytes;
  try { bytes = readFileSync(file, "utf8"); } catch { return null; }
  try {
    const entry = JSON.parse(bytes);
    if (typeof entry.code === "string") {
      cacheStats.hits += 1;
      rememberPack(key, entry.code);
      return entry.code;
    }
  } catch {}
  try { unlinkSync(file); } catch {}
  return null;
}
// Writes are queued and drained off the load path: a sync write per miss
// would serialize thousands of fs calls into first-boot module loading.
const writeQueue = [];
let draining = false;
async function drainWrites() {
  if (draining) return;
  draining = true;
  while (writeQueue.length > 0) {
    const batch = writeQueue.splice(0, 64);
    await Promise.all(batch.map(async ({ file, body }) => {
      try {
        await mkdir(dirname(file), { recursive: true });
        const tmp = file.replace(/\.json$/, ".tmp");
        await writeFile(tmp, body);
        await rename(tmp, file);
      } catch {}
    }));
  }
  draining = false;
}
function cachePut(key, code) {
  if (!key) { cacheStats.uncached += 1; return; }
  cacheStats.misses += 1;
  try {
    rememberPack(key, code);
    writeQueue.push({
      file: cachePath(key),
      body: JSON.stringify({ code, map_data_url: null, imports: [], is_boundary: false, kind: "ssr-loader" }),
    });
    setTimeout(drainWrites, 0);
  } catch {}
}
// Persistent resolution cache: (parentURL, specifier) -> resolved URL. The
// resolve hook's fs probing and Node's own fallback resolution dominate the
// hooks-thread time, so hits replace them with one existence check. Stored as
// one JSONL pack (header line carries the epoch) and loaded into a Map at
// init; entries whose file: target disappeared degrade to a miss. Accepted
// staleness edge: a hit only re-checks that the target still exists, so a
// newly added file that should shadow an old resolution (e.g. a new
// tsconfig-path match) lands on the next epoch change, not instantly.
const RESOLVE_CACHE_FILE = pathResolve(CACHE_DIR, "ssr-resolve.jsonl");
const resolveCache = new Map();
let resolveFileReady = false;
if (EPOCH) {
  try {
    const lines = readFileSync(RESOLVE_CACHE_FILE, "utf8").split("\n");
    if (JSON.parse(lines[0]).epoch === EPOCH) {
      resolveFileReady = true;
      for (let i = 1; i < lines.length; i++) {
        if (!lines[i]) continue;
        try { const e = JSON.parse(lines[i]); resolveCache.set(e.k, e); } catch {}
      }
    }
  } catch {}
}
let resolvePending = [];
let resolveFlushTimer = null;
async function flushResolveCache() {
  resolveFlushTimer = null;
  const batch = resolvePending;
  resolvePending = [];
  if (batch.length === 0) return;
  const body = batch.map((e) => JSON.stringify(e)).join("\n") + "\n";
  try {
    await mkdir(CACHE_DIR, { recursive: true });
    if (resolveFileReady) {
      await appendFile(RESOLVE_CACHE_FILE, body);
    } else {
      await writeFile(RESOLVE_CACHE_FILE, JSON.stringify({ epoch: EPOCH }) + "\n" + body);
      resolveFileReady = true;
    }
  } catch {}
}
function rememberResolve(key, entry) {
  if (!EPOCH || resolveCache.has(key)) return;
  entry.k = key;
  resolveCache.set(key, entry);
  resolvePending.push(entry);
  if (!resolveFlushTimer) resolveFlushTimer = setTimeout(flushResolveCache, 250);
}

// With synchronous in-thread hooks the event loop never turns during a boot
// or an SSR render, so the setTimeout-driven drains above starve until the
// process idles — and a dev server killed shortly after boot would persist
// almost nothing. The runner calls this at ready and after each response.
export function flushCaches() {
  if (writeQueue.length > 0) drainWrites();
  if (packPending.length > 0) flushLoadPack();
  if (resolvePending.length > 0) flushResolveCache();
}

function reportCacheStats() {
  if (!EPOCH) return;
  process.stderr.write(
    `oj: ssr loader cache: load ${cacheStats.hits}/${cacheStats.hits + cacheStats.misses} hits (${cacheStats.uncached} uncacheable), resolve ${cacheStats.rhits}/${cacheStats.rhits + cacheStats.rmisses} hits\n`,
  );
}

let V = 0;
export function setVersion(v) { V = v; }
export { reportCacheStats };
const stripQ = (u) => u.split("?")[0];
const withV = (u) => (V ? `${stripQ(u)}?ojv=${V}` : stripQ(u));
const isTanstack = (u) => /\/@tanstack\//.test(u) && /\.(js|mjs)$/.test(stripQ(u));
const ASSET_SUFFIX = /\?(raw|url|inline)$/;
const ASSET_EXT = /\.(png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf|eot|mp4|webm|wasm)$/;

const ALIASES = {
  "#tanstack-router-entry": pathResolve(APP, "src/router"),
  "#tanstack-start-entry": pathResolve(HERE, "start-entry.ts"),
  "#tanstack-start-plugin-adapters": pathResolve(HERE, "plugin-adapters.ts"),
  "#tanstack-start-server-fn-resolver": pathResolve(HERE, "server-fn-resolver.mjs"),
  // Lazy manifest: reads css-urls.json per call, so the SSR runner can boot
  // before bundle-client.mjs has collected the CSS URLs.
  "tanstack-start-manifest:v": pathResolve(HERE, "manifest-dev.ts"),
  "@cloudflare/vite-plugin/server": pathResolve(HERE, "cf-server.mjs"),
};

const IMPORT_RULES = (() => {
  try {
    return parseImportsField(JSON.parse(readFileSync(pathResolve(APP, "package.json"), "utf8")).imports ?? {});
  } catch {
    return [];
  }
})();
function resolveImports(spec) {
  for (const [pattern, target] of IMPORT_RULES) {
    const sub = substituteAlias(pattern, target, spec);
    if (sub == null) continue;
    const hit = probe(pathResolve(APP, sub));
    if (hit) return hit;
  }
  return null;
}

const TS = (() => {
  let file = pathResolve(APP, "tsconfig.json");
  const chain = [];
  for (let guard = 0; file && guard < 6; guard++) {
    const cfg = readJsonc(file);
    if (!cfg) break;
    chain.unshift({ cfg, dir: dirname(file) });
    file = typeof cfg.extends === "string" && cfg.extends.startsWith(".")
      ? pathResolve(dirname(file), cfg.extends.endsWith(".json") ? cfg.extends : cfg.extends + ".json")
      : null;
  }
  return mergeTsConfig(chain, APP);
})();
function resolveTsPaths(spec) {
  for (const [pattern, targets] of TS.rules) {
    for (const t of targets) {
      const sub = substituteAlias(pattern, t, spec);
      if (sub == null) continue;
      const hit = probe(pathResolve(TS.baseDir, sub));
      if (hit) return hit;
    }
  }
  return null;
}

// Hooks are registered with module.registerHooks(), so they run synchronously
// in-thread — and, unlike module.register(), they also see require() traffic.
// Bail out of require operations entirely: the async-hooks design never
// customized them, and the cjsFacade sources require() the module they front
// (customizing that would recurse).
const isRequire = (context) => context.conditions && context.conditions.includes("require");

export function resolve(spec, context, next) {
  if (isRequire(context)) return next(spec, context);
  if (context.parentURL) context = { ...context, parentURL: stripQ(context.parentURL) };
  const key = (context.parentURL ?? "") + "\0" + spec;
  const rc = EPOCH ? resolveCache.get(key) : undefined;
  if (rc !== undefined) {
    try {
      const cleanUrl = stripQ(rc.u);
      if (!cleanUrl.startsWith("file:") || isFile(fileURLToPath(cleanUrl))) {
        cacheStats.rhits += 1;
        const out = { url: rc.v ? withV(rc.u) : rc.u, shortCircuit: true };
        if (rc.f) out.format = rc.f;
        return out;
      }
    } catch {}
    resolveCache.delete(key);
  }
  const r = resolveUncached(spec, context, next);
  if (EPOCH && r && r.url && !r.url.startsWith(VIRTUAL_SCHEME)) {
    cacheStats.rmisses += 1;
    const versioned = r._ojv === true;
    rememberResolve(key, { u: versioned ? stripQ(r.url) : r.url, v: versioned ? 1 : 0, f: r.format ?? null });
  }
  if (r && r._ojv) delete r._ojv;
  return r;
}

function resolveUncached(spec, context, next) {
  if (container && (spec.startsWith("virtual:") || spec.startsWith("\0"))) {
    const importer = context.parentURL && context.parentURL.startsWith("file:")
      ? fileURLToPath(context.parentURL)
      : undefined;
    const rid = container.resolveId(spec, importer);
    if (rid != null) return { url: VIRTUAL_SCHEME + encodeURIComponent(rid), shortCircuit: true };
  }
  const suffix = spec.match(ASSET_SUFFIX);
  const isCss = !spec.includes("?") && /\.css$/.test(spec);
  const isAsset = !spec.includes("?") && ASSET_EXT.test(spec);
  if (suffix || isCss || isAsset) {
    const kind = suffix ? suffix[1] : isCss ? "css" : "url";
    const clean = spec.replace(ASSET_SUFFIX, "");
    let abs = null;
    if (clean.startsWith(".") && context.parentURL) {
      abs = probe(pathResolve(dirname(fileURLToPath(context.parentURL)), clean));
    } else if (ALIASES[clean]) {
      abs = probe(ALIASES[clean]);
    }
    if (!abs && clean.startsWith("#")) abs = resolveImports(clean);
    if (!abs) {
      try { abs = fileURLToPath(stripQ(next(clean, context).url)); } catch {}
    }
    if (abs) return { url: pathToFileURL(abs).href + `?ojasset=${kind}`, shortCircuit: true };
  }
  if (/\.svg\?react$/.test(spec)) {
    const clean = spec.replace(/\?react$/, "");
    let abs = null;
    if (clean.startsWith(".") && context.parentURL) {
      abs = probe(pathResolve(dirname(fileURLToPath(context.parentURL)), clean));
    } else if (clean.startsWith("#")) {
      abs = resolveImports(clean);
    } else if (!clean.startsWith("/")) {
      abs = resolveTsPaths(clean);
    }
    if (!abs) {
      try { abs = fileURLToPath(stripQ(next(clean, context).url)); } catch {}
    }
    if (abs) return { url: pathToFileURL(abs).href + "?ojsvg=react", shortCircuit: true };
  }
  if (ALIASES[spec]) {
    const hit = probe(ALIASES[spec]);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true, _ojv: true };
  }
  if (spec.startsWith("#")) {
    const hit = resolveImports(spec);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true, _ojv: true };
  }
  if (!spec.startsWith(".") && !spec.startsWith("/")) {
    const hit = resolveTsPaths(spec);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true, _ojv: true };
  }
  if (spec.startsWith(".") && context.parentURL) {
    const base = pathResolve(dirname(fileURLToPath(context.parentURL)), spec);
    const hit = probe(base);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true, _ojv: true };
  }
  let r;
  try {
    r = next(spec, context);
  } catch (err) {
    if (err && err.code === "ERR_MODULE_NOT_FOUND") {
      const tried = err.url && err.url.startsWith("file:")
        ? fileURLToPath(err.url)
        : (err.message.match(/Cannot find module '([^']+)'/) || [])[1];
      const hit = tried ? probe(tried) : null;
      if (hit) return { url: pathToFileURL(hit).href, shortCircuit: true };
    }
    throw err;
  }
  if (r && r.url && isTanstack(r.url)) return { ...r, url: withV(r.url), shortCircuit: true, _ojv: true };
  return r;
}

function transformServerFns(code, path) {
  const rel = path.startsWith(APP) ? path.slice(APP.length).replace(/^\//, "") : path;
  return rewriteServerFns(code, rel);
}

export function load(url, context, next) {
  if (isRequire(context)) return next(url, context);
  if (url.startsWith(VIRTUAL_SCHEME)) {
    const rid = decodeURIComponent(url.slice(VIRTUAL_SCHEME.length));
    const code = container ? container.load(rid) : null;
    return { format: "module", source: code ?? emptyVirtualStub(APP, rid), shortCircuit: true };
  }
  const clean = stripQ(url);
  const kind = /[?&]ojasset=(\w+)/.exec(url)?.[1];
  if (kind) {
    const path = fileURLToPath(clean);
    let src = "export default {};";
    if (kind === "raw") src = `export default ${JSON.stringify(readFileSync(path, "utf8"))};`;
    else if (kind === "url") src = `export default ${JSON.stringify("/@oj-start/fs" + path)};`;
    else if (kind === "inline")
      src = `export default ${JSON.stringify("data:application/octet-stream;base64," + readFileSync(path).toString("base64"))};`;
    return { format: "module", source: src, shortCircuit: true };
  }
  if (clean.endsWith(".json")) {
    return { format: "module", source: `export default ${readFileSync(fileURLToPath(clean), "utf8")};`, shortCircuit: true };
  }
  if (clean.endsWith(".tsx") || clean.endsWith(".ts")) {
    const path = fileURLToPath(clean);
    const userFile = container && !path.includes("/node_modules/");
    // Try the on-disk bytes first: files no plugin load() claims (the vast
    // majority) are stored under their disk-content key, so a hit skips the
    // container.load round trip entirely. Plugin-claimed files never land in
    // that namespace (their raw differs from disk), so a stale plugin result
    // can't be served from a disk key.
    let diskRaw = null;
    if (userFile) {
      try { diskRaw = readFileSync(path, "utf8"); } catch {}
      if (diskRaw != null) {
        const diskHit = cacheGet(cacheKey("ssr-loader", path, diskRaw));
        if (diskHit != null) return { format: "module", source: diskHit, shortCircuit: true };
      }
    }
    // A plugin load() overrides the on-disk file (Vite: load runs before fs read).
    // Its result can differ from the disk bytes, so it is what the key hashes.
    let raw = userFile ? container.load(path) : null;
    if (raw == null) raw = diskRaw ?? readFileSync(path, "utf8");
    const key = cacheKey("ssr-loader", path, raw);
    if (raw !== diskRaw) {
      const hit = cacheGet(key);
      if (hit != null) return { format: "module", source: hit, shortCircuit: true };
    }
    if (userFile) {
      const t = container.transformUserCode(raw, path);
      if (t != null) raw = t;
    }
    const src = transformServerFns(transformGlob(raw, path), path);
    const out = transformSync(path, src, {
      lang: clean.endsWith("tsx") ? "tsx" : "ts",
      jsx: { runtime: "automatic" },
      define: DEFINE,
    });
    // Glob expansion depends on directory contents, not source bytes: a file
    // added to a globbed dir would not change the key, so never persist these.
    cachePut(raw.includes("import.meta.glob") ? null : key, out.code);
    return { format: "module", source: out.code, shortCircuit: true };
  }
  if (url.includes("?ojv=") && isTanstack(url)) {
    return { format: "module", source: readFileSync(fileURLToPath(clean), "utf8"), shortCircuit: true };
  }
  if (clean.endsWith(".svg")) {
    const path = fileURLToPath(clean);
    const id = /[?&]ojsvg=react/.test(url) ? path + "?react" : path;
    const loaded = container ? container.load(id) : null;
    const src = loaded != null ? loaded : `export default ${JSON.stringify("/@oj-start/fs" + path)};`;
    return { format: "module", source: src, shortCircuit: true };
  }
  if (container && clean.endsWith(".mdx")) {
    const path = fileURLToPath(clean);
    const raw = readFileSync(path, "utf8");
    const key = cacheKey("ssr-loader-mdx", path, raw);
    const hit = cacheGet(key);
    if (hit != null) return { format: "module", source: hit, shortCircuit: true };
    const compiled = container.transform(raw, path);
    if (compiled != null) {
      const out = transformSync(path, compiled, {
        lang: "jsx", jsx: { runtime: "automatic" },
        define: DEFINE,
      });
      cachePut(compiled.includes("import.meta.glob") ? null : key, out.code);
      return { format: "module", source: out.code, shortCircuit: true };
    }
  }
  // A user plugin's load() may override a real on-disk .js/.mjs file (Vite:
  // load hooks run before the fs read). Consult it for user files; fall through
  // to Node's default loader when no plugin claims the module. (.cjs is excluded
  // — plugin content is served as an ES module.)
  if (container && (clean.endsWith(".js") || clean.endsWith(".mjs"))) {
    const path = fileURLToPath(clean);
    if (!path.includes("/node_modules/")) {
      const loaded = container.load(path);
      if (loaded != null) {
        return { format: "module", source: transformGlob(loaded, path), shortCircuit: true };
      }
    }
  }
  if (clean.startsWith("file:") && clean.includes("/node_modules/")) {
    const path = fileURLToPath(clean);
    if (isCjsFile(path)) {
      // cjsFacade require()s the module just to enumerate export names;
      // a hit skips that duplicate graph eval.
      let key = null;
      try { key = cacheKey("ssr-loader-cjs", path, readFileSync(path)); } catch {}
      const hit = cacheGet(key);
      if (hit != null) return { format: "module", source: hit, shortCircuit: true };
      try {
        const src = cjsFacade(path);
        cachePut(key, src);
        return { format: "module", source: src, shortCircuit: true };
      } catch {}
    }
  }
  return next(url, context);
}
