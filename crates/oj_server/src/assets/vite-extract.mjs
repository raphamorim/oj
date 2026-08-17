// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname } from "node:path";
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

async function loadConfig() {
  let viteErr = null;
  const vitePath = resolvePkg("vite");
  if (vitePath) {
    try {
      const vite = await import(pathToFileURL(vitePath).href);
      if (typeof vite.loadConfigFromFile === "function") {
        const loaded = await vite.loadConfigFromFile({ command, mode }, configPath, appRoot);
        if (loaded && loaded.config) return loaded.config;
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
      define: {
        __dirname: JSON.stringify(dirname(configPath)),
        __filename: JSON.stringify(configPath),
      },
    });
    const out = `${appRoot}/.oj-cache/oj-vite-config.mjs`;
    writeFileSync(out, r.outputFiles[0].text);
    const m = await import(pathToFileURL(out).href);
    return typeof m.default === "function" ? await m.default({ command, mode }) : m.default;
  }
  const m = await import(pathToFileURL(configPath).href);
  return typeof m.default === "function" ? await m.default({ command, mode }) : m.default;
}

function extractAlias(alias) {
  const out = {};
  if (!alias) return out;
  const entries = Array.isArray(alias)
    ? alias.map((e) => [e.find, e.replacement])
    : Object.entries(alias);
  for (const [find, replacement] of entries) {
    if (typeof find === "string" && typeof replacement === "string") out[find] = replacement;
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

function warnUnsupported(c) {
  if (c.css?.preprocessorOptions) warn("css.preprocessorOptions is not applied yet");
  if (c.esbuild && typeof c.esbuild === "object") warn("esbuild options are not applied");
  if (c.optimizeDeps) warn("optimizeDeps is ignored; oj has its own dependency optimizer");
  if (c.worker) warn("worker config is not applied");
  if (c.ssr) warn("ssr config is not applied");
  for (const k of ["strictPort", "open", "cors", "allowedHosts"]) {
    if (c.server?.[k] !== undefined) warn(`server.${k} is accepted but not applied`);
  }
}

try {
  const c = (await loadConfig()) ?? {};
  warnUnsupported(c);
  process.stdout.write(
    JSON.stringify({
      base: typeof c.base === "string" ? c.base : null,
      publicDir: typeof c.publicDir === "string" ? c.publicDir : null,
      port: typeof c.server?.port === "number" ? c.server.port : null,
      host: typeof c.server?.host === "string" ? c.server.host : null,
      define: c.define && typeof c.define === "object" ? c.define : null,
      alias: extractAlias(c.resolve?.alias),
      headers: stringMap(c.server?.headers),
      proxy: extractProxy(c.server?.proxy),
      rollupOptions: c.build?.rolldownOptions ?? c.build?.rollupOptions ?? null,
      assetsInlineLimit:
        typeof c.build?.assetsInlineLimit === "number" ? c.build.assetsInlineLimit : null,
    }),
  );
} catch (e) {
  process.stderr.write(`oj: could not extract vite.config values: ${(e && e.stack) || e}\n`);
  process.stdout.write("{}");
}
