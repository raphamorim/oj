// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const binary = path.join(root, "target", "debug", "oj");
const project = fs.mkdtempSync(path.join(os.tmpdir(), "oj-config-command-"));

try {
  fs.writeFileSync(path.join(project, "package.json"), JSON.stringify({ type: "module" }));
  fs.writeFileSync(path.join(project, "index.html"), '<html><body><script type="module" src="/main.js"></script></body></html>');
  fs.writeFileSync(path.join(project, "main.js"), 'document.body.textContent = __BUILD_MODE__;');
  fs.writeFileSync(path.join(project, "vite.config.mjs"), `
    export default ({ command, mode }) => ({
      base: command === "build" ? "/release/" : "/development/",
      define: { __BUILD_MODE__: JSON.stringify(mode) },
    });
  `);

  const build = spawnSync(binary, ["build", project, "--mode", "staging"], { cwd: root, encoding: "utf8" });
  assert.equal(build.status, 0, `build failed:\n${build.stdout}\n${build.stderr}`);

  const html = fs.readFileSync(path.join(project, "dist", "index.html"), "utf8");
  assert.match(html, /\/release\/assets\//, `Vite config must receive command=build:\n${html}`);

  const assets = fs.readdirSync(path.join(project, "dist", "assets"));
  const javascript = assets.filter((file) => file.endsWith(".js"))
    .map((file) => fs.readFileSync(path.join(project, "dist", "assets", file), "utf8"))
    .join("\n");
  assert.match(javascript, /staging/, `Vite config must receive the selected build mode:\n${javascript}`);

  console.log("VITE-CONFIG-COMMAND E2E PASSED");
} finally {
  fs.rmSync(project, { recursive: true, force: true });
}
