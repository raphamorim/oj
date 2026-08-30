// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The dev server serves module-runner-transformed code over
// /@ssr-module?...&runner=1 (the fetch-module backend a Vite module runner
// calls), while the default (no runner flag) path is unchanged ESM.

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-runner-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "runner-app", version: "1.0.0", type: "module" }));
fs.writeFileSync(path.join(app, "src", "dep.js"), "export const dep = 1;\n");
fs.writeFileSync(
  path.join(app, "src", "mod.js"),
  'import { dep } from "./dep.js";\nexport const val = () => dep + 1;\nexport default val;\n',
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><body><script type="module" src="/src/mod.js"></script></body></html>`,
);

const port = 5611;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
let failed = false;
try {
  for (let i = 0; i < 100; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  const modId = path.join(app, "src", "mod.js");

  const runner = await (await fetch(`http://localhost:${port}/@ssr-module?id=${encodeURIComponent(modId)}&runner=1`)).text();
  assert.match(runner, /await __vite_ssr_import__\("\.\/dep\.js", \{"importedNames":\["dep"\]\}\)/, `runner import: ${runner}`);
  assert.match(runner, /\(0, __vite_ssr_import_0__\.dep\)/, `runner live member: ${runner}`);
  assert.match(runner, /__vite_ssr_exportName__\("val"/, `runner export: ${runner}`);
  assert.doesNotMatch(runner, /^import /m, `runner still has an import statement: ${runner}`);

  const plain = await (await fetch(`http://localhost:${port}/@ssr-module?id=${encodeURIComponent(modId)}`)).text();
  assert.match(plain, /^import \{ dep \} from "\.\/dep\.js";/m, `non-runner should be plain ESM: ${plain}`);
  assert.doesNotMatch(plain, /__vite_ssr_/, `non-runner must not be runner-transformed: ${plain}`);

  console.log("SSR-RUNNER-ENDPOINT E2E PASSED");
} catch (err) {
  failed = true;
  console.error("SSR-RUNNER-ENDPOINT E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
