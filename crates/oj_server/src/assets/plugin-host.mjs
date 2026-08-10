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

async function run(hook, args) {
  if (hook === "transform") return transform(args[0], args[1]);
  if (hook === "resolveId") return resolveId(args[0], args[1]);
  if (hook === "load") return load(args[0]);
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
