// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// One-shot: load an app's vite.config and print the config values oj consumes
// (base, server.port/host, define) as JSON on stdout. Uses Vite's own config
// loader when available, else bundles with the app's esbuild, the same path
// the plugin host uses to read the `plugins` array.
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname } from "node:path";
import { writeFileSync, readFileSync } from "node:fs";

const configPath = process.argv[2];
const appRoot = process.argv[3];
const command = process.argv[4] || "serve";
const mode = process.argv[5] || "development";

// Resolve a package from an app that may use pnpm's strict, non-hoisted layout,
// where a transitive dep (vite's esbuild, etc.) isn't reachable from the app
// root: try the app root, then each direct dep as an anchor. Returns null when
// unresolvable (mirrors start/resolve-pkg).
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
  // Preferred: Vite's own loader. It handles TS/local imports/defineConfig and
  // uses Vite's bundled esbuild, so the app need not depend on esbuild itself.
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
      viteErr = e; // real config error — surfaced below if there is no fallback
    }
  }
  if (/\.(ts|tsx|mts|cts)$/.test(configPath)) {
    // Fallback only when esbuild is actually installed; otherwise surface the
    // real Vite error rather than a misleading "Cannot find module 'esbuild'".
    const esbuildPath = resolvePkg("esbuild");
    if (!esbuildPath) {
      throw viteErr ?? new Error("no vite or esbuild available to load the TS vite.config");
    }
    const esbuild = await import(pathToFileURL(esbuildPath).href);
    const r = await esbuild.build({
      entryPoints: [configPath], bundle: true, platform: "node", format: "esm",
      packages: "external", write: false, logLevel: "silent", absWorkingDir: appRoot,
    });
    const out = `${appRoot}/.oj-cache/oj-vite-config.mjs`;
    writeFileSync(out, r.outputFiles[0].text);
    const m = await import(pathToFileURL(out).href);
    return typeof m.default === "function" ? await m.default({ command, mode }) : m.default;
  }
  const m = await import(pathToFileURL(configPath).href);
  return typeof m.default === "function" ? await m.default({ command, mode }) : m.default;
}

// resolve.alias is either an object ({ find: replacement }) or an array of
// { find, replacement }. Keep only string find/replacement pairs (a RegExp
// find has no string form for oxc_resolver's prefix matcher).
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

// Keep only string-valued entries of an object (headers, etc.).
function stringMap(obj) {
  if (!obj || typeof obj !== "object") return null;
  const out = {};
  for (const [k, v] of Object.entries(obj)) if (typeof v === "string") out[k] = v;
  return Object.keys(out).length ? out : null;
}

try {
  const c = (await loadConfig()) ?? {};
  process.stdout.write(
    JSON.stringify({
      base: typeof c.base === "string" ? c.base : null,
      publicDir: typeof c.publicDir === "string" ? c.publicDir : null,
      port: typeof c.server?.port === "number" ? c.server.port : null,
      host: typeof c.server?.host === "string" ? c.server.host : null,
      define: c.define && typeof c.define === "object" ? c.define : null,
      alias: extractAlias(c.resolve?.alias),
      // server.headers: string values only (e.g. COOP/COEP).
      headers: stringMap(c.server?.headers),
    }),
  );
} catch (e) {
  process.stderr.write(`oj: could not extract vite.config values: ${(e && e.stack) || e}\n`);
  process.stdout.write("{}");
}
