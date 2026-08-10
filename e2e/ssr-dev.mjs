// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Standalone check for `oj dev --ssr` (SSR + HMR). Starts the SSR dev server
// against ./playground on a dedicated port, then verifies, in order:
//   1. first render is server-rendered ("ssr: 0"),
//   2. a source edit is reflected on the next full load (server rebuild),
//   3. the SSR HTML wires up hydration via the dev pipeline (refresh preamble,
//      HMR client, client entry) and the SSR class name matches the dev
//      pipeline's CSS-module class (no hydration mismatch),
//   4. (browser, if Playwright is installed) an edit hot-updates the running
//      page with React state preserved and no full reload — SSR HMR.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const port = 5235;
const base = `http://localhost:${port}`;
const counter = path.join(repo, "playground", "src", "Counter.tsx");
// Normalize to a known baseline so the test doesn't depend on leftover state.
const baseline = fs
  .readFileSync(counter, "utf8")
  .replace(/useState<number>\(\d+\)/, "useState<number>(0)")
  .replace("{label} = {count}", "{label}: {count}");
fs.writeFileSync(counter, baseline);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
try {
  execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs kill`, { shell: "/bin/bash", stdio: "ignore" });
} catch {} // nothing was listening

// Capture the server's stderr (which includes the runner's) so we can assert
// the server-side HMR push actually fired.
const errLog = path.join(here, ".ssr-stderr.log");
const errFd = fs.openSync(errLog, "w");
const server = spawn(
  path.join(repo, "target", "debug", "oj"),
  ["dev", path.join(repo, "playground"), "--ssr", "src/entry-server.tsx", "--port", String(port)],
  { stdio: ["ignore", "ignore", errFd] },
);

const cleanup = () => {
  fs.writeFileSync(counter, baseline);
  server.kill("SIGKILL");
  fs.rmSync(path.join(repo, "playground", ".oj-cache", "ssr"), { recursive: true, force: true });
  fs.rmSync(errLog, { force: true });
};

const get = async () => (await fetch(`${base}/`)).text();
const stripTags = (html) => html.replace(/<[^>]*>/g, "");
const waitFor = async (pred, tries = 60) => {
  for (let i = 0; i < tries; i++) {
    try {
      const html = await get();
      if (pred(html)) return html;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("timed out waiting for a render");
};

try {
  // 1. First render is server-rendered.
  const first = await waitFor((h) => h.includes("ssr"));
  if (!/ssr[^0-9]*0/.test(stripTags(first))) throw new Error(`first render missing "ssr: 0":\n${first}`);
  console.log("ssr-dev: first render ok (ssr: 0)");

  // 1b. Streaming: the response is chunked (not a buffered Content-Length body),
  //     and the deferred Suspense content resolved and streamed in — proof the
  //     renderToReadableStream path ran, not buffered renderToString.
  const headers = await new Promise((resolve, reject) => {
    http.get(`${base}/`, (res) => (res.resume(), resolve(res.headers))).on("error", reject);
  });
  if (headers["transfer-encoding"] !== "chunked" || headers["content-length"]) {
    throw new Error(`SSR response was not streamed (chunked): ${JSON.stringify(headers)}`);
  }
  if (!first.includes("deferred-streamed")) {
    throw new Error(`streamed Suspense content missing:\n${first}`);
  }
  console.log("ssr-dev: streaming ok (chunked transfer + deferred Suspense content)");

  // 1c. Per-route SSR: distinct paths render distinct trees; modules/assets and
  //     proxied paths do not get server-rendered.
  const about = await (await fetch(`${base}/about`)).text();
  if (!about.includes('data-page="about"') || !about.includes("About")) {
    throw new Error(`/about did not render the about route:\n${about}`);
  }
  if (about.includes("deferred-streamed")) throw new Error("/about leaked home-route content");
  if (!first.includes('data-page="home"')) throw new Error("/ did not render the home route");
  console.log("ssr-dev: per-route ok (/ -> home, /about -> about)");

  // 1d. Route data loading: the server-authoritative loader ran, its data is
  //     serialized into the document, and the route rendered with it.
  if (!first.includes("window.__OJ_DATA__=") || !first.includes('"likes":')) {
    throw new Error(`route data not serialized into the document:\n${first}`);
  }
  if (!first.includes('data-likes="0"')) throw new Error("home not rendered with loader data");
  // A server-authoritative loader fetch (oj-loader header) returns JSON data.
  const loaderRes = await fetch(`${base}/about`, { headers: { "oj-loader": "1" } });
  if (loaderRes.headers.get("content-type")?.includes("application/json") !== true) {
    throw new Error("loader fetch did not return JSON");
  }
  if (typeof (await loaderRes.json()).likes !== "number") throw new Error("loader JSON missing likes");
  console.log("ssr-dev: route data loaded server-side + serialized + loader fetch");

  // 2. The persistent module runner re-evaluates changed modules on the next
  //    load — two consecutive edits both show up, with no Rolldown SSR bundle.
  fs.writeFileSync(counter, baseline.replace("useState<number>(0)", "useState<number>(41)"));
  const edited = await waitFor((h) => /ssr[^0-9]*41/.test(stripTags(h)));
  if (!/ssr[^0-9]*41/.test(stripTags(edited))) throw new Error(`edit not reflected:\n${edited}`);
  fs.writeFileSync(counter, baseline.replace("useState<number>(0)", "useState<number>(8)"));
  await waitFor((h) => /ssr[^0-9]*8[^0-9]/.test(stripTags(h) + " "));
  fs.writeFileSync(counter, baseline);
  const runnerScript = path.join(repo, "playground", ".oj-cache", "ssr", "runner.mjs");
  if (!fs.existsSync(runnerScript)) throw new Error("module runner script was not spawned");
  console.log("ssr-dev: module runner re-eval ok (ssr: 41, then 8)");

  // 2b. Server-side HMR push: the runner subscribes to the dev server's HMR
  //     channel and invalidates the SSR graph on the server's change event
  //     (not lazily at render). The edits above should have pushed to it.
  let pushed = false;
  for (let i = 0; i < 20 && !pushed; i++) {
    pushed = fs.readFileSync(errLog, "utf8").includes("hmr push -> invalidated");
    if (!pushed) await new Promise((r) => setTimeout(r, 250));
  }
  if (!pushed) throw new Error("runner did not receive a server-side HMR push");
  console.log("ssr-dev: server-side HMR push ok (runner invalidated on change event)");

  // 3. Hydration wiring + CSS-module class parity with the dev pipeline.
  const html = await waitFor((h) => /ssr[^0-9]*0/.test(stripTags(h)));
  for (const needle of [
    'src="/@oj/refresh-preamble.js"',
    'src="/@oj/client.js"',
    'src="/src/entry-client.tsx"',
  ]) {
    if (!html.includes(needle)) throw new Error(`SSR HTML missing ${needle}`);
  }
  if (!(await fetch(`${base}/src/entry-client.tsx`)).ok) throw new Error("dev pipeline did not serve the client entry");
  const cls = html.match(/class="([^"]*button[^"]*)"/)?.[1];
  const cssModule = await (await fetch(`${base}/src/Counter.module.css?import`)).text();
  if (!cls || !cssModule.includes(cls)) {
    throw new Error(`SSR class ${cls} absent from dev CSS module (hydration-mismatch risk)`);
  }
  console.log("ssr-dev: hydration wiring + class parity ok");

  // 4. Real SSR HMR: hot edit applies with state preserved and no reload.
  let pw = null;
  try {
    pw = await import("playwright");
  } catch {} // not installed in this environment
  if (pw) {
    const browser = await pw.chromium.launch();
    try {
      // 4a. Pending + error UI states. A slow loader shows a "loading" state; a
      //     failing loader (/boom) and a render throw (/crash) both render the
      //     route error UI without crashing the app. (A caught render error
      //     logs to the console by design, so this page doesn't assert clean.)
      const sp = await browser.newPage();
      await sp.goto(`${base}/`, { waitUntil: "networkidle" });
      await sp.evaluate(() => (window.__marker = 5));
      await sp.locator('a[href="/about"]').click();
      await sp.waitForSelector('[data-pending="loading"]', { timeout: 3000 });
      await sp.waitForSelector('[data-page="about"]');
      if ((await sp.locator("[data-pending]").count()) !== 0) throw new Error("pending state did not clear");
      await sp.goBack();
      await sp.waitForSelector('[data-page="home"]');
      await sp.locator('a[href="/boom"]').click(); // loader throws -> 500 -> error UI
      await sp.waitForSelector('[data-error]', { timeout: 3000 });
      if (!(await sp.locator("[data-error] p").textContent()).includes("request failed")) {
        throw new Error("loader error not shown as route error");
      }
      await sp.locator('[data-error] a[href="/"]').click(); // recover
      await sp.waitForSelector('[data-page="home"]');
      await sp.locator('a[href="/crash"]').click(); // render throws -> ErrorBoundary
      await sp.waitForSelector('[data-error]', { timeout: 3000 });
      if (!(await sp.locator("[data-error] p").textContent()).includes("render threw")) {
        throw new Error("render error not caught by ErrorBoundary");
      }
      if ((await sp.evaluate(() => window.__marker)) !== 5) throw new Error("an error state caused a full reload");
      await sp.close();
      console.log("ssr-dev: pending + error UI states ok (loading, loader error, render error, no reload)");

      // 4b. Action + SPA + HMR, all in place, on a clean page (asserts no
      //     unexpected console errors).
      const page = await browser.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e)));
      page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
      await page.goto(`${base}/`, { waitUntil: "networkidle" });
      await page.waitForSelector("button");
      const counterBtn = page.locator("button", { hasText: "ssr" });
      const likeBtn = page.locator("button", { hasText: "like" });
      // The streamed Suspense content survived hydration (no mismatch).
      const deferred = await page.locator("[data-deferred]").textContent();
      if (!deferred.includes("deferred-streamed")) throw new Error(`deferred content lost: ${deferred}`);
      // One window marker proves nothing below triggers a full reload — not
      // the action, not the SPA navigations, not the hot edit.
      await page.evaluate(() => (window.__marker = 7));

      // Action/mutation: submitting the form runs the server-side action and
      // revalidates the loader, in place (no reload).
      if ((await page.locator("[data-likes]").getAttribute("data-likes")) !== "0") throw new Error("likes did not start at 0");
      await likeBtn.click();
      await page.waitForSelector('[data-pending="submitting"]', { timeout: 3000 }); // pending during the action
      await page.waitForSelector('[data-likes="1"]', { timeout: 5000 });
      console.log("ssr-dev: action ok (pending 'submitting' -> likes 1, no reload)");

      // Client-side SPA routing: a link click navigates without a reload, the
      // navigated route's data comes from the server (shared store shows the
      // mutation), and the back button restores the previous route (popstate).
      await page.locator('a[href="/about"]').click();
      await page.waitForSelector('[data-page="about"]');
      if ((await page.evaluate(() => location.pathname)) !== "/about") throw new Error("pushState did not update the URL");
      await page.waitForSelector('[data-likes="1"]', { timeout: 5000 }); // server-authoritative loader
      await page.goBack();
      await page.waitForSelector('[data-page="home"]');
      if ((await page.evaluate(() => window.__marker)) !== 7) throw new Error("SPA navigation caused a full reload");
      console.log("ssr-dev: SPA routing + server loader ok (link -> /about, back -> /, no reload)");

      for (let i = 0; i < 3; i++) await counterBtn.click();
      const clicked = (await counterBtn.textContent()).trim();
      if (!clicked.includes(": 3")) throw new Error(`hydration did not attach handler: ${clicked}`);

      // Hot edit: change the separator, keep the component and its state.
      fs.writeFileSync(counter, baseline.replace("{label}: {count}", "{label} = {count}"));
      await counterBtn.filter({ hasText: " = " }).waitFor({ timeout: 10000 });
      const afterEdit = (await counterBtn.textContent()).trim();
      const marker = await page.evaluate(() => window.__marker);
      if (!afterEdit.includes("= 3")) throw new Error(`state lost or edit not applied: ${afterEdit}`);
      if (marker !== 7) throw new Error("page fully reloaded (state would be lost)");
      if (errors.length) throw new Error(`console errors: ${errors.join("; ")}`);
      console.log(`ssr-dev: SSR HMR ok (${clicked} -> hot edit -> ${afterEdit}, no reload)`);
    } finally {
      await browser.close();
    }
  } else {
    console.log("ssr-dev: playwright not installed, skipped browser HMR check");
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
