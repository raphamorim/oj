// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// One-shot: load an app's vite.config and print the config VALUES oj consumes
// (base, server.port/host, define) as JSON on stdout. Uses Vite's own config
// loader when available, else bundles with the app's esbuild — the same path
// the plugin host uses to read the `plugins` array.
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { writeFileSync } from "node:fs";

const configPath = process.argv[2];
const appRoot = process.argv[3];
const command = process.argv[4] || "serve";
const mode = process.argv[5] || "development";
const req = createRequire(appRoot + "/package.json");

async function loadConfig() {
  try {
    const vite = await import(req.resolve("vite"));
    if (typeof vite.loadConfigFromFile === "function") {
      const loaded = await vite.loadConfigFromFile({ command, mode }, configPath, appRoot);
      if (loaded && loaded.config) return loaded.config;
    }
  } catch {}
  if (/\.(ts|tsx|mts|cts)$/.test(configPath)) {
    const esbuild = await import(req.resolve("esbuild"));
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

try {
  const c = (await loadConfig()) ?? {};
  process.stdout.write(
    JSON.stringify({
      base: typeof c.base === "string" ? c.base : null,
      port: typeof c.server?.port === "number" ? c.server.port : null,
      host: typeof c.server?.host === "string" ? c.server.host : null,
      define: c.define && typeof c.define === "object" ? c.define : null,
      alias: extractAlias(c.resolve?.alias),
    }),
  );
} catch (e) {
  process.stderr.write(`oj: could not extract vite.config values: ${(e && e.stack) || e}\n`);
  process.stdout.write("{}");
}
