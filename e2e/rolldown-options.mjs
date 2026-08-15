// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-rolldownopts-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "ro-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.config.json"),
  JSON.stringify({
    build: {
      rolldownOptions: {
        output: { entryFileNames: "custom/[name].js", chunkFileNames: "custom/[name]-[hash].js" },
        external: ["cdn-lib"],
      },
    },
  }),
);
fs.writeFileSync(path.join(app, "src", "main.js"), `import { thing } from "cdn-lib";\nwindow.__X = thing;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  assert.ok(fs.existsSync(path.join(app, "dist", "custom", "main.js")), "entryFileNames template applied");
  assert.ok(!fs.existsSync(path.join(app, "dist", "assets")), "default assets dir not used");

  const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
  assert.match(html, /src="\/custom\/main\.js"/, "html references the custom entry path");

  const entry = fs.readFileSync(path.join(app, "dist", "custom", "main.js"), "utf8");
  assert.match(entry, /["']cdn-lib["']/, "external module left as a bare import, not bundled");

  console.log("ROLLDOWN-OPTIONS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("ROLLDOWN-OPTIONS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
