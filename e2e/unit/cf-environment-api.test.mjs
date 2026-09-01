// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// @cloudflare/vite-plugin (and any Environment-API plugin) subclasses
// vite.DevEnvironment and reads `server.environments[env].initRunner` plus a
// callable `server.middlewares` connect app. oj does not depend on Vite: its
// plugin host loads the *app's* installed Vite, resolves a real config, and
// builds each `server.environments[name]` via that env's
// `dev.createEnvironment(name, config, { ws })`. This pins that wiring with a
// stub `vite` in the fixture (no real Vite/Miniflare needed).

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

test("cloudflare-style plugin gets server.environments (built from the app's Vite) and a callable middlewares app", async () => {
  const fx = tmpProject({ prefix: "oj-cf-envapi-" });

  // Stub the app's installed Vite: resolveConfig returns two environments,
  // each with a dev.createEnvironment factory the host must call.
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      export function resolveConfig(inline) {
        const mkEnv = (extra) => (name) => ({ name, init() {}, hot: { handleInvoke() {}, on() {} }, ...extra(name) });
        return {
          root: inline.root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            client: { dev: { createEnvironment: mkEnv((name) => ({ pluginContainer: { buildStart() {} } })) } },
            worker: { dev: { createEnvironment: mkEnv((name) => ({ initRunner() {} })) } },
          },
        };
      }
      export class DevEnvironment {}
      export function createRunnableDevEnvironment(name) { return { name, init() {} }; }
    `,
  });

  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [{
       name: "vite-plugin-cloudflare:dev",
       configureServer(server) {
         seen.middlewaresCallable = typeof server.middlewares === "function";
         seen.middlewaresHasUse = typeof server.middlewares.use === "function";
         seen.hasClose = typeof server.close === "function";
         seen.hasTransformIndexHtml = typeof server.transformIndexHtml === "function";
         seen.hasEnvironments = !!server.environments && typeof server.environments === "object";
         seen.envNames = server.environments ? Object.keys(server.environments) : [];
         seen.workerHasInitRunner =
           !!(server.environments && server.environments.worker &&
              typeof server.environments.worker.initRunner === "function");
         // a registered middleware runs when the app is invoked as a function
         let ran = false;
         server.middlewares.use((req, res, next) => { ran = true; next(); });
         server.middlewares({ url: "/x" }, { setHeader() {}, end() {} }, () => {});
         seen.middlewareRan = ran;
       },
       transform(code, id) {
         if (id.endsWith("probe.js")) return "export default " + JSON.stringify(seen) + ";";
         return null;
       },
     }];\n`,
  );

  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });

  try {
    const res = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "probe.js")] });
    const seen = JSON.parse(JSON.parse(res.result).code.replace(/^export default /, "").replace(/;$/, ""));

    // The Environment API: the host called the app's Vite and built one
    // DevEnvironment per resolved environment via its dev.createEnvironment.
    assert.equal(seen.hasEnvironments, true, "configureServer sees server.environments");
    assert.deepEqual(seen.envNames.sort(), ["client", "worker"], "one environment per resolved config env");
    assert.equal(seen.workerHasInitRunner, true, "the worker env is the plugin's own instance (has initRunner)");

    // server.middlewares is a callable connect app AND has .use().
    assert.equal(seen.middlewaresCallable, true, "server.middlewares is callable");
    assert.equal(seen.middlewaresHasUse, true, "server.middlewares.use exists");
    assert.equal(seen.middlewareRan, true, "a registered middleware runs when the app is invoked");

    // Surface the cloudflare plugin also reads.
    assert.equal(seen.hasClose, true, "server.close is provided");
    assert.equal(seen.hasTransformIndexHtml, true, "server.transformIndexHtml is provided");
  } finally {
    host.close();
    fx.cleanup();
  }
});
