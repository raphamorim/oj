// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { writeFileSync, readFileSync } from "node:fs";

const configPath = process.argv[2];
const appRoot = process.argv[3];
const command = process.argv[4] || "serve";
const mode = process.argv[5] || "development";

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
          const resolved = await vite.resolveConfig(
            { root: appRoot, configFile: configPath, mode },
            command,
            mode,
            mode,
          );
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
      out[find] = replacement;
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
  if (typeof b.sourcemap === "boolean") out.sourcemap = b.sourcemap;
  else if (typeof b.sourcemap === "string") {
    warn(`build.sourcemap "${b.sourcemap}" is applied as a regular sourcemap`);
    out.sourcemap = true;
  }
  if (typeof b.minify === "boolean") out.minify = b.minify;
  else if (typeof b.minify === "string") out.minify = true;
  if (typeof b.cssCodeSplit === "boolean") out.cssCodeSplit = b.cssCodeSplit;
  if (typeof b.target === "string") {
    if (b.target !== "modules" && b.target !== "baseline-widely-available") out.target = b.target;
  } else if (Array.isArray(b.target)) {
    warn("build.target array form is not applied; set a single target");
  }
  if (typeof b.ssr === "string") out.ssr = b.ssr;
  return Object.keys(out).length ? out : null;
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
  if (c.ssr) warn("ssr config is not applied");
  for (const k of ["strictPort", "open", "cors", "allowedHosts"]) {
    if (c.server?.[k] !== undefined) warn(`server.${k} is accepted but not applied`);
  }
}

const isMainRun = import.meta.url === pathToFileURL(process.argv[1] || "").href;
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
      rollupOptions: c.build?.rolldownOptions ?? c.build?.rollupOptions ?? null,
      assetsInlineLimit:
        typeof c.build?.assetsInlineLimit === "number" ? c.build.assetsInlineLimit : null,
      dedupe: Array.isArray(c.resolve?.dedupe)
        ? c.resolve.dedupe.filter((x) => typeof x === "string")
        : null,
      optimizeDeps: extractOptimizeDeps(c.optimizeDeps),
      build: extractBuild(c.build),
      oxc: extractOxc(c.oxc),
      esbuild: extractEsbuild(c.esbuild),
      css: extractCss(c.css),
    }),
  );
} catch (e) {
  process.stderr.write(`oj: could not extract vite.config values: ${(e && e.stack) || e}\n`);
  process.stdout.write("{}");
}
