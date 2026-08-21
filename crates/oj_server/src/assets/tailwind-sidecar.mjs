// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { existsSync, fstatSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import readline from "node:readline";

const processors = new Map();

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
      postcss = null;
    }
    if (postcss) {
      const mod = await import(pathToFileURL(cfgPath).href);
      const config = mod.default ?? mod;
      const raw = config.plugins ?? {};
      const plugins = [];
      if (Array.isArray(raw)) {
        for (const p of raw) if (p) plugins.push(p);
      } else {
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
  let inflight = 0;
  let stdinClosed = false;
  const maybeExit = () => { if (stdinClosed && inflight === 0) process.exit(0); };
  try {
    if (fstatSync(0).isFIFO()) rl.once("close", () => { stdinClosed = true; maybeExit(); });
  } catch {}
  rl.on("line", async (line) => {
    let msg;
    try { msg = JSON.parse(line); } catch { return; }
    inflight += 1;
    try {
      const css = await compileCss(msg.base, msg.css, msg.from);
      process.stdout.write(JSON.stringify({ id: msg.id, css }) + "\n");
    } catch (err) {
      process.stdout.write(JSON.stringify({ id: msg.id, error: String(err) }) + "\n");
    } finally {
      inflight -= 1;
      maybeExit();
    }
  });
}
