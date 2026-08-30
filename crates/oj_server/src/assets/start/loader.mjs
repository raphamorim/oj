// SPDX-License-Identifier: MIT

import { pathToFileURL, fileURLToPath } from "node:url";
import { readFileSync, unlinkSync, statSync, openSync, readSync, realpathSync } from "node:fs";
import { writeFile, appendFile, rename, mkdir } from "node:fs/promises";
import { createHash } from "node:crypto";
import { resolve as pathResolve, dirname } from "node:path";
import { importPkg, viteEnvDefine, emptyVirtualStub, jsxTransformOptions } from "./resolve-pkg.mjs";
import { loadPluginContainerSync } from "./container-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";
import {
  EXTS, isFile, JS_TO_TS, probe, RESERVED, nearestPkgType,
  hasEsmSyntax, isCjsFile, cjsFacade, stripJsonc, readJsonc, rewriteServerFns, substituteAlias,
  parseImportsField, mergeTsConfig,
  PACK_FMT, PACK_PREFIX, packHash, packLine, packIntegrityFail, scanPack,
} from "./loader-util.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const { transformSync } = await importPkg(APP, "rolldown/experimental", ["vite", "@tanstack/react-start"]);
const rawContainer = loadPluginContainerSync(APP, { command: "serve", environment: "ssr" });
const VIRTUAL_SCHEME = "ojvirtual:///";
const PUBLIC_DIR = process.env.OJ_PUBLIC_DIR
  ? pathResolve(APP, process.env.OJ_PUBLIC_DIR)
  : pathResolve(APP, "public");
const CACHE_DIR = process.env.OJ_CACHE_ROOT ?? pathResolve(HERE, "..");

const BASE_FILES = [
  "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lockb", "package.json",
  "vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts", "vite.config.cjs", "vite.config.cts",
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
  if (!rawContainer) return {};
  let deltaKey = null;
  if (process.env.OJ_SSR_LOADER_CACHE !== "off") {
    try {
      const h = createHash("sha256");
      hashAdd(h, "v", "oj-ssr-env-delta-v2");
      hashBaseInputs(h);
      for (const name of [".env", ".env.local", ".env.development", ".env.development.local"]) {
        try { hashAdd(h, name, readFileSync(pathResolve(APP, name))); } catch {}
      }
      hashAdd(h, "git", gitStateBytes());
      // config() hooks read arbitrary env (a sidecar URL, a profile toggle), so
      // the cached delta must not outlive an env change; skip shell-session noise.
      const ENV_HASH_SKIP = new Set(["_", "PWD", "OLDPWD", "SHLVL", "TERM", "TERM_PROGRAM", "TERM_SESSION_ID", "SSH_AUTH_SOCK", "XPC_SERVICE_NAME", "XPC_FLAGS"]);
      hashAdd(h, "penv", Object.entries(process.env)
        .filter(([k]) => !ENV_HASH_SKIP.has(k))
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([k, v]) => `${k}=${v}`)
        .join("\n"));
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
  try { fresh = rawContainer.env() ?? {}; } catch {}
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
// The config's `define` map: `OJ_DEFINE` from oj (vite.config/oj.config values)
// and the plugin container's own defines. Config defines win over Vite's env
// defines, as in Vite's define plugin.
const USER_DEFINE = (() => {
  try { return JSON.parse(process.env.OJ_DEFINE || "{}") || {}; } catch { return {}; }
})();
const configDefines = (() => {
  try {
    return Object.fromEntries(
      Object.entries(rawContainer?.defines() ?? {})
        .map(([key, value]) => [key, typeof value === "string" ? value : JSON.stringify(value)])
        .filter(([, value]) => typeof value === "string"),
    );
  } catch {
    return {};
  }
})();
const DEFINE = { ...viteEnvDefine({ ssr: true, env: { ...process.env, ...envDelta } }), ...USER_DEFINE, ...configDefines };

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

const VERIFY_FULL = process.env.OJ_CACHE_VERIFY === "full";
const packHeaderLine = () => packLine({ fmt: PACK_FMT, epoch: EPOCH });

const SPEC_FILE = pathResolve(CACHE_DIR, "ssr-bridge-pack.jsonl");
const specPack = new Map();
let specFileReady = false;
if (EPOCH && rawContainer) {
  const bytes = scanPack(SPEC_FILE, "bridge-pack", EPOCH, true, (rec) => {
    let e;
    try { e = JSON.parse(rec.payload.toString("utf8")); } catch { return false; }
    specPack.set(e.k, e.v);
  });
  if (bytes != null) specFileReady = true;
}
let specPending = [];
let specDirty = false;
let specFlushChain = Promise.resolve();
function flushSpecPack() {
  if (specDirty) {
    specDirty = false;
    specPending = [];
    const body = [packHeaderLine()];
    for (const [k, v] of specPack) body.push(packLine({ k, v }));
    specFileReady = true;
    specFlushChain = specFlushChain.then(() => writeFile(SPEC_FILE, body.join(""))).catch(() => {});
    return;
  }
  const batch = specPending;
  specPending = [];
  if (batch.length === 0) return;
  const body = batch.map((e) => packLine(e)).join("");
  const first = !specFileReady;
  specFileReady = true;
  specFlushChain = specFlushChain
    .then(() => (first ? writeFile(SPEC_FILE, packHeaderLine() + body) : appendFile(SPEC_FILE, body)))
    .catch(() => {});
}
function specKey(method, args) {
  try {
    const h = createHash("sha256");
    h.update(method);
    for (const a of args) { h.update("\0"); h.update(String(a ?? "")); }
    if (method === "load") {
      const p = String(args[0] ?? "").split("?")[0];
      if (p.startsWith("/") && isFile(p)) { h.update("\0src\0"); h.update(readFileSync(p)); }
    }
    return h.digest("hex");
  } catch { return null; }
}
const speculatedCalls = new Map();
let specWindowOpen = true;
function speculativeContainer(raw) {
  const wrap = (method) => (...args) => {
    const key = EPOCH ? specKey(method, args) : null;
    if (key && specWindowOpen && specPack.has(key)) {
      if (!speculatedCalls.has(key)) speculatedCalls.set(key, { method, args });
      return specPack.get(key);
    }
    const v = raw[method](...args) ?? null;
    if (key && !specPack.has(key)) {
      specPack.set(key, v);
      specPending.push({ k: key, v });
    }
    return v;
  };
  return {
    ...raw,
    resolveId: wrap("resolveId"),
    load: wrap("load"),
    transformUserCode: wrap("transformUserCode"),
  };
}
const container = rawContainer ? speculativeContainer(rawContainer) : null;

export function revalidateSpeculation() {
  specWindowOpen = false;
  let stale = false;
  for (const [key, { method, args }] of speculatedCalls) {
    let live;
    try { live = rawContainer[method](...args) ?? null; } catch { continue; }
    if (JSON.stringify(live) !== JSON.stringify(specPack.get(key) ?? null)) {
      specPack.set(key, live);
      specDirty = true;
      stale = true;
    }
  }
  speculatedCalls.clear();
  if (specDirty) flushSpecPack();
  return stale;
}

const LOAD_PACK_FILE = pathResolve(CACHE_DIR, "ssr-load-pack.jsonl");
const loadPack = new Map();
const packIndex = new Map();
const PACK_KEY_RE = /^[0-9a-f]{64}$/;
let packFd = -1;
let packFileBytes = 0;
let loadPackFileReady = false;
if (EPOCH) {
  const bytes = scanPack(LOAD_PACK_FILE, "load-pack", EPOCH, VERIFY_FULL, (rec, buf) => {
    if (rec.len > 70 && buf[rec.payloadOff] === 0x7b) {
      const key = buf.toString("utf8", rec.payloadOff + 6, rec.payloadOff + 70);
      if (PACK_KEY_RE.test(key)) packIndex.set(key, [rec.payloadOff, rec.len, rec.hash]);
    }
  });
  if (bytes != null) {
    loadPackFileReady = true;
    packFileBytes = bytes;
    if (packIndex.size > 0) {
      try { packFd = openSync(LOAD_PACK_FILE, "r"); } catch { packIndex.clear(); }
    }
  }
}
function packGet(key) {
  const mem = loadPack.get(key);
  if (mem !== undefined) return mem;
  const loc = packIndex.get(key);
  if (loc === undefined || packFd < 0) return undefined;
  try {
    const b = Buffer.allocUnsafe(loc[1]);
    readSync(packFd, b, 0, loc[1], loc[0]);
    if (packHash(b) === loc[2]) {
      const e = JSON.parse(b.toString("utf8"));
      if (e.k === key && typeof e.c === "string") return e.c;
    }
  } catch {}
  packIntegrityFail("load-pack", { action: "drop", key });
  packIndex.delete(key);
  return undefined;
}
let packPending = [];
let packFlushChain = Promise.resolve();
function flushLoadPack() {
  const batch = packPending;
  packPending = [];
  if (batch.length === 0) return;
  const lines = batch.map((e) => packLine(e));
  const first = !loadPackFileReady;
  loadPackFileReady = true;
  packFlushChain = packFlushChain.then(async () => {
    try {
      await mkdir(CACHE_DIR, { recursive: true });
      if (first) {
        const header = packHeaderLine();
        await writeFile(LOAD_PACK_FILE, header + lines.join(""));
        packFileBytes = Buffer.byteLength(header);
      } else {
        await appendFile(LOAD_PACK_FILE, lines.join(""));
      }
      for (let i = 0; i < batch.length; i++) {
        const len = Buffer.byteLength(lines[i]);
        packIndex.set(batch[i].k, [packFileBytes + PACK_PREFIX, len - PACK_PREFIX - 1, lines[i].slice(8, PACK_PREFIX)]);
        packFileBytes += len;
        loadPack.delete(batch[i].k);
      }
      if (packFd < 0) packFd = openSync(LOAD_PACK_FILE, "r");
    } catch {}
  });
}
function rememberPack(key, code) {
  if (!EPOCH || loadPack.has(key) || packIndex.has(key)) return;
  loadPack.set(key, code);
  packPending.push({ k: key, c: code });
}

function cacheGet(key) {
  if (!key) return null;
  const packed = packGet(key);
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
const RESOLVE_CACHE_FILE = pathResolve(CACHE_DIR, "ssr-resolve.jsonl");
const resolveCache = new Map();
let resolveFileReady = false;
if (EPOCH) {
  const bytes = scanPack(RESOLVE_CACHE_FILE, "resolve-cache", EPOCH, true, (rec) => {
    let e;
    try { e = JSON.parse(rec.payload.toString("utf8")); } catch { return false; }
    resolveCache.set(e.k, e);
  });
  if (bytes != null) resolveFileReady = true;
}
let resolvePending = [];
let resolveFlushTimer = null;
let resolveFlushChain = Promise.resolve();
function flushResolveCache() {
  resolveFlushTimer = null;
  const batch = resolvePending;
  resolvePending = [];
  if (batch.length === 0) return;
  const body = batch.map((e) => packLine(e)).join("");
  const first = !resolveFileReady;
  resolveFileReady = true;
  resolveFlushChain = resolveFlushChain
    .then(async () => {
      await mkdir(CACHE_DIR, { recursive: true });
      if (first) await writeFile(RESOLVE_CACHE_FILE, packHeaderLine() + body);
      else await appendFile(RESOLVE_CACHE_FILE, body);
    })
    .catch(() => {});
}
function rememberResolve(key, entry) {
  if (!EPOCH || resolveCache.has(key)) return;
  entry.k = key;
  resolveCache.set(key, entry);
  resolvePending.push(entry);
  if (!resolveFlushTimer) resolveFlushTimer = setTimeout(flushResolveCache, 250);
}

export function flushCaches() {
  if (writeQueue.length > 0) drainWrites();
  if (packPending.length > 0) flushLoadPack();
  if (resolvePending.length > 0) flushResolveCache();
  if (specPending.length > 0 || specDirty) flushSpecPack();
}

function reportCacheStats() {
  if (!EPOCH) return;
  process.stderr.write(
    `oj: ssr loader cache: load ${cacheStats.hits}/${cacheStats.hits + cacheStats.misses} hits (${cacheStats.uncached} uncacheable), resolve ${cacheStats.rhits}/${cacheStats.rhits + cacheStats.rmisses} hits\n`,
  );
}

export function memStats() {
  let packMemChars = 0;
  for (const c of loadPack.values()) packMemChars += c.length;
  let resolveChars = 0;
  for (const e of resolveCache.values()) {
    resolveChars += e.k.length + e.u.length + (e.f ? e.f.length : 0);
  }
  let containerHeap = null;
  try { containerHeap = container ? container.heap() : null; } catch {}
  return {
    packIndexEntries: packIndex.size,
    packMemEntries: loadPack.size,
    packMemChars,
    resolveEntries: resolveCache.size,
    resolveChars,
    containerHeap,
  };
}

let V = 0;
export function setVersion(v) { V = v; }
export { reportCacheStats };
const stripQ = (u) => u.split("?")[0];
const withV = (u) => (V ? `${stripQ(u)}?ojv=${V}` : stripQ(u));
const isTanstack = (u) => /\/@tanstack\//.test(u) && /\.(js|mjs)$/.test(stripQ(u));
const ASSET_SUFFIX = /\?(raw|url|inline)$/;
// Kept in sync with is_style_ext on the Rust side.
const STYLE_EXT = /\.(css|scss|sass|less|styl|stylus)$/;
// Kept in sync with is_importable_asset_ext on the Rust side (the client
// graph's classifier), plus wasm which only the SSR loader URL-exports; svg is
// excluded here too, it routes through the svgr path.
const ASSET_EXT = /\.(png|jpe?g|gif|webp|avif|ico|bmp|woff2?|ttf|otf|eot|mp4|webm|ogg|mp3|wav|flac|m4a|aac|mov|pdf|webmanifest|wasm)$/;
// Vite inlines with the file's real content type (mrmime lookup, falling back
// to octet-stream); match it for the types oj treats as assets.
const MIME = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
  webp: "image/webp", avif: "image/avif", ico: "image/x-icon", svg: "image/svg+xml",
  woff: "font/woff", woff2: "font/woff2", ttf: "font/ttf", otf: "font/otf",
  eot: "application/vnd.ms-fontobject", mp4: "video/mp4", webm: "video/webm",
  ogg: "audio/ogg", mp3: "audio/mpeg", wav: "audio/wav", flac: "audio/flac",
  m4a: "audio/mp4", aac: "audio/aac", mov: "video/quicktime", bmp: "image/bmp",
  pdf: "application/pdf", webmanifest: "application/manifest+json",
  wasm: "application/wasm", css: "text/css", json: "application/json", txt: "text/plain",
};
const mimeOf = (p) => MIME[p.slice(p.lastIndexOf(".") + 1).toLowerCase()] ?? "application/octet-stream";

// CSS modules: the client serves the real class-name map (css_exports from
// oj_css::compile_css_rebased, lightningcss pattern "[name]_[local]_[hash]"),
// so SSR must produce identical names. [hash] is Rust's DefaultHasher
// (SipHash-1-3, zero keys) over the root-relative URL, truncated to u32,
// encoded with lightningcss's base64 alphabet. Pinned on both sides:
// oj_css css_modules_scoped_name_matches_ssr_loader and
// e2e/unit/ssr-loader-css-modules.test.mjs assert the same literal names; a
// lightningcss upgrade that changes naming must update this to match.
const SIP_MASK = 0xffffffffffffffffn;
const sipRotl = (x, b) => ((x << BigInt(b)) | (x >> BigInt(64 - b))) & SIP_MASK;
function sipHash13(bytes) {
  let v0 = 0x736f6d6570736575n, v1 = 0x646f72616e646f6dn;
  let v2 = 0x6c7967656e657261n, v3 = 0x7465646279746573n;
  const round = () => {
    v0 = (v0 + v1) & SIP_MASK; v1 = sipRotl(v1, 13); v1 ^= v0; v0 = sipRotl(v0, 32);
    v2 = (v2 + v3) & SIP_MASK; v3 = sipRotl(v3, 16); v3 ^= v2;
    v0 = (v0 + v3) & SIP_MASK; v3 = sipRotl(v3, 21); v3 ^= v0;
    v2 = (v2 + v1) & SIP_MASK; v1 = sipRotl(v1, 17); v1 ^= v2; v2 = sipRotl(v2, 32);
  };
  const len = bytes.length;
  const end = len - (len % 8);
  for (let i = 0; i < end; i += 8) {
    let m = 0n;
    for (let j = 7; j >= 0; j--) m = (m << 8n) | BigInt(bytes[i + j]);
    v3 ^= m; round(); v0 ^= m;
  }
  let b = (BigInt(len) & 0xffn) << 56n;
  for (let j = end; j < len; j++) b |= BigInt(bytes[j]) << BigInt(8 * (j - end));
  v3 ^= b; round(); v0 ^= b;
  v2 ^= 0xffn;
  round(); round(); round();
  return (v0 ^ v1 ^ v2 ^ v3) & SIP_MASK;
}
const CSS_MODULE_ALPHABET = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890_-";
function cssModuleHash(rel) {
  // Rust `Hash for str` feeds the bytes plus a 0xff terminator.
  const h = sipHash13([...Buffer.from(rel, "utf8"), 0xff]);
  const u32 = Number(h & 0xffffffffn);
  const b = [u32 & 0xff, (u32 >>> 8) & 0xff, (u32 >>> 16) & 0xff, (u32 >>> 24) & 0xff];
  let out = "", acc = 0, nbits = 0;
  for (const byte of b) {
    acc = (acc << 8) | byte; nbits += 8;
    while (nbits >= 6) { out += CSS_MODULE_ALPHABET[(acc >>> (nbits - 6)) & 63]; nbits -= 6; }
  }
  if (nbits > 0) out += CSS_MODULE_ALPHABET[(acc << (6 - nbits)) & 63];
  return out;
}
const isCssModule = (p) => p.slice(p.lastIndexOf("/") + 1).includes(".module.");
function cssModuleExports(path, source) {
  // The hash input must be the same URL the client compiled under: url_of on
  // the Rust side gives "/{rel}" inside the root and "/@fs{abs}" outside it.
  const rel = path.startsWith(APP + "/") ? path.slice(APP.length) : "/@fs" + path;
  const hash = cssModuleHash(rel);
  const base = path.slice(path.lastIndexOf("/") + 1);
  const stem = base.slice(0, base.lastIndexOf(".")).replace(/\./g, "-");
  // Keep only selector-position tokens: strip strings, comments, url() and
  // :global(...) scopes, then collect class/id selectors and @keyframes
  // names, which is what components read from the map. Extra keys from
  // letter-leading hex colors are harmless (same formula, never read).
  // Known approximations, all unread in practice: container-name,
  // @counter-style and bare animation-name references are also scoped by
  // lightningcss but not collected here; a sass &-suffix class is invisible
  // in the source; composes maps to its own single name, matching what the
  // client serves (css_exports drops composed chains too).
  const body = source
    .replace(/"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/g, "")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/url\([^)]*\)/g, "")
    .replace(/\/\/[^\n]*/g, "")
    .replace(/:global\s*\([^)]*\)/g, "");
  const out = {};
  for (const m of body.matchAll(/(?:[.#]|@keyframes\s+)(-?[A-Za-z_][A-Za-z0-9_-]*)/g)) {
    out[m[1]] ??= `${stem}_${m[1]}_${hash}`;
  }
  return out;
}

const ALIASES = {
  "#tanstack-router-entry": pathResolve(APP, "src/router"),
  "#tanstack-start-entry": pathResolve(HERE, "start-entry.ts"),
  "#tanstack-start-plugin-adapters": pathResolve(HERE, "plugin-adapters.ts"),
  "#tanstack-start-server-fn-resolver": pathResolve(HERE, "server-fn-resolver.mjs"),
  "tanstack-start-manifest:v": pathResolve(HERE, "manifest-dev.ts"),
  "tanstack-start-injected-head-scripts:v": pathResolve(HERE, "injected-head-scripts.ts"),
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

// Resolve a directory whose own package.json names the entry (module/main),
// the legacy subpath-entry pattern Node ESM rejects; prefer the ESM `module`.
function resolveDirEntry(dir) {
  // realpath the entry so its own bare imports resolve against the real store
  // location (under pnpm the dir is a symlink, and a transitive dep like immer
  // lives beside the real package, not beside the symlink).
  const real = (p) => {
    try { return realpathSync(p); } catch { return p; }
  };
  try {
    const pj = JSON.parse(readFileSync(pathResolve(dir, "package.json"), "utf8"));
    for (const key of ["module", "main"]) {
      if (typeof pj[key] === "string") {
        const p = pathResolve(dir, pj[key]);
        if (isFile(p)) return real(p);
      }
    }
  } catch {}
  for (const idx of ["index.mjs", "index.js", "index.cjs"]) {
    const p = pathResolve(dir, idx);
    if (isFile(p)) return real(p);
  }
  return null;
}

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

// One resolution ladder for every specifier, shared by asset, svg?react, and
// plain module paths: relative probe, built-in aliases, package.json #imports,
// tsconfig paths, then Node's resolver with its two error recoveries. `via`
// records how the hit was found so the caller can keep versioning semantics:
// "local" hits are user-reachable files that take a ?ojv version, "recovered"
// hits short-circuit unversioned, "default" passes Node's result through.
function resolveSpec(clean, context, next) {
  if (clean.startsWith(".")) {
    if (context.parentURL && context.parentURL.startsWith("file:")) {
      const hit = probe(pathResolve(dirname(fileURLToPath(context.parentURL)), clean));
      if (hit) return { abs: hit, via: "local" };
    }
  } else {
    // The app's own package.json "imports" first: ALIASES is a set of
    // conventions for the framework seam (the router entry, the manifest), and a
    // declaration must win over a convention.
    if (clean.startsWith("#")) {
      const hit = resolveImports(clean);
      if (hit) return { abs: hit, via: "local" };
    }
    if (ALIASES[clean]) {
      const hit = probe(ALIASES[clean]);
      if (hit) return { abs: hit, via: "local" };
    }
    if (clean.startsWith("/")) {
      // A /-prefixed id resolves against the project root (Vite's asSrc
      // behavior; the client graph already serves these URLs directly), then
      // against publicDir (Vite's checkPublicFile). True absolute fs paths miss
      // both probes and fall through to Node.
      const hit = probe(pathResolve(APP, clean.slice(1))) || probe(pathResolve(PUBLIC_DIR, clean.slice(1)));
      if (hit) return { abs: hit, via: "local" };
    } else {
      const hit = resolveTsPaths(clean);
      if (hit) return { abs: hit, via: "local" };
    }
  }
  let r;
  try {
    r = next(clean, context);
  } catch (err) {
    if (err && err.code === "ERR_MODULE_NOT_FOUND") {
      const tried = err.url && err.url.startsWith("file:")
        ? fileURLToPath(err.url)
        : (err.message.match(/Cannot find module '([^']+)'/) || [])[1];
      const hit = tried ? probe(tried) : null;
      if (hit) return { abs: hit, via: "recovered" };
    }
    // A dep subpath that is a directory with its own package.json (the legacy
    // `pkg/query/react/package.json` entry pattern, e.g. @reduxjs/toolkit): Node
    // ESM refuses the directory import, but CJS resolvers and bundlers read its
    // module/main. Resolve it to that entry so oj serves the same file Vite does.
    if (err && err.code === "ERR_UNSUPPORTED_DIR_IMPORT") {
      const dir = err.url && err.url.startsWith("file:")
        ? fileURLToPath(err.url)
        : (err.message.match(/Directory import '([^']+)'/) || [])[1];
      const hit = dir ? resolveDirEntry(dir) : null;
      if (hit) return { abs: hit, via: "recovered" };
    }
    throw err;
  }
  return { result: r, via: "default" };
}

// Inline the transform's source map so Node (started with --enable-source-maps)
// reports stack frames at the original .tsx positions, as Vite's
// ssrFixStacktrace does for its runner.
function withInlineMap(out) {
  if (!out || !out.map) return out.code;
  try {
    const map = typeof out.map === "string" ? out.map : JSON.stringify(out.map);
    return out.code + "\n//# sourceMappingURL=data:application/json;base64," + Buffer.from(map).toString("base64") + "\n";
  } catch {
    return out.code;
  }
}

// Vite's `ssr.noExternal`: dependencies the loader must transform itself (define,
// import.meta.env, glob) instead of leaving to Node. Names, `@scope/*` globs,
// RegExp sources, or everything (`true`); `external` names always stay Node's.
const SSR_EXTERNALS = (() => {
  try { return JSON.parse(process.env.OJ_SSR_EXTERNALS || "null"); } catch { return null; }
})();
function pkgNameOfPath(path) {
  const i = path.lastIndexOf("/node_modules/");
  if (i < 0) return null;
  const parts = path.slice(i + "/node_modules/".length).split("/");
  return parts[0]?.startsWith("@") ? (parts[1] ? `${parts[0]}/${parts[1]}` : null) : parts[0] || null;
}
function globMatch(pattern, value) {
  if (!pattern.includes("*")) return pattern === value;
  const re = new RegExp("^" + pattern.split("*").map((p) => p.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join(".*") + "$");
  return re.test(value);
}
function isNoExternalDep(path) {
  if (!SSR_EXTERNALS) return false;
  const pkg = pkgNameOfPath(path);
  if (!pkg) return false;
  if ((SSR_EXTERNALS.external || []).includes(pkg) || SSR_EXTERNALS.externalAll) return false;
  if (SSR_EXTERNALS.noExternalAll) return true;
  if ((SSR_EXTERNALS.noExternal || []).some((p) => globMatch(p, pkg))) return true;
  return (SSR_EXTERNALS.noExternalRegex || []).some((src) => { try { return new RegExp(src).test(pkg); } catch { return false; } });
}

// oxc `lang` for a module url, or null for anything the transform does not own.
function SRC_LANG_OF(clean) {
  if (clean.endsWith(".tsx")) return "tsx";
  if (clean.endsWith(".ts") || clean.endsWith(".mts") || clean.endsWith(".cts")) return "ts";
  if (clean.endsWith(".jsx")) return "jsx";
  if (clean.endsWith(".js") || clean.endsWith(".mjs")) return "js";
  return null;
}

const toPath = (u) => {
  try { return fileURLToPath(u); } catch { return null; }
};

// Resolve first, classify second (Vite's ordering: its alias plugin rewrites
// the specifier before the asset plugin ever sees it). Strip the intent query,
// resolve the cleaned specifier through the shared ladder, then decide asset /
// svg-react / plain-module treatment from the *resolved* path, so every
// resolution route (aliases, exports maps, error recoveries) gets the same
// asset tagging.
function resolveUncached(spec, context, next) {
  if (container && (spec.startsWith("virtual:") || spec.startsWith("\0"))) {
    const importer = context.parentURL && context.parentURL.startsWith("file:")
      ? fileURLToPath(context.parentURL)
      : undefined;
    const rid = container.resolveId(spec, importer);
    if (rid != null) {
      const u = VIRTUAL_SCHEME + encodeURIComponent(rid);
      return { url: V ? `${u}?ojv=${V}` : u, shortCircuit: true };
    }
  }
  const svgReact = /\.svg\?react$/.test(spec);
  const intent = svgReact ? null : (spec.match(ASSET_SUFFIX)?.[1] ?? null);
  const clean = svgReact || intent ? spec.replace(/\?(raw|url|inline|react)$/, "") : spec;

  const res = resolveSpec(clean, context, next);
  const abs = res.abs ?? (res.result && res.result.url && res.result.url.startsWith("file:")
    ? toPath(stripQ(res.result.url))
    : null);

  if (abs) {
    if (svgReact) return { url: pathToFileURL(abs).href + "?ojsvg=react", shortCircuit: true };
    // Only classify by extension when the specifier carried no query: a spec
    // with a leftover query is either already-tagged (a runner re-importing a
    // ?ojasset/?ojv URL, which must pass through with its query intact) or
    // asks for something we don't handle.
    const kind = intent ?? (clean.includes("?") ? null : STYLE_EXT.test(abs) ? "css" : ASSET_EXT.test(abs) ? "url" : null);
    if (kind) return { url: pathToFileURL(abs).href + `?ojasset=${kind}`, shortCircuit: true };
  }
  if (res.via === "local") return { url: withV(pathToFileURL(res.abs).href), shortCircuit: true, _ojv: true };
  if (res.via === "recovered") return { url: pathToFileURL(res.abs).href, shortCircuit: true };
  const r = res.result;
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
    const rid = decodeURIComponent(stripQ(url).slice(VIRTUAL_SCHEME.length));
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
      // Stylesheets inline as css text (the client and Vite serve the
      // compiled css string; here it is the on-disk source, uncompiled),
      // everything else as a data URL with the file's content type.
      src = `export default ${JSON.stringify(STYLE_EXT.test(path)
        ? readFileSync(path, "utf8")
        : `data:${mimeOf(path)};base64,` + readFileSync(path).toString("base64"))};`;
    else if (kind === "css" && isCssModule(path))
      src = `export default ${JSON.stringify(cssModuleExports(path, readFileSync(path, "utf8")))};`;
    return { format: "module", source: src, shortCircuit: true };
  }
  if (clean.endsWith(".json")) {
    return { format: "module", source: `export default ${readFileSync(fileURLToPath(clean), "utf8")};`, shortCircuit: true };
  }
  // TypeScript/JSX always; plain .js/.mjs/.mts/.cts for the app's own modules
  // too, so import.meta.env, define, import.meta.glob and server functions are
  // rewritten there as well (Vite transforms every module in the graph; a raw
  // `import.meta.env.DEV` reaching Node throws on `undefined`).
  // A plain .js is only ours when it is app source: not a dependency, and not
  // a CommonJS file (a `file:` package or a .cjs-style module keeps Node's
  // loader, whose lexer provides its named exports).
  const srcLang = SRC_LANG_OF(clean);
  const inDeps = clean.includes("/node_modules/");
  const ownsJs = srcLang === "js" && clean.startsWith("file:")
    && (!inDeps || isNoExternalDep(fileURLToPath(clean)))
    && !isCjsFile(fileURLToPath(clean));
  if (srcLang && (srcLang !== "js" || ownsJs)) {
    const path = fileURLToPath(clean);
    const userFile = container && !path.includes("/node_modules/");
    let diskRaw = null;
    if (userFile) {
      try { diskRaw = readFileSync(path, "utf8"); } catch {}
      if (diskRaw != null) {
        const diskHit = cacheGet(cacheKey("ssr-loader", path, diskRaw));
        if (diskHit != null) return { format: "module", source: diskHit, shortCircuit: true };
      }
    }
    // A plugin load() overrides the on-disk file (Vite: load runs before fs read).
    let raw = userFile ? container.load(path) : null;
    if (raw == null) raw = diskRaw ?? readFileSync(path, "utf8");
    const key = cacheKey("ssr-loader", path, raw);
    if (raw !== diskRaw) {
      const hit = cacheGet(key);
      if (hit != null) return { format: "module", source: hit, shortCircuit: true };
    }
    if (userFile) {
      const tucKey = cacheKey("ssr-tuc", path, raw);
      const tucHit = cacheGet(tucKey);
      if (tucHit != null) {
        if (tucHit !== "\0none") raw = tucHit;
      } else {
        const t = container.transformUserCode(raw, path);
        cachePut(tucKey, t ?? "\0none");
        if (t != null) raw = t;
      }
    }
    const src = transformServerFns(transformGlob(raw, path), path);
    const out = transformSync(path, src, {
      lang: srcLang,
      jsx: jsxTransformOptions(),
      define: DEFINE,
    });
    const code = withInlineMap(out);
    cachePut(raw.includes("import.meta.glob") ? null : key, code);
    return { format: "module", source: code, shortCircuit: true };
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
        lang: "jsx", jsx: jsxTransformOptions(),
        define: DEFINE,
      });
      const code = withInlineMap(out);
      cachePut(compiled.includes("import.meta.glob") ? null : key, code);
      return { format: "module", source: code, shortCircuit: true };
    }
  }
  // A user plugin's load() may override a real on-disk .js/.mjs file (Vite:
  // load hooks run before the fs read). Consult it for user files; fall through
  // to Node's default loader when no plugin claims the module. (.cjs is excluded
  // — plugin content is served as an ES module.)
  if (container && (clean.endsWith(".js") || clean.endsWith(".mjs"))) {
    const path = fileURLToPath(clean);
    if (!path.includes("/node_modules/")) {
      let diskRaw = null;
      try { diskRaw = readFileSync(path, "utf8"); } catch {}
      const key = diskRaw != null ? cacheKey("ssr-loader-unclaimed", path, diskRaw) : null;
      if (key && cacheGet(key) != null) return next(url, context);
      const loaded = container.load(path);
      if (loaded != null) {
        return { format: "module", source: transformGlob(loaded, path), shortCircuit: true };
      }
      if (diskRaw != null && diskRaw.includes("import.meta.glob")) {
        return { format: "module", source: transformGlob(diskRaw, path), shortCircuit: true };
      }
      cachePut(key, "1");
    }
  }
  if (clean.startsWith("file:") && clean.includes("/node_modules/")) {
    const path = fileURLToPath(clean);
    if (isCjsFile(path)) {
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
