// SPDX-License-Identifier: MIT
// Unit tests for esbuild-assets.mjs: the content-hash emitter (incl. CSS url()
// rewriting + Tailwind compile hook), workspaceRoot, pnpmStorePaths, and the
// needsCssCompile predicate.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  needsCssCompile,
  workspaceRoot,
  pnpmStorePaths,
  contentHashEmitter,
} from "../../crates/oj_server/src/assets/start/esbuild-assets.mjs";

const tmp = (p) => mkdtempSync(join(tmpdir(), "oj-assets-" + p + "-"));

test("needsCssCompile detects Tailwind markers only", () => {
  assert.ok(needsCssCompile('@import "tailwindcss";'));
  assert.ok(needsCssCompile("@tailwind base;"));
  assert.ok(needsCssCompile('@plugin "@tailwindcss/typography";'));
  assert.ok(needsCssCompile(".b{@apply flex}"));
  assert.ok(!needsCssCompile(".b{color:red}"));
  assert.ok(!needsCssCompile("@font-face{src:url(x.woff2)}"));
});

test("workspaceRoot returns the farthest ancestor with node_modules", () => {
  const base = tmp("ws");
  try {
    mkdirSync(join(base, "node_modules"), { recursive: true });
    mkdirSync(join(base, "web", "node_modules"), { recursive: true });
    mkdirSync(join(base, "web", "src"), { recursive: true });
    assert.equal(workspaceRoot(join(base, "web")), base);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("pnpmStorePaths lists every .pnpm package node_modules dir", () => {
  const base = tmp("pnpm");
  try {
    const store = join(base, "node_modules", ".pnpm");
    mkdirSync(join(store, "react@19", "node_modules"), { recursive: true });
    mkdirSync(join(store, "@babel+runtime@7", "node_modules"), { recursive: true });
    mkdirSync(join(store, "no-nm-here"), { recursive: true }); // skipped (no node_modules)
    const paths = pnpmStorePaths(base);
    assert.equal(paths.length, 2);
    assert.ok(paths.some((p) => p.endsWith(join("react@19", "node_modules"))));
    assert.ok(paths.some((p) => p.endsWith(join("@babel+runtime@7", "node_modules"))));
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit copies a file under a content hash and returns its URL", async () => {
  const base = tmp("emit");
  try {
    const src = join(base, "src");
    mkdirSync(src, { recursive: true });
    writeFileSync(join(src, "logo.png"), "PNGDATA");
    const emit = contentHashEmitter(join(base, "client"));
    const url = await emit(join(src, "logo.png"));
    assert.match(url, /^\/assets\/logo-[0-9a-f]{8}\.png$/);
    assert.ok(existsSync(join(base, "client", "assets", url.slice("/assets/".length))));
    // identical bytes -> identical URL (idempotent, no manifest needed)
    assert.equal(await emit(join(src, "logo.png")), url);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit rewrites CSS url() refs and emits the referenced assets", async () => {
  const base = tmp("css");
  try {
    const styles = join(base, "styles");
    mkdirSync(join(styles, "fonts"), { recursive: true });
    writeFileSync(join(styles, "fonts", "font.woff2"), "WOFF2");
    writeFileSync(
      join(styles, "app.css"),
      "@font-face{font-family:x;src:url(./fonts/font.woff2)}.d{background:url(data:image/gif;base64,AA)}",
    );
    const emit = contentHashEmitter(join(base, "client"));
    const url = await emit(join(styles, "app.css"));
    const out = readFileSync(join(base, "client", "assets", url.slice("/assets/".length)), "utf8");
    // the font ref is rewritten to a hashed /assets URL...
    assert.match(out, /url\("\/assets\/font-[0-9a-f]{8}\.woff2"\)/);
    // ...and the font itself was emitted
    assert.ok(readdirSync(join(base, "client", "assets")).some((f) => /^font-[0-9a-f]{8}\.woff2$/.test(f)));
    // data: urls are left untouched
    assert.match(out, /url\(data:image\/gif;base64,AA\)/);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit compiles CSS via compileCss before rewriting when it needs it", async () => {
  const base = tmp("tw");
  try {
    mkdirSync(join(base, "styles"), { recursive: true });
    writeFileSync(join(base, "styles", "globals.css"), '@import "tailwindcss";');
    let called = 0;
    const compileCss = async (from, src) => {
      called++;
      assert.ok(src.includes("tailwindcss"));
      return ".compiled{color:red}"; // stand-in for Tailwind output
    };
    const emit = contentHashEmitter(join(base, "client"), compileCss);
    const url = await emit(join(base, "styles", "globals.css"));
    const out = readFileSync(join(base, "client", "assets", url.slice("/assets/".length)), "utf8");
    assert.equal(called, 1);
    assert.equal(out, ".compiled{color:red}");
    assert.ok(!out.includes('@import "tailwindcss"'));
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("emit does not compile plain CSS (no markers)", async () => {
  const base = tmp("plain");
  try {
    mkdirSync(join(base, "styles"), { recursive: true });
    writeFileSync(join(base, "styles", "plain.css"), ".x{color:blue}");
    let called = 0;
    const emit = contentHashEmitter(join(base, "client"), async () => { called++; return ""; });
    const url = await emit(join(base, "styles", "plain.css"));
    const out = readFileSync(join(base, "client", "assets", url.slice("/assets/".length)), "utf8");
    assert.equal(called, 0);
    assert.equal(out, ".x{color:blue}");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
