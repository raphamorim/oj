// SPDX-License-Identifier: MIT

import module, { createRequire } from "node:module";
import { existsSync, fstatSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import readline from "node:readline";

const send = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const req = createRequire(APP + "/package.json");

let postcssProcessor;
async function loadPostcss() {
  if (postcssProcessor !== undefined) return postcssProcessor;
  const cfgPath = ["postcss.config.js", "postcss.config.cjs", "postcss.config.mjs"]
    .map((n) => `${APP}/${n}`)
    .find((p) => existsSync(p));
  postcssProcessor = null;
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
      postcssProcessor = postcss(plugins);
    }
  }
  return postcssProcessor;
}

// Tailwind v4 via @tailwindcss/vite: no PostCSS plugin, compile through
// @tailwindcss/node + scan class candidates with @tailwindcss/oxide.
async function v4Compile(css, from) {
  const tw = await import(req.resolve("@tailwindcss/node"));
  const oxide = await import(req.resolve("@tailwindcss/oxide"));
  const compiler = await tw.compile(css, { base: APP, from, onDependency: () => {} });
  const scanner = new oxide.Scanner({ sources: [{ base: APP, pattern: "**/*", negated: false }] });
  return compiler.build(scanner.scan());
}

async function compile(path) {
  const src = readFileSync(path, "utf8");
  const processor = await loadPostcss();
  if (processor) {
    const result = await processor.process(src, { from: path, map: false });
    return result.css;
  }
  return v4Compile(src, path);
}

const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
process.stderr.write(`${OJ} css host: ready\n`);
// stdin EOF means oj died without tearing us down (SIGKILL skips
// kill_on_drop): exit instead of idling as an orphan.
const onParentGone = () => {
  try { module.flushCompileCache?.(); } catch {}
  process.exit(0);
};
try {
  if (fstatSync(0).isFIFO()) {
    process.stdin.once("end", onParentGone);
    process.stdin.once("close", onParentGone);
  }
} catch {}

const rl = readline.createInterface({ input: process.stdin });
for await (const line of rl) {
  let msg;
  try { msg = JSON.parse(line); } catch { continue; }
  try {
    send(JSON.stringify({ id: msg.id, css: await compile(msg.path) }) + "\n");
  } catch (e) {
    send(JSON.stringify({ id: msg.id, error: String((e && e.stack) || e) }) + "\n");
  }
}
