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

// A plugin `transform` must run on CSS/preprocessor sources before oj compiles
// them, so directive transformers (UnoCSS `@apply`, etc.) resolve. Here a plugin
// rewrites a marker in a .scss source; the rewrite must reach the output CSS.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-csstx-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "csstx-app", version: "1.0.0" }));
// Nested SCSS + a marker only a plugin transform can resolve.
fs.writeFileSync(path.join(app, "src", "styles.scss"), `.card { __BRAND__ .inner { font-weight: bold; } }\n`);
fs.writeFileSync(path.join(app, "src", "entry.js"), `import "./styles.scss";\nconsole.log("app");\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
     name: "css-directive",
     transform(code, id) {
       if (id.endsWith(".scss") || id.endsWith(".css")) {
         return { code: code.replace("__BRAND__", "color: rebeccapurple;"), map: null };
       }
       return null;
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "pipe" });

  const cssDir = path.join(app, "dist", "assets");
  const css = fs
    .readdirSync(cssDir)
    .filter((f) => f.endsWith(".css"))
    .map((f) => fs.readFileSync(path.join(cssDir, f), "utf8"))
    .join("\n");

  // lightningcss minifies `rebeccapurple` to `#639`.
  assert.ok(
    css.includes("rebeccapurple") || css.includes("#639"),
    "the plugin transform ran on the CSS source before compilation",
  );
  assert.ok(!css.includes("__BRAND__"), "the source marker was resolved, not left raw");
  // The surrounding SCSS still compiled (nesting flattened).
  assert.ok(/\.card \.inner/.test(css) || /\.inner/.test(css), "the SCSS around the injected rule still compiled");

  console.log("CSS-PLUGIN-TRANSFORM E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-PLUGIN-TRANSFORM E2E FAILED:", (err.stderr && err.stderr.toString()) || err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
