// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Persistent plugin host: loads Vite/Rollup-style plugins from the app's
// plugins module and runs their `transform` hooks against oj's compile
// pipeline. JSON-lines over stdio with correlation ids (many transforms can be
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

// Minimal Rollup plugin context. Enough for transform-only plugins; hooks that
// reach for this.resolve/this.emitFile aren't supported yet.
const ctx = {
  warn: (m) => process.stderr.write(`oj plugin warn: ${m}\n`),
  error: (m) => {
    throw typeof m === "string" ? new Error(m) : m;
  },
};

// Run every plugin's transform in order, chaining the code (Rollup semantics).
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

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const { id, code, path } = msg;
  try {
    process.stdout.write(JSON.stringify({ id, code: await transform(code ?? "", path ?? "") }) + "\n");
  } catch (e) {
    process.stdout.write(JSON.stringify({ id, error: String((e && e.stack) || e) }) + "\n");
  }
});
