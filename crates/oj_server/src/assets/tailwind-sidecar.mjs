// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// oj tailwind v4 sidecar. Resolves tailwindcss from the APP's node_modules.
// Protocol: one JSON per line on stdin {id, base, css, from} -> stdout
// {id, css} | {id, error}. `--once <cssfile> <base>` prints compiled css.
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import readline from "node:readline";

async function loadTailwind(base) {
  const req = createRequire(base + "/package.json");
  // The JS API is @tailwindcss/node; bare "tailwindcss" resolves to CSS.
  const tw = await import(req.resolve("@tailwindcss/node"));
  const oxide = await import(req.resolve("@tailwindcss/oxide"));
  return { tw, oxide };
}

async function compileCss(base, css, from) {
  const { tw, oxide } = await loadTailwind(base);
  const compiler = await tw.compile(css, { base, from, onDependency: () => {} });
  const scanner = new oxide.Scanner({ sources: [{ base, pattern: "**/*", negated: false }] });
  return compiler.build(scanner.scan());
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
