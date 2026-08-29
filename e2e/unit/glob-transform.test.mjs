// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { transformGlob } from "../../crates/oj_server/src/assets/start/glob-transform.mjs";

function fixture() {
  const dir = mkdtempSync(join(tmpdir(), "oj-glob-"));
  mkdirSync(join(dir, "content"), { recursive: true });
  writeFileSync(join(dir, "content", "a.md"), "# a");
  writeFileSync(join(dir, "content", "b.md"), "# b");
  writeFileSync(join(dir, "content", "index.json"), "{}");
  mkdirSync(join(dir, "pages", "home"), { recursive: true });
  mkdirSync(join(dir, "pages", "about"), { recursive: true });
  writeFileSync(join(dir, "pages", "home", "page.tsx"), "export default 1");
  writeFileSync(join(dir, "pages", "about", "page.tsx"), "export default 2");
  return dir;
}

test("leaves code without import.meta.glob untouched", () => {
  const code = "export const x = 1;\nconsole.log('hi');";
  assert.equal(transformGlob(code, "/app/x.ts"), code);
});

test("expands a lazy glob to a map of dynamic imports", () => {
  const dir = fixture();
  try {
    const out = transformGlob('const m = import.meta.glob("./content/*.md");', join(dir, "index.ts"));
    assert.match(out, /"\.\/content\/a\.md":\s*\(\)\s*=>\s*import\("\.\/content\/a\.md"\)/);
    assert.match(out, /"\.\/content\/b\.md":/);
    assert.doesNotMatch(out, /index\.json/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("handles the <T> type argument and a single-star segment", () => {
  const dir = fixture();
  try {
    const out = transformGlob('const p = import.meta.glob<string>("./pages/*/page.tsx");', join(dir, "index.ts"));
    assert.match(out, /"\.\/pages\/home\/page\.tsx":/);
    assert.match(out, /"\.\/pages\/about\/page\.tsx":/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Mirrors web/shared/pulse/components/data.ts, which broke oj's start SSR: a
// nested generic argument the old <[^>]*> regex could not skip.
test("handles a nested generic type argument (Record<string, unknown>)", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const m = import.meta.glob<Record<string, unknown>>(["./content/*.md"]);',
      join(dir, "index.ts"),
    );
    assert.match(out, /"\.\/content\/a\.md":/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("handles an object-literal generic type argument", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const m = import.meta.glob<{ default: string }>("./content/*.md", { eager: true });',
      join(dir, "index.ts"),
    );
    assert.match(out, /^import\s+\*\s+as\s+\w+\s+from\s+"\.\/content\/a\.md";/m);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("expands every call when a module has several generic globs at module scope", () => {
  const dir = fixture();
  try {
    const code = [
      'const a = import.meta.glob<{ default: Foo }>("./content/*.json");',
      'const b = import.meta.glob<SidecarModule>("./pages/**/*.tsx");',
      'const c = import.meta.glob<Record<string, unknown>>(["./content/*.md"]);',
    ].join("\n");
    const out = transformGlob(code, join(dir, "index.ts"));
    assert.doesNotMatch(out, /import\.meta\.glob/);
    assert.match(out, /"\.\/pages\/home\/page\.tsx":/);
    assert.match(out, /"\.\/content\/a\.md":/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("expands a pattern that climbs above the importer directory", () => {
  const dir = fixture();
  try {
    mkdirSync(join(dir, "app", "deep"), { recursive: true });
    const out = transformGlob(
      'const m = import.meta.glob<Record<string, unknown>>("../../content/*.md");',
      join(dir, "app", "deep", "index.ts"),
    );
    assert.match(out, /"\.\.\/\.\.\/content\/a\.md":/);
    assert.doesNotMatch(out, /import\.meta\.glob/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("eager + query + import:default produces static imports of the ?query", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const s = import.meta.glob("./content/*.md", { eager: true, query: "?raw", import: "default" });',
      join(dir, "index.ts"),
    );
    assert.match(out, /^import\s+\w+\s+from\s+"\.\/content\/a\.md\?raw";/m);
    assert.match(out, /^import\s+\w+\s+from\s+"\.\/content\/b\.md\?raw";/m);
    assert.doesNotMatch(out, /\(\)\s*=>\s*import/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("non-eager import:default awaits the default export", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const s = import.meta.glob("./content/*.md", { import: "default" });',
      join(dir, "index.ts"),
    );
    assert.match(out, /\(\)\s*=>\s*import\("\.\/content\/a\.md"\)\.then\(\(m\)\s*=>\s*m\.default\)/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("negated patterns exclude matches", () => {
  const dir = fixture();
  try {
    const out = transformGlob(
      'const m = import.meta.glob(["./content/*.md", "!./content/b.md"]);',
      join(dir, "index.ts"),
    );
    assert.match(out, /"\.\/content\/a\.md":/);
    assert.doesNotMatch(out, /"\.\/content\/b\.md":/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an empty glob yields an empty map, not a crash", () => {
  const dir = fixture();
  try {
    const out = transformGlob('const m = import.meta.glob("./nope/*.md");', join(dir, "index.ts"));
    assert.match(out, /const m = \{\}/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
