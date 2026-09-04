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

test("a source edit sends a targeted update to the accept boundary; dead ends still full-reload", async () => {
  const fx = tmpProject({ prefix: "oj-cf-hmr-" });

  // Stub vite whose worker environment carries a small module graph:
  //   entry (self-accepting, the cloudflare plugin's worker entry shape)
  //     <- route.ts   (update must stop at the entry boundary)
  //   dead.ts         (no importers, no boundary: full-reload)
  //   ignored.ts      (a hotUpdate hook filters it out: nothing sent)
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      function node(id, url, file, extra) {
        return { id, url, file, type: "js", importers: new Set(), acceptedHmrDeps: new Set(),
                 acceptedHmrExports: null, isSelfAccepting: false, importedBindings: null,
                 invalidations: [], ...extra };
      }
      export function resolveConfig(inline) {
        const root = inline.root;
        const entry = node("\\0virtual:worker-entry", "virtual:worker-entry", null, { isSelfAccepting: true });
        const route = node(root + "/src/route.ts", "/src/route.ts", root + "/src/route.ts");
        const dead = node(root + "/src/dead.ts", "/src/dead.ts", root + "/src/dead.ts");
        const ignored = node(root + "/src/ignored.ts", "/src/ignored.ts", root + "/src/ignored.ts");
        route.importers.add(entry);
        const byFile = new Map([[route.file, new Set([route])], [dead.file, new Set([dead])], [ignored.file, new Set([ignored])]]);
        const mkEnv = (name, graphed) => {
          const sends = [];
          return {
            name,
            __sends: sends,
            __nodes: { entry, route, dead },
            __hotUpdateFiles: [],
            moduleGraph: {
              onFileChanges: [],
              getModulesByFile(f) { return graphed ? byFile.get(f) : undefined; },
              onFileChange(f) { this.onFileChanges.push(f); },
              invalidateModule(mod, seen, ts, isHmr) { mod.invalidations.push({ ts, isHmr }); },
            },
            hot: { send: (p) => sends.push(p), on() {}, handleInvoke() {} },
            init() {},
          };
        };
        return {
          root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            client: { consumer: "client", dev: { createEnvironment: (name) => mkEnv(name, false) } },
            worker: {
              dev: {
                createEnvironment: (name, rc) => {
                  const env = mkEnv(name, true);
                  env.__config = rc;
                  env.plugins = [{
                    name: "hu",
                    hotUpdate(options) {
                      env.__hotUpdateFiles.push(options.file);
                      if (options.file.endsWith("ignored.ts")) return [];
                    },
                  }];
                  return env;
                },
              },
            },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });

  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "vite-plugin-cloudflare:dev",
       configureServer(server) {
         server.middlewares.use("/__probe", (req, res) => {
           const w = server.environments.worker;
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({
             workerSends: w.__sends,
             clientSends: server.environments.client.__sends,
             onFileChanges: w.moduleGraph.onFileChanges,
             hotUpdateFiles: w.__hotUpdateFiles,
             routeInvalidations: w.__nodes.route.invalidations,
             deadInvalidations: w.__nodes.dead.invalidations,
             preTransform: {
               worker: w.__config.environments.worker.dev.preTransformRequests ?? null,
               client: w.__config.environments.client.dev.preTransformRequests ?? null,
             },
           }));
         });
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
    const portRes = await host.send({ id: 1, hook: "getMiddlewarePort" });
    const port = Number(portRes.result);
    assert.ok(port > 0, "middleware server is up");
    const cf = await host.send({ id: 2, hook: "getCfEnvironments" });
    assert.equal(cf.result, "true", "the host reports built cloudflare environments");

    const invalidate = async (rel) => {
      const res = await fetch(`http://127.0.0.1:${port}/__oj_invalidate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ paths: [path.join(fx.root, rel)] }),
      });
      assert.equal(res.status, 204);
    };
    const probe = async () => (await fetch(`http://127.0.0.1:${port}/__probe`)).json();

    // A change inside the graph with a self-accepting entry above it: one
    // targeted update to the entry boundary, no full-reload.
    await invalidate("src/route.ts");
    let seen = await probe();
    assert.equal(seen.workerSends.length, 1, `one payload, got ${JSON.stringify(seen.workerSends)}`);
    const update = seen.workerSends[0];
    assert.equal(update.type, "update");
    assert.equal(update.updates.length, 1);
    assert.equal(update.updates[0].type, "js-update");
    assert.equal(update.updates[0].path, "/@id/virtual:worker-entry");
    assert.equal(update.updates[0].acceptedPath, "/@id/virtual:worker-entry");
    assert.ok(update.updates[0].timestamp > 0, "update carries the timestamp");
    assert.equal(seen.routeInvalidations.length, 1, "the changed module was invalidated");
    assert.equal(seen.routeInvalidations[0].isHmr, true, "invalidated as an HMR invalidation");
    assert.ok(seen.onFileChanges.includes(path.join(fx.root, "src/route.ts")), "moduleGraph.onFileChange ran");
    assert.ok(seen.hotUpdateFiles.includes(path.join(fx.root, "src/route.ts")), "plugin hotUpdate hooks ran");
    assert.deepEqual(seen.clientSends, [], "the client environment had no matching modules: nothing sent");

    // Propagation dead-ends (no importers, no boundary): full-reload, as Vite.
    await invalidate("src/dead.ts");
    seen = await probe();
    assert.equal(seen.workerSends.length, 2);
    assert.equal(seen.workerSends[1].type, "full-reload");
    assert.equal(seen.deadInvalidations.length, 1, "the dead-end module is still invalidated");

    // A hotUpdate hook returning [] suppresses the update entirely.
    await invalidate("src/ignored.ts");
    seen = await probe();
    assert.equal(seen.workerSends.length, 2, "filtered-out change sends nothing");

    // A file outside the graph: nothing to send for this environment.
    await invalidate("src/nowhere.ts");
    seen = await probe();
    assert.equal(seen.workerSends.length, 2, "unknown file sends nothing");

    // preTransformRequests: defaulted on for the server consumer, left alone
    // for the client.
    assert.equal(seen.preTransform.worker, true, "server env gets dev.preTransformRequests");
    assert.equal(seen.preTransform.client, null, "client env is not touched");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a user-set preTransformRequests is not overridden", async () => {
  const fx = tmpProject({ prefix: "oj-cf-pretransform-" });
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      export function resolveConfig(inline) {
        return {
          root: inline.root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            worker: { dev: { createEnvironment: (name, rc) => ({ name, init() {}, hot: { send() {}, on() {} }, __config: rc }) } },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "vite-plugin-cloudflare:dev",
       configureServer(server) {
         server.middlewares.use("/__probe", (req, res) => {
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({
             worker: server.environments.worker.__config.environments.worker.dev.preTransformRequests ?? null,
           }));
         });
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        // The user's config chose a value: the host must not flip it.
        config: { root: fx.root, environments: { worker: { dev: { preTransformRequests: false } } } },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const portRes = await host.send({ id: 1, hook: "getMiddlewarePort" });
    const port = Number(portRes.result);
    assert.ok(port > 0, "middleware server is up");
    const seen = await (await fetch(`http://127.0.0.1:${port}/__probe`)).json();
    assert.equal(seen.worker, null, "the resolved value stands; the host does not force preTransformRequests");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a non-cloudflare plugin set gets oj-backed client/ssr stand-ins, not Vite-built environments", async () => {
  const fx = tmpProject({ prefix: "oj-cf-noenv-" });
  // No Vite in node_modules and no cloudflare plugin: buildEnvironments must not
  // run (nothing throws), yet server.environments still has client and ssr, as
  // Vite's createServer always provides them.
  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [{
       name: "plain-plugin",
       configureServer(server) {
         seen.envNames = Object.keys(server.environments).sort();
         seen.stubbed = server.environments.client.__ojStub === true && server.environments.ssr.__ojStub === true;
         seen.middlewaresCallable = typeof server.middlewares === "function";
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
    assert.deepEqual(seen.envNames, ["client", "ssr"], "client and ssr are always exposed");
    assert.equal(seen.stubbed, true, "they are oj stand-ins, no Vite environment construction ran");
    assert.equal(seen.middlewaresCallable, true, "callable middlewares still provided to every app");
  } finally {
    host.close();
    fx.cleanup();
  }
});
