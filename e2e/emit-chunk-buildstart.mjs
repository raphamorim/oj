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

// A plugin emits its root chunk in buildStart (the way @crxjs emits its manifest
// chunk) and reads that chunk's hashed name via getFileName() in generateBundle.
// buildStart runs as a rolldown hook, so the emitted chunk becomes a build root.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-bsemit-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "bs-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "entry.js"), `console.log("entry");\n`);
fs.writeFileSync(path.join(app, "src", "background.js"), `export const bg = "background-chunk-loaded";\n`);
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
     name: "buildstart-emitter",
     buildStart(options) {
       // Emit only when input is defined, mirroring @crxjs's manifest-loader.
       if (typeof options.input !== "undefined") {
         ref = this.emitFile({ type: "chunk", id: path.join(root, "src", "background.js"), name: "background" });
       }
     },
     generateBundle() {
       this.emitFile({ type: "asset", fileName: "bg-name.txt", source: this.getFileName(ref) });
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  const namePath = path.join(app, "dist", "bg-name.txt");
  assert.ok(fs.existsSync(namePath), "generateBundle wrote the chunk name asset");
  const chunkName = fs.readFileSync(namePath, "utf8").trim();
  assert.match(chunkName, /background/, "getFileName resolved the buildStart-emitted chunk");
  assert.match(chunkName, /\.js$/, "the resolved name is a js chunk");

  const chunkPath = path.join(app, "dist", chunkName);
  assert.ok(fs.existsSync(chunkPath), `emitted chunk ${chunkName} was bundled as a build root`);
  assert.match(fs.readFileSync(chunkPath, "utf8"), /background-chunk-loaded/, "chunk contains its source");

  console.log("EMIT-CHUNK-BUILDSTART E2E PASSED");
} catch (err) {
  failed = true;
  console.error("EMIT-CHUNK-BUILDSTART E2E FAILED:", err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
