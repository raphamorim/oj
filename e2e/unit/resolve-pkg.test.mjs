// SPDX-License-Identifier: MIT
// Unit tests for resolve-pkg.mjs: viteEnvDefine (import.meta.env) and the
// pnpm-strict-aware makeResolver (resolve a transitive dep through a direct-dep
// anchor).
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { viteEnvDefine, makeResolver, importPkg } from "../../crates/oj_server/src/assets/start/resolve-pkg.mjs";

test("viteEnvDefine builds import.meta.env with the standard flags", () => {
  process.env.VITE_ONLY_FOR_TEST = "hello";
  try {
    const def = viteEnvDefine({ ssr: false, mode: "development" });
    const env = JSON.parse(def["import.meta.env"]);
    assert.equal(env.MODE, "development");
    assert.equal(env.DEV, true);
    assert.equal(env.PROD, false);
    assert.equal(env.SSR, false);
    assert.equal(env.BASE_URL, "/");
    assert.equal(env.VITE_ONLY_FOR_TEST, "hello");
  } finally {
    delete process.env.VITE_ONLY_FOR_TEST;
  }
});

test("viteEnvDefine reflects ssr and production", () => {
  const env = JSON.parse(viteEnvDefine({ ssr: true, mode: "production" })["import.meta.env"]);
  assert.equal(env.SSR, true);
  assert.equal(env.MODE, "production");
  assert.equal(env.PROD, true);
  assert.equal(env.DEV, false);
});

function pkg(dir, name, main = "index.js") {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name, main, version: "1.0.0" }));
  writeFileSync(join(dir, main), "module.exports = {};");
}

test("makeResolver resolves a direct dep from the app root", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    pkg(join(root, "node_modules", "foo"), "foo");
    const resolve = makeResolver(root);
    assert.match(resolve("foo"), /node_modules[/\\]foo[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver reaches a transitive dep through a direct-dep anchor", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-anchor-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    // `anchor` is at the app root; `dep` is only under anchor (pnpm-strict style)
    pkg(join(root, "node_modules", "anchor"), "anchor");
    pkg(join(root, "node_modules", "anchor", "node_modules", "dep"), "dep");
    const resolve = makeResolver(root);
    // not reachable directly...
    assert.throws(() => resolve("dep"));
    // ...but reachable via the anchor
    assert.match(resolve("dep", ["anchor"]), /anchor[/\\]node_modules[/\\]dep[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver throws a clear error for a missing package", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-miss-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    const resolve = makeResolver(root);
    assert.throws(() => resolve("does-not-exist"), /cannot resolve/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

function writePkg(root, name, files) {
  const dir = join(root, "node_modules", name);
  mkdirSync(dir, { recursive: true });
  for (const [f, contents] of Object.entries(files)) writeFileSync(join(dir, f), contents);
}

test("importPkg imports a CJS package and unwraps module.exports", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-cjs-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "cjspkg", {
      "package.json": '{"name":"cjspkg","main":"index.js"}',
      "index.js": 'module.exports = { version: "1.0", build: () => "built" };',
    });
    // the `default` (CJS module.exports) is unwrapped so callers get the object
    const mod = await importPkg(root, "cjspkg");
    assert.equal(mod.version, "1.0");
    assert.equal(typeof mod.build, "function");
    assert.equal(mod.build(), "built");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg imports an ESM package and returns its namespace", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-esm-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "esmpkg", {
      "package.json": '{"name":"esmpkg","main":"index.mjs"}',
      // no default export, so importPkg returns the module namespace
      "index.mjs": 'export const value = 42; export function greet() { return "hi"; }',
    });
    const mod = await importPkg(root, "esmpkg");
    assert.equal(mod.value, 42);
    assert.equal(mod.greet(), "hi");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg reaches a transitive dep through a preferred anchor", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-anchor-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "anchor", { "package.json": '{"name":"anchor"}' });
    writePkg(root, join("anchor", "node_modules", "dep"), {
      "package.json": '{"name":"dep","main":"index.js"}',
      "index.js": 'module.exports = { ok: true };',
    });
    // not a direct dep of the app; only reachable via the anchor
    const mod = await importPkg(root, "dep", ["anchor"]);
    assert.equal(mod.ok, true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg rejects when the package cannot be resolved", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-miss-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    await assert.rejects(importPkg(root, "not-installed"), /cannot resolve/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
