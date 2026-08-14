// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Warm-cache regression for plugin transform-time side effects.
//
// oj's content-addressed cache serves a module without re-running the plugin
// `transform` hook on a cache hit. Side effects that hook performs therefore
// have to be replayed from cache metadata on a warm-cache serve, exactly like
// graph edges and /@fs allowances already are -- otherwise they silently
// vanish after the first `oj dev` run. This restarts the dev server against a
// WARM playground/.oj-cache with a fresh plugin host and asserts:
//   - moduleParsed still records App.tsx (replayed for graph-observing plugins)
//   - this.addWatchFile still forces a full reload on the watched .txt file
// Both broke on the second run onward before the replay/re-apply fix.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const playground = path.join(repo, "playground");
const WATCHED = path.join(playground, "plugin-watched.txt");
const PORT = 5197;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function killPort() {
  try {
    execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill`, { shell: "/bin/bash", stdio: "ignore" });
  } catch {} // nothing listening
}
function start() {
  killPort();
  return spawn(oj, ["dev", playground, "--port", String(PORT)], { stdio: "ignore" });
}
async function up() {
  for (let i = 0; i < 80; i++) {
    try {
      if ((await fetch(`http://localhost:${PORT}/`)).ok) return;
    } catch {}
    await sleep(250);
  }
  throw new Error("dev server did not start");
}
const parsedModules = async () =>
  (await (await fetch(`http://localhost:${PORT}/__oj_parsed`)).text()).trim().split(",");
function fail(msg) {
  console.error("FAIL:", msg);
  process.exit(1);
}

// 1) COLD: clear the cache, boot, load "/", confirm the transform ran and
// moduleParsed recorded App.tsx (baseline the warm run must match).
fs.rmSync(path.join(playground, ".oj-cache"), { recursive: true, force: true });
let srv = start();
try {
  await up();
  await fetch(`http://localhost:${PORT}/`);
  await sleep(800);
  if (!(await parsedModules()).includes("App.tsx")) fail("cold: moduleParsed missing App.tsx");
} finally {
  srv.kill("SIGKILL");
}
await sleep(500);

// 2) WARM: reboot against the populated cache with a fresh (empty) plugin host.
srv = start();
try {
  await up();
  await fetch(`http://localhost:${PORT}/`);
  await sleep(800);

  // moduleParsed replay: App.tsx is a cache hit (transform did not re-run), yet
  // the graph-observing plugin must still see it.
  if (!(await parsedModules()).includes("App.tsx")) {
    fail("warm: moduleParsed did not replay App.tsx (transform side effect lost on cache hit)");
  }
  console.log("warm-cache: moduleParsed replayed for cache-hit App.tsx");

  // addWatchFile re-apply: editing the plugin-watched .txt must force a full
  // reload even though App.tsx's transform (which registered the watch) was a
  // cache hit. Observe the reload over the HMR WebSocket.
  const original = fs.readFileSync(WATCHED, "utf8");
  const ws = new WebSocket(`ws://localhost:${PORT}/__ws`);
  const gotReload = new Promise((resolve) => {
    ws.addEventListener("message", (e) => {
      try {
        if (JSON.parse(e.data).type === "full-reload") resolve(true);
      } catch {}
    });
  });
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve);
    ws.addEventListener("error", reject);
  });
  fs.writeFileSync(WATCHED, "warm-v2\n");
  const reloaded = await Promise.race([gotReload, sleep(8000).then(() => false)]);
  fs.writeFileSync(WATCHED, original);
  ws.close();
  if (!reloaded) {
    fail("warm: addWatchFile watch lost on cache hit (no full-reload on plugin-watched.txt edit)");
  }
  console.log("warm-cache: addWatchFile re-applied -> full reload on watched-file edit");
} finally {
  srv.kill("SIGKILL");
  killPort();
}

console.log("\nWARM-CACHE REGRESSION PASSED");
