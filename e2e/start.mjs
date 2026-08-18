// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
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
    ["commonjs subpath (extensionless)", "[deep:ok]"],
    ["plugin virtual module", "fixture-virtual-ok"],
    ["import.meta.glob", "Alpha Widget, Beta Widget"],
    ["?raw import", "raw-notes-marker"],
    ["svgr bare .svg component", "<rect"],
    ["svgr ?react component", "<polygon"],
    ["mdx module", "mdx-content-marker"],
  ];
  for (const [what, marker] of want) {
    if (!h.includes(marker)) throw new Error(`${label}: missing ${what} ("${marker}")`);
  }
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

  const head = h.slice(0, h.indexOf("</head>"));
  if (!/<link[^>]*rel="stylesheet"[^>]*\.css/.test(head)) {
    throw new Error(`${label}: stylesheet not linked in the SSR head (FOUC)`);
  }

  console.log(`${label}: ${want.length} features + /about + publicDir + css-in-head ok`);
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
  const clientDir = path.join(dist, "client", "assets");
  const clientJs = fs
    .readdirSync(clientDir)
    .filter((f) => f.startsWith("client-") && f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(clientDir, f), "utf8"))
    .join("");
  if (/process\.env\.[A-Za-z_]/.test(clientJs)) {
    throw new Error("prod: client bundle has a bare process.env access (would crash hydration)");
  }
  const port = 3098;
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
