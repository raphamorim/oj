// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// oj CSS sidecar. If the app has a postcss.config.* it runs CSS through PostCSS
// with the app's own plugins (Tailwind v3 or v4-via-@tailwindcss/postcss,
// autoprefixer, ...) — exactly like Vite. Otherwise it falls back to the
// Tailwind v4 JS API (@tailwindcss/node). Everything resolves from the APP's
// node_modules. Protocol: one JSON per line on stdin {id, base, css, from} ->
// stdout {id, css} | {id, error}. `--once <cssfile> <base>` prints compiled css.
import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import readline from "node:readline";

// One PostCSS processor per app root (config + plugins loaded once).
const processors = new Map();

// Build a PostCSS processor from the app's postcss.config.*, or null if the app
// has none (then we use the Tailwind v4 fallback below).
async function loadPostcss(base) {
  if (processors.has(base)) return processors.get(base);
  const req = createRequire(base + "/package.json");
  const cfgPath = ["postcss.config.js", "postcss.config.cjs", "postcss.config.mjs"]
    .map((n) => `${base}/${n}`)
    .find((p) => existsSync(p));
  let processor = null;
  if (cfgPath) {
    let postcss;
    try {
      postcss = (await import(req.resolve("postcss"))).default;
    } catch {
      postcss = null; // postcss not installed -> fall back
    }
    if (postcss) {
      const mod = await import(pathToFileURL(cfgPath).href);
      const config = mod.default ?? mod;
      const raw = config.plugins ?? {};
      const plugins = [];
      if (Array.isArray(raw)) {
        // Already-instantiated plugins (array form).
        for (const p of raw) if (p) plugins.push(p);
      } else {
        // Object map { "plugin-name": options | false }.
        for (const [name, opts] of Object.entries(raw)) {
          if (opts === false) continue;
          const imported = await import(req.resolve(name));
          const factory = imported.default ?? imported;
          plugins.push(typeof factory === "function" ? factory(opts ?? {}) : factory);
        }
      }
      processor = postcss(plugins);
    }
  }
  processors.set(base, processor);
  return processor;
}

// Tailwind v4 JS API fallback (no postcss.config): @tailwindcss/node + oxide.
async function v4Compile(base, css, from) {
  const req = createRequire(base + "/package.json");
  const tw = await import(req.resolve("@tailwindcss/node"));
  const oxide = await import(req.resolve("@tailwindcss/oxide"));
  const compiler = await tw.compile(css, { base, from, onDependency: () => {} });
  const scanner = new oxide.Scanner({ sources: [{ base, pattern: "**/*", negated: false }] });
  return compiler.build(scanner.scan());
}

async function compileCss(base, css, from) {
  const processor = await loadPostcss(base);
  if (processor) {
    const result = await processor.process(css, { from, map: false });
    return result.css;
  }
  return v4Compile(base, css, from);
}

if (process.argv[2] === "--once") {
  const [file, base] = process.argv.slice(3);
  compileCss(base, readFileSync(file, "utf8"), file)
    .then((css) => { process.stdout.write(css); })
    .catch((err) => { console.error(String(err)); process.exit(1); });
} else {
  const rl = readline.createInterface({ input: process.stdin });
  rl.on("line", async (line) => {
    let msg;
    try { msg = JSON.parse(line); } catch { return; }
    try {
      const css = await compileCss(msg.base, msg.css, msg.from);
      process.stdout.write(JSON.stringify({ id: msg.id, css }) + "\n");
    } catch (err) {
      process.stdout.write(JSON.stringify({ id: msg.id, error: String(err) }) + "\n");
    }
  });
}
