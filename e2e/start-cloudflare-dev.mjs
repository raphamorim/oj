// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A TanStack Start app on Cloudflare under `oj dev`: with @cloudflare/vite-plugin
// in the config the plugin's worker (Miniflare/workerd) renders the documents.
// This drives the edit loop end to end: boot, render a route in the worker,
// verify the live-reload client is injected into the worker-served document,
// edit a route file and require the next document to be fresh (the targeted
// HMR invalidation of the worker environments). Deps come from the same
// install recipe as start-cloudflare.mjs (or OJ_E2E_CF_DEPS).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6840;

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start cloudflare dev: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-cf-dev-"));
const app = path.join(tmp, "app");
const keep = !!process.env.OJ_E2E_KEEP;
// Retried: the SIGKILLed server's node children flush caches for a beat and
// race the removal (ENOTEMPTY), like start.mjs handles.
const cleanup = () => {
  if (keep) return;
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(tmp, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};

function installCloudflareDeps() {
  const prepared = process.env.OJ_E2E_CF_DEPS;
  if (prepared) {
    fs.symlinkSync(path.join(path.resolve(prepared), "node_modules"), path.join(tmp, "node_modules"), "dir");
    return;
  }
  fs.writeFileSync(path.join(tmp, "package.json"), JSON.stringify({ name: "oj-cf-deps", private: true, type: "module" }));
  execSync("npm install --no-audit --no-fund --no-package-lock @cloudflare/vite-plugin wrangler", { cwd: tmp, stdio: "inherit" });
}

function makeApp() {
  fs.mkdirSync(app);
  for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  fs.writeFileSync(path.join(app, "wrangler.jsonc"), [
    "{",
    '  "name": "oj-start-fixture",',
    '  "main": "@tanstack/react-start/server-entry",',
    '  "compatibility_date": "2025-09-01",',
    '  "compatibility_flags": ["nodejs_compat"],',
    '  "vars": { "EDITION": "fixture-edition" }',
    "}",
    "",
  ].join("\n"));
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const withImport = original.replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { cloudflare } from "@cloudflare/vite-plugin";',
  );
  const config = withImport.replace(/^(\s*)tanstackStart\(/m, '$1cloudflare({ viteEnvironment: { name: "ssr" } }),\n$1tanstackStart(');
  if (config === original || config === withImport) throw new Error("fixture vite.config.ts changed shape; update this script");
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);
  // The dev worker path runs the app through the app's OWN Vite pipeline into
  // workerd, where the full fixture index route does not run even under
  // `vite dev` + the plugin (its CommonJS dep fails in the ESM module runner,
  // and the tsconfig-paths alias has no plugin support). Slim the index route
  // to what this test drives: a worker render with a server function reading a
  // wrangler var. The full-fat route stays covered by start.mjs (Node runner)
  // and start-cloudflare.mjs (prod worker build).
  fs.writeFileSync(path.join(app, "src", "routes", "index.tsx"), [
    'import { createRoute, useLoaderData } from "@tanstack/react-router";',
    'import { rootRoute } from "./__root";',
    'import { getGreeting } from "../server/data";',
    'import { shout } from "#lib/format";',
    "",
    "export const indexRoute = createRoute({",
    "  getParentRoute: () => rootRoute,",
    '  path: "/",',
    "  loader: async () => await getGreeting(),",
    "  component: Index,",
    "});",
    "",
    "function Index() {",
    "  const data = useLoaderData({ from: indexRoute.id });",
    "  return (",
    "    <main>",
    '      <h1 className="fixture-heading">{shout("home")}</h1>',
    '      <p data-testid="server-fn">{data.message} / edition={data.edition}</p>',
    "    </main>",
    "  );",
    "}",
    "",
  ].join("\n"));
  // The fixture reads the wrangler var through `@cloudflare/vite-plugin/server`,
  // an alias oj itself provides; in the plugin's own worker pipeline the real
  // API is the `cloudflare:workers` env import.
  fs.writeFileSync(path.join(app, "src", "server", "data.ts"), [
    'import { createServerFn } from "@tanstack/react-start";',
    'import { env } from "cloudflare:workers";',
    "",
    'export const getGreeting = createServerFn({ method: "GET" }).handler(async () => {',
    "  const edition = (env as unknown as Record<string, unknown>).EDITION ?? \"unknown\";",
    '  return { message: "server-fn-marker", edition: String(edition) };',
    "});",
    "",
  ].join("\n"));
}

const get = async (route) => {
  const res = await fetch(`http://127.0.0.1:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

async function runDev() {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1", FORCE_COLOR: "0" },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const stop = () => {
    try { process.kill(-srv.pid, "SIGTERM"); } catch {}
    setTimeout(() => { try { process.kill(-srv.pid, "SIGKILL"); } catch {} }, 2000).unref();
  };
  try {
    let up = false;
    for (let i = 0; i < 240 && !up; i++) {
      if (srv.exitCode != null) break;
      try { up = (await fetch(`http://127.0.0.1:${PORT}/`)).status === 200; } catch {}
      if (!up) await new Promise((r) => setTimeout(r, 500));
    }
    if (!up) throw new Error(`oj dev did not serve on :${PORT}; log:\n${log.slice(-4000)}`);

    // The document must have rendered in the worker (the wrangler var proves
    // it) and must carry the live-reload client even though the plugin
    // middleware served it.
    const home = await get("/");
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    for (const [what, marker] of [
      ["home render", "HOME!"],
      ["worker render (wrangler var)", "fixture-edition"],
      ["server function", "server-fn-marker"],
    ]) {
      if (!home.body.includes(marker)) throw new Error(`/: missing ${what} ("${marker}")\n${home.body.slice(0, 1500)}`);
    }
    if (!home.body.includes("/@oj-start/live-reload.js")) {
      throw new Error("worker-served document lacks the live-reload client");
    }

    const about = await get("/about");
    if (about.status !== 200 || !about.body.includes("about-page-marker")) {
      throw new Error(`/about did not render (${about.status})`);
    }

    // The edit loop: change the route, require the next document to be fresh.
    const target = path.join(app, "src", "routes", "about.tsx");
    fs.writeFileSync(
      target,
      fs.readFileSync(target, "utf8").replace("about-page-marker", "about-page-edited-marker"),
    );
    const t0 = Date.now();
    let fresh = null;
    for (let i = 0; i < 200; i++) {
      const res = await get("/about");
      if (res.status === 200 && res.body.includes("about-page-edited-marker")) {
        fresh = Date.now() - t0;
        break;
      }
      await new Promise((r) => setTimeout(r, 100));
    }
    if (fresh == null) {
      throw new Error(`/about still stale 20s after the edit; log tail:\n${log.slice(-4000)}`);
    }

    // Rapid edits: a second edit landing while the first edit's client
    // rebundle is in flight must not queue behind it. Its worker invalidate
    // fires promptly, so the next document is fresh well under the ~4.6s the
    // old serialized loop took.
    const indexTarget = path.join(app, "src", "routes", "index.tsx");
    fs.writeFileSync(
      indexTarget,
      fs.readFileSync(indexTarget, "utf8").replace('shout("home")', 'shout("home-rapid")'),
    );
    await new Promise((r) => setTimeout(r, 300));
    fs.writeFileSync(
      target,
      fs.readFileSync(target, "utf8").replace("about-page-edited-marker", "about-page-rapid-marker"),
    );
    const t1 = Date.now();
    let rapid = null;
    // A generous deadline: the machine is saturated by the first edit's
    // rebundle here, so a loaded box can take a while even though the prompt
    // invalidate usually lands well under a second. The measured elapsed ms is
    // printed below, so the fast path stays visible in the logs.
    for (let i = 0; i < 300; i++) {
      const res = await get("/about");
      if (res.status === 200 && res.body.includes("about-page-rapid-marker")) {
        rapid = Date.now() - t1;
        break;
      }
      await new Promise((r) => setTimeout(r, 50));
    }
    if (rapid == null) {
      throw new Error(`/about still stale 15s after the rapid second edit; log tail:\n${log.slice(-4000)}`);
    }

    // The coalesced rebundle must not lose the first edit either.
    let firstEditFresh = false;
    for (let i = 0; i < 100 && !firstEditFresh; i++) {
      const res = await get("/");
      firstEditFresh = res.status === 200 && res.body.includes("HOME-RAPID!");
      if (!firstEditFresh) await new Promise((r) => setTimeout(r, 100));
    }
    if (!firstEditFresh) {
      throw new Error(`/ never picked up the first rapid edit; log tail:\n${log.slice(-4000)}`);
    }

    // A new route file: the run regenerates routeTree.gen.ts, and the worker
    // environments must be invalidated for the regenerated output AFTER the
    // regen (the watcher deliberately never forwards routeTree.gen events).
    // This fixture's router is code-based (src/routeTree.ts), so the generated
    // tree is never imported by the app and the new route cannot serve;
    // instead assert through the logs that oj sent the post-regen invalidate
    // and the plugin host received it (the graph not knowing routeTree.gen.ts
    // makes the host log the unmatched change, which doubles as a receipt).
    fs.writeFileSync(path.join(app, "src", "routes", "extra.tsx"), [
      'import { createFileRoute } from "@tanstack/react-router";',
      "",
      'export const Route = createFileRoute("/extra")({',
      "  component: () => <main>extra-page-marker</main>,",
      "});",
      "",
    ].join("\n"));
    let regenSent = false;
    let regenReceived = false;
    for (let i = 0; i < 300 && !(regenSent && regenReceived); i++) {
      regenSent = /regen outputs changed \([^)]*routeTree\.gen\.ts/.test(log);
      regenReceived = /routeTree\.gen\.ts matched no module/.test(log);
      await new Promise((r) => setTimeout(r, 100));
    }
    if (!regenSent) {
      throw new Error(`no post-regen worker invalidate for routeTree.gen.ts within 30s of a new route file; log tail:\n${log.slice(-4000)}`);
    }
    if (!regenReceived) {
      throw new Error(`the plugin host never received the routeTree.gen.ts invalidate; log tail:\n${log.slice(-4000)}`);
    }
    // The generator run must not have broken the app.
    const after = await get("/");
    if (after.status !== 200 || !after.body.includes("HOME-RAPID!")) {
      throw new Error(`/ broken after the new-route regen (${after.status}); log tail:\n${log.slice(-4000)}`);
    }

    console.log(
      `start-cloudflare-dev: worker render + live-reload client + fresh document ${fresh}ms after the edit, ${rapid}ms after a rapid second edit, post-regen routeTree invalidate observed`,
    );
  } finally {
    stop();
  }
}

// Slow-boot regression: a plugin host whose init outlives the boot-time RPC
// deadline (74-plugin fleets, a Miniflare boot inside configureServer) used to
// leave the worker path silently inactive — no "plugin middleware: forwarding"
// line, every document degraded to the Node SSR runner. The host now pushes its
// serve info on stdout when ready and oj activates the middleware late. This
// simulates the slow boot with a configureServer that sleeps past the (shrunk
// via OJ_PLUGIN_TIMEOUT) RPC deadline, and requires the late activation line
// plus a worker-rendered document.
function addSlowBootPlugin() {
  const cfg = path.join(app, "vite.config.ts");
  const original = fs.readFileSync(cfg, "utf8");
  const patched = original.replace(
    /^(\s*)cloudflare\(/m,
    '$1{ name: "slow-boot-probe", async configureServer() { await new Promise((r) => setTimeout(r, 8000)); } },\n$1cloudflare(',
  );
  if (patched === original) throw new Error("vite.config.ts changed shape; update this script");
  fs.writeFileSync(cfg, patched);
}

// Its own port: the first phase's server may still be tearing down on PORT and
// oj would silently bind PORT+1, leaving the probes pointed at a dead socket.
const SLOW_PORT = PORT + 3;

async function runSlowBootDev() {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(SLOW_PORT)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      WRANGLER_SEND_METRICS: "false",
      CI: "1",
      NO_COLOR: "1",
      FORCE_COLOR: "0",
      // Shrink the plugin RPC deadline so the 8s configureServer sleep reliably
      // outlives it, the way a heavy real boot outlives the default 20s.
      OJ_PLUGIN_TIMEOUT: "2",
    },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const stop = () => {
    try { process.kill(-srv.pid, "SIGTERM"); } catch {}
    setTimeout(() => { try { process.kill(-srv.pid, "SIGKILL"); } catch {} }, 2000).unref();
  };
  try {
    // The middleware must activate late: the boot-time serve-info RPC timed
    // out, and the host's ojServeInfo push flips the path once init completes.
    const lateLine = /forwarding unmatched requests to :\d+ \(host came up after boot\)/;
    let activated = false;
    for (let i = 0; i < 240 && !activated; i++) {
      if (srv.exitCode != null) break;
      activated = lateLine.test(log);
      if (!activated) await new Promise((r) => setTimeout(r, 500));
    }
    if (!activated) {
      throw new Error(`the worker path never activated after the slow boot; log tail:\n${log.slice(-4000)}`);
    }

    // And the documents must then render in the worker (wrangler var proves it),
    // with the live-reload client injected like any worker-served document.
    let home = null;
    for (let i = 0; i < 120; i++) {
      try {
        const res = await fetch(`http://127.0.0.1:${SLOW_PORT}/`);
        const body = await res.text();
        if (res.status === 200 && body.includes("fixture-edition")) {
          home = { status: res.status, body };
          break;
        }
      } catch {}
      await new Promise((r) => setTimeout(r, 500));
    }
    if (!home) {
      throw new Error(`no worker-rendered document after late activation; log tail:\n${log.slice(-4000)}`);
    }
    if (!home.body.includes("/@oj-start/live-reload.js")) {
      throw new Error("late-activated worker document lacks the live-reload client");
    }
    console.log("start-cloudflare-dev: slow boot — worker path activated late and served the document");
  } finally {
    stop();
    // Let the SIGTERM land before the next phase (or cleanup) touches the tree.
    await new Promise((r) => setTimeout(r, 500));
  }
}

try {
  installCloudflareDeps();
  makeApp();
  await runDev();
  addSlowBootPlugin();
  await runSlowBootDev();
  cleanup();
} catch (e) {
  console.error(e?.stack || e);
  if (keep) console.error(`kept ${tmp}`);
  else cleanup();
  process.exit(1);
}
