// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The plugin host drops the JS plugins a native plugin declares it replaces
// (`nativePluginNames` in its startup config), the way it has always dropped
// the React plugins oj reimplements; the built-in four keep working with or
// without the extra names.

import assert from "node:assert/strict";
import path from "node:path";
import { test } from "node:test";
import { rpcSidecar, tmpProject } from "./harness.mjs";

function spawnHost(fx, initial = {}) {
  return rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ config: { root: fx.root }, ...initial })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
}

const PLUGINS = `export default [
  { name: "vite:react-babel", transform(code) { return code + "/*babel*/"; } },
  { name: "vite:example-marker", transform(code) { return code + "/*marker*/"; } },
  { name: "keep", transform(code) { return code + "/*keep*/"; } },
];\n`;

test("without nativePluginNames only the built-in React names are dropped", async () => {
  const fx = tmpProject({ prefix: "oj-native-names-" });
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = spawnHost(fx);
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "2", "vite:react-babel is dropped, the other two run");
    const res = await host.send({ id: 2, hook: "transform", args: ["x", path.join(fx.root, "a.js")] });
    const out = JSON.parse(res.result).code;
    assert.ok(out.includes("/*marker*/") && out.includes("/*keep*/") && !out.includes("/*babel*/"), out);
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("nativePluginNames from oj are unioned into the skip list", async () => {
  const fx = tmpProject({ prefix: "oj-native-names-" });
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = spawnHost(fx, { nativePluginNames: ["vite:example-marker"] });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", "the replaced plugin and the React plugin are both dropped");
    const res = await host.send({ id: 2, hook: "transform", args: ["x", path.join(fx.root, "a.js")] });
    const out = JSON.parse(res.result).code;
    assert.ok(out.includes("/*keep*/") && !out.includes("/*marker*/") && !out.includes("/*babel*/"), out);
  } finally {
    host.close();
    fx.cleanup();
  }
});
