// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire, isBuiltin } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, isAbsolute, resolve } from "node:path";
import { writeFileSync, readFileSync, realpathSync, existsSync, unlinkSync } from "node:fs";

const configPath = process.argv[2];
const appRoot = process.argv[3];
const command = process.argv[4] || "serve";
const mode = process.argv[5] || "development";
// "default": the mode is the command's default, not a CLI `--mode`, so a `mode`
// named by the config file may win (Vite: inlineConfig.mode || config.mode).
const modeExplicit = process.argv[6] !== "default";
// Where the extracted JSON goes. A file, not stdout: evaluating the config runs
// plugin code (a route generator, a banner) that may print to stdout, which
// used to corrupt the JSON the caller parses.
const resultPath = process.argv[7] || null;

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

// Vite's `externalize-deps` config-bundling plugin, mirrored (one copy lives
// in vite-extract.mjs, its twin in plugin-host.mjs; both assets are
// deliberately self-contained -- keep the two copies byte-identical).
// `packages: "external"` kept bare specifiers bare, so the bundle -- imported
// from the cache dir -- re-resolved them from there at import time. A config
// that relatively imports a sibling workspace package's source (a monorepo
// shape) inlines that source, whose bare deps live under the sibling's own
// node_modules, unreachable from the cache dir. Resolve every bare import from
// its importer at bundle time and externalize the resolved absolute path as a
// file:// URL instead (the output is always ESM, so an external require()
// would only reach esbuild's throwing __require shim). First-party
// resolutions -- TS/TSX sources, which Node cannot import, and anything
// outside node_modules, e.g. a tsconfig-paths alias -- are bundled like
// relative imports, as is .json (an externalized ESM .json import would need
// an import attribute).
function externalizeDepsPlugin() {
  const resolving = Symbol("oj-externalize-deps");
  const warned = new Set();
  return {
    name: "externalize-deps",
    setup(build) {
      build.onResolve({ filter: /^[^./#]/ }, async (args) => {
        // Re-entrant call from build.resolve below: let esbuild resolve it.
        if (args.pluginData === resolving) return null;
        const { path: id, importer, kind } = args;
        if (!importer || isAbsolute(id)) return null;
        if (id.startsWith("node:") || id.startsWith("bun:") || isBuiltin(id)) {
          return { path: id, external: true };
        }
        // npm: specifiers stay bare-external (Vite keeps them bare too); any
        // other scheme-shaped id (data:, ...) is esbuild's to handle natively
        // -- pathToFileURL on those would produce garbage.
        if (id.startsWith("npm:")) return { path: id, external: true };
        if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(id)) return null;
        let resolved = null;
        try {
          resolved = await build.resolve(id, {
            importer,
            resolveDir: dirname(importer),
            kind,
            pluginData: resolving,
          });
        } catch {}
        if (!resolved || resolved.errors.length > 0 || !resolved.path) {
          // Unresolvable here; keep the bare id external (the old behavior) --
          // it may still resolve at import time, and failing the bundle would
          // regress configs that load today. Say so once per id: if it does
          // fail at import time, that error names a deleted tmp bundle, not
          // the file that wrote the import.
          if (!warned.has(id)) {
            warned.add(id);
            const detail = resolved?.errors?.[0]?.text ? ` (${resolved.errors[0].text})` : "";
            process.stderr.write(
              `oj: vite.config: could not resolve "${id}" imported from ${importer}${detail}; kept as a bare import that must resolve when the bundled config loads\n`,
            );
          }
          return { path: id, external: true };
        }
        if (resolved.external) return { path: resolved.path, external: true };
        // Another resolver's non-file namespace is not ours to externalize.
        if (resolved.namespace && resolved.namespace !== "file") return null;
        if (
          /\.(?:ts|tsx|mts|cts)$/.test(resolved.path) ||
          !/[\\/]node_modules[\\/]/.test(resolved.path) ||
          resolved.path.endsWith(".json")
        ) {
          return { path: resolved.path };
        }
        return { path: pathToFileURL(resolved.path).href, external: true };
      });
    },
  };
}

// Vite's `inject-file-scope-variables` config-bundling plugin, mirrored (same
// two byte-identical copies as externalizeDepsPlugin above). The build defines
// `__dirname` / `__filename` / `import.meta.url` (and `import.meta.dirname` /
// `.filename`) as `__vite_injected_original_*` identifiers, and this onLoad
// prepends per-file consts carrying THAT file's real location, so a bundle
// imported from the cache dir still sees the original paths -- for every
// inlined file, not just the config entry. esbuild scopes and renames the
// consts per module, so each file keeps its own values.
const CONFIG_BUNDLE_DEFINES = {
  __dirname: "__vite_injected_original_dirname",
  __filename: "__vite_injected_original_filename",
  "import.meta.url": "__vite_injected_original_import_meta_url",
  "import.meta.dirname": "__vite_injected_original_dirname",
  "import.meta.filename": "__vite_injected_original_filename",
};
function injectFileScopeVariablesPlugin() {
  return {
    name: "inject-file-scope-variables",
    setup(build) {
      build.onLoad({ filter: /\.[cm]?[jt]sx?$/, namespace: "file" }, (args) => {
        const contents = readFileSync(args.path, "utf8");
        const inject =
          `const __vite_injected_original_dirname = ${JSON.stringify(dirname(args.path))};` +
          `const __vite_injected_original_filename = ${JSON.stringify(args.path)};` +
          `const __vite_injected_original_import_meta_url = ${JSON.stringify(pathToFileURL(args.path).href)};`;
        let code;
        if (contents.startsWith("#!")) {
          const nl = contents.indexOf("\n");
          code = nl === -1 ? contents + "\n" + inject : contents.slice(0, nl + 1) + inject + contents.slice(nl + 1);
        } else {
          code = inject + contents;
        }
        const loader = args.path.endsWith(".tsx")
          ? "tsx"
          : /\.(?:ts|mts|cts)$/.test(args.path)
            ? "ts"
            : args.path.endsWith(".jsx")
              ? "jsx"
              : "js";
        return { contents: code, loader };
      });
    },
  };
}

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
            // The user's own file, for the "not applied" warnings: the resolved
            // config carries Vite's defaults for every option (esbuild.jsxDev,
            // worker, ssr.resolve, cors.origin, terserOptions...), which are not
            // configuration oj is failing to honor.
            let raw = null;
            try {
              raw = (await vite.loadConfigFromFile({ command, mode }, configPath, appRoot))?.config ?? null;
            } catch {}
            return { config: resolved, raw, deps: absDeps(resolved.configFileDependencies) };
          }
        } catch {
          // fall through to the raw loader below
        }
      }
      if (typeof vite.loadConfigFromFile === "function") {
        const loaded = await vite.loadConfigFromFile({ command, mode }, configPath, appRoot);
        if (loaded && loaded.config) {
          return { config: loaded.config, raw: loaded.config, deps: absDeps(loaded.dependencies) };
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
      // Node's resolver, not esbuild's bundler defaults: `conditions` pins
      // "node" (import/require still apply per kind) and drops esbuild's
      // implicit "module" condition; `mainFields` skips "module" as Node does.
      conditions: ["node"], mainFields: ["main"],
      plugins: [externalizeDepsPlugin(), injectFileScopeVariablesPlugin()],
      write: false, logLevel: "silent", absWorkingDir: appRoot,
      metafile: true,
      define: CONFIG_BUNDLE_DEFINES,
    });
    // A unique path per process: the plugin host bundles the same config into
    // this directory concurrently at boot, and two writers on one filename made
    // either importer see a truncated bundle.
    const out = resolve(
      dirname(fileURLToPath(import.meta.url)),
      `oj-vite-config-${process.pid}-${Math.random().toString(36).slice(2)}.tmp.mjs`,
    );
    writeFileSync(out, r.outputFiles[0].text);
    let m;
    try {
      m = await import(pathToFileURL(out).href);
    } finally {
      try { unlinkSync(out); } catch {}
    }
    return {
      config: typeof m.default === "function" ? await m.default({ command, mode }) : m.default,
      deps: absDeps(Object.keys(r.metafile?.inputs ?? {})),
    };
  }
  const m = await import(pathToFileURL(configPath).href);
  return {
    config: typeof m.default === "function" ? await m.default({ command, mode }) : m.default,
    deps: relativeImportDeps(configPath),
  };
}

// A plain JS config loaded straight by Node has no bundler metafile to name its
// imports, so walk its relative `import` specifiers (Vite's
// configFileDependencies for the same file): the dev server restarts when one
// of those files changes, as it does for the config itself.
function relativeImportDeps(entry) {
  const seen = new Set();
  const stack = [entry];
  const spec = /(?:\bfrom\s*|\bimport\s*\(?\s*)["'](\.{1,2}\/[^"']+)["']/g;
  while (stack.length) {
    const file = stack.pop();
    if (seen.has(file)) continue;
    seen.add(file);
    let src;
    try { src = readFileSync(file, "utf8"); } catch { continue; }
    for (const m of src.matchAll(spec)) {
      const dep = resolve(dirname(file), m[1]);
      if (!seen.has(dep) && existsSync(dep)) stack.push(dep);
    }
  }
  seen.delete(entry);
  return [...seen];
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

const VITE_CLIENT_ALIAS = /^\^\\\/\?@vite\\\/(env|client)$/;

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
      // Vite adds these two for its own client (config.ts clientAlias); oj serves
      // /@vite/client and /@vite/env itself, so they are not user aliases to warn about.
      if (VITE_CLIENT_ALIAS.test(find.source)) continue;
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
      if (typeof v.secure === "boolean") entry.secure = v.secure;
      if (typeof v.rewriteWsOrigin === "boolean") entry.rewriteWsOrigin = v.rewriteWsOrigin;
      // Function-valued options cannot cross the config bridge (oj reads the
      // extracted config as JSON): the entry still proxies, without them.
      if (typeof v.rewrite === "function") {
        warn(`server.proxy["${ctx}"].rewrite is a function; oj applies only {from,to} string rewrites`);
      }
      for (const fn of ["configure", "bypass"]) {
        if (typeof v[fn] === "function") {
          warn(`server.proxy["${ctx}"].${fn} is a function and cannot cross the config bridge; ignored`);
        }
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
  const interop = strArr(od.needsInterop);
  if (inc) out.include = inc;
  if (exc) out.exclude = exc;
  if (ent) out.entries = ent;
  if (interop) out.needsInterop = interop;
  if (typeof od.force === "boolean") out.force = od.force;
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
  if (typeof b.cssCodeSplit === "boolean") out.cssCodeSplit = b.cssCodeSplit;
  if (typeof b.target === "string") out.target = b.target;
  else if (Array.isArray(b.target)) out.target = b.target.filter((t) => typeof t === "string");
  else if (b.target === false) warn("build.target false is not supported; the default baseline is used");
  if (typeof b.cssTarget === "string") out.cssTarget = b.cssTarget;
  else if (Array.isArray(b.cssTarget)) out.cssTarget = b.cssTarget.filter((t) => typeof t === "string");
  else if (b.cssTarget === false) warn("build.cssTarget false is not supported; build.target is used");
  if (typeof b.cssMinify === "boolean" || typeof b.cssMinify === "string") out.cssMinify = b.cssMinify;
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
  if (typeof b.manifest === "string" || typeof b.manifest === "boolean") out.manifest = b.manifest;
  if (typeof b.cssMinify === "boolean" || typeof b.cssMinify === "string") out.cssMinify = b.cssMinify;
  if (typeof b.assetsDir === "string") out.assetsDir = b.assetsDir;
  if (typeof b.reportCompressedSize === "boolean") out.reportCompressedSize = b.reportCompressedSize;
  if (typeof b.chunkSizeWarningLimit === "number") out.chunkSizeWarningLimit = b.chunkSizeWarningLimit;
  if (b.write === false) out.write = false;
  if (b.watch && typeof b.watch === "object") out.watch = {};
  if (b.license && b.license !== false) out.license = true;
  // Resolved defaults (cssTarget = target, commonjsOptions = { include:
  // [/node_modules/], extensions: [".js", ".cjs"] }) are not user choices.
  if (b.cssTarget !== undefined && JSON.stringify(b.cssTarget) !== JSON.stringify(b.target)) out.cssTarget = b.cssTarget;
  const cjs = b.commonjsOptions;
  if (cjs && typeof cjs === "object") {
    const extra = Object.keys(cjs).filter((k) => k !== "include" && k !== "extensions");
    const defaultInclude = Array.isArray(cjs.include) && cjs.include.length === 1 && String(cjs.include[0]) === "/node_modules/";
    const defaultExt = JSON.stringify(cjs.extensions) === JSON.stringify([".js", ".cjs"]);
    if (extra.length || !defaultInclude || !defaultExt) out.commonjsOptions = {};
  }
  if (b.lib && typeof b.lib === "object") {
    const lib = {};
    const e = b.lib.entry;
    const isStrList = (v) => Array.isArray(v) && v.every((s) => typeof s === "string");
    const isStrMap = (v) => v && typeof v === "object" && !Array.isArray(v) && Object.values(v).every((s) => typeof s === "string");
    if (typeof e === "string" || isStrList(e) || isStrMap(e)) lib.entry = e;
    if (typeof b.lib.name === "string") lib.name = b.lib.name;
    if (isStrList(b.lib.formats)) lib.formats = b.lib.formats;
    if (typeof b.lib.fileName === "string") lib.fileName = b.lib.fileName;
    else if (typeof b.lib.fileName === "function") warn("build.lib.fileName is a function and cannot be applied; the default file name is used");
    if (typeof b.lib.cssFileName === "string") lib.cssFileName = b.lib.cssFileName;
    if (lib.entry) out.lib = lib;
    else warn("build.lib.entry is required when build.lib is set");
  }
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
function extractSsr(ssr, ssrEnvironment) {
  // Vite treats `ssr.*` as sugar for `environments.ssr.*`; the environment
  // spelling wins where both name the same option.
  const envResolve = ssrEnvironment && typeof ssrEnvironment === "object" ? ssrEnvironment.resolve : null;
  const merged = { ...(ssr && typeof ssr === "object" ? ssr : {}) };
  if (envResolve || merged.resolve) merged.resolve = { ...(merged.resolve ?? {}), ...(envResolve ?? {}) };
  ssr = Object.keys(merged).length ? merged : null;
  if (!ssr) return null;
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
  const res = extractResolve(ssr.resolve);
  if (res) out.resolve = res;
  return Object.keys(out).length ? out : null;
}
function extractResolve(r) {
  if (!r || typeof r !== "object") return null;
  const out = {};
  const strArr = (v) => (Array.isArray(v) ? v.filter((x) => typeof x === "string") : null);
  for (const k of ["extensions", "mainFields", "conditions", "externalConditions"]) {
    const v = strArr(r[k]);
    if (v && v.length) out[k] = v;
  }
  if (typeof r.preserveSymlinks === "boolean") out.preserveSymlinks = r.preserveSymlinks;
  return Object.keys(out).length ? out : null;
}
function extractServerFlags(s, legacy, appType) {
  const out = {};
  if (appType === "spa" || appType === "mpa" || appType === "custom") out.appType = appType;
  if (s && typeof s === "object") {
    if (typeof s.strictPort === "boolean") out.strictPort = s.strictPort;
    // Vite admits `open: true | string`; oj opens the served url in both cases.
    if (s.open === true || typeof s.open === "string") out.open = true;
    else if (s.open === false) out.open = false;
    // server.hmr object options reach the served client (Vite's clientInjections).
    if (s.hmr && typeof s.hmr === "object") {
      const h = {};
      for (const k of ["path", "host", "protocol"]) if (typeof s.hmr[k] === "string") h[k] = s.hmr[k];
      for (const k of ["port", "clientPort", "timeout"]) if (typeof s.hmr[k] === "number") h[k] = s.hmr[k];
      if (typeof s.hmr.overlay === "boolean") h.overlay = s.hmr.overlay;
      if (Object.keys(h).length) out.hmr = h;
    }
    if (s.fs && typeof s.fs === "object" && typeof s.fs.strict === "boolean") out.fsStrict = s.fs.strict;
    // server.watch.ignored: string globs only (RegExp/functions cannot cross the bridge).
    if (s.watch && typeof s.watch === "object" && s.watch.ignored != null) {
      const raw = Array.isArray(s.watch.ignored) ? s.watch.ignored : [s.watch.ignored];
      const ignored = raw.filter((x) => typeof x === "string");
      if (ignored.length) out.watch = { ignored };
      if (ignored.length !== raw.length) {
        warn("server.watch.ignored RegExp or function entries are not applied (string globs are)");
      }
    }
  }
  if (legacy?.skipWebSocketTokenCheck === true) out.skipWebSocketTokenCheck = true;
  return Object.keys(out).length ? out : null;
}
// `preview.*` as resolved by Vite (inheriting `server.*` except the port).
function extractPreview(p) {
  if (!p || typeof p !== "object") return null;
  const out = {};
  if (typeof p.port === "number") out.port = p.port;
  if (typeof p.host === "string") out.host = p.host;
  else if (p.host === true) out.host = "true";
  if (typeof p.strictPort === "boolean") out.strictPort = p.strictPort;
  if (p.open === true || typeof p.open === "string") out.open = p.open;
  const cors = extractCors(p.cors);
  if (cors !== null && cors !== undefined) out.cors = cors;
  const hosts = extractAllowedHosts(p.allowedHosts);
  if (hosts !== null && hosts !== undefined) out.allowedHosts = hosts;
  const headers = stringMap(p.headers);
  if (headers) out.headers = headers;
  if (p.proxy && typeof p.proxy === "object" && Object.keys(p.proxy).length) out.proxy = {};
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
  if (css.modules && typeof css.modules === "object") {
    for (const k of ["localsConvention", "generateScopedName"]) {
      if (typeof css.modules[k] === "function") warn(`css.modules.${k} is a function; only the string form is applied`);
    }
    if (typeof css.modules.getJSON === "function") warn("css.modules.getJSON is not applied");
  }
  try {
    const out = JSON.parse(JSON.stringify(css));
    // RegExps (globalModulePaths) and functions do not survive JSON; mark them
    // the way rollup options are marked so Rust can read the sources.
    if (css.modules && typeof css.modules === "object") out.modules = markFunctions(css.modules);
    return out && Object.keys(out).length ? out : null;
  } catch {
    return null;
  }
}

// Warns about the options in the USER's config that oj does not apply. Call it
// with the raw config file's export, not the resolved config: Vite's resolveConfig
// fills every option with defaults (esbuild.jsxDev/charset/legalComments, worker,
// ssr.resolve, cors.origin, optimizeDeps.esbuildOptions, terserOptions) and none of
// those are configuration to warn about.
function warnUnsupported(c) {
  if (!c || typeof c !== "object") return;
  if (c.build?.terserOptions) warn("build.terserOptions is not applied (oj minifies with oxc)");
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
  if (typeof c.build?.assetsInlineLimit === "function") {
    warn("build.assetsInlineLimit is a function and cannot be applied; the 4096 byte default is used");
  }
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
export { extractAlias, extractOptimizeDeps, extractProxy, extractResolve, extractSsr, warnUnsupported };

const emitResult = (json) => {
  if (resultPath) writeFileSync(resultPath, json);
  else process.stdout.write(json);
};

if (isMainRun) try {
  const { config, raw, deps } = (await loadConfig()) ?? {};
  const c = config ?? {};
  warnUnsupported(raw ?? c);
  emitResult(
    JSON.stringify({
      __ok: true,
      __deps: deps ?? [],
      base: typeof c.base === "string" ? c.base : null,
      publicDir: typeof c.publicDir === "string" ? c.publicDir : c.publicDir === false ? false : null,
      port: typeof c.server?.port === "number" ? c.server.port : null,
      host: typeof c.server?.host === "string" ? c.server.host : null,
      hmr: c.server?.hmr === false ? false : null,
      fsAllow: Array.isArray(c.server?.fs?.allow)
        ? c.server.fs.allow.filter((x) => typeof x === "string")
        : null,
      fsStrict: typeof c.server?.fs?.strict === "boolean" ? c.server.fs.strict : null,
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
      ssr: extractSsr(c.ssr, c.environments?.ssr),
      mode: typeof c.mode === "string" ? c.mode : null,
      resolve: extractResolve(c.resolve),
      serverFlags: extractServerFlags(c.server, c.legacy, c.appType),
      css: extractCss(c.css),
      envPrefix: extractEnvPrefix(c.envPrefix),
      envDir: typeof c.envDir === "string" ? c.envDir : null,
      cors: extractCors(c.server?.cors),
      allowedHosts: extractAllowedHosts(c.server?.allowedHosts),
      preview: extractPreview(c.preview),
      appType: typeof c.appType === "string" ? c.appType : null,
      html: typeof c.html?.cspNonce === "string" ? { cspNonce: c.html.cspNonce } : null,
    }),
  );
} catch (e) {
  process.stderr.write(`oj: could not extract vite.config values: ${(e && e.stack) || e}\n`);
  emitResult("{}");
}
