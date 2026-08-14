// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// node bench/build.mjs 5000
import { execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const N = parseInt(process.argv[2] ?? "5000", 10);
const here = path.dirname(fileURLToPath(import.meta.url));
const app = path.join(here, "apps", `app-${N}`);
const OJ = path.join(here, "..", "target", "release", "oj");

const jsBytes = (dir) => {
  let total = 0;
  const walk = (d) => {
    for (const f of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, f.name);
      if (f.isDirectory()) walk(p);
      else if (f.name.endsWith(".js")) total += fs.statSync(p).size;
    }
  };
  if (fs.existsSync(dir)) walk(dir);
  return total;
};

const run = (label, cmd, outDir) => {
  fs.rmSync(outDir, { recursive: true, force: true });
  const t0 = Date.now();
  execSync(cmd, { cwd: app, stdio: "pipe" });
  const ms = Date.now() - t0;
  console.log(`${label.padEnd(5)} | ${String(ms + "ms").padEnd(8)} | ${(jsBytes(outDir) / 1024).toFixed(0)}kB js`);
};

// console.table?
console.log(`production build, ${N} components`);
console.log("tool  | time     | output");
run("oj", `${OJ} build . --out dist-oj`, path.join(app, "dist-oj"));
run("vite", `node node_modules/vite/bin/vite.js build --outDir dist-vite`, path.join(app, "dist-vite"));
