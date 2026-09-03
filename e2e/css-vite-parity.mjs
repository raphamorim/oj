// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite parity for specifiers inside stylesheets, in dev and in the build:
// resolve.alias applies to @import / @use / url(); a root-absolute /src/x is
// resolved against the root (public dir first); legacy hacks the postcss
// pipeline tolerates never fail a stylesheet.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 6350;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cssparity-"));
fs.mkdirSync(path.join(app, "src", "ui"), { recursive: true });
fs.mkdirSync(path.join(app, "public"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cssparity-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `import { fileURLToPath } from "node:url";\n` +
    `export default { resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } } };\n`,
);
fs.writeFileSync(path.join(app, "src", "vars.css"), `:root { --v: 1; }\n`);
fs.writeFileSync(path.join(app, "src", "root.css"), `.root-abs { color: green; }\n`);
fs.writeFileSync(path.join(app, "src", "img.png"), Buffer.alloc(5000, 9));
fs.writeFileSync(path.join(app, "public", "pub.png"), Buffer.alloc(100, 1));
fs.writeFileSync(path.join(app, "src", "_tokens.scss"), `$c: #123456;\n`);
fs.writeFileSync(
  path.join(app, "src", "ui", "theme.scss"),
  `@use "@/tokens" as t;\n.theme { color: t.$c; background: url("@/img.png"); }\n`,
);
fs.writeFileSync(
  path.join(app, "src", "ui", "app.css"),
  `@import "@/vars.css";\n@import "/src/root.css";\n` +
    `.a { background: url("@/img.png"); }\n.b { background: url(/src/img.png); }\n.c { background: url(/pub.png); }\n` +
    `.hack { *zoom: 1; color: red; }\n`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./ui/app.css";\nimport "./ui/theme.scss";\nwindow.__OK = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  for (let i = 0; i < 100; i++) { try { if ((await fetch(`http://localhost:${PORT}/`)).ok) break; } catch {} await sleep(200); }
  const res = await fetch(`http://localhost:${PORT}/src/ui/app.css?direct`);
  const css = await res.text();
  assert.equal(res.status, 200, `stylesheet with a legacy hack still serves:\n${css}`);
  assert.match(css, /--v:\s*1/, `aliased @import inlined:\n${css}`);
  assert.match(css, /\.root-abs/, `root-absolute @import inlined:\n${css}`);
  assert.match(css, /\.a\s*\{[^}]*url\(["']?\/src\/img\.png/, `aliased url() -> served url of the file:\n${css}`);
  assert.doesNotMatch(css, /\/src\/@\//, `alias treated as a relative segment:\n${css}`);
  assert.match(css, /\.b\s*\{[^}]*url\(["']?\/src\/img\.png/, "root-absolute url() kept");
  assert.match(css, /url\(["']?\/pub\.png/, "public url kept");
  assert.match(css, /\.hack\s*\{[^}]*color:\s*red/, `rule with a dropped hack keeps its other declarations:\n${css}`);
  const scss = await (await fetch(`http://localhost:${PORT}/src/ui/theme.scss?direct`)).text();
  assert.match(scss, /#123456/, `sass @use through the alias:\n${scss}`);
  assert.match(scss, /url\(["']?\/src\/img\.png/, `aliased url() in sass output:\n${scss}`);
  console.log("[dev] alias + root-absolute + error recovery OK");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const files = fs.readdirSync(assets);
  const cssFiles = files.filter((f) => f.endsWith(".css"));
  assert.ok(cssFiles.length > 0, `chunk stylesheet emitted: ${files}`);
  const built = cssFiles.map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
  const img = files.find((f) => /^img-[A-Za-z0-9_-]+\.png$/.test(f));
  assert.ok(img, `aliased / root-absolute url() asset emitted: ${files}`);
  assert.match(built, /--v:\s*1/, "aliased @import inlined in build");
  assert.match(built, /\.root-abs/, "root-absolute @import inlined in build");
  assert.match(built, /#123456/, "sass alias in build");
  assert.doesNotMatch(built, /@\/img\.png|url\(["']?\/src\/img\.png/, `alias and root-absolute urls rewritten to the emitted asset:\n${built}`);
  const refs = built.match(new RegExp(img.replace(/\./g, "\\."), "g")) || [];
  assert.ok(refs.length >= 1, `css references the emitted asset:\n${built}`);
  assert.match(built, /url\(["']?\/pub\.png/, "public url kept in build");
  assert.match(built, /\.hack\{color:red\}/, `hack dropped, rule kept in build:\n${built}`);
  console.log("[build] alias + root-absolute + error recovery OK");
  console.log("CSS-VITE-PARITY E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-VITE-PARITY E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
