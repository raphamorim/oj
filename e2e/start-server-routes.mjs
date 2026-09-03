// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start request handling, dev and the generated prod server: a
// non-GET on a dotted path reaches the app's server route (Vite hands every
// request nothing else owns to the app), bodies stream through as bytes (no
// size cap, binary intact) and every set-cookie header of the response
// survives the proxy.

import { spawn, execSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const DEV_PORT = Number(process.env.OJ_E2E_PORT || 3094);
const PROD_PORT = DEV_PORT + 1;

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start server routes: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const rm = (p) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(p, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};
const must = (cond, msg) => { if (!cond) throw new Error(msg); };
const waitUp = async (port) => {
  for (let i = 0; i < 120; i++) {
    try { if ((await fetch(`http://localhost:${port}/`)).ok) return; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`server on :${port} did not start`);
};

async function assertServerRoutes(port, label) {
  // 6 MB of random bytes: not valid UTF-8, well past any small-body path.
  const blob = randomBytes(6 * 1024 * 1024);
  const res = await fetch(`http://localhost:${port}/api/export.csv`, {
    method: "POST",
    headers: { "content-type": "application/octet-stream" },
    body: blob,
  });
  must(res.status === 200, `${label}: POST /api/export.csv returned ${res.status} (dotted non-GET must reach the server route)`);
  must(res.headers.get("x-body-bytes") === String(blob.length), `${label}: the server route saw ${res.headers.get("x-body-bytes")} body bytes, want ${blob.length}`);
  const echoed = Buffer.from(await res.arrayBuffer());
  must(echoed.equals(blob), `${label}: echoed body differs from what was sent (binary body mangled)`);
  const cookies = res.headers.getSetCookie();
  must(cookies.length === 2 && cookies[0].startsWith("first=1") && cookies[1].startsWith("second=2"),
    `${label}: expected two set-cookie headers, got ${JSON.stringify(cookies)}`);

  // An unowned dotted PUT is the app's too: its router answers (an HTML 404
  // here), not the dev server's method/route rejection.
  const put = await fetch(`http://localhost:${port}/files/a.b`, { method: "PUT", body: "x" });
  must((put.headers.get("content-type") || "").includes("text/html"),
    `${label}: PUT /files/a.b should reach the app (got ${put.status} ${put.headers.get("content-type")})`);
  console.log(`${label}: dotted POST server route + binary echo + 2 set-cookie + dotted PUT ok`);
}

async function devPhase() {
  rm(path.join(app, ".oj-cache"));
  const srv = spawn(oj, ["dev", app, "--port", String(DEV_PORT)], { stdio: "ignore" });
  try {
    await waitUp(DEV_PORT);
    await assertServerRoutes(DEV_PORT, "start-dev");
  } finally {
    srv.kill("SIGKILL");
  }
}

async function prodPhase() {
  rm(path.join(app, "dist"));
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  const server = fs.readFileSync(path.join(app, "dist", "server.mjs"), "utf8");
  must(server.includes("getSetCookie") && server.includes("Readable.fromWeb"),
    "prod: dist/server.mjs should stream responses and keep every set-cookie");
  const srv = spawn("node", [path.join(app, "dist", "server.mjs")], {
    cwd: app, stdio: "ignore", env: { ...process.env, PORT: String(PROD_PORT) },
  });
  try {
    await waitUp(PROD_PORT);
    await assertServerRoutes(PROD_PORT, "start-prod");
  } finally {
    srv.kill("SIGKILL");
  }
}

try {
  await devPhase();
  await prodPhase();
  console.log("\nSTART SERVER ROUTES PASSED");
} catch (e) {
  console.error("\nSTART SERVER ROUTES FAILED:", e.message);
  process.exit(1);
}
