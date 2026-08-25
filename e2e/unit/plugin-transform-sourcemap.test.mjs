// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The plugin host must forward each plugin transform's sourcemap to Rust (in a
// `maps` array), so oj can compose it with its own oxc map. Before, only
// `code`/`watchFiles` were returned and the map was dropped.
test("plugin-host forwards plugin transform sourcemaps to Rust", async () => {
  const fx = tmpProject({ prefix: "oj-pmap-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
      name: "adds-map",
      transform(code, id) {
        return { code: code + "\\n/* transformed */", map: { version: 3, sources: [id], names: [], mappings: "AAAA" } };
      },
    }];\n`,
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
      args: ["let a = 1;", path.join(fx.root, "a.js")],
    });
    assert.equal(res.id, 1, "reply keyed by request id");
    const out = JSON.parse(res.result);
    assert.match(out.code, /transformed/, "the transform ran");
    assert.equal(out.maps.length, 1, "the plugin's map is captured");
    assert.match(out.maps[0], /"mappings":"AAAA"/, "the plugin's map is forwarded verbatim");
  } finally {
    host.close();
    fx.cleanup();
  }
});
