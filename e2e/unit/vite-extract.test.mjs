// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { extractAlias } from "../../crates/oj_server/src/assets/vite-extract.mjs";

test("string aliases pass through unchanged", () => {
  const out = extractAlias({ "@app": "/src", "~": "/src/lib" });
  assert.deepEqual(out, { "@app": "/src", "~": "/src/lib" });
});

test("array-form string aliases are read from find/replacement", () => {
  const out = extractAlias([{ find: "@app", replacement: "/src" }]);
  assert.deepEqual(out, { "@app": "/src" });
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
