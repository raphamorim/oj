// SPDX-License-Identifier: MIT
// Node ESM loader hook: resolves TanStack Start's four framework aliases,
// compiles TS/JSX (via the app's esbuild), and probes extensions for
// bundler-style extensionless imports. Registered by runner.mjs.
import { pathToFileURL, fileURLToPath } from "node:url";
import { readFileSync, existsSync } from "node:fs";
import { resolve as pathResolve, dirname } from "node:path";
import { transformSync } from "esbuild";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const EXTS = [".ts", ".tsx", ".js", ".jsx", "/index.ts", "/index.tsx"];

function probe(base) {
  if (existsSync(base)) return base;
  for (const ext of EXTS) if (existsSync(base + ext)) return base + ext;
  return null;
}

const ALIASES = {
  "#tanstack-router-entry": pathResolve(APP, "src/router"),
  "#tanstack-start-entry": pathResolve(HERE, "start-entry.ts"),
  "#tanstack-start-plugin-adapters": pathResolve(HERE, "plugin-adapters.ts"),
  "tanstack-start-manifest:v": pathResolve(HERE, "manifest.ts"),
};

export async function resolve(spec, context, next) {
  if (ALIASES[spec]) {
    const hit = probe(ALIASES[spec]);
    if (hit) return { url: pathToFileURL(hit).href, shortCircuit: true };
  }
  if (spec.startsWith(".") && context.parentURL) {
    const base = pathResolve(dirname(fileURLToPath(context.parentURL)), spec);
    const hit = probe(base);
    if (hit && hit !== base) return { url: pathToFileURL(hit).href, shortCircuit: true };
  }
  return next(spec, context);
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
    const id = JSON.stringify(`${rel}#${name}`);
    const meta = `{ id: ${id}, name: ${JSON.stringify(name)}, filename: ${JSON.stringify(rel)} }`;
    const rpc =
      `${indent}const ${name}_createServerFn_handler = createServerRpc(${meta}, ` +
      `(opts) => ${name}.__executeServer(opts));\n`;
    return `${pre}${rpc}${indent}${decl}${name}_createServerFn_handler, `;
  });
  if (!changed) return code;
  return `import { createServerRpc } from "@tanstack/react-start/server-rpc";\n${out}`;
}

export async function load(url, context, next) {
  if (url.endsWith(".tsx") || url.endsWith(".ts")) {
    const path = fileURLToPath(url);
    const src = transformServerFns(readFileSync(path, "utf8"), path);
    const out = transformSync(src, {
      loader: url.endsWith("tsx") ? "tsx" : "ts",
      format: "esm", jsx: "automatic", sourcefile: path,
    });
    return { format: "module", source: out.code, shortCircuit: true };
  }
  return next(url, context);
}
