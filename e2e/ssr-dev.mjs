// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Standalone check for `oj dev --ssr`: starts the SSR dev server against
// ./playground, asserts the first render, edits a source component and asserts
// the next render reflects the edit (rebuild-on-change), then verifies client
// hydration — the injected client bundle makes the server-rendered markup
// interactive (a browser click increments the counter with no mismatch).
// Runs its own server on a dedicated port, independent of run.mjs's shared
// dev server. The browser step is skipped if Playwright isn't installed.
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

  // Hydration wiring: the SSR HTML injects the client bundle + stylesheet, and
  // the scoped CSS-module class in the markup exists in the served CSS (so the
  // class React renders on the client matches — no hydration mismatch).
  const html = await get();
  const base = `http://localhost:${port}`;
  if (!html.includes('src="/@oj/entry-client.js"')) throw new Error("no hydration script injected");
  if (!html.includes('href="/@oj/entry-client.css"')) throw new Error("no client stylesheet injected");
  if (!(await fetch(`${base}/@oj/entry-client.js`)).ok) throw new Error("client bundle not served");
  const clientCss = await (await fetch(`${base}/@oj/entry-client.css`)).text();
  const cls = html.match(/class="([^"]*button[^"]*)"/)?.[1];
  if (!cls || !clientCss.includes(cls)) {
    throw new Error("SSR class name absent from client CSS (hydration-mismatch risk)");
  }
  console.log("ssr-dev: hydration wiring ok (script + css + class parity)");

  // Interactivity: hydration must attach the click handler. Value-agnostic so
  // it doesn't matter what the current baseline render is.
  let pw = null;
  try {
    pw = await import("playwright");
  } catch {} // not installed in this environment
  if (pw) {
    const browser = await pw.chromium.launch();
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    try {
      await page.goto(`${base}/`, { waitUntil: "networkidle" });
      const before = (await page.locator("button").textContent()).trim();
      const n = parseInt(before.replace(/\D/g, ""), 10) || 0;
      await page.locator("button").click();
      await page.waitForFunction(
        (want) => document.querySelector("button").textContent.includes(`: ${want}`),
        n + 1,
        { timeout: 5000 },
      );
      const after = (await page.locator("button").textContent()).trim();
      if (errors.length) throw new Error(`console errors during hydration: ${errors.join("; ")}`);
      console.log(`ssr-dev: hydration interactive ok (${before} -> ${after})`);
    } finally {
      await browser.close();
    }
  } else {
    console.log("ssr-dev: playwright not installed, skipped browser hydration check");
  }

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
