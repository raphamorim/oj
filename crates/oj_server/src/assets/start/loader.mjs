// SPDX-License-Identifier: MIT
// Node ESM loader hook: resolves TanStack Start's four framework aliases,
// compiles TS/JSX (via the app's esbuild), and probes extensions for
// bundler-style extensionless imports. Registered by runner.mjs.
import { pathToFileURL, fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";
import { resolve as pathResolve, dirname } from "node:path";
import { importPkg, viteEnvDefine } from "./resolve-pkg.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";
import {
  EXTS, isFile, JS_TO_TS, probe, RESERVED, nearestPkgType,
  hasEsmSyntax, isCjsFile, cjsFacade, stripJsonc, readJsonc,
} from "./loader-util.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const { transformSync } = await importPkg(APP, "esbuild", ["vite", "@tanstack/react-start"]);
// The app's vite.config plugin container, for `virtual:*` ids on the SSR side
// (environment "ssr"). Null when there's no config/Vite; then virtuals just
// fall through to normal resolution as before.
const container = await loadPluginContainer(APP, { command: "serve", environment: "ssr" });
const VIRTUAL_SCHEME = "ojvirtual:///";

// Warm-reload versioning. On a dev rebuild the runner bumps V (over a
// MessagePort) and re-imports with `?ojv=V`. App files and `@tanstack/*`
// modules (ESM) get the query, so they re-evaluate fresh (resetting the
// framework's module-scope caches like `entriesPromise`); React and other
// node_modules stay unversioned and warm, keeping a single React instance.
let V = 0;
export async function initialize(data) {
  if (data && data.port) data.port.on("message", (v) => (V = v));
}
const stripQ = (u) => u.split("?")[0];
const withV = (u) => (V ? `${stripQ(u)}?ojv=${V}` : stripQ(u));
const isTanstack = (u) => /\/@tanstack\//.test(u) && /\.(js|mjs)$/.test(stripQ(u));
const ASSET_SUFFIX = /\?(raw|url|inline)$/;
// `.svg` handled separately (svgr may make it a component); see load() below.
const ASSET_EXT = /\.(png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf|eot|mp4|webm|wasm)$/;

const ALIASES = {
  "#tanstack-router-entry": pathResolve(APP, "src/router"),
  "#tanstack-start-entry": pathResolve(HERE, "start-entry.ts"),
  "#tanstack-start-plugin-adapters": pathResolve(HERE, "plugin-adapters.ts"),
  "#tanstack-start-server-fn-resolver": pathResolve(HERE, "server-fn-resolver.mjs"),
  "tanstack-start-manifest:v": pathResolve(HERE, "manifest.ts"),
  // The Cloudflare Vite plugin injects this virtual module; supply a dev shim
  // exposing getCloudflareContext() with the wrangler `vars` + `.dev.vars`.
  "@cloudflare/vite-plugin/server": pathResolve(HERE, "cf-server.mjs"),
};

// App package.json "imports" subpath map (e.g. "#shared/*" -> "./shared/*").
// Node resolves these but never probes extensions, and the app writes
// extensionless, bundler-style imports -- so mirror the map here with probing.
const IMPORT_RULES = (() => {
  try {
    const imports = JSON.parse(readFileSync(pathResolve(APP, "package.json"), "utf8")).imports ?? {};
    return Object.entries(imports)
      .map(([pattern, target]) => [pattern, typeof target === "string" ? target : target?.import ?? target?.default ?? target?.node])
      .filter(([, t]) => typeof t === "string");
  } catch {
    return [];
  }
})();
function resolveImports(spec) {
  for (const [pattern, target] of IMPORT_RULES) {
    if (pattern.endsWith("/*") && target.endsWith("/*")) {
      const pfx = pattern.slice(0, -1);
      if (spec.startsWith(pfx)) {
        const hit = probe(pathResolve(APP, target.slice(0, -1) + spec.slice(pfx.length)));
        if (hit) return hit;
      }
    } else if (spec === pattern) {
      const hit = probe(pathResolve(APP, target));
      if (hit) return hit;
    }
  }
  return null;
}

// tsconfig `paths` aliases (e.g. "@platform/auth" -> "./lib/auth/adapter.ts").
// esbuild reads these natively for the client bundle; Node's SSR resolver does
// not, so mirror them here (tolerant JSONC parse, follow `extends`, probe).
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
  let paths = {}, baseDir = APP;
  for (const { cfg, dir } of chain) {
    const co = cfg.compilerOptions || {};
    if (co.paths) paths = { ...paths, ...co.paths };
    baseDir = co.baseUrl != null ? pathResolve(dir, co.baseUrl) : dir;
  }
  return { rules: Object.entries(paths).map(([k, v]) => [k, Array.isArray(v) ? v : [v]]), baseDir };
})();
function resolveTsPaths(spec) {
  for (const [pattern, targets] of TS.rules) {
    if (pattern.includes("*")) {
      const [pre, post = ""] = pattern.split("*");
      if (spec.startsWith(pre) && spec.endsWith(post) && spec.length >= pre.length + post.length) {
        const mid = spec.slice(pre.length, spec.length - post.length);
        for (const t of targets) {
          const hit = probe(pathResolve(TS.baseDir, t.replace("*", mid)));
          if (hit) return hit;
        }
      }
    } else if (spec === pattern) {
      for (const t of targets) {
        const hit = probe(pathResolve(TS.baseDir, t));
        if (hit) return hit;
      }
    }
  }
  return null;
}

export async function resolve(spec, context, next) {
  if (context.parentURL) context = { ...context, parentURL: stripQ(context.parentURL) };
  // Plugin-owned virtual modules (virtual:* or a \0-prefixed resolved id from
  // within loaded virtual code): resolve via the vite plugin container and
  // carry the resolved id in a custom scheme that load() below serves.
  if (container && (spec.startsWith("virtual:") || spec.startsWith("\0"))) {
    const importer = context.parentURL && context.parentURL.startsWith("file:")
      ? fileURLToPath(context.parentURL)
      : undefined;
    const rid = await container.resolveId(spec, importer);
    if (rid != null) return { url: VIRTUAL_SCHEME + encodeURIComponent(rid), shortCircuit: true };
  }
  // Asset conventions: resolve the underlying file, then tag the URL so load()
  // returns a string / URL / data URI (?url mirrors the client's /@oj-start/fs
  // path) or a no-op for side-effect CSS (styling is a client concern in SSR).
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
      try { abs = fileURLToPath(stripQ((await next(clean, context)).url)); } catch {}
    }
    if (abs) return { url: pathToFileURL(abs).href + `?ojasset=${kind}`, shortCircuit: true };
  }
  // svgr's explicit-component query: resolve the real .svg, then tag it so
  // load() runs svgr with the ?react id (svgr keys on the query for a component).
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
      try { abs = fileURLToPath(stripQ((await next(clean, context)).url)); } catch {}
    }
    if (abs) return { url: pathToFileURL(abs).href + "?ojsvg=react", shortCircuit: true };
  }
  if (ALIASES[spec]) {
    const hit = probe(ALIASES[spec]);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  // App "#" subpath imports with bundler-style extension probing.
  if (spec.startsWith("#")) {
    const hit = resolveImports(spec);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  // tsconfig `paths` aliases (bare-looking specifiers like "@platform/auth").
  if (!spec.startsWith(".") && !spec.startsWith("/")) {
    const hit = resolveTsPaths(spec);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  if (spec.startsWith(".") && context.parentURL) {
    const base = pathResolve(dirname(fileURLToPath(context.parentURL)), spec);
    const hit = probe(base);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  const r = await next(spec, context);
  // Version-bust @tanstack/* (ESM) so their module state re-evaluates on reload.
  if (r && r.url && isTanstack(r.url)) return { ...r, url: withV(r.url), shortCircuit: true };
  return r;
}

// createServerFn provider transform (server side). A bare `.handler(fn)`
// leaves the runtime's `extractedFn` returning the raw value, but it expects
// `{ result }`. Rewrite each top-level `const NAME = createServerFn(...).handler(FN)`
// to the provider shape so `NAME()` runs the handler in-process during SSR:
//   const NAME_createServerFn_handler = createServerRpc({id,name,filename},
//     (opts) => NAME.__executeServer(opts));
//   const NAME = createServerFn(...).handler(NAME_createServerFn_handler, FN);
function transformServerFns(code, path) {
  if (!code.includes("createServerFn")) return code;
  const rel = path.startsWith(APP) ? path.slice(APP.length).replace(/^\//, "") : path;
  const re =
    /(^|[\n;])([ \t]*)((?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\()/g;
  let changed = false;
  const out = code.replace(re, (_m, pre, indent, decl, name) => {
    changed = true;
    const id = JSON.stringify(Buffer.from(`${rel}#${name}`).toString("base64url"));
    const meta = `{ id: ${id}, name: ${JSON.stringify(name)}, filename: ${JSON.stringify(rel)} }`;
    // Exported so the server-fn resolver can import it for HTTP dispatch.
    const rpc =
      `${indent}export const ${name}_createServerFn_handler = createServerRpc(${meta}, ` +
      `(opts) => ${name}.__executeServer(opts));\n`;
    return `${pre}${rpc}${indent}${decl}${name}_createServerFn_handler, `;
  });
  if (!changed) return code;
  return `import { createServerRpc } from "@tanstack/react-start/server-rpc";\n${out}`;
}

export async function load(url, context, next) {
  // Plugin-owned virtual module: load its code from the container.
  if (url.startsWith(VIRTUAL_SCHEME)) {
    const rid = decodeURIComponent(url.slice(VIRTUAL_SCHEME.length));
    const code = container ? await container.load(rid) : null;
    return { format: "module", source: code ?? "export default undefined;", shortCircuit: true };
  }
  const clean = stripQ(url);
  // Tagged asset (from resolve above).
  const kind = /[?&]ojasset=(\w+)/.exec(url)?.[1];
  if (kind) {
    const path = fileURLToPath(clean);
    let src = "export default {};"; // css: no-op on the server
    if (kind === "raw") src = `export default ${JSON.stringify(readFileSync(path, "utf8"))};`;
    else if (kind === "url") src = `export default ${JSON.stringify("/@oj-start/fs" + path)};`;
    else if (kind === "inline")
      src = `export default ${JSON.stringify("data:application/octet-stream;base64," + readFileSync(path).toString("base64"))};`;
    return { format: "module", source: src, shortCircuit: true };
  }
  // JSON without an import attribute: hand back a module (Node would reject it).
  if (clean.endsWith(".json")) {
    return { format: "module", source: `export default ${readFileSync(fileURLToPath(clean), "utf8")};`, shortCircuit: true };
  }
  if (clean.endsWith(".tsx") || clean.endsWith(".ts")) {
    const path = fileURLToPath(clean);
    const src = transformServerFns(transformGlob(readFileSync(path, "utf8"), path), path);
    const out = transformSync(src, {
      loader: clean.endsWith("tsx") ? "tsx" : "ts",
      format: "esm", jsx: "automatic", sourcefile: path,
      define: viteEnvDefine({ ssr: true }),
    });
    return { format: "module", source: out.code, shortCircuit: true };
  }
  // Versioned @tanstack/* module: load its ESM source fresh under the new URL.
  if (url.includes("?ojv=") && isTanstack(url)) {
    return { format: "module", source: readFileSync(fileURLToPath(clean), "utf8"), shortCircuit: true };
  }
  // .svg: svgr (a `load` hook) may turn it into a React component; otherwise
  // serve it as a URL like any other asset.
  if (clean.endsWith(".svg")) {
    const path = fileURLToPath(clean);
    // A ?react-tagged svg (see resolve) is handed to svgr with the query.
    const id = /[?&]ojsvg=react/.test(url) ? path + "?react" : path;
    const loaded = container ? await container.load(id) : null;
    const src = loaded != null ? loaded : `export default ${JSON.stringify("/@oj-start/fs" + path)};`;
    return { format: "module", source: src, shortCircuit: true };
  }
  // A file a vite plugin compiles end-to-end (e.g. .mdx via customerMdx): run
  // its transform hooks, then esbuild the JSX result to ESM.
  if (container && clean.endsWith(".mdx")) {
    const path = fileURLToPath(clean);
    const compiled = await container.transform(readFileSync(path, "utf8"), path);
    if (compiled != null) {
      const out = transformSync(compiled, {
        loader: "jsx", format: "esm", jsx: "automatic", sourcefile: path,
        define: viteEnvDefine({ ssr: true }),
      });
      return { format: "module", source: out.code, shortCircuit: true };
    }
  }
  // CJS dependency imported via ESM: facade it so named imports resolve.
  if (clean.startsWith("file:") && clean.includes("/node_modules/")) {
    const path = fileURLToPath(clean);
    if (isCjsFile(path)) {
      try { return { format: "module", source: cjsFacade(path), shortCircuit: true }; } catch {}
    }
  }
  return next(url, context);
}
