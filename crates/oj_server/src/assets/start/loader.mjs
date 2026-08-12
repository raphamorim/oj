// SPDX-License-Identifier: MIT
// Node ESM loader hook: resolves TanStack Start's four framework aliases,
// compiles TS/JSX (via the app's esbuild), and probes extensions for
// bundler-style extensionless imports. Registered by runner.mjs.
import { pathToFileURL, fileURLToPath } from "node:url";
import { readFileSync, existsSync } from "node:fs";
import { resolve as pathResolve, dirname } from "node:path";
import { importPkg } from "./resolve-pkg.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const { transformSync } = await importPkg(APP, "esbuild", ["vite", "@tanstack/react-start"]);
const EXTS = [".ts", ".tsx", ".js", ".jsx", "/index.ts", "/index.tsx"];

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
const ASSET_EXT = /\.(svg|png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf|eot|mp4|webm|wasm)$/;

function probe(base) {
  if (existsSync(base)) return base;
  for (const ext of EXTS) if (existsSync(base + ext)) return base + ext;
  return null;
}

const ALIASES = {
  "#tanstack-router-entry": pathResolve(APP, "src/router"),
  "#tanstack-start-entry": pathResolve(HERE, "start-entry.ts"),
  "#tanstack-start-plugin-adapters": pathResolve(HERE, "plugin-adapters.ts"),
  "#tanstack-start-server-fn-resolver": pathResolve(HERE, "server-fn-resolver.mjs"),
  "tanstack-start-manifest:v": pathResolve(HERE, "manifest.ts"),
};

export async function resolve(spec, context, next) {
  if (context.parentURL) context = { ...context, parentURL: stripQ(context.parentURL) };
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
    if (!abs) {
      try { abs = fileURLToPath(stripQ((await next(clean, context)).url)); } catch {}
    }
    if (abs) return { url: pathToFileURL(abs).href + `?ojasset=${kind}`, shortCircuit: true };
  }
  if (ALIASES[spec]) {
    const hit = probe(ALIASES[spec]);
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
    const src = transformServerFns(readFileSync(path, "utf8"), path);
    const out = transformSync(src, {
      loader: clean.endsWith("tsx") ? "tsx" : "ts",
      format: "esm", jsx: "automatic", sourcefile: path,
    });
    return { format: "module", source: out.code, shortCircuit: true };
  }
  // Versioned @tanstack/* module: load its ESM source fresh under the new URL.
  if (url.includes("?ojv=") && isTanstack(url)) {
    return { format: "module", source: readFileSync(fileURLToPath(clean), "utf8"), shortCircuit: true };
  }
  return next(url, context);
}
