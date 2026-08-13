// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Integration test for the TanStack Start adapter. Runs oj against a real,
// self-contained Start app (e2e/fixtures/start-app) in both modes and asserts
// that a server-rendered "/" wires together every seam the adapter supports:
//   - file/code-based routing (/ and /about), server functions
//   - import.meta.glob, ?raw / ?url asset conventions, svgr, MDX
//   - Tailwind v4 CSS compilation, a plugin-owned virtual module
//   - tsconfig paths + package.json "imports" aliases, a CommonJS dep
//   - the Cloudflare context shim (wrangler vars), a non-default publicDir
//
//   node e2e/start.mjs        # dev + prod
//   node e2e/start.mjs --dev  # dev only
//   node e2e/start.mjs --prod # prod only
//
// The fixture's dependencies are not vendored. If they are not installed the
// test SKIPS (exit 0) with an install hint, so a clean checkout stays green:
//   (cd e2e/fixtures/start-app && npm install)
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const onlyDev = process.argv.includes("--dev");
const onlyProd = process.argv.includes("--prod");
const runDev = !onlyProd;
const runProd = !onlyDev;

// Skip cleanly when the fixture has no installed deps (nothing to test against).
const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "esbuild"));
if (!installed) {
  console.log("SKIP start integration: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const rm = (p) => fs.rmSync(p, { recursive: true, force: true });

const get = async (port, route = "/") => {
  const res = await fetch(`http://localhost:${port}${route}`);
  return { status: res.status, body: await res.text() };
};
const waitUp = async (port) => {
  for (let i = 0; i < 120; i++) {
    try { if ((await fetch(`http://localhost:${port}/`)).ok) return; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`server on :${port} did not start`);
};

// The full-stack assertions run identically against dev and prod output.
async function assertApp(port, label) {
  const home = await get(port, "/");
  if (home.status !== 200) throw new Error(`${label}: / returned ${home.status}`);
  const h = home.body;
  const want = [
    ["#lib alias (shout)", "HOME!"],
    ["server function ran", "server-fn-marker"],
    ["cloudflare wrangler var", "fixture-edition"],
    ["tsconfig paths alias", "ALIAS!"],
    ["commonjs dep facade", "[INTEROP]"],
    ["plugin virtual module", "fixture-virtual-ok"],
    ["import.meta.glob", "Alpha Widget, Beta Widget"],
    ["?raw import", "raw-notes-marker"],
    ["svgr component", "<rect"],
    ["mdx module", "mdx-content-marker"],
  ];
  for (const [what, marker] of want) {
    if (!h.includes(marker)) throw new Error(`${label}: missing ${what} ("${marker}")`);
  }
  // ?url resolves to /@oj-start/fs/...hero.png in dev, /assets/hero-<hash>.png
  // in prod; both reference the hero png from the img src.
  if (!/src="[^"]*hero[^"]*\.png"/.test(h)) {
    throw new Error(`${label}: missing ?url import (hero png src)`);
  }

  const about = await get(port, "/about");
  if (about.status !== 200 || !about.body.includes("about-page-marker")) {
    throw new Error(`${label}: /about did not render (status ${about.status})`);
  }

  const asset = await get(port, "/favicon.txt");
  if (!asset.body.includes("public-dir-marker")) {
    throw new Error(`${label}: publicDir asset not served`);
  }
  console.log(`${label}: ${want.length} features + /about + publicDir ok`);
}

async function devPhase() {
  rm(path.join(app, ".oj-cache"));
  const port = 3097;
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  try {
    await waitUp(port);
    await assertApp(port, "start-dev");
  } finally {
    srv.kill("SIGKILL");
  }
}

async function prodPhase() {
  rm(path.join(app, "dist"));
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  const dist = path.join(app, "dist");
  for (const f of ["server.mjs", "server-bundle.mjs", "client"]) {
    if (!fs.existsSync(path.join(dist, f))) throw new Error(`prod: dist/${f} missing`);
  }
  const port = 3098;
  // cwd is the app root so the Cloudflare shim finds wrangler.jsonc.
  const srv = spawn("node", [path.join(dist, "server.mjs")], {
    cwd: app, stdio: "ignore", env: { ...process.env, PORT: String(port) },
  });
  try {
    await waitUp(port);
    await assertApp(port, "start-prod");
  } finally {
    srv.kill("SIGKILL");
  }
}

try {
  if (runDev) await devPhase();
  if (runProd) await prodPhase();
  console.log("\nSTART INTEGRATION PASSED");
} catch (e) {
  console.error("\nSTART INTEGRATION FAILED:", e.message);
  process.exit(1);
}
