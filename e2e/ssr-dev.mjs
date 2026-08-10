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
  // File-based routing: routes come from src/routes/**; a `$id` file segment is
  // a dynamic param passed to the loader and component.
  const user = await (await fetch(`${base}/users/42`)).text();
  if (!user.includes('data-page="user"') || !user.includes('data-user-id="42"')) {
    throw new Error(`dynamic file-based route /users/42 did not render:\n${user}`);
  }
  console.log("ssr-dev: per-route ok (/, /about, dynamic /users/42 from src/routes/**)");

  // 1e. Nested layouts + per-layout loaders: the root layout wraps every route
  //     and has its own loader data; the users layout wraps only /users/* and
  //     loads its own (section) data; the page loads its own — all composed.
  if (!first.includes('data-layout="root"')) throw new Error("root layout did not wrap /");
  if (about.includes('data-layout="users"')) throw new Error("users layout leaked onto /about");
  if (!user.includes('data-layout="root"') || !user.includes('data-layout="users"')) {
    throw new Error("nested layouts did not compose on /users/42");
  }
  if (!user.includes('data-app-name="oj"') || !user.includes('data-users-count="3"') || !user.includes('data-user-id="42"')) {
    throw new Error(`per-layout loaders did not each provide their data:\n${user}`);
  }
  console.log("ssr-dev: nested layouts + per-layout loaders ok (root, users, page each loaded)");

  // 1f. Per-route head/meta: each route's title/meta is rendered into <head>
  //     (page title overrides the root layout's; params flow into the title).
  if (!first.includes("<title>Home - oj</title>")) throw new Error("/ did not render its route title");
  if (!first.includes('name="generator"')) throw new Error("root-layout meta missing");
  if (!about.includes("<title>About - oj</title>") || !about.includes('name="description"')) {
    throw new Error("/about did not render its title + meta");
  }
  if (!user.includes("<title>User 42 - oj</title>")) throw new Error("/users/42 did not render its param title");
  // Open Graph: site-wide tags from the root layout, per-route og:title merged.
  if (!first.includes('property="og:site_name" content="oj"')) throw new Error("layout og:site_name missing on /");
  if (!about.includes('property="og:title" content="About oj"')) throw new Error("/about og:title missing");
  if (!user.includes('property="og:title" content="User 42"') || !user.includes('property="og:site_name"')) {
    throw new Error("/users/42 did not merge page og:title with layout og:site_name");
  }
  console.log("ssr-dev: per-route head/meta + Open Graph ok (title/name/property, page overrides layout)");

  // 1d. Route data loading: the loaders ran server-side, the whole chain's data
  //     map is serialized, and each level rendered with its own slice.
  if (!first.includes("window.__OJ_DATA__=") || !first.includes('"likes":')) {
    throw new Error(`route data not serialized into the document:\n${first}`);
  }
  if (!first.includes('data-likes="0"')) throw new Error("home not rendered with loader data");
  // A server-authoritative loader fetch (oj-loader header) returns the chain's
  // data map keyed by route/layout id.
  const loaderRes = await fetch(`${base}/about`, { headers: { "oj-loader": "1" } });
  if (loaderRes.headers.get("content-type")?.includes("application/json") !== true) {
    throw new Error("loader fetch did not return JSON");
  }
  const loaderMap = await loaderRes.json();
  if (typeof loaderMap["about"]?.likes !== "number" || loaderMap["layout"]?.app !== "oj") {
    throw new Error(`loader map missing page or root-layout data: ${JSON.stringify(loaderMap)}`);
  }
  console.log("ssr-dev: route data loaded server-side + serialized + chain loader map");

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

  // 3b. Per-environment plugin pipelines (Vite Environment API). The SSR module
  //     compile runs the "ssr" plugin host; the dev pipeline runs the "client"
  //     one. applyToEnvironment gates each plugin to exactly one — a clean
  //     mirror on the same source file.
  {
    const appAbs = path.join(repo, "playground", "src", "App.tsx");
    const ssrMod = await (await fetch(`${base}/@ssr-module?id=${encodeURIComponent(appAbs)}`)).text();
    if (!ssrMod.includes("ssr-ran")) throw new Error("ssr env: applyToEnvironment('ssr') plugin did not run server-side");
    if (!ssrMod.includes("__ENV_CLIENT__")) throw new Error("ssr env: client-gated plugin must NOT run server-side");
    const clientMod = await (await fetch(`${base}/src/App.tsx`)).text();
    if (!clientMod.includes("client-ran")) throw new Error("client env: client plugin did not run");
    if (!clientMod.includes("__ENV_SSR__")) throw new Error("client env: ssr-gated plugin must NOT run in client");
    console.log("ssr-dev: per-environment plugin pipelines ok (ssr host transforms server modules)");
  }

  // 3c. SSR resolveId + load: a plugin virtual module resolves and loads on the
  //     server side (the runner links it via these same endpoints).
  {
    const entryAbs = path.join(repo, "playground", "src", "entry-server.tsx");
    const resolved = await (await fetch(
      `${base}/@ssr-resolve?importer=${encodeURIComponent(entryAbs)}&spec=virtual:plugin-greeting`,
    )).json();
    if (resolved.external || !resolved.id || !resolved.id.includes("virtual:plugin-greeting")) {
      throw new Error("ssr resolveId did not resolve the plugin virtual module: " + JSON.stringify(resolved));
    }
    const mod = await (await fetch(`${base}/@ssr-module?id=${encodeURIComponent(resolved.id)}`)).text();
    if (!mod.includes("hello from plugin")) {
      throw new Error("ssr load did not return the plugin virtual module source: " + mod);
    }
    console.log("ssr-dev: plugin resolveId + load ok (virtual module resolves/loads server-side)");
  }

  // Playwright is optional (skipped where it isn't installed); shared by the
  // browser phases below.
  let pw = null;
  try {
    pw = await import("playwright");
  } catch {}

  // 3d. Server functions: a `*.server.ts` module is a client stub that RPCs to
  //     /__oj_fn, where the real function runs on the SSR module runner.
  if (pw) {
    const browser = await pw.chromium.launch();
    try {
      const sp = await browser.newPage();
      // The client receives a stub, not the server implementation.
      const stub = await (await fetch(`${base}/src/greeting.server.ts`)).text();
      if (!stub.includes("__ojServerCall") || stub.includes("process.versions")) {
        throw new Error("server module was not replaced by a client stub:\n" + stub);
      }
      await sp.goto(`${base}/`, { waitUntil: "domcontentloaded" });
      const result = await sp.evaluate(async () => {
        const m = await import("/src/greeting.server.ts");
        return m.greet("oj");
      });
      if (result !== "hello, oj (server=true, call=1)") {
        throw new Error("server function RPC returned unexpected result: " + JSON.stringify(result));
      }
      console.log("ssr-dev: server functions ok (client stub -> /__oj_fn -> server exec)");
    } finally {
      await browser.close();
    }
  }

  // 4. Real SSR HMR: hot edit applies with state preserved and no reload.
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

      // 4a2. Nested-layout persistence: navigating within a section keeps the
      //      section layout mounted (its local state survives); leaving it
      //      unmounts the section layout but keeps the root layout.
      const lp = await browser.newPage();
      await lp.goto(`${base}/users/42`, { waitUntil: "networkidle" });
      if ((await lp.title()) !== "User 42 - oj") throw new Error("SSR document title wrong for /users/42");
      await lp.locator("[data-layout-inc]").click();
      await lp.locator("[data-layout-inc]").click();
      if ((await lp.locator("[data-layout-count]").textContent()) !== "2") throw new Error("layout state not set");
      await lp.locator('a[href="/users/43"]').click(); // navigate within /users/*
      await lp.waitForFunction(() => document.querySelector("[data-user-id]")?.textContent === "43", { timeout: 5000 });
      if ((await lp.locator("[data-layout-count]").textContent()) !== "2") {
        throw new Error("section layout remounted (state lost) on intra-section navigation");
      }
      // The head/title AND Open Graph tags update on client navigation.
      await lp.waitForFunction(() => document.title === "User 43 - oj", { timeout: 3000 });
      await lp.waitForFunction(
        () => document.querySelector('meta[property="og:title"]')?.content === "User 43",
        { timeout: 3000 },
      );
      await lp.locator('a[href="/"]').click(); // leave the section
      await lp.waitForSelector('[data-page="home"]');
      await lp.waitForFunction(() => document.title === "Home - oj", { timeout: 3000 });
      if ((await lp.locator('[data-layout="users"]').count()) !== 0) throw new Error("section layout did not unmount");
      if ((await lp.locator('[data-layout="root"]').count()) !== 1) throw new Error("root layout did not persist");
      await lp.close();
      console.log("ssr-dev: nested-layout persistence + per-nav title ok (state kept, title 42->43->home)");

      // 4a3. Prefetch: links in the viewport warm on load; a below-the-fold
      //      link warms only on hover.
      const pf = await browser.newPage();
      const reqs = [];
      pf.on("request", (r) => reqs.push(r.url()));
      await pf.goto(`${base}/`, { waitUntil: "networkidle" });
      const hasChunk = (name) => reqs.some((u) => u.includes(`routes/${name}.tsx`));
      // Viewport: the visible /crash link is prefetched without any interaction.
      await pf.waitForFunction(
        () => performance.getEntriesByType("resource").some((e) => e.name.includes("routes/crash.tsx")),
        { timeout: 3000 },
      );
      if (!hasChunk("crash")) throw new Error("visible link was not viewport-prefetched");
      // Opt-out: the /about link (data-no-prefetch) is NOT prefetched.
      if (hasChunk("about")) throw new Error("data-no-prefetch link was prefetched");
      // Below the fold: /deep isn't prefetched on load, only on hover.
      if (hasChunk("deep")) throw new Error("below-fold link was prefetched before entering the viewport");
      await pf.locator("[data-deep-link]").hover();
      await pf.waitForFunction(
        () => performance.getEntriesByType("resource").some((e) => e.name.includes("routes/deep.tsx")),
        { timeout: 3000 },
      );
      if (!hasChunk("deep")) throw new Error("hovering the below-fold link did not prefetch it");
      await pf.close();
      console.log("ssr-dev: prefetch ok (viewport warms visible, hover warms below-fold, opt-out respected)");

      // 4a4. Connection-aware gating + change events: under Data Saver,
      //      speculative prefetch is suppressed (an explicit click still
      //      navigates); when the connection improves and fires `change`,
      //      links in view are warmed.
      const cg = await browser.newPage();
      await cg.addInitScript(() => {
        const listeners = new Set();
        const conn = {
          saveData: true,
          effectiveType: "4g",
          addEventListener: (t, cb) => t === "change" && listeners.add(cb),
          removeEventListener: (t, cb) => listeners.delete(cb),
        };
        window.__conn = conn;
        window.__fireConn = () => listeners.forEach((cb) => cb());
        Object.defineProperty(navigator, "connection", { configurable: true, get: () => conn });
      });
      const cgReqs = [];
      cg.on("request", (r) => cgReqs.push(r.url()));
      await cg.goto(`${base}/`, { waitUntil: "networkidle" });
      const crashChunk = () => cgReqs.some((u) => u.includes("routes/crash.tsx"));
      await cg.locator('a[href="/crash"]').hover();
      await cg.waitForTimeout(400);
      if (crashChunk()) throw new Error("prefetch was not gated under Data Saver");
      // Connection improves -> warm in-view links.
      await cg.evaluate(() => {
        window.__conn.saveData = false;
        window.__fireConn();
      });
      await cg.waitForFunction(
        () => performance.getEntriesByType("resource").some((e) => e.name.includes("routes/crash.tsx")),
        { timeout: 3000 },
      );
      if (!crashChunk()) throw new Error("prefetch did not resume after the connection improved");
      await cg.locator('a[href="/about"]').click(); // explicit nav always works
      await cg.waitForSelector('[data-page="about"]');
      await cg.close();
      console.log("ssr-dev: connection gating + change ok (Data Saver suppresses, improve resumes, click navigates)");

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
