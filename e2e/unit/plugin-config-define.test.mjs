// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The Rust side asks the host for what the plugins' config() hooks contributed
// (getPluginConfig) so `define` reaches oj's compile the way Vite's merged
// config.define does. Only plugin-returned entries are reported; the user's own
// define (already in the initial config) is not echoed back.
test("getPluginConfig reports define entries returned by config() hooks", async () => {
  const fx = tmpProject({ prefix: "oj-cfgdef-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [
      { name: "a", config() { return { define: { __A__: JSON.stringify("a"), __NUM__: 7 } }; } },
      { name: "b", config: { order: "post", handler() { return { define: { __B__: "true" }, resolve: { alias: { x: "/x" } } }; } } },
      { name: "silent", config() {} },
    ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root, define: { __USER__: '"u"' } },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "getPluginConfig", args: [] });
    assert.equal(res.id, 1);
    const cfg = JSON.parse(res.result);
    assert.deepEqual(cfg.define, { __A__: '"a"', __NUM__: 7, __B__: "true" }, "every plugin's define, merged");
    assert.equal("__USER__" in cfg.define, false, "the user's own define is not echoed back");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("getPluginConfig reports no define when no config() hook returned one", async () => {
  const fx = tmpProject({ prefix: "oj-cfgdef-" });
  fx.write("oj.plugins.mjs", `export default [{ name: "plain", transform() { return null; } }];\n`);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ config: { root: fx.root } })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "getPluginConfig", args: [] });
    assert.deepEqual(JSON.parse(res.result), { define: null });
  } finally {
    host.close();
    fx.cleanup();
  }
});
