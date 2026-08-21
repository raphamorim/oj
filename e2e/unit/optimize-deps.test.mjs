// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..", "..");
const sidecar = path.join(repo, "crates/oj_server/src/assets/optimize-deps.mjs");
const esbuildSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild");

// The pre-bundler shells out to esbuild via the start-app fixture's install;
// skip (rather than hard-fail) where that fixture has no node_modules, matching
// the rolldown-fixture convention in asset-routing.test.mjs.
const it = fs.existsSync(esbuildSrc)
  ? test
  : (name, fn) => test(name, { skip: "fixture esbuild not installed" }, () => {});

const pkg = (nm, root, main, files) => {
  const dir = path.join(root, "node_modules", nm);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ name: nm, version: "1.0.0", main }));
  for (const [f, c] of Object.entries(files)) fs.writeFileSync(path.join(dir, f), c);
};

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx" }));

  pkg("defprop", root, "index.js", {
    "index.js":
      `"use strict";\n` +
      `Object.defineProperty(exports, "__esModule", { value: true });\n` +
      `Object.defineProperty(exports, "greet", { enumerable: true, get: function () { return greet; } });\n` +
      `function greet(n) { return "hi " + n; }\n`,
  });
  pkg("babeldefault", root, "index.js", {
    "index.js":
      `"use strict";\n` +
      `Object.defineProperty(exports, "__esModule", { value: true });\n` +
      `exports.default = void 0;\n` +
      `var _default = function () { return 42; };\n` +
      `exports.default = _default;\n`,
  });
  pkg("plaincjs", root, "index.js", {
    "index.js": `exports.a = 1;\nexports.b = 2;\n`,
  });

  fs.writeFileSync(
    path.join(root, "entry.js"),
    `import { greet } from "defprop";\n` +
      `import fortytwo from "babeldefault";\n` +
      `import { a, b } from "plaincjs";\n` +
      `export const out = greet("x") + "|" + fortytwo() + "|" + a + "|" + b;\n`,
  );
  return root;
}

it("optimize-deps: scans + pre-bundles CJS deps with correct interop", async () => {
  const root = fixture();
  const outDir = path.join(root, ".oj-cache", "deps");
  const cfg = JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")] });
  const stdout = execFileSync("node", [sidecar, cfg], { encoding: "utf8" });
  const { metadata } = JSON.parse(stdout);

  assert.deepEqual(Object.keys(metadata).sort(), ["babeldefault", "defprop", "plaincjs"]);
  for (const m of Object.values(metadata)) {
    assert.ok(fs.existsSync(path.join(outDir, m.file)), `missing ${m.file}`);
    assert.equal(m.needsInterop, true, "CJS dep flagged needsInterop");
  }

  const load = (dep) => import(pathToFileURL(path.join(outDir, metadata[dep].file)).href);

  const defprop = await load("defprop");
  assert.equal(defprop.default.greet("x"), "hi x", "Object.defineProperty export preserved through CJS->ESM");
  const babel = await load("babeldefault");
  assert.equal(babel.default.__esModule, true, "Babel __esModule flag preserved (consumer interop unwraps .default)");
  assert.equal(babel.default.default(), 42);
  const plain = await load("plaincjs");
  assert.equal(plain.default.a, 1);
  assert.equal(plain.default.b, 2);

  fs.rmSync(root, { recursive: true, force: true });
});

it("optimize-deps: resolves tsconfig `paths` with /* and externalizes a dep's CSS/font", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdeps2-"));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(root, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({ name: "fx2" }));

  // A tsconfig `paths` value contains `/*` — the exact shape a naive JSONC
  // comment stripper corrupts. If stripJsonc mishandles it, the alias never
  // loads, `@/aliased` is treated as an external bare dep, and `defprop`
  // (reachable ONLY through the alias) is never discovered.
  fs.writeFileSync(
    path.join(root, "tsconfig.json"),
    JSON.stringify({ compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } } }),
  );
  pkg("defprop", root, "index.js", {
    "index.js":
      `"use strict";\nObject.defineProperty(exports, "__esModule", { value: true });\n` +
      `Object.defineProperty(exports, "greet", { enumerable: true, get: function () { return greet; } });\n` +
      `function greet(n) { return "hi " + n; }\n`,
  });
  // A JS dep whose (relative) CSS pulls a (relative) .woff2. If those are not
  // externalized, esbuild fails the whole pre-bundle with "No loader is
  // configured for .woff2" and nothing gets optimized.
  pkg("uikit", root, "index.js", {
    "index.js": `import "./style.css";\nexport const ok = 1;\n`,
    "style.css": `@font-face { font-family: x; src: url(./f.woff2) format("woff2"); }\n`,
    "f.woff2": "not-a-real-font",
  });
  fs.mkdirSync(path.join(root, "src"), { recursive: true });
  fs.writeFileSync(path.join(root, "src", "aliased.js"), `import { greet } from "defprop";\nexport const v = greet("z");\n`);
  fs.writeFileSync(
    path.join(root, "entry.js"),
    `import { v } from "@/aliased";\nimport { ok } from "uikit";\nexport const out = v + ok;\n`,
  );

  const outDir = path.join(root, ".oj-cache", "deps");
  const cfg = JSON.stringify({ root, outDir, entries: [path.join(root, "entry.js")] });
  const stdout = execFileSync("node", [sidecar, cfg], { encoding: "utf8" });
  const { metadata } = JSON.parse(stdout);
  const names = Object.keys(metadata).sort();

  assert.ok(
    names.includes("defprop"),
    `dep reached only through the tsconfig \`@/\` alias must be discovered (stripJsonc + alias traversal); got ${names.join(", ")}`,
  );
  assert.ok(
    names.includes("uikit"),
    `JS dep with a CSS/font import must still pre-bundle (assets externalized); got ${names.join(", ")}`,
  );
  for (const m of Object.values(metadata)) {
    assert.ok(fs.existsSync(path.join(outDir, m.file)), `missing ${m.file}`);
  }

  fs.rmSync(root, { recursive: true, force: true });
});
