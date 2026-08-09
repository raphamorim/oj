// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Standalone check for `oj dev --ssr`: starts the SSR dev server against
// ./playground, asserts the first render, edits a source component, and
// asserts the next render reflects the edit (rebuild-on-change). Runs its own
// server on a dedicated port, independent of run.mjs's shared dev server.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const port = 5233;
const counter = path.join(repo, "playground", "src", "Counter.tsx");
// Normalize the fixture to a known baseline so the test doesn't depend on
// leftover state from a prior interrupted run, then restore that baseline.
const baseline = fs
  .readFileSync(counter, "utf8")
  .replace(/useState<number>\(\d+\)/, "useState<number>(0)");
fs.writeFileSync(counter, baseline);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
try {
  execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs kill`, { shell: "/bin/bash", stdio: "ignore" });
} catch {} // nothing was listening

const server = spawn(
  path.join(repo, "target", "debug", "oj"),
  ["dev", path.join(repo, "playground"), "--ssr", "src/entry-server.tsx", "--port", String(port)],
  { stdio: "ignore" },
);

const cleanup = () => {
  fs.writeFileSync(counter, baseline);
  server.kill("SIGKILL");
  fs.rmSync(path.join(repo, "playground", ".oj-cache", "ssr"), { recursive: true, force: true });
};

const get = async () => (await fetch(`http://localhost:${port}/`)).text();
const waitFor = async (needle, tries = 60) => {
  for (let i = 0; i < tries; i++) {
    try {
      const html = await get();
      if (html.includes(needle)) return html;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`timed out waiting for "${needle}"`);
};

try {
  const first = await waitFor("ssr");
  if (!/ssr[^0-9]*0/.test(first.replace(/<[^>]*>/g, ""))) {
    throw new Error(`first render missing "ssr: 0":\n${first}`);
  }
  console.log("ssr-dev: first render ok (ssr: 0)");

  fs.writeFileSync(counter, baseline.replace("useState<number>(0)", "useState<number>(41)"));
  const edited = await waitFor("41");
  if (!/ssr[^0-9]*41/.test(edited.replace(/<[^>]*>/g, ""))) {
    throw new Error(`edited render missing "ssr: 41":\n${edited}`);
  }
  console.log("ssr-dev: rebuild-on-edit ok (ssr: 41)");
  console.log("\nSSR-DEV TEST PASSED");
} catch (e) {
  console.error(`\nSSR-DEV TEST FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  // The spawned server keeps the event loop alive, so tear it down and exit
  // explicitly rather than waiting for a natural exit that never comes.
  cleanup();
  process.exit(process.exitCode ?? 0);
}
