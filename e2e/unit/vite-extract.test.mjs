// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { extractAlias, extractProxy } from "../../crates/oj_server/src/assets/vite-extract.mjs";

test("string aliases pass through unchanged", () => {
  const out = extractAlias({ "@app": "/src", "~": "/src/lib" });
  assert.deepEqual(out, { "@app": "/src", "~": "/src/lib" });
});

test("array-form string aliases are read from find/replacement", () => {
  const out = extractAlias([{ find: "@app", replacement: "/src" }]);
  assert.deepEqual(out, { "@app": "/src" });
});

test("duplicate array-form aliases preserve Vite first-match precedence", () => {
  const out = extractAlias([
    { find: "@app", replacement: "/first/src" },
    { find: "@app", replacement: "/second/src" },
  ]);

  assert.equal(out["@app"], "/first/src");
});

test("a monorepo regex alias pair collapses to one directory alias", () => {
  // The standard TanStack/monorepo shape: an exact match to the package entry
  // plus a subpath match. Both must collapse to `@x/pkg -> .../src`.
  const out = extractAlias([
    { find: /^@excalidraw\/excalidraw$/, replacement: "/repo/packages/excalidraw/index.ts" },
    { find: /^@excalidraw\/excalidraw\/(.*)/, replacement: "/repo/packages/excalidraw/$1" },
  ]);
  assert.equal(out["@excalidraw/excalidraw"], "/repo/packages/excalidraw");
});

test("subpath-only regex derives the key and strips the trailing capture", () => {
  const out = extractAlias([{ find: /^@x\/pkg\/(.*)$/, replacement: "/abs/pkg/$1" }]);
  assert.equal(out["@x/pkg"], "/abs/pkg");
});

test("index.ext replacement suffix is stripped to the directory", () => {
  const out = extractAlias([{ find: /^lib$/, replacement: "/abs/lib/index.tsx" }]);
  assert.equal(out["lib"], "/abs/lib");
});

test("a regex too complex to express as a path alias is skipped, not mis-resolved", () => {
  const out = extractAlias([{ find: /^@x\/.*\/deep/, replacement: "/abs/$1" }]);
  assert.ok(!("@x" in out), `must not produce a bogus key: ${JSON.stringify(out)}`);
});

test("non-string replacements are ignored", () => {
  const out = extractAlias([{ find: "@fn", replacement: () => "/x" }]);
  assert.deepEqual(out, {});
});

function captureStderr(fn) {
  const lines = [];
  const write = process.stderr.write;
  process.stderr.write = (chunk) => { lines.push(String(chunk)); return true; };
  try { fn(); } finally { process.stderr.write = write; }
  return lines.join("");
}

test("proxy contexts pass through verbatim, regex (^) contexts included", () => {
  const out = extractProxy({
    "/api": "http://localhost:3000",
    "^/re/.*": { target: "http://localhost:3001", ws: true, changeOrigin: true },
  });
  assert.deepEqual(out, {
    "/api": "http://localhost:3000",
    "^/re/.*": { target: "http://localhost:3001", ws: true, changeOrigin: true },
  });
});

test("function-valued proxy options are dropped with a warning, the entry still proxies", () => {
  let out;
  const err = captureStderr(() => {
    out = extractProxy({
      "/api": {
        target: "http://localhost:3000",
        rewrite: (p) => p.replace(/^\/api/, ""),
        configure: () => {},
        bypass: () => null,
      },
    });
  });
  assert.deepEqual(out, { "/api": { target: "http://localhost:3000" } });
  assert.match(err, /server\.proxy\["\/api"\]\.rewrite is a function/);
  assert.match(err, /server\.proxy\["\/api"\]\.configure is a function and cannot cross the config bridge/);
  assert.match(err, /server\.proxy\["\/api"\]\.bypass is a function and cannot cross the config bridge/);
});
