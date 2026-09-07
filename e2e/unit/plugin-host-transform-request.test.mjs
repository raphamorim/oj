// SPDX-License-Identifier: MIT
//
// A plugin's transform can depend on `this.environment.transformRequest(id)` to
// load a dependency on demand. TanStack Start's server-fn compiler does exactly
// this in dev: to classify a module it calls
// `this.environment.transformRequest(<dep>?tss-server-fn-lookup)` so a sibling
// "capture" transform hook ingests <dep> into the compiler's module cache
// before it reads it via getModuleInfo. If oj stubs transformRequest to a no-op
// the ingest never happens, and correctness falls to module ordering -- a cold
// concurrent first load that transforms the dependent before the dependency
// throws "could not load module info" -> a hard 500 that breaks hydration.
//
// This mirrors that pattern with minimal plugins: transforming `csrf.js` on a
// cold shared cache must drive `transformRequest("<middleware>?lookup")`, which
// must run the capture hook and populate the cache, so the compiler succeeds.
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

test("this.environment.transformRequest runs the pipeline so an on-demand dependency ingest works (no ordering race)", async () => {
  const fx = tmpProject({ prefix: "oj-tr-" });
  const mid = path.join(fx.root, "middleware.js");
  const csrf = path.join(fx.root, "csrf.js");
  fx.write("middleware.js", "export const createMiddleware = () => ({});\n");
  fx.write("csrf.js", 'import { createMiddleware } from "./middleware.js";\nexport const x = createMiddleware();\n');
  // Two plugins sharing a module-scope cache, like TanStack's compiler + capture
  // hook. The compiler hook needs the dependency in `cache` and, on a miss,
  // drives it in via transformRequest("<dep>?lookup"); the capture hook is the
  // only thing that populates `cache`, and only for the `?lookup` request.
  fx.write(
    "oj.plugins.mjs",
    [
      `const MID = ${JSON.stringify(mid)};`,
      `const CSRF = ${JSON.stringify(csrf)};`,
      "const cache = new Set();",
      "export default [",
      "  { name: 'capture', transform(code, id) {",
      "      if (id.endsWith('?lookup')) { cache.add(id.slice(0, -'?lookup'.length)); return null; }",
      "      return null;",
      "  } },",
      "  { name: 'compiler', async transform(code, id) {",
      "      if (id !== CSRF) return null;",
      "      if (!cache.has(MID)) {",
      "        await this.environment.transformRequest(MID + '?lookup');",
      "        if (!cache.has(MID)) throw new Error('could not load module info for ' + MID);",
      "      }",
      "      return { code: 'export const x = () => {};' };",
      "  } },",
      "];",
      "",
    ].join("\n"),
  );

  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    // Transform csrf.js FIRST, on a cold shared cache -> forces the miss the
    // ordering race exposes. With a working transformRequest this succeeds.
    const res = await host.send({ id: 1, hook: "transform", args: ["import '/x';\n", csrf, "null"] });
    assert.equal(res.error, undefined, `transform must not error: ${res.error}`);
    const out = JSON.parse(res.result);
    assert.equal(out.code, "export const x = () => {};", "compiler ran after the dependency was ingested on demand");
  } finally {
    host.close();
    fx.cleanup();
  }
});
