// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-inline-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "inline-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "small.svg"), `<svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>`);
fs.writeFileSync(path.join(app, "src", "big.svg"), `<svg xmlns="http://www.w3.org/2000/svg">${"<rect/>".repeat(900)}</svg>`);
fs.writeFileSync(
  path.join(app, "src", "dot.png"),
  Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==", "base64"),
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import small from "./small.svg";\nimport big from "./big.svg";\nimport png from "./dot.png";\n` +
    `window.__S = small; window.__B = big; window.__PNG = png; window.__READY = true;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

const build = () => {
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
};
const assets = () => fs.readdirSync(path.join(app, "dist", "assets"));

let failed = false;
try {
  build();
  let files = assets();
  assert.ok(files.some((f) => f.startsWith("big-") && f.endsWith(".svg")), "big.svg emitted (over limit)");
  assert.ok(!files.some((f) => f.startsWith("small-") && f.endsWith(".svg")), "small.svg inlined (under limit)");
  assert.ok(!files.some((f) => f.endsWith(".png")), "png inlined (under limit)");

  const srv = spawn(oj, ["preview", app, "--port", "5351"], { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch("http://localhost:5351/")).ok) break; } catch {} await sleep(200); }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5351/", { timeout: 30000 });
    await page.waitForFunction(() => window.__READY, { timeout: 10000 });
    const r = await page.evaluate(() => ({ s: window.__S, b: window.__B, png: window.__PNG }));
    assert.match(r.s, /^data:image\/svg\+xml,/, "small svg is an inline data uri");
    assert.match(r.png, /^data:image\/png;base64,/, "png is an inline base64 data uri");
    assert.match(r.b, /\/assets\/big-.*\.svg$/, "big svg is an emitted file url");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
    await sleep(300);
  }

  fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ build: { assetsInlineLimit: 0 } }));
  build();
  files = assets();
  assert.ok(files.some((f) => f.startsWith("small-")) && files.some((f) => f.endsWith(".png")), "limit 0 emits every asset");

  console.log("ASSETS-INLINE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("ASSETS-INLINE E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
