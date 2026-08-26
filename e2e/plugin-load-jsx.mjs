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

// A plugin-served virtual module with a `.jsx` id must be parsed as JSX (not
// plain JS). Regression for the unplugin-icons `~icons/*.jsx` case: oj used to
// hardcode module type Js for every plugin `load`, so oxc rejected the JSX.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-loadjsx-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "lj-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "entry.js"),
  `import { Icon } from "virtual:icon.jsx";\nwindow.__icon = Icon();\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
     name: "virtual-jsx",
     resolveId(id) {
       if (id === "virtual:icon.jsx") return "\\0virtual:icon.jsx";
       if (id === "react/jsx-runtime") return "\\0jsx-runtime";
       return null;
     },
     load(id) {
       if (id === "\\0virtual:icon.jsx") return "export const Icon = () => <svg width=\\"1em\\" data-mark=\\"icon-ok\\" />;";
       if (id === "\\0jsx-runtime") return "export const jsx = (t, p) => ({ t, p }); export const jsxs = jsx; export const Fragment = 'F';";
       return null;
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "pipe" });

  const jsFiles = fs
    .readdirSync(path.join(app, "dist", "assets"))
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(app, "dist", "assets", f), "utf8"));
  const all = jsFiles.join("\n");

  // JSX was transformed to a runtime call, not left as raw markup.
  assert.ok(!all.includes("<svg"), "raw JSX must not survive into the bundle");
  assert.ok(all.includes("icon-ok"), "the component's props made it through the JSX transform");

  console.log("PLUGIN-LOAD-JSX E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-LOAD-JSX E2E FAILED:", (err.stderr && err.stderr.toString()) || err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
