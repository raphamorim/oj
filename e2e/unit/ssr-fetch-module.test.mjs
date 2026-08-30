// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { createInvokeHandler } from "../../crates/oj_server/src/assets/start/ssr-fetch-module.mjs";

// deps contract:
//   resolveId(url, importer) -> { id, external?, type?, file? } | null
//   load(id) -> string | null
//   transform(code, id) -> string     (the ssr_transform_module output)
//   isBuiltin(spec) -> bool           (optional)
//   builtins() -> Array<...>          (optional)
function handler(overrides = {}) {
  return createInvokeHandler({
    resolveId: async (url) => ({ id: "/abs" + url.replace(/^\./, ""), external: false }),
    load: async () => "export const x = 1",
    transform: async (code) => `/*t*/${code}`,
    builtins: () => [{ type: "string", value: "node:fs" }],
    ...overrides,
  });
}

const send = (name, ...data) => ({ type: "custom", event: "vite:invoke", data: { id: "send", name, data } });

test("data: URL externalizes as builtin", async () => {
  const r = await handler()(send("fetchModule", "data:text/js,1", undefined, {}));
  assert.deepEqual(r.result, { externalize: "data:text/js,1", type: "builtin" });
});

test("node: builtin externalizes as builtin", async () => {
  const r = await handler()(send("fetchModule", "node:path", "/imp.js", {}));
  assert.deepEqual(r.result, { externalize: "node:path", type: "builtin" });
});

test("isBuiltin-reported specifier externalizes as builtin", async () => {
  const r = await handler({ isBuiltin: (s) => s === "virtual:special" })(
    send("fetchModule", "virtual:special", "/imp.js", {}),
  );
  assert.deepEqual(r.result, { externalize: "virtual:special", type: "builtin" });
});

test("http URL externalizes as network", async () => {
  const r = await handler()(send("fetchModule", "https://cdn/x.js", "/imp.js", {}));
  assert.deepEqual(r.result, { externalize: "https://cdn/x.js", type: "network" });
});

test("resolver-marked external returns externalize with its type", async () => {
  const r = await handler({
    resolveId: async () => ({ id: "file:///node_modules/dep/index.cjs", external: true, type: "commonjs" }),
  })(send("fetchModule", "dep", "/imp.js", {}));
  assert.deepEqual(r.result, { externalize: "file:///node_modules/dep/index.cjs", type: "commonjs" });
});

test("internal module is loaded, transformed, and returned inlined", async () => {
  const r = await handler()(send("fetchModule", "./mod.js", "/imp.js", {}));
  assert.equal(r.result.code, "/*t*/export const x = 1");
  assert.equal(r.result.id, "/abs/mod.js");
  assert.equal(r.result.url, "./mod.js");
  assert.equal(r.result.file, "/abs/mod.js");
  assert.equal(r.result.invalidate, true);
});

test("cached option sets invalidate=false", async () => {
  const r = await handler()(send("fetchModule", "./mod.js", "/imp.js", { cached: true }));
  assert.equal(r.result.invalidate, false);
});

test("getBuiltins passes through", async () => {
  const r = await handler()(send("getBuiltins"));
  assert.deepEqual(r.result, [{ type: "string", value: "node:fs" }]);
});

test("unresolvable module returns an error, does not throw", async () => {
  const r = await handler({ resolveId: async () => null })(send("fetchModule", "./nope.js", "/imp.js", {}));
  assert.ok(r.error, "should surface error");
  assert.match(r.error.message, /Cannot resolve/);
});

test("unknown method returns an error", async () => {
  const r = await handler()(send("bogus"));
  assert.ok(r.error);
  assert.match(r.error.message, /unknown invoke method/);
});

test("malformed payload returns TransportError", async () => {
  const r = await handler()({ type: "custom", event: "vite:invoke", data: {} });
  assert.equal(r.error.name, "TransportError");
});
