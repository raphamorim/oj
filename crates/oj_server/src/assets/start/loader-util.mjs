// SPDX-License-Identifier: MIT
// Pure resolution/interop helpers for the SSR loader (loader.mjs). Split out so
// they can be unit-tested without triggering loader.mjs's import-time bootstrap
// (which loads the app's esbuild and plugin container). Nothing here depends on
// the app root or any module-level state beyond a per-process pkg-type cache.
import { readFileSync, statSync } from "node:fs";
import { resolve as pathResolve, dirname } from "node:path";
import { pathToFileURL } from "node:url";
import { createRequire } from "node:module";

export const EXTS = [
  ".ts", ".tsx", ".js", ".jsx", ".mjs",
  "/index.ts", "/index.tsx", "/index.js", "/index.jsx", "/index.mjs",
];

export const isFile = (p) => {
  try { return statSync(p).isFile(); } catch { return false; }
};

// TS/bundler convention: source imports carry a `.js`/`.jsx`/`.mjs` extension
// but the file on disk is the TS equivalent.
export const JS_TO_TS = { ".js": [".ts", ".tsx"], ".jsx": [".tsx"], ".mjs": [".mts"], ".cjs": [".cts"] };

export function probe(base) {
  if (isFile(base)) return base;
  for (const [js, tss] of Object.entries(JS_TO_TS)) {
    if (base.endsWith(js)) {
      const stem = base.slice(0, -js.length);
      for (const ts of tss) if (isFile(stem + ts)) return stem + ts;
    }
  }
  for (const ext of EXTS) if (isFile(base + ext)) return base + ext;
  return null;
}

// CJS -> ESM interop. Node builds named exports for a required CJS module with
// cjs-module-lexer, which misses exports it can't statically see (e.g.
// `module.exports = {...}`), so `import { x } from "cjs-pkg"` throws. Facade the
// module instead: require() it at load time and re-export its ACTUAL runtime
// keys -- strictly more complete than any static analysis, and it fixes every
// import shape (default, named, namespace) at once.
export const RESERVED = new Set(
  ("break case catch class const continue debugger default delete do else enum export extends false finally " +
    "for function if import in instanceof new null return super switch this throw true try typeof var void " +
    "while with yield let static await").split(" "),
);

const pkgTypeCache = new Map();
export function nearestPkgType(startDir) {
  if (pkgTypeCache.has(startDir)) return pkgTypeCache.get(startDir);
  let d = startDir, type = "commonjs";
  for (let i = 0; i < 40 && d; i++) {
    const pj = pathResolve(d, "package.json");
    if (isFile(pj)) {
      try { type = JSON.parse(readFileSync(pj, "utf8")).type || "commonjs"; } catch {}
      break;
    }
    const parent = dirname(d);
    if (parent === d) break;
    d = parent;
  }
  pkgTypeCache.set(startDir, type);
  return type;
}

// A `.js` file is CJS only if its package isn't type:module AND it has no ESM
// syntax -- Node 22 detects ESM syntax in a type-less .js and loads it as ESM
// (dual packages ship `export`-using `.js` behind the `import` condition), so
// faceting those with require() would throw ERR_REQUIRE_CYCLE_MODULE.
export function hasEsmSyntax(path) {
  try {
    const s = readFileSync(path, "utf8");
    return /(^|[\n;])\s*export\s/.test(s) || /(^|[\n;])\s*import\s[^(]/.test(s);
  } catch {
    return false;
  }
}

export function isCjsFile(path) {
  if (path.endsWith(".cjs")) return true;
  if (path.endsWith(".mjs")) return false;
  if (!path.endsWith(".js")) return false;
  if (nearestPkgType(dirname(path)) === "module") return false;
  return !hasEsmSyntax(path);
}

export function cjsFacade(path) {
  const url = pathToFileURL(path).href;
  const mod = createRequire(url)(path);
  const isEsm = !!(mod && mod.__esModule);
  const enumerable = mod && (typeof mod === "object" || typeof mod === "function") ? Object.keys(mod) : [];
  const names = enumerable.filter((k) => k !== "default" && /^[A-Za-z_$][\w$]*$/.test(k) && !RESERVED.has(k));
  return [
    `import { createRequire as _cr } from "node:module";`,
    `const _m = _cr(${JSON.stringify(url)})(${JSON.stringify(path)});`,
    `export default ${isEsm ? "(_m && _m.default !== undefined ? _m.default : _m)" : "_m"};`,
    ...names.map((n) => `export const ${n} = _m[${JSON.stringify(n)}];`),
  ].join("\n");
}

// Strip // and /* */ comments only outside strings, so comment syntax inside
// glob patterns (e.g. "./shared/*") survives, then drop trailing commas.
export function stripJsonc(s) {
  let out = "", i = 0, inStr = false, q = "";
  while (i < s.length) {
    const c = s[i], n = s[i + 1];
    if (inStr) {
      out += c;
      if (c === "\\") { out += n ?? ""; i += 2; continue; }
      if (c === q) inStr = false;
      i++; continue;
    }
    if (c === '"' || c === "'") { inStr = true; q = c; out += c; i++; continue; }
    if (c === "/" && n === "/") { while (i < s.length && s[i] !== "\n") i++; continue; }
    if (c === "/" && n === "*") { i += 2; while (i < s.length && !(s[i] === "*" && s[i + 1] === "/")) i++; i += 2; continue; }
    out += c; i++;
  }
  return out;
}

export function readJsonc(file) {
  try {
    return JSON.parse(stripJsonc(readFileSync(file, "utf8")).replace(/,(\s*[}\]])/g, "$1"));
  } catch {
    return null;
  }
}

// Match a single-`*` alias pattern (a tsconfig `paths` key or a package.json
// `imports` subpath) against a specifier and return the target with `*` filled
// in, or null when it does not match. A pattern without `*` matches only an
// exact specifier. Mirrors how esbuild and Node substitute these aliases so the
// SSR loader resolves the same ids the bundler does.
export function substituteAlias(pattern, target, spec) {
  if (pattern.includes("*")) {
    const [pre, post = ""] = pattern.split("*");
    if (!spec.startsWith(pre) || !spec.endsWith(post) || spec.length < pre.length + post.length) return null;
    return target.replace("*", spec.slice(pre.length, spec.length - post.length));
  }
  return spec === pattern ? target : null;
}

// createServerFn provider transform (server side). A bare `.handler(fn)` leaves
// the runtime's `extractedFn` returning the raw value, but it expects
// `{ result }`. Rewrite each top-level `const NAME = createServerFn(...).handler(FN)`
// to the provider shape so `NAME()` runs the handler in-process during SSR:
//   export const NAME_createServerFn_handler = createServerRpc({id,name,filename},
//     (opts) => NAME.__executeServer(opts));
//   const NAME = createServerFn(...).handler(NAME_createServerFn_handler, FN);
// `rel` is the app-relative module path; it seeds the base64url handler id the
// server-fn resolver dispatches on. Returns the code unchanged when there is no
// createServerFn to rewrite.
export function rewriteServerFns(code, rel) {
  if (!code.includes("createServerFn")) return code;
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
