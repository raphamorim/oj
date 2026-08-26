// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Vite hands the `config` hook the user config (whose `plugins` is the flat
// plugin array) and `configResolved` a fully-defaulted config. Plugins such as
// @crxjs read `config.plugins`, and UnoCSS resolves `config.build.outDir` /
// reads `config.build.rollupOptions.output`. oj must supply both.
test("config and configResolved expose config.plugins and build defaults", async () => {
  const fx = tmpProject({ prefix: "oj-cfg-" });
  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [
       {
         name: "reader",
         config(config) {
           seen.configPlugins = Array.isArray(config.plugins) ? config.plugins.length : -1;
         },
         configResolved(config) {
           seen.outDir = config.build.outDir;
           seen.assetsDir = config.build.assetsDir;
           seen.hasRollupOptions = config.build.rollupOptions !== undefined;
           seen.resolvedPlugins = Array.isArray(config.plugins) ? config.plugins.length : -1;
         },
         transform(code, id) {
           if (id.endsWith("probe.js")) return "export default " + JSON.stringify(seen) + ";";
           return null;
         },
       },
       { name: "sibling-a" },
       { name: "sibling-b" },
     ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "transform",
      args: ["", path.join(fx.root, "probe.js")],
    });
    const seen = JSON.parse(JSON.parse(res.result).code.replace(/^export default /, "").replace(/;$/, ""));
    assert.equal(seen.configPlugins, 3, "the config hook sees the flat plugin array");
    assert.equal(seen.resolvedPlugins, 3, "configResolved sees the plugin array too");
    assert.equal(seen.outDir, "dist", "build.outDir defaults to dist");
    assert.equal(seen.assetsDir, "assets", "build.assetsDir defaults to assets");
    assert.equal(seen.hasRollupOptions, true, "build.rollupOptions is present");
  } finally {
    host.close();
    fx.cleanup();
  }
});
