// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { mkdirSync, rmSync, readFileSync, writeFileSync, renameSync, existsSync, realpathSync } from "node:fs";
import path from "node:path";
import builtinModules from "node:module";

const { root, outDir, entries, include = [], exclude = [], dedupe = [], alias = [], autoDiscover = false, esbuildOptions: rawEsbuildOptions = {} } = JSON.parse(process.argv[2]);

const ESBUILD_OPTION_KEYS = new Set([
  "define", "target", "supported", "loader", "jsx", "jsxDev", "jsxSideEffects",
  "jsxFactory", "jsxFragment", "jsxImportSource", "mainFields", "conditions",
  "resolveExtensions", "preserveSymlinks", "keepNames", "minify", "minifyWhitespace",
  "minifyIdentifiers", "minifySyntax", "treeShaking", "platform", "external", "banner",
  "footer", "inject", "alias", "drop", "pure", "charset", "legalComments", "tsconfig",
  "tsconfigRaw", "ignoreAnnotations",
]);
const esbuildOptions = Object.fromEntries(
  Object.entries(rawEsbuildOptions ?? {}).filter(([k]) => ESBUILD_OPTION_KEYS.has(k)),
);

const DEDUPE = new Set([
  "react",
  "react-dom",
  "react-dom/client",
  "react/jsx-runtime",
  "react/jsx-dev-runtime",
  ...dedupe,
]);

function detectEntries() {
  const html = path.join(root, "index.html");
  if (!existsSync(html)) return [];
  const src = readFileSync(html, "utf8");
  const found = [];
  for (const tag of src.match(/<script\b[^>]*>/gi) || []) {
    if (!/type\s*=\s*["']module["']/i.test(tag)) continue;
    const m = tag.match(/\bsrc\s*=\s*["']([^"']+)["']/i);
    if (!m || /^https?:/i.test(m[1])) continue;
    const abs = path.join(root, m[1].replace(/^\//, ""));
    if (existsSync(abs)) found.push(abs);
  }
  return found;
}

const entryList = entries && entries.length ? entries : detectEntries();

const req = createRequire(path.join(root, "package.json"));
// Vite 8 apps use rolldown for optimizeDeps and don't depend on esbuild directly,
// but esbuild is still a Vite transitive dep -- resolve it from the app's Vite when
// the app itself doesn't expose it, so the dep pre-bundler still runs.
function resolveEsbuild() {
  try {
    return req.resolve("esbuild");
  } catch {}
  const vitePkg = req.resolve("vite/package.json");
  return createRequire(vitePkg).resolve("esbuild");
}
const esbuild = await import(pathToFileURL(resolveEsbuild()).href).then((m) => m.default ?? m);

const NODE_BUILTINS = new Set([...builtinModules.builtinModules, ...builtinModules.builtinModules.map((m) => "node:" + m)]);
const isBare = (id) => id && !id.startsWith(".") && !id.startsWith("/") && !id.startsWith("\0") && !NODE_BUILTINS.has(id);
const excludeSet = new Set(exclude);

// Strip // and /* */ comments from JSONC, but only outside string literals. A
// regex stripper is wrong here: a path pattern like "./src/modules/*" contains
// `/*`, which a naive block-comment regex treats as a comment start and eats
// through the next `*/`, corrupting the file (this silently broke tsconfig
// `paths` loading, so aliased imports were never resolved during the dep scan).
function stripJsonc(s) {
  let out = "";
  let inStr = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    const c2 = s[i + 1];
    if (inStr) {
      out += c;
      if (c === "\\") {
        out += c2 ?? "";
        i++;
      } else if (c === '"') {
        inStr = false;
      }
      continue;
    }
    if (c === '"') {
      inStr = true;
      out += c;
      continue;
    }
    if (c === "/" && c2 === "/") {
      while (i < s.length && s[i] !== "\n") i++;
      out += "\n";
      continue;
    }
    if (c === "/" && c2 === "*") {
      i += 2;
      while (i < s.length && !(s[i] === "*" && s[i + 1] === "/")) i++;
      i++;
      continue;
    }
    out += c;
  }
  return out;
}

function loadTsconfigAliases(dir) {
  for (const name of ["tsconfig.json", "tsconfig.app.json"]) {
    const p = path.join(dir, name);
    if (!existsSync(p)) continue;
    let json;
    try {
      json = JSON.parse(stripJsonc(readFileSync(p, "utf8")));
    } catch {
      continue;
    }
    const co = json.compilerOptions || {};
    if (!co.paths) continue;
    const base = path.resolve(dir, co.baseUrl || ".");
    const out = [];
    for (const [key, targets] of Object.entries(co.paths)) {
      if (!Array.isArray(targets) || !targets.length) continue;
      const t = targets[0];
      if (key.endsWith("/*") && t.endsWith("/*")) {
        out.push({ prefix: key.slice(0, -1), target: path.resolve(base, t.slice(0, -1)) });
      } else {
        out.push({ exact: key, target: path.resolve(base, t) });
      }
    }
    if (out.length) return out;
  }
  return [];
}

const aliasEntries = [
  ...loadTsconfigAliases(root),
  ...(alias || []).map(([find, replacement]) => ({ exact: find, prefix: find + "/", target: replacement })),
];

function aliasResolve(id) {
  for (const a of aliasEntries) {
    if (a.exact && id === a.exact) return a.target;
    if (a.prefix && id.startsWith(a.prefix)) return path.join(a.target, id.slice(a.prefix.length));
  }
  return null;
}

// Match Vite's esbuildDepPlugin: never let esbuild bundle a non-JS type into a
// dep. Styles, preprocessors, wasm, single-file-component types, and known asset
// types (fonts, images, media) are externalized — a relative one resolved to an
// absolute path so its URL is correct, a bare/absolute one kept as-is — and left
// for oj's own asset/CSS pipeline to serve. Without this a dep whose CSS pulls
// in a `.woff2` fails the entire pre-bundle ("No loader is configured").
const EXTERNAL_TYPES = [
  "css", "scss", "sass", "less", "styl", "stylus", "pcss", "postcss",
  "wasm",
  "vue", "svelte", "astro", "imba", "marko",
  "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "avif", "bmp", "cur",
  "woff", "woff2", "ttf", "otf", "eot",
  "mp4", "webm", "ogg", "mp3", "wav", "flac", "aac", "mov", "m4a",
];
const EXTERNAL_RE = new RegExp(`\\.(${EXTERNAL_TYPES.join("|")})(\\?.*)?$`, "i");
const externalizeNonJs = {
  name: "oj-externalize-non-js",
  setup(build) {
    build.onResolve({ filter: EXTERNAL_RE }, (args) => {
      if (args.path.startsWith(".")) {
        return { path: path.resolve(args.resolveDir, args.path), external: true };
      }
      return { path: args.path, external: true };
    });
  },
};

async function scan() {
  const found = new Set();
  const collector = {
    name: "oj-scan",
    setup(build) {
      build.onResolve({ filter: /.*/ }, async (args) => {
        if (args.kind === "entry-point") return null;
        const aliased = aliasResolve(args.path);
        if (aliased) {
          const r = await build.resolve(aliased, { kind: args.kind, resolveDir: root });
          if (r.errors && r.errors.length) return null;
          return { path: r.path, external: r.external };
        }
        if (isBare(args.path) && !excludeSet.has(args.path)) {
          // A specifier with a query (`x?worker`, `x?url`, `x?raw`) is a special
          // import handled by oj's worker/asset pipeline, not a plain dep — never
          // pre-bundle it (the optimized-dep URL would 404). Externalize it so it
          // is served directly. Vite excludes queried imports from the optimizer.
          if (!args.path.includes("?")) {
            found.add(args.path);
          }
          return { path: args.path, external: true };
        }
        if (!args.path.startsWith(".") && !path.isAbsolute(args.path)) return { path: args.path, external: true };
        return null;
      });
    },
  };
  try {
    await esbuild.build({
      jsx: "automatic",
      ...esbuildOptions,
      entryPoints: entryList,
      bundle: true,
      write: false,
      logLevel: "silent",
      platform: esbuildOptions.platform ?? "browser",
      loader: { ".js": "jsx", ".ts": "ts", ".tsx": "tsx", ".jsx": "jsx", ...(esbuildOptions.loader ?? {}) },
      plugins: [...(esbuildOptions.plugins ?? []), externalizeNonJs, collector],
      metafile: false,
    });
  } catch {}
  return found;
}

// Only pre-bundle the explicit optimizeDeps.include list by default (a small,
// author-vetted, well-behaved set). Full-graph auto-discovery via the esbuild
// scan is opt-in: converting an entire app's CommonJS/UMD dependency tree to ESM
// with esbuild has a real interop tail (UMD `this`->void 0, cross-dep shape) that
// can break an app, so oj's robust per-module wrap_cjs serves undiscovered deps
// instead. See oj-native partial bundling for the eventual request-count fix.
const scanned = autoDiscover ? await scan() : new Set();
for (const inc of include) scanned.add(inc);
const deps = [...scanned].filter((d) => !excludeSet.has(d));

const entryPoints = {};
const nameOf = {};
for (const dep of deps) {
  // Vite's nested-dependency syntax: "a > b" pre-bundles the copy of `b` nested
  // inside `a` (each segment resolved from the previous one's directory). The
  // optimized dep registers under the LAST segment, which is what the app imports;
  // esbuild gets the concrete resolved file for that nested copy. A broken nested
  // include is dropped, not allowed to fail the whole pre-bundle.
  if (dep.includes(">")) {
    const parts = dep.split(">").map((s) => s.trim()).filter(Boolean);
    let resolved;
    let fromDir;
    try {
      for (const part of parts) {
        resolved = fromDir ? req.resolve(part, { paths: [fromDir] }) : req.resolve(part);
        fromDir = path.dirname(resolved);
      }
    } catch {
      continue;
    }
    const last = parts[parts.length - 1];
    const name = last.replace(/^@/, "").replace(/[^\w.-]/g, "_");
    entryPoints[name] = resolved;
    nameOf[last] = name;
    continue;
  }
  // A package.json `#imports` subpath (e.g. `#shared/i18n/compiled/messages`)
  // resolves to a file INSIDE the project, not a node_modules dep. Vite's optimizer
  // targets node_modules; it does not pre-bundle project source, and neither should
  // oj. These are served as source and handled by the app's own plugins (the
  // i18n-dev `load` hook collapses the message barrel into grouped virtual modules),
  // so pre-bundling them would both fight that plugin and split the module instance
  // between SSR and client.
  if (dep.startsWith("#")) continue;
  let entry;
  try {
    entry = req.resolve(dep);
  } catch {
    continue;
  }
  // Skip linked / workspace packages (symlinked into node_modules): pre-bundling
  // freezes their source so edits to them stop HMR-ing. Vite excludes these too.
  // An explicit optimizeDeps.include still forces optimization.
  if (!include.includes(dep)) {
    let real = entry;
    try {
      real = realpathSync(entry);
    } catch {}
    if (!real.split(path.sep).includes("node_modules")) {
      continue;
    }
  }
  // The lingui macro entrypoints are served by oj's runtime shim, never bundled:
  // pre-bundling them would drag in the whole babel macro toolchain (which
  // imports node builtins) and, worse, route the specifier to the optimized dep
  // instead of the shim. oj externalizes these at serve time too.
  if (/^@lingui\/(macro|core\/macro|react\/macro)$/.test(dep)) {
    continue;
  }
  // Only pre-bundle JavaScript deps. A CSS-only dep (e.g. `@fontsource/*`) is
  // not a JS module: esbuild would emit a stray .css and choke on the fonts its
  // @font-face rules pull in, failing the *whole* build. Vite excludes these
  // from the dep optimizer too; oj serves them directly through its CSS pipeline.
  if (/\.(css|scss|sass|less|styl|woff2?|ttf|otf|eot|svg|png|jpe?g|gif|webp|avif|mp4|webm|wasm)$/i.test(entry)) {
    continue;
  }
  const name = dep.replace(/^@/, "").replace(/[^\w.-]/g, "_");
  // Hand esbuild the BARE specifier, not the Node-resolved path: `req.resolve`
  // picks the `require`/`node` condition (uuid's ./dist/cjs, which `require`s
  // node crypto), and bundling that fixed file skips browser resolution. A bare
  // entry lets esbuild's platform:"browser" pick the browser build. Vite does
  // the same via its browser-aware resolver in esbuildDepPlugin.
  entryPoints[name] = dep;
  nameOf[dep] = name;
}

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

const metadata = {};
if (Object.keys(entryPoints).length) {
  const result = await esbuild.build({
    mainFields: ["browser", "module", "main"],
    conditions: ["browser", "module", "import", "development"],
    target: "esnext",
    ...esbuildOptions,
    entryPoints,
    absWorkingDir: root,
    bundle: true,
    splitting: true,
    format: "esm",
    outdir: outDir,
    outExtension: { ".js": ".mjs" },
    platform: esbuildOptions.platform ?? "browser",
    define: { "process.env.NODE_ENV": JSON.stringify("development"), ...(esbuildOptions.define ?? {}) },
    // A node-oriented dep (e.g. @react-pdf/renderer, cosmiconfig) may import a
    // node builtin; externalize them so one such dep can't fail the whole
    // pre-bundle. The import stays in the output and oj serves a browser stub,
    // matching Vite's esbuildDepPlugin, which also externalizes builtins.
    external: [...NODE_BUILTINS, ...(esbuildOptions.external ?? [])],
    plugins: [...(esbuildOptions.plugins ?? []), externalizeNonJs],
    logLevel: "silent",
    metafile: true,
    write: true,
  });
  const exportsOf = {};
  for (const [out, meta] of Object.entries(result.metafile.outputs)) {
    if (meta.entryPoint) exportsOf[path.basename(out)] = meta.exports || [];
  }
  const IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;
  function namedExportsOf(dep) {
    try {
      const m = req(dep);
      const mod = m && m.__esModule && m.default && typeof m.default === "object" ? m.default : m;
      if (!mod || (typeof mod !== "object" && typeof mod !== "function")) return [];
      return [...new Set(Object.keys(mod))].filter((k) => k !== "default" && k !== "__esModule" && IDENT.test(k));
    } catch {
      return [];
    }
  }
  for (const [dep, name] of Object.entries(nameOf)) {
    const file = `${name}.mjs`;
    const exports = exportsOf[file] || [];
    const bundledInterop = exports.length === 0 || (exports.length === 1 && exports[0] === "default");
    if (bundledInterop && DEDUPE.has(dep)) {
      const names = namedExportsOf(dep);
      if (names.length) {
        const cjsFile = `${name}-cjs.mjs`;
        renameSync(path.join(outDir, file), path.join(outDir, cjsFile));
        const proxy =
          `import __m from "./${cjsFile}";\n` +
          `export default __m;\n` +
          `export const __cjs_exports = __m;\n` +
          `export const { ${names.join(", ")} } = __m;\n`;
        writeFileSync(path.join(outDir, file), proxy);
        metadata[dep] = { file, needsInterop: false, exports: ["default", ...names] };
        continue;
      }
    }
    metadata[dep] = { file, needsInterop: bundledInterop, exports };
  }
}

process.stdout.write(JSON.stringify({ metadata }));
