// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const bundle = process.argv.includes("--bundle");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
fs.rmSync(path.join(repo, "playground", ".oj-cache"), { recursive: true, force: true });
try {
  execSync("lsof -ti:5199 -sTCP:LISTEN | xargs kill", { shell: "/bin/bash", stdio: "ignore" });
} catch {}
const args = ["dev", path.join(repo, "playground"), "--port", "5199"];
if (bundle) args.push("--bundle");
const server = spawn(path.join(repo, "target", "debug", "oj"), args, { stdio: "ignore" });

const up = async () => {
  for (let i = 0; i < 60; i++) {
    try { if ((await fetch("http://localhost:5199/")).ok) return; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("dev server did not start");
};
await up();

const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

let failed = 0;
for (const file of fs.readdirSync(here).filter((f) => f.endsWith(".test.js")).sort()) {
  process.stdout.write(`\n${file} (${bundle ? "bundle" : "unbundled"})\n`);
  try {
    execSync(`node ${path.join(here, file)}`, {
      stdio: "inherit",
      cwd: here,
      env: { ...process.env, OJ_E2E_MODE: bundle ? "bundle" : "unbundled" },
    });
  } catch {
    failed++;
  }
  sleep(1200);
}
server.kill("SIGKILL");
console.log(failed ? `\n${failed} test(s) FAILED` : "\nALL E2E TESTS PASSED");
process.exit(failed ? 1 : 0);
