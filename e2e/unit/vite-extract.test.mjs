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

// The runner-backed signal is Vite's declaration mechanism, never a vendor
// name check: the RAW config, or a plugin's `config` hook return merged
// Vite-style (runConfigHook), declares `environments.<name>.dev.createEnvironment`.
// The RESOLVED config's environments are deliberately not consulted: Vite
// fills every environment's dev.createEnvironment with its own default
// factory, so presence there says nothing.
test("detectSsrRunnerBacked: raw or config-hook environment declaration", async () => {
  // A user-declared custom dev runtime factory in the RAW config.
  const rawWithFactory = { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } };
  assert.equal(await detectSsrRunnerBacked(rawWithFactory), true);

  // A plugin declaring its environment from its `config` hook, in a raw
  // (nested, factory-returned) plugin list. The name does not matter — any
  // declaring plugin counts (the Cloudflare plugin is one such).
  const declaring = {
    name: "acme-runner",
    config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [[{ name: "react" }, [declaring]]] }), true);

  // The Cloudflare plugin's own shape (vite-plugin-cloudflare:config returns
  // environments from its config hook) must be seen — it is on no skip list.
  const cfConfig = {
    name: "vite-plugin-cloudflare:config",
    config: () => ({ environments: { ssr: { dev: { createEnvironment: () => ({}) } } } }),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [{ name: "vite-plugin-cloudflare" }, cfConfig] }), true);

  // Object-form hook with `order: "pre"` runs first (getSortedPluginsByHook),
  // and later hooks see earlier merges (runConfigHook merges successively):
  // `late` declares only if `early` already contributed its marker.
  const early = { name: "early", config: { order: "pre", handler: () => ({ custom: { flag: true } }) } };
  const late = {
    name: "late",
    config: {
      handler: (conf) =>
        conf.custom?.flag
          ? { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } }
          : null,
    },
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [late, early] }), true);

  // A skipped plugin's hook is never run, so its declaration is invisible:
  // the skip exists for side effects (the router plugin's config hook starts
  // its generator), and not running the hook is the only way to avoid them —
  // a return cannot be captured without executing it.
  let tanstackRan = false;
  const skipped = {
    name: "tanstack-router",
    config: () => {
      tanstackRan = true;
      return { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } };
    },
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [skipped] }), false);
  assert.equal(tanstackRan, false);
  const checker = {
    name: "vite-plugin-checker",
    config: () => ({ environments: { ssr: { dev: { createEnvironment: () => ({}) } } } }),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [checker] }), false);

  // A throwing config hook must not fail extraction nor mask other plugins.
  const throwing = { name: "boom", config: () => { throw new Error("boom"); } };
  assert.equal(await detectSsrRunnerBacked({ plugins: [throwing, declaring] }), true);
  assert.equal(await detectSsrRunnerBacked({ plugins: [throwing] }), false);

  // `apply` gates the hook exactly as Vite's filterPlugin does.
  assert.equal(
    await detectSsrRunnerBacked({ plugins: [{ ...declaring, apply: "build" }] }, { command: "serve" }),
    false,
  );
  assert.equal(
    await detectSsrRunnerBacked({ plugins: [{ ...declaring, apply: "build" }] }, { command: "build" }),
    true,
  );

  // No declaration: a plain SSR app is not runner-backed, and a factory-less
  // environments block (a resolved-config shape) cannot false-positive.
  assert.equal(await detectSsrRunnerBacked({ ssr: { target: "node" }, plugins: [{ name: "react" }] }), false);
  assert.equal(await detectSsrRunnerBacked(null), false);
  assert.equal(await detectSsrRunnerBacked({ environments: { ssr: { resolve: { conditions: ["workerd"] } } } }), false);
});
