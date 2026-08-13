// SPDX-License-Identifier: MIT
// Unit tests for resolve-pkg.mjs: viteEnvDefine (import.meta.env) and the
// pnpm-strict-aware makeResolver (resolve a transitive dep through a direct-dep
// anchor).
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { viteEnvDefine, makeResolver } from "../../crates/oj_server/src/assets/start/resolve-pkg.mjs";

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
