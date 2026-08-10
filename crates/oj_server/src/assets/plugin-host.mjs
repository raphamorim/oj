// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Persistent plugin host: loads Vite/Rollup-style plugins from the app's
// plugins module and runs their hooks (transform / resolveId / load) against
// oj's pipeline. JSON-lines over stdio with correlation ids (many calls can be
// in flight; a cancelled caller just drops its response).
import { pathToFileURL } from "node:url";
import readline from "node:readline";

const pluginsPath = process.argv[2];
let plugins = [];
try {
  const mod = await import(pathToFileURL(pluginsPath).href);
  const list = mod.default ?? mod.plugins ?? [];
  plugins = (Array.isArray(list) ? list : [list]).filter(Boolean);
  process.stderr.write(`oj plugin host: loaded ${plugins.length} plugin(s) from ${pluginsPath}\n`);
} catch (e) {
  process.stderr.write(`oj plugin host: failed to load ${pluginsPath}: ${(e && e.stack) || e}\n`);
}

// Minimal Rollup plugin context. Enough for transform/resolveId/load plugins;
// hooks that reach for this.resolve/this.emitFile aren't supported yet.
const ctx = {
  warn: (m) => process.stderr.write(`oj plugin warn: ${m}\n`),
  error: (m) => {
    throw typeof m === "string" ? new Error(m) : m;
  },
};

// config() / configResolved() handshake, once at startup. Each plugin's
// config(config, env) may return a partial that is deep-merged into the
// resolved config; then every plugin's configResolved(finalConfig) runs so it
// can capture what it needs for later hooks.
function deepMerge(a, b) {
  if (Array.isArray(a) && Array.isArray(b)) return [...a, ...b];
  if (a && b && typeof a === "object" && typeof b === "object") {
    const out = { ...a };
    for (const k of Object.keys(b)) out[k] = k in a ? deepMerge(a[k], b[k]) : b[k];
    return out;
  }
  return b === undefined ? a : b;
}

async function runConfigHooks() {
  const initial = JSON.parse(process.argv[3] ?? "{}");
  let config = initial.config ?? {};
  const env = initial.env ?? { command: "serve", mode: "development" };
  for (const p of plugins) {
    if (typeof p.config !== "function") continue;
    const partial = await p.config.call(ctx, config, env);
    if (partial) config = deepMerge(config, partial);
  }
  for (const p of plugins) {
    if (typeof p.configResolved === "function") await p.configResolved.call(ctx, config);
  }
}
await runConfigHooks();

// transform chains through all plugins (Rollup semantics); returns the final code.
async function transform(code, id) {
  let current = code;
  for (const p of plugins) {
    if (typeof p.transform !== "function") continue;
    const r = await p.transform.call(ctx, current, id);
    if (r == null) continue;
    current = typeof r === "string" ? r : (r.code ?? current);
  }
  return current;
}

// resolveId / load are first-non-null-wins.
async function resolveId(source, importer) {
  for (const p of plugins) {
    if (typeof p.resolveId !== "function") continue;
    const r = await p.resolveId.call(ctx, source, importer || undefined);
    if (r == null) continue;
    return typeof r === "string" ? r : (r.id ?? null);
  }
  return null;
}

async function load(id) {
  for (const p of plugins) {
    if (typeof p.load !== "function") continue;
    const r = await p.load.call(ctx, id);
    if (r == null) continue;
    return typeof r === "string" ? r : (r.code ?? null);
  }
  return null;
}

// handleHotUpdate: plugins customize HMR for a changed file. oj's simplified
// contract — return "full-reload" to force a reload, [] to suppress HMR, or
// undefined to let default HMR proceed. First decisive result wins.
async function handleHotUpdate(file, timestamp) {
  let suppress = false;
  for (const p of plugins) {
    if (typeof p.handleHotUpdate !== "function") continue;
    const r = await p.handleHotUpdate.call(ctx, { file, timestamp: Number(timestamp) });
    if (r === "full-reload") return "full-reload";
    if (Array.isArray(r) && r.length === 0) suppress = true;
  }
  return suppress ? "skip" : null;
}

async function run(hook, args) {
  if (hook === "transform") return transform(args[0], args[1]);
  if (hook === "resolveId") return resolveId(args[0], args[1]);
  if (hook === "load") return load(args[0]);
  if (hook === "handleHotUpdate") return handleHotUpdate(args[0], args[1]);
  return null;
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const { id, hook, args } = msg;
  try {
    const result = await run(hook, args ?? []);
    process.stdout.write(JSON.stringify({ id, result: result ?? null }) + "\n");
  } catch (e) {
    process.stdout.write(JSON.stringify({ id, error: String((e && e.stack) || e) }) + "\n");
  }
});
