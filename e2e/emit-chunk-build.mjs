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

// A plugin emits an extra chunk from a real source file during transform, then
// reads that chunk's final hashed name via getFileName() in generateBundle and
// writes it into an asset. This is the crx spine: emitFile({type:'chunk'}) ->
// the chunk becomes a bundled entry -> getFileName(refId) resolves to its name.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-emitchunk-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "ec-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "entry.js"), `console.log("entry");\n`);
fs.writeFileSync(path.join(app, "src", "content.js"), `export const marker = "content-chunk-loaded";\n`);
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
     name: "chunk-emitter",
     transform(code, id) {
       if (id.endsWith("entry.js")) {
         ref = this.emitFile({ type: "chunk", id: path.join(root, "src", "content.js"), name: "content" });
       }
       return null;
     },
     generateBundle() {
       this.emitFile({ type: "asset", fileName: "emitted-chunk-name.txt", source: this.getFileName(ref) });
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  const namePath = path.join(app, "dist", "emitted-chunk-name.txt");
  assert.ok(fs.existsSync(namePath), "generateBundle wrote the chunk name asset");
  const chunkName = fs.readFileSync(namePath, "utf8").trim();
  assert.match(chunkName, /content/, "getFileName resolved to the emitted chunk's name");
  assert.match(chunkName, /\.js$/, "the resolved name is a js chunk");

  const chunkPath = path.join(app, "dist", chunkName);
  assert.ok(fs.existsSync(chunkPath), `emitted chunk ${chunkName} was actually bundled`);
  const chunkCode = fs.readFileSync(chunkPath, "utf8");
  assert.match(chunkCode, /content-chunk-loaded/, "the emitted chunk contains its source");

  console.log("EMIT-CHUNK-BUILD E2E PASSED");
} catch (err) {
  failed = true;
  console.error("EMIT-CHUNK-BUILD E2E FAILED:", err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
