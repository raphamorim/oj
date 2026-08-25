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

// A project with NO root index.html: entries come from build.rolldownOptions.input,
// two HTML pages under nested directories, each with a page-relative <script>.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-multihtml-"));
fs.mkdirSync(path.join(app, "pages", "admin"), { recursive: true });
fs.mkdirSync(path.join(app, "pages", "app"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "mh-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.config.json"),
  JSON.stringify({
    build: {
      rolldownOptions: {
        input: {
          admin: "pages/admin/index.html",
          app: "pages/app/index.html",
        },
      },
    },
  }),
);
fs.writeFileSync(path.join(app, "pages", "admin", "admin.js"), `document.title = "admin";\n`);
fs.writeFileSync(path.join(app, "pages", "app", "app.js"), `document.title = "app";\n`);
fs.writeFileSync(
  path.join(app, "pages", "admin", "index.html"),
  `<!doctype html><html><head><title>admin</title></head><body><script type="module" src="./admin.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "pages", "app", "index.html"),
  `<!doctype html><html><head><title>app</title></head><body><script type="module" src="./app.js"></script></body></html>`,
);

let failed = false;
try {
  // Must not fail with "no index.html" now that explicit input is honored.
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  const adminHtmlPath = path.join(app, "dist", "pages", "admin", "index.html");
  const appHtmlPath = path.join(app, "dist", "pages", "app", "index.html");
  assert.ok(fs.existsSync(adminHtmlPath), "admin page emitted at its root-relative path");
  assert.ok(fs.existsSync(appHtmlPath), "app page emitted at its root-relative path");

  const adminHtml = fs.readFileSync(adminHtmlPath, "utf8");
  const appHtml = fs.readFileSync(appHtmlPath, "utf8");

  // The page-relative script was rewritten to a hashed, server-absolute chunk.
  const adminSrc = adminHtml.match(/src="(\/assets\/[^"]+\.js)"/);
  const appSrc = appHtml.match(/src="(\/assets\/[^"]+\.js)"/);
  assert.ok(adminSrc, "admin html references a hashed chunk");
  assert.ok(appSrc, "app html references a hashed chunk");
  assert.notEqual(adminSrc[1], appSrc[1], "each page has its own entry chunk");

  for (const rel of [adminSrc[1], appSrc[1]]) {
    assert.ok(fs.existsSync(path.join(app, "dist", rel.replace(/^\//, ""))), `chunk ${rel} exists`);
  }

  // The build manifest lists both entries.
  const manifest = JSON.parse(fs.readFileSync(path.join(app, "dist", ".vite", "manifest.json"), "utf8"));
  const keys = Object.keys(manifest);
  assert.ok(
    keys.some((k) => k.includes("pages/admin/admin.js")),
    "manifest keys the admin entry by its source path",
  );
  assert.ok(
    keys.some((k) => k.includes("pages/app/app.js")),
    "manifest keys the app entry by its source path",
  );

  console.log("MULTI-HTML-INPUT E2E PASSED");
} catch (err) {
  failed = true;
  console.error("MULTI-HTML-INPUT E2E FAILED:", err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
