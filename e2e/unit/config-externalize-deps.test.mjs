// SPDX-License-Identifier: MIT

// A TS vite.config bundled with `packages: "external"` keeps bare specifiers
// bare, and the bundle is imported from the cache dir, so they re-resolve from
// THERE at import time. The monorepo shape breaks it: the config relatively
// imports a sibling workspace package's source (which gets inlined), and that
// source's bare deps live under the sibling's own node_modules -- unreachable
// from the cache dir ("Cannot find package 'picomatch' imported from
// .../.oj-cache/v1/oj-vite-config-....tmp.mjs"). A present-but-unloadable
// vite.config is a hard error, so `oj dev` dies (issue #146's shape).
//
// The fix mirrors Vite's `externalize-deps` plugin in both config bundlers:
// every bare import is resolved from its importer at bundle time and the
// resolved absolute path is externalized instead.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { asset, repo, rpcSidecar, testWithEsbuild } from "./harness.mjs";

const testEsbuild = testWithEsbuild(test);

// base/
//   node_modules/esbuild        (symlinked from the start-app fixture)
//   app/vite.config.ts          imports ../pkg/src/plugin (inlined by esbuild)
//   pkg/src/plugin.ts           imports "only-dep" (bare)
//   pkg/node_modules/only-dep/  the ONLY place the dep exists
function monorepoFixture() {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cfg-ext-"));
  const write = (rel, content) => {
    const p = path.join(base, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, content);
  };
  fs.mkdirSync(path.join(base, "node_modules"));
  fs.symlinkSync(
    path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild"),
    path.join(base, "node_modules/esbuild"),
  );
  const scoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(scoped)) fs.symlinkSync(scoped, path.join(base, "node_modules/@esbuild"));

  write("app/package.json", JSON.stringify({ name: "app", type: "module" }));
  write(
    "app/vite.config.ts",
    `import { makePlugin, ojBase } from "../pkg/src/plugin";
export default {
  base: ojBase,
  plugins: [makePlugin()],
};
`,
  );
  write("pkg/package.json", JSON.stringify({ name: "pkg", type: "module" }));
  write(
    "pkg/src/plugin.ts",
    `import { fromDep } from "only-dep";
export const ojBase = fromDep;
export function makePlugin() {
  return {
    name: "pkg-plugin",
    config() {
      return { define: { __FROM_PKG__: JSON.stringify(fromDep) } };
    },
  };
}
`,
  );
  write(
    "pkg/node_modules/only-dep/package.json",
    JSON.stringify({ name: "only-dep", version: "1.0.0", type: "module", main: "index.js" }),
  );
  write("pkg/node_modules/only-dep/index.js", `export const fromDep = "/from-pkg-dep/";\n`);
  return {
    base,
    appRoot: path.join(base, "app"),
    configPath: path.join(base, "app", "vite.config.ts"),
    cleanup: () => fs.rmSync(base, { recursive: true, force: true }),
  };
}

testEsbuild("vite-extract's esbuild fallback loads a config importing a sibling package's source", () => {
  const fx = monorepoFixture();
  // Run a copy of the extractor from its own directory, the way it runs from a
  // fresh cache dir: nothing from pkg/node_modules is resolvable from there.
  const runDir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cfg-ext-run-"));
  try {
    const script = path.join(runDir, "vite-extract.mjs");
    fs.copyFileSync(asset("vite-extract.mjs"), script);
    const out = execFileSync(
      process.execPath,
      [script, fx.configPath, fx.appRoot, "serve", "development", "default"],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: 60_000 },
    );
    const json = JSON.parse(out);
    assert.equal(json.__ok, true, `extraction failed, got: ${out}`);
    assert.equal(json.base, "/from-pkg-dep/", "base set from the constant only pkg/node_modules provides");
  } finally {
    fs.rmSync(runDir, { recursive: true, force: true });
    fx.cleanup();
  }
});

testEsbuild("the plugin host loads the same config and the ../pkg plugin is active", async () => {
  const fx = monorepoFixture();
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      fx.configPath,
      JSON.stringify({
        pluginsFormat: "vite",
        config: { root: fx.appRoot },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.appRoot },
    cwd: fx.appRoot,
  });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", `plugin from ../pkg did not load; stderr:\n${host.stderr()}`);
    const res = await host.send({ id: 2, hook: "getPluginConfig", args: [] });
    const cfg = JSON.parse(res.result);
    assert.equal(cfg.define?.__FROM_PKG__, '"/from-pkg-dep/"', "config() hook ran with the dep's value");
  } finally {
    host.close();
    fx.cleanup();
  }
});
