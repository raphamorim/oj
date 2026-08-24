// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const binary = path.join(root, "target", "debug", "oj");
const project = fs.mkdtempSync(path.join(os.tmpdir(), "oj-multi-page-"));

try {
  fs.writeFileSync(path.join(project, "package.json"), JSON.stringify({ type: "module" }));
  fs.writeFileSync(path.join(project, "index.html"), '<html><body>home<script type="module" src="/home.js"></script></body></html>');
  fs.writeFileSync(path.join(project, "about.html"), '<html><body>about<script type="module" src="/about.js"></script></body></html>');
  fs.writeFileSync(path.join(project, "home.js"), 'document.body.dataset.page = "home";');
  fs.writeFileSync(path.join(project, "about.js"), 'document.body.dataset.page = "about";');
  fs.writeFileSync(path.join(project, "vite.config.mjs"), `
    export default {
      build: {
        rollupOptions: {
          input: {
            home: new URL("./index.html", import.meta.url).pathname,
            about: new URL("./about.html", import.meta.url).pathname,
          },
        },
      },
    };
  `);

  const build = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
  assert.equal(build.status, 0, `build failed:\n${build.stdout}\n${build.stderr}`);

  const home = fs.readFileSync(path.join(project, "dist", "index.html"), "utf8");
  assert.match(home, /\/assets\/home-[A-Za-z0-9_-]+\.js/);
  const aboutPath = path.join(project, "dist", "about.html");
  assert.ok(fs.existsSync(aboutPath), "every HTML entry declared in rollupOptions.input must be emitted");
  const about = fs.readFileSync(aboutPath, "utf8");
  assert.match(about, /\/assets\/about-[A-Za-z0-9_-]+\.js/);

  console.log("MULTI-PAGE-BUILD E2E PASSED");
} finally {
  fs.rmSync(project, { recursive: true, force: true });
}
