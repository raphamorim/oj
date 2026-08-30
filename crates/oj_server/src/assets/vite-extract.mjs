// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { writeFileSync, readFileSync, realpathSync } from "node:fs";

const configPath = process.argv[2];
const appRoot = process.argv[3];
const command = process.argv[4] || "serve";
const mode = process.argv[5] || "development";
// "default": the mode is the command's default, not a CLI `--mode`, so a `mode`
// named by the config file may win (Vite: inlineConfig.mode || config.mode).
const modeExplicit = process.argv[6] !== "default";

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";

const appRequire = createRequire(pathToFileURL(appRoot + "/package.json").href);
let directDeps = [];
try {
  const pkg = JSON.parse(readFileSync(appRoot + "/package.json", "utf8"));
  directDeps = Object.keys({ ...pkg.dependencies, ...pkg.devDependencies });
} catch {}
function resolvePkg(spec) {
  try {
    return appRequire.resolve(spec);
  } catch {}
  for (const anchor of directDeps) {
    let dir;
    try {
      dir = dirname(appRequire.resolve(anchor + "/package.json"));
    } catch {
      continue;
    }
    try {
      return appRequire.resolve(spec, { paths: [dir] });
    } catch {}
  }
  return null;
}

const absDeps = (deps) =>
  (deps ?? []).filter((d) => typeof d === "string").map((d) => resolve(appRoot, d));

async function loadConfig() {
  let viteErr = null;
  const vitePath = resolvePkg("vite");
  if (vitePath) {
    try {
      const vite = await import(pathToFileURL(vitePath).href);
      // resolveConfig runs the plugins' config hooks, so plugin-injected values
      // (e.g. TanStack Start's resolve.alias for `#tanstack-router-entry`) are
      // present. loadConfigFromFile only reads the raw user config and misses them.
      if (typeof vite.resolveConfig === "function") {
        try {
          const inline = { root: appRoot, configFile: configPath };
          if (modeExplicit) inline.mode = mode;
          const resolved = await vite.resolveConfig(inline, command, mode, mode);
          if (resolved) {
            return { config: resolved, deps: absDeps(resolved.configFileDependencies) };
          }
        } catch {
          // fall through to the raw loader below
        }
      }
      if (typeof vite.loadConfigFromFile === "function") {
        const loaded = await vite.loadConfigFromFile({ command, mode }, configPath, appRoot);
        if (loaded && loaded.config) {
          return { config: loaded.config, deps: absDeps(loaded.dependencies) };
        }
      }
    } catch (e) {
      viteErr = e;
    }
  }
  if (/\.(ts|tsx|mts|cts)$/.test(configPath)) {
    const esbuildPath = resolvePkg("esbuild");
    if (!esbuildPath) {
      throw viteErr ?? new Error("no vite or esbuild available to load the TS vite.config");
    }
    const esbuild = await import(pathToFileURL(esbuildPath).href);
    const r = await esbuild.build({
      entryPoints: [configPath], bundle: true, platform: "node", format: "esm",
      packages: "external", write: false, logLevel: "silent", absWorkingDir: appRoot,
      metafile: true,
      define: {
        __dirname: JSON.stringify(dirname(configPath)),
        __filename: JSON.stringify(configPath),
      },
    });
    const out = resolve(dirname(fileURLToPath(import.meta.url)), "oj-vite-config.mjs");
    writeFileSync(out, r.outputFiles[0].text);
    const m = await import(pathToFileURL(out).href);
    return {
      config: typeof m.default === "function" ? await m.default({ command, mode }) : m.default,
      deps: absDeps(Object.keys(r.metafile?.inputs ?? {})),
    };
  }
  const m = await import(pathToFileURL(configPath).href);
  return {
    config: typeof m.default === "function" ? await m.default({ command, mode }) : m.default,
    deps: [],
  };
}

function aliasKeyFromRegex(source) {
  let s = source;
  if (s.startsWith("^")) s = s.slice(1);
  if (s.endsWith("$")) s = s.slice(0, -1);
  s = s.replace(/\\?\/?\((?:\.\*|\.\+)\??\)\??$/, "");
  s = s.replace(/\\(.)/g, "$1");
  return s.replace(/\/$/, "");
}
function aliasDirFromReplacement(replacement) {
  return replacement
    .replace(/[\\/]\$\d+$/, "")
    .replace(/[\\/]index\.[a-z]+$/i, "");
}

function extractAlias(alias) {
  const out = {};
  if (!alias) return out;
  const entries = Array.isArray(alias)
    ? alias.map((e) => [e.find, e.replacement])
    : Object.entries(alias);
  for (const [find, replacement] of entries) {
    if (typeof replacement !== "string") continue;
    if (typeof find === "string") {
      if (out[find] == null) out[find] = replacement;
    } else if (find instanceof RegExp) {
      const key = aliasKeyFromRegex(find.source);
      if (!key || /[.*+?()[\]{}|^$]/.test(key)) {
        warn(`resolve.alias regex ${find} not convertible to a path alias; skipped`);
        continue;
      }
      if (out[key] == null) out[key] = aliasDirFromReplacement(replacement);
    }
  }
  return out;
}

function stringMap(obj) {
  if (!obj || typeof obj !== "object") return null;
  const out = {};
  for (const [k, v] of Object.entries(obj)) if (typeof v === "string") out[k] = v;
  return Object.keys(out).length ? out : null;
}

const warn = (msg) => process.stderr.write(`oj: vite.config: ${msg}\n`);

function extractProxy(proxy) {
  if (!proxy || typeof proxy !== "object") return null;
  const out = {};
  for (const [ctx, v] of Object.entries(proxy)) {
    if (typeof v === "string") {
      out[ctx] = v;
    } else if (v && typeof v === "object" && typeof v.target === "string") {
      const entry = { target: v.target };
      if (typeof v.changeOrigin === "boolean") entry.changeOrigin = v.changeOrigin;
      if (typeof v.ws === "boolean") entry.ws = v.ws;
      if (typeof v.rewrite === "function") {
        warn(`server.proxy["${ctx}"].rewrite is a function; oj applies only {from,to} string rewrites`);
      }
      out[ctx] = entry;
    }
  }
  return Object.keys(out).length ? out : null;
}

function extractOptimizeDeps(od) {
  if (!od || typeof od !== "object") return null;
  const strArr = (v) =>
    typeof v === "string" ? [v] : Array.isArray(v) ? v.filter((x) => typeof x === "string") : undefined;
  const out = {};
  const inc = strArr(od.include);
  const exc = strArr(od.exclude);
  const ent = strArr(od.entries);
  if (inc) out.include = inc;
  if (exc) out.exclude = exc;
  if (ent) out.entries = ent;
  return Object.keys(out).length ? out : null;
}

// `build` values oj's build honors. `resolveConfig` returns Vite's defaults for
// anything the user left unset (outDir "dist", minify "oxc", sourcemap false,
// cssCodeSplit true, target "baseline-widely-available"); the boolean/string
// shapes oj adopts match its own defaults for those, and the special target
// names (which rolldown's transform does not parse) are skipped so oj's default
// lowering stands.
function extractBuild(b) {
  if (!b || typeof b !== "object") return null;
  const out = {};
  if (typeof b.outDir === "string") out.outDir = b.outDir;
  if (typeof b.sourcemap === "boolean" || typeof b.sourcemap === "string") out.sourcemap = b.sourcemap;
  if (typeof b.minify === "boolean" || typeof b.minify === "string") out.minify = b.minify;
  if (b.terserOptions) warn("build.terserOptions is not applied (oj minifies with oxc)");
  if (typeof b.cssCodeSplit === "boolean") out.cssCodeSplit = b.cssCodeSplit;
  if (typeof b.target === "string") out.target = b.target;
  else if (Array.isArray(b.target)) out.target = b.target.filter((t) => typeof t === "string");
  else if (b.target === false) warn("build.target false is not supported; the default baseline is used");
  if (typeof b.emptyOutDir === "boolean") out.emptyOutDir = b.emptyOutDir;
  if (b.modulePreload === false) out.modulePreload = false;
  else if (b.modulePreload && typeof b.modulePreload === "object") {
    if (typeof b.modulePreload.resolveDependencies === "function") {
      warn("build.modulePreload.resolveDependencies is a function and cannot be applied");
    }
    if (typeof b.modulePreload.polyfill === "boolean") out.modulePreload = { polyfill: b.modulePreload.polyfill };
  }
  if (typeof b.ssr === "string" || typeof b.ssr === "boolean") out.ssr = b.ssr;
  if (typeof b.ssrManifest === "string" || typeof b.ssrManifest === "boolean") out.ssrManifest = b.ssrManifest;
  if (typeof b.copyPublicDir === "boolean") out.copyPublicDir = b.copyPublicDir;
  return Object.keys(out).length ? out : null;
}

// rollup/rolldown options travel as JSON. A function (manualChunks, *FileNames,
// external) or RegExp cannot; mark them so oj warns instead of silently
// dropping the option.
function markFunctions(v) {
  if (typeof v === "function") return "__oj_fn__";
  if (v instanceof RegExp) return { __oj_regex__: v.source };
  if (Array.isArray(v)) return v.map(markFunctions);
  if (v && typeof v === "object") {
    const out = {};
    for (const [k, x] of Object.entries(v)) out[k] = markFunctions(x);
    return out;
  }
  return v;
}


const JSX_ESBUILD_KEYS = ["jsx", "jsxImportSource", "jsxFactory", "jsxFragment"];
function extractOxc(oxc) {
  const jsx = oxc && typeof oxc === "object" ? oxc.jsx : null;
  if (!jsx || typeof jsx !== "object") return null;
  const out = {};
  for (const k of ["runtime", "importSource", "pragma", "pragmaFrag"]) {
    if (typeof jsx[k] === "string") out[k] = jsx[k];
  }
  return Object.keys(out).length ? { jsx: out } : null;
}
function extractEsbuild(es) {
  if (!es || typeof es !== "object") return null;
  const out = {};
  for (const k of JSX_ESBUILD_KEYS) if (typeof es[k] === "string") out[k] = es[k];
  return Object.keys(out).length ? out : null;
}

// `ssr.noExternal`/`external` entries: strings and globs pass through, RegExps
// become `{ regex }`, `true` stays `true`.
function extractSsr(ssr) {
  if (!ssr || typeof ssr !== "object") return null;
  const list = (v) => {
    if (v === true) return true;
    const arr = Array.isArray(v) ? v : v == null ? [] : [v];
    const out = [];
    for (const e of arr) {
      if (typeof e === "string") out.push(e);
      else if (e instanceof RegExp) out.push({ regex: e.source });
    }
    return out;
  };
  const out = {};
  const ne = list(ssr.noExternal);
  const ex = list(ssr.external);
  if (ne === true || (Array.isArray(ne) && ne.length)) out.noExternal = ne;
  if (ex === true || (Array.isArray(ex) && ex.length)) out.external = ex;
  if (typeof ssr.target === "string") out.target = ssr.target;
  return Object.keys(out).length ? out : null;
}
function extractResolve(r) {
  if (!r || typeof r !== "object") return null;
  const out = {};
  const strArr = (v) => (Array.isArray(v) ? v.filter((x) => typeof x === "string") : null);
  for (const k of ["extensions", "mainFields", "conditions"]) {
    const v = strArr(r[k]);
    if (v && v.length) out[k] = v;
  }
  if (typeof r.preserveSymlinks === "boolean") out.preserveSymlinks = r.preserveSymlinks;
  return Object.keys(out).length ? out : null;
}
function extractServerFlags(s) {
  if (!s || typeof s !== "object") return null;
  const out = {};
  if (typeof s.strictPort === "boolean") out.strictPort = s.strictPort;
  // Vite admits `open: true | string`; oj opens the served url in both cases.
  if (s.open === true || typeof s.open === "string") out.open = true;
  else if (s.open === false) out.open = false;
  return Object.keys(out).length ? out : null;
}
function extractEnvPrefix(p) {
  if (typeof p === "string") return [p];
  if (Array.isArray(p)) return p.filter((x) => typeof x === "string");
  return null;
}
function extractCors(cors) {
  if (typeof cors === "boolean") return cors;
  if (!cors || typeof cors !== "object") return null;
  const out = {};
  const strOrList = (v) =>
    typeof v === "string" ? v : Array.isArray(v) ? v.filter((x) => typeof x === "string") : undefined;
  if (cors.origin === true || cors.origin === false) out.origin = cors.origin;
  else if (strOrList(cors.origin) !== undefined) out.origin = strOrList(cors.origin);
  if (strOrList(cors.methods) !== undefined) out.methods = strOrList(cors.methods);
  if (strOrList(cors.allowedHeaders) !== undefined) out.allowedHeaders = strOrList(cors.allowedHeaders);
  if (typeof cors.credentials === "boolean") out.credentials = cors.credentials;
  if (typeof cors.maxAge === "number") out.maxAge = cors.maxAge;
  return out;
}
function extractAllowedHosts(v) {
  if (v === true) return true;
  if (Array.isArray(v)) return v.filter((x) => typeof x === "string");
  return null;
}

// The `css` block as JSON: preprocessorOptions (additionalData, loadPaths, Less
// and Stylus options), devSourcemap, modules. Function-valued options (an
// `additionalData` callback) cannot cross to Rust and are dropped with a warning.
function extractCss(css) {
  if (!css || typeof css !== "object") return null;
  const po = css.preprocessorOptions;
  if (po && typeof po === "object") {
    for (const [lang, opts] of Object.entries(po)) {
      if (opts && typeof opts.additionalData === "function") {
        warn(`css.preprocessorOptions.${lang}.additionalData is a function; only the string form is applied`);
      }
    }
  }
  try {
    const out = JSON.parse(JSON.stringify(css));
    return out && Object.keys(out).length ? out : null;
  } catch {
    return null;
  }
}

function warnUnsupported(c) {
  if (c.esbuild?.jsx === "preserve" || c.oxc?.jsx === "preserve") {
    warn("jsx: \"preserve\" is not supported; JSX is compiled with the automatic runtime");
  }
  if (c.esbuild && typeof c.esbuild === "object") {
    const rest = Object.keys(c.esbuild).filter((k) => !JSX_ESBUILD_KEYS.includes(k));
    if (rest.length) warn(`esbuild options ${rest.join(", ")} are not applied (jsx* are)`);
  }
  if (c.optimizeDeps?.esbuildOptions || c.optimizeDeps?.rollupOptions) {
    warn("optimizeDeps.esbuildOptions/rollupOptions are not applied; include/exclude/entries are");
  }
  if (c.worker) warn("worker config is not applied");
  if (c.ssr?.resolve) warn("ssr.resolve is not applied (noExternal/external/target are)");
  if (c.server?.cors && typeof c.server.cors === "object" && c.server.cors.origin instanceof RegExp) {
    warn("server.cors.origin RegExp is not applied; the localhost default is used");
  }
}

// Whether this module is the entry, compared on the real path rather than the
// spelling. Node canonicalizes the entry module, so import.meta.url is always
// symlink-free while argv[1] is whatever the caller typed -- on macOS a path
// under /var reaches us as /private/var, and the two never match. Getting this
// wrong is silent: the body below is skipped, nothing is written, and the
// process exits 0 as though the config simply had nothing in it.
const isMainRun = (() => {
  const entry = process.argv[1];
  if (!entry) return false;
  const self = fileURLToPath(import.meta.url);
  const real = (p) => {
    try {
      return realpathSync(p);
    } catch {
      return resolve(p);
    }
  };
  return real(self) === real(entry);
})();
export { extractAlias };

if (isMainRun) try {
  const { config, deps } = (await loadConfig()) ?? {};
  const c = config ?? {};
  warnUnsupported(c);
  process.stdout.write(
    JSON.stringify({
      __ok: true,
      __deps: deps ?? [],
      base: typeof c.base === "string" ? c.base : null,
      publicDir: typeof c.publicDir === "string" ? c.publicDir : null,
      port: typeof c.server?.port === "number" ? c.server.port : null,
      host: typeof c.server?.host === "string" ? c.server.host : null,
      hmr: c.server?.hmr === false ? false : null,
      fsAllow: Array.isArray(c.server?.fs?.allow)
        ? c.server.fs.allow.filter((x) => typeof x === "string")
        : null,
      define: c.define && typeof c.define === "object" ? c.define : null,
      alias: extractAlias(c.resolve?.alias),
      headers: stringMap(c.server?.headers),
      proxy: extractProxy(c.server?.proxy),
      rollupOptions: markFunctions(c.build?.rolldownOptions ?? c.build?.rollupOptions ?? null),
      assetsInlineLimit:
        typeof c.build?.assetsInlineLimit === "number" ? c.build.assetsInlineLimit : null,
      dedupe: Array.isArray(c.resolve?.dedupe)
        ? c.resolve.dedupe.filter((x) => typeof x === "string")
        : null,
      optimizeDeps: extractOptimizeDeps(c.optimizeDeps),
      build: extractBuild(c.build),
      oxc: extractOxc(c.oxc),
      esbuild: extractEsbuild(c.esbuild),
      ssr: extractSsr(c.ssr),
      mode: typeof c.mode === "string" ? c.mode : null,
      resolve: extractResolve(c.resolve),
      serverFlags: extractServerFlags(c.server),
      css: extractCss(c.css),
      envPrefix: extractEnvPrefix(c.envPrefix),
      envDir: typeof c.envDir === "string" ? c.envDir : null,
      cors: extractCors(c.server?.cors),
      allowedHosts: extractAllowedHosts(c.server?.allowedHosts),
    }),
  );
} catch (e) {
  process.stderr.write(`oj: could not extract vite.config values: ${(e && e.stack) || e}\n`);
  process.stdout.write("{}");
}
