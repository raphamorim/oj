// SPDX-License-Identifier: MIT
// Unit tests for the plugin-container gating helpers (vite-plugin-bridge.mjs):
// the id filter, `apply` matching, enforce ordering, and hook shape parsing.
// Getting these wrong lets a build-only, id-filtered stub swallow every id.
import { test } from "node:test";
import assert from "node:assert/strict";
import { __test } from "../../crates/oj_server/src/assets/start/vite-plugin-bridge.mjs";

const { matchOne, idAllowed, applyMatches, ordered, hookHandler, hookFilter, ojReimplemented, envAllows } = __test;

test("matchOne: RegExp tests, string is a substring match", () => {
  assert.ok(matchOne(/\.mdx$/, "/a/b.mdx"));
  assert.ok(!matchOne(/\.mdx$/, "/a/b.tsx"));
  assert.ok(matchOne("virtual:", "virtual:foo"));
  assert.ok(!matchOne("virtual:", "./real"));
});

test("idAllowed: no filter allows everything", () => {
  assert.ok(idAllowed(undefined, "anything"));
  assert.ok(idAllowed(null, "anything"));
});

test("idAllowed: filter.id include (single + array)", () => {
  assert.ok(idAllowed({ id: /\.svg$/ }, "/icons/x.svg"));
  assert.ok(!idAllowed({ id: /\.svg$/ }, "/icons/x.png"));
  assert.ok(idAllowed({ id: [/\.svg$/, /\.png$/] }, "/icons/x.png"));
});

test("idAllowed: include/exclude object, exclude wins", () => {
  const f = { id: { include: /\/src\//, exclude: /\.test\./ } };
  assert.ok(idAllowed(f, "/src/app.ts"));
  assert.ok(!idAllowed(f, "/src/app.test.ts")); // excluded
  assert.ok(!idAllowed(f, "/lib/app.ts")); // not included
});

test("idAllowed: a bare filter (not wrapped in .id) still applies", () => {
  assert.ok(idAllowed(/\.css$/, "/a.css"));
  assert.ok(!idAllowed(/\.css$/, "/a.js"));
});

test("applyMatches: no apply -> always; string vs command; function form", () => {
  assert.ok(applyMatches({}, "serve"));
  assert.ok(applyMatches({ apply: "serve" }, "serve"));
  assert.ok(!applyMatches({ apply: "build" }, "serve"));
  assert.ok(applyMatches({ apply: (_c, { command }) => command === "serve" }, "serve"));
  assert.ok(!applyMatches({ apply: (_c, { command }) => command === "build" }, "serve"));
  // a throwing apply fn is treated as "applies" (safe default)
  assert.ok(applyMatches({ apply: () => { throw new Error("x"); } }, "serve"));
});

test("ordered: pre first, normal next, post last (stable within a band)", () => {
  const names = ordered([
    { name: "n1" },
    { name: "post1", enforce: "post" },
    { name: "pre1", enforce: "pre" },
    { name: "n2" },
    { name: "pre2", enforce: "pre" },
  ]).map((p) => p.name);
  assert.deepEqual(names, ["pre1", "pre2", "n1", "n2", "post1"]);
});

test("ojReimplemented: skips React / Vite built-ins / TanStack; keeps app plugins", () => {
  // Framework/built-in plugins oj reimplements -- their transforms must NOT
  // re-run on app source (double JSX, or TanStack's server-fn transform
  // fighting oj's own server-fn resolver -> a /_serverFn/ URL parse crash).
  for (const n of [
    "vite:react-babel", "vite:react-refresh", "vite:esbuild", "vite:import-glob",
    "tanstack-start-core::server-fn:client", "tanstack:router-generator",
    "tanstack-router:code-splitter:compile-reference-file", "@tanstack/react-start",
  ]) {
    assert.ok(ojReimplemented(n), `expected ${n} to be oj-reimplemented`);
  }
  // The app's own plugins -- their transforms DO need to run.
  for (const n of ["fixture-i18n", "ssr-stub-scopes", "transitive-preloads", "customer-mdx", "i18n-dev", ""]) {
    assert.ok(!ojReimplemented(n), `expected ${n} to be treated as an app plugin`);
  }
});

test("envAllows: applyToEnvironment gates per environment (the ssr-stub-scopes regression)", () => {
  // No gate -> runs everywhere.
  assert.ok(envAllows({}, "client"));
  assert.ok(envAllows({}, "ssr"));
  // The exact ssr-stub-scopes shape: SSR-only. Must be SKIPPED on the client
  // (running it there stubbed every module and blew up the client bundle with
  // hundreds of "no matching export" errors).
  const ssrOnly = { name: "ssr-stub-scopes", applyToEnvironment: (env) => env.name === "ssr" };
  assert.ok(!envAllows(ssrOnly, "client"), "ssr-only plugin must be skipped on client");
  assert.ok(envAllows(ssrOnly, "ssr"), "ssr-only plugin must run on ssr");
  // A client-only gate is the mirror image.
  const clientOnly = { applyToEnvironment: (env) => env.name === "client" };
  assert.ok(envAllows(clientOnly, "client"));
  assert.ok(!envAllows(clientOnly, "ssr"));
  // A throwing gate defaults to "applies" (safe -- degrade like no gate).
  assert.ok(envAllows({ applyToEnvironment: () => { throw new Error("x"); } }, "client"));
});

test("hookHandler / hookFilter: function form and object form", () => {
  const fn = () => 1;
  assert.equal(hookHandler(fn), fn);
  assert.equal(hookFilter(fn), undefined);

  const obj = { handler: fn, filter: { id: /\.mdx$/ }, order: "pre" };
  assert.equal(hookHandler(obj), fn);
  assert.deepEqual(hookFilter(obj), { id: /\.mdx$/ });

  assert.equal(hookHandler(undefined), null);
  assert.equal(hookHandler({ handler: "not-a-fn" }), null);
});
