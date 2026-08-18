// SPDX-License-Identifier: MIT

import { pathToFileURL, fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";
import { resolve as pathResolve, dirname } from "node:path";
import { importPkg, viteEnvDefine, emptyVirtualStub } from "./resolve-pkg.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";
import {
  EXTS, isFile, JS_TO_TS, probe, RESERVED, nearestPkgType,
  hasEsmSyntax, isCjsFile, cjsFacade, stripJsonc, readJsonc, rewriteServerFns, substituteAlias,
  parseImportsField, mergeTsConfig,
} from "./loader-util.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const { transformSync } = await importPkg(APP, "rolldown/experimental", ["vite", "@tanstack/react-start"]);
const container = await loadPluginContainer(APP, { command: "serve", environment: "ssr" });
const VIRTUAL_SCHEME = "ojvirtual:///";

let V = 0;
export async function initialize(data) {
  if (data && data.port) data.port.on("message", (v) => (V = v));
}
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
  "tanstack-start-manifest:v": pathResolve(HERE, "manifest.ts"),
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

export async function resolve(spec, context, next) {
  if (context.parentURL) context = { ...context, parentURL: stripQ(context.parentURL) };
  if (container && (spec.startsWith("virtual:") || spec.startsWith("\0"))) {
    const importer = context.parentURL && context.parentURL.startsWith("file:")
      ? fileURLToPath(context.parentURL)
      : undefined;
    const rid = await container.resolveId(spec, importer);
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
      try { abs = fileURLToPath(stripQ((await next(clean, context)).url)); } catch {}
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
      try { abs = fileURLToPath(stripQ((await next(clean, context)).url)); } catch {}
    }
    if (abs) return { url: pathToFileURL(abs).href + "?ojsvg=react", shortCircuit: true };
  }
  if (ALIASES[spec]) {
    const hit = probe(ALIASES[spec]);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  if (spec.startsWith("#")) {
    const hit = resolveImports(spec);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  if (!spec.startsWith(".") && !spec.startsWith("/")) {
    const hit = resolveTsPaths(spec);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  if (spec.startsWith(".") && context.parentURL) {
    const base = pathResolve(dirname(fileURLToPath(context.parentURL)), spec);
    const hit = probe(base);
    if (hit) return { url: withV(pathToFileURL(hit).href), shortCircuit: true };
  }
  let r;
  try {
    r = await next(spec, context);
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
  if (r && r.url && isTanstack(r.url)) return { ...r, url: withV(r.url), shortCircuit: true };
  return r;
}

function transformServerFns(code, path) {
  const rel = path.startsWith(APP) ? path.slice(APP.length).replace(/^\//, "") : path;
  return rewriteServerFns(code, rel);
}

export async function load(url, context, next) {
  if (url.startsWith(VIRTUAL_SCHEME)) {
    const rid = decodeURIComponent(url.slice(VIRTUAL_SCHEME.length));
    const code = container ? await container.load(rid) : null;
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
    let raw = readFileSync(path, "utf8");
    if (container && !path.includes("/node_modules/")) {
      const t = await container.transformUserCode(raw, path);
      if (t != null) raw = t;
    }
    const src = transformServerFns(transformGlob(raw, path), path);
    const out = transformSync(path, src, {
      lang: clean.endsWith("tsx") ? "tsx" : "ts",
      jsx: { runtime: "automatic" },
      define: viteEnvDefine({ ssr: true }),
    });
    return { format: "module", source: out.code, shortCircuit: true };
  }
  if (url.includes("?ojv=") && isTanstack(url)) {
    return { format: "module", source: readFileSync(fileURLToPath(clean), "utf8"), shortCircuit: true };
  }
  if (clean.endsWith(".svg")) {
    const path = fileURLToPath(clean);
    const id = /[?&]ojsvg=react/.test(url) ? path + "?react" : path;
    const loaded = container ? await container.load(id) : null;
    const src = loaded != null ? loaded : `export default ${JSON.stringify("/@oj-start/fs" + path)};`;
    return { format: "module", source: src, shortCircuit: true };
  }
  if (container && clean.endsWith(".mdx")) {
    const path = fileURLToPath(clean);
    const compiled = await container.transform(readFileSync(path, "utf8"), path);
    if (compiled != null) {
      const out = transformSync(path, compiled, {
        lang: "jsx", jsx: { runtime: "automatic" },
        define: viteEnvDefine({ ssr: true }),
      });
      return { format: "module", source: out.code, shortCircuit: true };
    }
  }
  if (clean.startsWith("file:") && clean.includes("/node_modules/")) {
    const path = fileURLToPath(clean);
    if (isCjsFile(path)) {
      try { return { format: "module", source: cjsFacade(path), shortCircuit: true }; } catch {}
    }
  }
  return next(url, context);
}
