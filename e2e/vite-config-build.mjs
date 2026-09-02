// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A `vite.config` exported as a function branches on `command`/`mode`, and its
// `build` block names the output. `oj build` must evaluate it as a build (not as
// `serve`/`development`) and honor `build.outDir`/`build.sourcemap`, for the
// default mode and for `--mode`.

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vitecfg-build-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "vitecfg-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `export default ({ command, mode }) => ({\n` +
    `  define: { __CMD__: JSON.stringify(command), __MODE__: JSON.stringify(mode) },\n` +
    `  build: { outDir: command === "build" ? "out-" + mode : "out-serve", sourcemap: true },\n` +
    `});\n`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__CMD = __CMD__; window.__MODE = __MODE__;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

function builtMain(outDir) {
  const assets = path.join(app, outDir, "assets");
  assert.ok(fs.existsSync(path.join(app, outDir, "index.html")), `${outDir}/index.html written`);
  const js = fs.readdirSync(assets).find((f) => f.startsWith("main-") && f.endsWith(".js"));
  assert.ok(js, `${outDir}: main chunk emitted`);
  assert.ok(fs.existsSync(path.join(assets, js + ".map")), `${outDir}: build.sourcemap honored`);
  return fs.readFileSync(path.join(assets, js), "utf8");
}

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  assert.ok(!fs.existsSync(path.join(app, "out-serve")), "config was evaluated as serve");
  assert.ok(!fs.existsSync(path.join(app, "dist")), "build.outDir ignored (fell back to dist)");
  const prod = builtMain("out-production");
  assert.match(prod, /[`"]build[`"]/, "define saw command=build");
  assert.match(prod, /[`"]production[`"]/, "define saw mode=production");

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app} --mode staging`, { stdio: "ignore" });
  const staging = builtMain("out-staging");
  assert.match(staging, /[`"]staging[`"]/, "define saw mode=staging");

  console.log("VITE-CONFIG-BUILD E2E PASSED");
} catch (err) {
  failed = true;
  console.error("VITE-CONFIG-BUILD E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
