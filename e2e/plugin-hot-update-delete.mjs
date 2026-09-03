// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite dispatches hotUpdate / watchChange for a removed file with type "delete"
// (the watcher's unlink); oj used to skip deleted files entirely and always say
// "update" for the rest.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 6404;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hotdel-"));
// Outside the app root so the watcher never sees the mark file itself.
const marks = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hotdel-marks-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hotdel", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "a.js"), `export const A = "a";\n`);
fs.writeFileSync(path.join(app, "src", "gone.js"), `export const G = "g";\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./a.js";\nimport "./gone.js";\nwindow.__ok = 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import fs from "node:fs";
const log = (line) => fs.appendFileSync(${JSON.stringify(marks)} + "/events", line + "\\n");
export default [{
  name: "watcher",
  watchChange(id, { event }) { log("watchChange:" + id.split("/").pop() + ":" + event); },
  hotUpdate(ctx) { log("hotUpdate:" + ctx.file.split("/").pop() + ":" + ctx.type); },
}];\n`,
);

const events = () => (fs.existsSync(path.join(marks, "events")) ? fs.readFileSync(path.join(marks, "events"), "utf8").trim().split("\n") : []);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  for (let i = 0; i < 100; i++) { try { if ((await fetch(`http://localhost:${PORT}/`)).ok) break; } catch {} await sleep(200); }
  await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  await (await fetch(`http://localhost:${PORT}/src/a.js`)).text();
  await (await fetch(`http://localhost:${PORT}/src/gone.js`)).text();
  await sleep(500);

  fs.writeFileSync(path.join(app, "src", "a.js"), `export const A = "a2";\n`);
  for (let i = 0; i < 50 && !events().some((e) => e.startsWith("hotUpdate:a.js")); i++) await sleep(100);
  assert.ok(events().includes("hotUpdate:a.js:update"), `an edited file is an update:\n${events().join("\n")}`);
  assert.ok(events().includes("watchChange:a.js:update"), `watchChange sees update:\n${events().join("\n")}`);

  fs.rmSync(path.join(app, "src", "gone.js"));
  for (let i = 0; i < 50 && !events().some((e) => e.startsWith("hotUpdate:gone.js")); i++) await sleep(100);
  assert.ok(events().includes("hotUpdate:gone.js:delete"), `a removed file reaches hotUpdate with type delete:\n${events().join("\n")}`);
  assert.ok(events().includes("watchChange:gone.js:delete"), `watchChange sees delete:\n${events().join("\n")}`);
  console.log("PLUGIN-HOT-UPDATE-DELETE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-HOT-UPDATE-DELETE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(marks, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
