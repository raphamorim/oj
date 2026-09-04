// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The native plugin seam in `oj build`, through the in-tree example plugin:
// the one oj-provided rolldown adapter runs the same AST pass as dev (the
// marker is rewritten in the chunk, JSX compiled after it), the plugin's
// virtual sheet is emitted as assets/marker-<hash>.css with a manifest row and
// the page's dev link to it repointed, and the `@marker;` directive is
// expanded inside the compiled stylesheet asset.

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj --features example-plugin", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-native-plugin-build-"));
const write = (rel, content) => {
  const p = path.join(app, rel);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, content);
};
write("package.json", JSON.stringify({ name: "native-plugin-app", private: true, type: "module" }));
write("oj.config.json", JSON.stringify({ marker: { prefix: "mk" }, build: { manifest: true } }));
write(
  "index.html",
  `<!doctype html><html><head>
<link rel="stylesheet" href="/src/styles.css" />
<link rel="stylesheet" href="/@oj/marker.css" />
</head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);
// `imported.css` is loaded by rolldown while other modules are still being
// transformed; its directive must still see every marked module.
let mainSrc = `import "./imported.css";\nimport { hello } from "./hello";\n`;
const MANY = 24;
for (let i = 0; i < MANY; i++) {
  write(`src/m${i}.tsx`, `export const c${i} = __MARKER__;\nexport const el${i} = <b className={__MARKER__}>x</b>;\n`);
  mainSrc += `import { c${i} } from "./m${i}";\n`;
}
mainSrc += `document.getElementById("root").textContent = hello() + [${Array.from({ length: MANY }, (_, i) => `c${i}`).join(",")}].join();\n`;
write("src/main.tsx", mainSrc);
write("src/imported.css", `.c { color: green }\n@marker;\n.d { color: black }\n`);
write(
  "src/hello.tsx",
  `export function hello(): string {
  const tag: string = __MARKER__;
  const el = <span className={__MARKER__}>{tag}</span>;
  return tag + ":" + typeof el;
}
`,
);
write("src/styles.css", `.a { color: red }\n@marker;\n.b { color: blue }\n`);
write(
  "node_modules/react/package.json",
  JSON.stringify({
    name: "react",
    version: "0.0.0-stub",
    type: "module",
    exports: { ".": "./index.js", "./jsx-runtime": "./jsx-runtime.js", "./jsx-dev-runtime": "./jsx-dev-runtime.js" },
  }),
);
write("node_modules/react/index.js", "export default {};\n");
write("node_modules/react/jsx-runtime.js", "export const Fragment = 0;\nexport function jsx(type, props) { return { type, props }; }\nexport const jsxs = jsx;\n");
write("node_modules/react/jsx-dev-runtime.js", "export const Fragment = 0;\nexport function jsxDEV(type, props) { return { type, props }; }\n");

let failed = false;
try {
  const out = execSync(`${oj} build ${app}`, { stdio: "pipe" }).toString();
  assert.ok(out.includes("native plugins: example-marker"), `the plugin announces itself:\n${out}`);
  const dist = path.join(app, "dist");
  const assets = fs.readdirSync(path.join(dist, "assets"));

  // The rewritten chunk.
  const js = assets.filter((f) => f.endsWith(".js"));
  assert.equal(js.length, 1, `one chunk: ${assets}`);
  const chunk = fs.readFileSync(path.join(dist, "assets", js[0]), "utf8");
  assert.ok(chunk.includes("mk-hello"), `marker rewritten in the chunk:\n${chunk}`);
  assert.ok(!chunk.includes("__MARKER__"), "no marker left in the chunk");

  // The emitted virtual sheet, hashed and in the manifest.
  const marker = assets.find((f) => /^marker-[0-9a-f]{8}\.css$/.test(f));
  assert.ok(marker, `virtual sheet emitted as assets/marker-<hash>.css: ${assets}`);
  const markerCss = fs.readFileSync(path.join(dist, "assets", marker), "utf8");
  assert.ok(markerCss.includes(".mk-hello{--marker-count:2}"), `virtual sheet has hello: ${markerCss}`);
  assert.equal((markerCss.match(/--marker-count/g) || []).length, MANY + 1, `virtual sheet has every marked module: ${markerCss}`);
  const manifest = JSON.parse(fs.readFileSync(path.join(dist, ".vite", "manifest.json"), "utf8"));
  assert.equal(manifest["@oj/marker.css"]?.file, `assets/${marker}`, `manifest row: ${JSON.stringify(manifest, null, 1)}`);
  assert.ok(manifest["src/main.tsx"].css.includes(`assets/${marker}`), "the entry lists the sheet");

  // The page: the dev link is repointed at the asset, and only once.
  const html = fs.readFileSync(path.join(dist, "index.html"), "utf8");
  assert.ok(html.includes(`href="/assets/${marker}"`), `page links the emitted sheet:\n${html}`);
  assert.ok(!html.includes("/@oj/marker.css"), "the dev url is gone");
  assert.equal(html.split(marker).length - 1, 1, "linked exactly once");

  // The directive, expanded inside the compiled stylesheet.
  const styles = assets.find((f) => /^styles-[0-9a-f]{8}\.css$/.test(f));
  assert.ok(styles, `styles asset: ${assets}`);
  const css = fs.readFileSync(path.join(dist, "assets", styles), "utf8");
  const a = css.indexOf(".a{");
  const m = css.indexOf(".mk-hello{--marker-count:2}");
  const b = css.indexOf(".b{");
  assert.ok(a >= 0 && m > a && b > m, `directive expanded between .a and .b:\n${css}`);
  assert.ok(!css.includes("@oj-directive") && !css.includes("@marker"), "no sentinel leaks");
  assert.ok(!out.includes("Unknown at rule: @oj-directive"), `the sentinel is not reported as a warning:\n${out}`);

  // The JS-imported stylesheet: expanded after the bundle, so the directive
  // sees all MANY + 1 marked modules, not whatever had been transformed when
  // rolldown happened to load the sheet.
  const imported = assets.find((f) => /^main-[0-9a-f]{8}\.css$/.test(f));
  assert.ok(imported, `JS-imported sheet emitted with its chunk: ${assets}`);
  const icss = fs.readFileSync(path.join(dist, "assets", imported), "utf8");
  assert.equal((icss.match(/--marker-count/g) || []).length, MANY + 1, `directive in a JS-imported sheet saw every module:\n${icss}`);
  const c = icss.indexOf(".c{");
  const first = icss.indexOf("--marker-count");
  const d = icss.indexOf(".d{");
  assert.ok(c >= 0 && first > c && d > first, `expanded in place between .c and .d:\n${icss}`);
  assert.ok(!icss.includes("@oj-directive") && !icss.includes("@marker"), "no sentinel leaks in the imported sheet");

  console.log("native-plugin-build: ok");
} catch (e) {
  failed = true;
  console.error(e);
  if (e.stderr) console.error(e.stderr.toString());
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
