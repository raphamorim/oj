// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const binary = path.join(root, "target", "debug", "oj");
const project = fs.mkdtempSync(path.join(os.tmpdir(), "oj-unresolved-"));

try {
  fs.writeFileSync(path.join(project, "package.json"), JSON.stringify({ type: "module" }));
  fs.writeFileSync(path.join(project, "index.html"), '<html><body><script type="module" src="/main.js"></script></body></html>');
  fs.writeFileSync(path.join(project, "main.js"), 'import "missing-example-dependency"; document.body.textContent = "ready";');

  const missing = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
  assert.notEqual(missing.status, 0, `an unresolved dependency must fail the build:\n${missing.stdout}\n${missing.stderr}`);
  assert.match(`${missing.stdout}\n${missing.stderr}`, /missing-example-dependency/);

  fs.writeFileSync(path.join(project, "oj.config.json"), JSON.stringify({
    build: { rollupOptions: { external: ["missing-example-dependency"] } },
  }));
  const external = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
  assert.equal(external.status, 0, `an explicitly external dependency must remain allowed:\n${external.stdout}\n${external.stderr}`);

  console.log("BUILD-UNRESOLVED E2E PASSED");
} finally {
  fs.rmSync(project, { recursive: true, force: true });
}
