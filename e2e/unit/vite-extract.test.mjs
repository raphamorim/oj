// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { detectSsrRunnerBacked, extractAlias, extractOptimizeDeps, extractProxy, extractResolve, extractSsr, warnUnsupported } from "../../crates/oj_server/src/assets/vite-extract.mjs";

test("optimizeDeps carries needsInterop and force alongside the lists", () => {
  const out = extractOptimizeDeps({
    include: ["a", "pkg/*"],
    exclude: "b",
    needsInterop: ["cjs-ish", 42],
    force: true,
    esbuildOptions: { target: "es2020" },
  });
  assert.deepEqual(out, { include: ["a", "pkg/*"], exclude: ["b"], needsInterop: ["cjs-ish"], force: true });
  assert.equal(extractOptimizeDeps({ force: "yes" }), null, "non-boolean force is ignored");
  assert.equal(extractOptimizeDeps(undefined), null);
});

test("string aliases pass through unchanged", () => {
  const out = extractAlias({ "@app": "/src", "~": "/src/lib" });
  assert.deepEqual(out, { "@app": "/src", "~": "/src/lib" });
});

test("array-form string aliases are read from find/replacement", () => {
  const out = extractAlias([{ find: "@app", replacement: "/src" }]);
  assert.deepEqual(out, { "@app": "/src" });
});

test("duplicate array-form aliases preserve Vite first-match precedence", () => {
  const out = extractAlias([
    { find: "@app", replacement: "/first/src" },
    { find: "@app", replacement: "/second/src" },
  ]);

  assert.equal(out["@app"], "/first/src");
});

test("a monorepo regex alias pair collapses to one directory alias", () => {
  // The standard TanStack/monorepo shape: an exact match to the package entry
  // plus a subpath match. Both must collapse to `@x/pkg -> .../src`.
  const out = extractAlias([
    { find: /^@excalidraw\/excalidraw$/, replacement: "/repo/packages/excalidraw/index.ts" },
    { find: /^@excalidraw\/excalidraw\/(.*)/, replacement: "/repo/packages/excalidraw/$1" },
  ]);
  assert.equal(out["@excalidraw/excalidraw"], "/repo/packages/excalidraw");
});

test("subpath-only regex derives the key and strips the trailing capture", () => {
  const out = extractAlias([{ find: /^@x\/pkg\/(.*)$/, replacement: "/abs/pkg/$1" }]);
  assert.equal(out["@x/pkg"], "/abs/pkg");
});

test("index.ext replacement suffix is stripped to the directory", () => {
  const out = extractAlias([{ find: /^lib$/, replacement: "/abs/lib/index.tsx" }]);
  assert.equal(out["lib"], "/abs/lib");
});

test("a regex too complex to express as a path alias is skipped, not mis-resolved", () => {
  const out = extractAlias([{ find: /^@x\/.*\/deep/, replacement: "/abs/$1" }]);
  assert.ok(!("@x" in out), `must not produce a bogus key: ${JSON.stringify(out)}`);
});

test("non-string replacements are ignored", () => {
  const out = extractAlias([{ find: "@fn", replacement: () => "/x" }]);
  assert.deepEqual(out, {});
});

function captureStderr(fn) {
  const lines = [];
  const write = process.stderr.write;
  process.stderr.write = (chunk) => { lines.push(String(chunk)); return true; };
  try { fn(); } finally { process.stderr.write = write; }
  return lines.join("");
}

test("proxy contexts pass through verbatim, regex (^) contexts included", () => {
  const out = extractProxy({
    "/api": "http://localhost:3000",
    "^/re/.*": { target: "http://localhost:3001", ws: true, changeOrigin: true },
  });
  assert.deepEqual(out, {
    "/api": "http://localhost:3000",
    "^/re/.*": { target: "http://localhost:3001", ws: true, changeOrigin: true },
  });
});

test("function-valued proxy options are dropped with a warning, the entry still proxies", () => {
  let out;
  const err = captureStderr(() => {
    out = extractProxy({
      "/api": {
        target: "http://localhost:3000",
        rewrite: (p) => p.replace(/^\/api/, ""),
        configure: () => {},
        bypass: () => null,
      },
    });
  });
  assert.deepEqual(out, { "/api": { target: "http://localhost:3000" } });
  assert.match(err, /server\.proxy\["\/api"\]\.rewrite is a function/);
  assert.match(err, /server\.proxy\["\/api"\]\.configure is a function and cannot cross the config bridge/);
  assert.match(err, /server\.proxy\["\/api"\]\.bypass is a function and cannot cross the config bridge/);
});


test("Vite's own client aliases (@vite/env, @vite/client) are skipped without a warning", () => {
  let out;
  const stderr = captureStderr(() => {
    out = extractAlias([
      { find: /^\/?@vite\/env/, replacement: "/vite/dist/client/env.mjs" },
      { find: /^\/?@vite\/client/, replacement: "/vite/dist/client/client.mjs" },
      { find: "@", replacement: "/app/src" },
    ]);
  });
  assert.deepEqual(out, { "@": "/app/src" });
  assert.equal(stderr, "", "Vite's built-in aliases are not the user's configuration");
});

test("warnUnsupported reports only options the user's config sets", () => {
  // The shape resolveConfig produces for a config that sets none of these: all
  // defaults Vite injects. Called with the RAW config instead, nothing is reported.
  const resolvedDefaults = {
    esbuild: { jsxDev: true, charset: "utf8", legalComments: "none" },
    optimizeDeps: { esbuildOptions: { preserveSymlinks: false } },
    worker: { format: "iife", plugins: () => [] },
    ssr: { resolve: { conditions: [], externalConditions: [] } },
    server: { cors: { origin: /^https?:\/\/(?:(?:[^:]+\.)?localhost|127\.0\.0\.1|\[::1\])(?::\d+)?$/ } },
    build: { terserOptions: {} },
  };
  assert.notEqual(captureStderr(() => warnUnsupported(resolvedDefaults)), "", "the resolved shape would warn");
  const rawUserConfig = { plugins: [], tanstackStart: { server: { entry: "server" } } };
  assert.equal(captureStderr(() => warnUnsupported(rawUserConfig)), "");
  assert.equal(captureStderr(() => warnUnsupported(null)), "");
  const userSets = captureStderr(() => warnUnsupported({ worker: { format: "es" }, esbuild: { charset: "ascii" }, build: { terserOptions: { compress: true } } }));
  assert.match(userSets, /worker config is not applied/);
  assert.match(userSets, /esbuild options charset are not applied/);
  assert.match(userSets, /build.terserOptions is not applied/);

  // ssr.resolve.conditions/externalConditions ARE applied (the preferred
  // source for the Node SSR consumers): no stale blanket warning for them.
  const applied = captureStderr(() =>
    warnUnsupported({ ssr: { resolve: { conditions: ["workerd"], externalConditions: ["workerd"] } } }),
  );
  assert.equal(applied, "", "applied ssr.resolve subkeys must not warn");
  // The genuinely-inert subkeys still do, named individually.
  const inert = captureStderr(() =>
    warnUnsupported({ ssr: { resolve: { conditions: ["workerd"], mainFields: ["module"], extensions: [".ts"] } } }),
  );
  assert.match(inert, /ssr\.resolve\.mainFields\/extensions is not applied \(conditions\/externalConditions are\)/);
});

test("resolve.externalConditions is extracted, top-level and through the ssr sugar", () => {
  assert.deepEqual(
    extractResolve({ conditions: ["custom"], externalConditions: ["custom-ext"] }),
    { conditions: ["custom"], externalConditions: ["custom-ext"] },
  );

  // Vite's `ssr.resolve` sugar carries it too.
  const viaSsr = extractSsr({ noExternal: ["dep"], resolve: { externalConditions: ["workerd-ext"] } });
  assert.deepEqual(viaSsr.resolve, { externalConditions: ["workerd-ext"] });
  assert.deepEqual(viaSsr.noExternal, ["dep"]);

  // The environments.ssr spelling wins over ssr.resolve where both are set.
  const viaEnv = extractSsr(
    { resolve: { externalConditions: ["old"] } },
    { resolve: { externalConditions: ["env-ext"], conditions: ["workerd"] } },
  );
  assert.deepEqual(viaEnv.resolve, { conditions: ["workerd"], externalConditions: ["env-ext"] });

  // An environments-only config still produces the ssr resolve block.
  const envOnly = extractSsr(undefined, { resolve: { externalConditions: ["only-env"] } });
  assert.deepEqual(envOnly.resolve, { externalConditions: ["only-env"] });
});

// The runner-backed signal is structural, never config-text matching: the RAW
// config declares `environments.ssr.dev.createEnvironment`, or the
// instantiated plugin list carries the Cloudflare dev plugin by its declared
// name — the exact gate plugin-host.mjs's buildEnvironments uses. The RESOLVED
// config's environments are deliberately not consulted: Vite fills every
// environment's dev.createEnvironment with its own default factory, so
// presence there says nothing.
test("detectSsrRunnerBacked: raw createEnvironment or the Cloudflare dev plugin, structurally", () => {
  // A user-declared custom dev runtime factory in the RAW config.
  const rawWithFactory = { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } };
  assert.equal(detectSsrRunnerBacked(rawWithFactory, []), true);

  // The Cloudflare dev plugin, matched on the plugin object's declared name —
  // in a resolved (flat) list and in a raw (nested, factory-returned) list.
  const cf = { name: "vite-plugin-cloudflare:dev", configureServer: () => {} };
  assert.equal(detectSsrRunnerBacked(null, [{ name: "react" }, cf]), true);
  assert.equal(detectSsrRunnerBacked({}, [[{ name: "vite-plugin-cloudflare" }, [cf]]]), true);

  // Neither signal: a plain SSR app is not runner-backed, and a comment or a
  // similarly named string in config text cannot false-positive (only plugin
  // objects are consulted).
  assert.equal(detectSsrRunnerBacked({ ssr: { target: "node" } }, [{ name: "react" }]), false);
  assert.equal(detectSsrRunnerBacked(null, undefined), false);
  assert.equal(detectSsrRunnerBacked({ environments: { ssr: { resolve: { conditions: ["workerd"] } } } }, []), false);
});
