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

// A plugin emits an HTML page as a chunk (the way @crxjs emits its manifest's
// popup/options pages). oj must treat the `.html` id as a page: extract its
// <script type=module>, bundle those as entries, emit the processed HTML at its
// root-relative path, and resolve getFileName(refId) to the page's output path.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-htmlchunk-"));
fs.mkdirSync(path.join(app, "src", "pages", "popup"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hc-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "entry.js"), `console.log("entry");\n`);
fs.writeFileSync(path.join(app, "src", "pages", "popup", "popup.js"), `export const popup = "popup-page-body";\n`);
fs.writeFileSync(
  path.join(app, "src", "pages", "popup", "index.html"),
  `<!doctype html><html><head><title>popup</title></head><body><script type="module" src="./popup.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import path from "node:path";
   import { fileURLToPath } from "node:url";
   const root = path.dirname(fileURLToPath(import.meta.url));
   let ref;
   export default [{
     name: "html-page-emitter",
     buildStart(options) {
       if (typeof options.input !== "undefined") {
         ref = this.emitFile({ type: "chunk", id: path.join(root, "src", "pages", "popup", "index.html"), name: "popup" });
       }
     },
     generateBundle() {
       this.emitFile({ type: "asset", fileName: "popup-path.txt", source: this.getFileName(ref) });
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  // The emitted page was rendered at its root-relative output path.
  const pagePath = path.join(app, "dist", "src", "pages", "popup", "index.html");
  assert.ok(fs.existsSync(pagePath), "emitted .html page rendered at its root-relative path");
  const pageHtml = fs.readFileSync(pagePath, "utf8");

  // Its page-relative script was bundled and rewritten to a hashed chunk.
  const m = pageHtml.match(/src="(\/assets\/[^"]+\.js)"/);
  assert.ok(m, "the page's script was rewritten to a hashed chunk");
  const chunkPath = path.join(app, "dist", m[1].replace(/^\//, ""));
  assert.ok(fs.existsSync(chunkPath), "the page's entry chunk was bundled");
  assert.match(fs.readFileSync(chunkPath, "utf8"), /popup-page-body/, "chunk holds the page's source");

  // getFileName(refId) for the emitted page returns its output path.
  const resolved = fs.readFileSync(path.join(app, "dist", "popup-path.txt"), "utf8").trim();
  assert.equal(resolved, "src/pages/popup/index.html", "getFileName resolves the emitted page path");

  // The root index.html still built.
  assert.ok(fs.existsSync(path.join(app, "dist", "index.html")), "root page still built");

  console.log("EMIT-HTML-CHUNK E2E PASSED");
} catch (err) {
  failed = true;
  console.error("EMIT-HTML-CHUNK E2E FAILED:", err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
