// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test, loadPluginContainer } from "../../crates/oj_server/src/assets/start/vite-plugin-bridge.mjs";

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
  assert.ok(!idAllowed(f, "/src/app.test.ts"));
  assert.ok(!idAllowed(f, "/lib/app.ts"));
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
  for (const n of [
    "vite:react-babel", "vite:react-refresh", "vite:esbuild", "vite:import-glob",
    "tanstack-start-core::server-fn:client", "tanstack:router-generator",
    "tanstack-router:code-splitter:compile-reference-file", "@tanstack/react-start",
  ]) {
    assert.ok(ojReimplemented(n), `expected ${n} to be oj-reimplemented`);
  }
  for (const n of ["fixture-i18n", "ssr-stub-scopes", "transitive-preloads", "customer-mdx", "i18n-dev", ""]) {
    assert.ok(!ojReimplemented(n), `expected ${n} to be treated as an app plugin`);
  }
});

test("envAllows: applyToEnvironment gates per environment (the ssr-stub-scopes regression)", () => {
  assert.ok(envAllows({}, "client"));
  assert.ok(envAllows({}, "ssr"));
  const ssrOnly = { name: "ssr-stub-scopes", applyToEnvironment: (env) => env.name === "ssr" };
  assert.ok(!envAllows(ssrOnly, "client"), "ssr-only plugin must be skipped on client");
  assert.ok(envAllows(ssrOnly, "ssr"), "ssr-only plugin must run on ssr");
  const clientOnly = { applyToEnvironment: (env) => env.name === "client" };
  assert.ok(envAllows(clientOnly, "client"));
  assert.ok(!envAllows(clientOnly, "ssr"));
  assert.ok(envAllows({ applyToEnvironment: () => { throw new Error("x"); } }, "client"));
});

test("envAllows: passes env.config.consumer (@vitejs/plugin-react reads it)", () => {
  // The env handed to applyToEnvironment must carry config.consumer
  // ("client"/"server"); plugin-react reads it directly, so a bare {name}
  // env threw and killed the client bundle. consumer must match the env.
  const consumerOf = (env) => env.config.consumer;
  assert.equal(envAllows({ applyToEnvironment: consumerOf }, "client"), true);
  // a plugin-react-style gate: only the client consumer
  const clientConsumer = { applyToEnvironment: (env) => env.config.consumer === "client" };
  assert.ok(envAllows(clientConsumer, "client"));
  assert.ok(!envAllows(clientConsumer, "ssr"));
  // an async applyToEnvironment cannot be awaited in the sync filter, so a
  // thenable is treated as allowed rather than throwing on `!== false`.
  const asyncGate = { applyToEnvironment: async (env) => env.config.consumer === "client" };
  assert.ok(envAllows(asyncGate, "ssr"));
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

test("plugin config hooks initialize state and merge nested configuration", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-plugin-config-hooks-"));
  try {
    const vite = join(root, "node_modules", "vite");
    mkdirSync(vite, { recursive: true });
    writeFileSync(join(root, "package.json"), '{"name":"synthetic-app"}');
    writeFileSync(join(root, "vite.config.mjs"), "export default {};\n");
    writeFileSync(join(vite, "package.json"), '{"name":"vite","type":"module","main":"./index.mjs"}');
    writeFileSync(join(vite, "index.mjs"), `
      export async function loadConfigFromFile() {
        let observed;
        return {
          config: {
            define: { EXISTING: "preserved" },
            resolve: { alias: ["initial"] },
            plugins: [
              {
                name: "synthetic-unsupported-config",
                config(config) {
                  return config.build.rollupOptions;
                },
              },
              {
                name: "synthetic-config-producer",
                async config(_config, environment) {
                  return {
                    define: { TOKEN: environment.command + ":" + environment.mode },
                    resolve: { alias: ["configured"] },
                    publicDir: "configured-public",
                  };
                },
              },
              {
                name: "synthetic-config-consumer",
                config(config) {
                  observed = [config.define.TOKEN, config.define.EXISTING, config.resolve.alias.join(",")].join("|");
                },
                transform() {
                  return observed ? "export default " + JSON.stringify(observed) + ";" : null;
                },
              },
            ],
          },
        };
      }
    `);

    const container = await loadPluginContainer(root, { command: "serve", mode: "staging" });

    assert.equal(
      await container.transform("", "/app.ts"),
      'export default "serve:staging|preserved|initial,configured";',
    );
    assert.equal(container.publicDir, "configured-public");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
