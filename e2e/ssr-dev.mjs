// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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
const baseline = fs
  .readFileSync(counter, "utf8")
  .replace(/useState<number>\(\d+\)/, "useState<number>(0)")
  .replace("{label} = {count}", "{label}: {count}");
fs.writeFileSync(counter, baseline);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
try {
  execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs kill`, { shell: "/bin/bash", stdio: "ignore" });
} catch {}

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
  const first = await waitFor((h) => h.includes("ssr"));
  if (!/ssr[^0-9]*0/.test(stripTags(first))) throw new Error(`first render missing "ssr: 0":\n${first}`);
  console.log("ssr-dev: first render ok (ssr: 0)");

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

  const about = await (await fetch(`${base}/about`)).text();
  if (!about.includes('data-page="about"') || !about.includes("About")) {
    throw new Error(`/about did not render the about route:\n${about}`);
  }
  if (about.includes("deferred-streamed")) throw new Error("/about leaked home-route content");
  if (!first.includes('data-page="home"')) throw new Error("/ did not render the home route");
  const user = await (await fetch(`${base}/users/42`)).text();
  if (!user.includes('data-page="user"') || !user.includes('data-user-id="42"')) {
    throw new Error(`dynamic file-based route /users/42 did not render:\n${user}`);
  }
  console.log("ssr-dev: per-route ok (/, /about, dynamic /users/42 from src/routes/**)");

  if (!first.includes('data-layout="root"')) throw new Error("root layout did not wrap /");
  if (about.includes('data-layout="users"')) throw new Error("users layout leaked onto /about");
  if (!user.includes('data-layout="root"') || !user.includes('data-layout="users"')) {
    throw new Error("nested layouts did not compose on /users/42");
  }
  if (!user.includes('data-app-name="oj"') || !user.includes('data-users-count="3"') || !user.includes('data-user-id="42"')) {
    throw new Error(`per-layout loaders did not each provide their data:\n${user}`);
  }
  console.log("ssr-dev: nested layouts + per-layout loaders ok (root, users, page each loaded)");

  if (!first.includes("<title>Home - oj</title>")) throw new Error("/ did not render its route title");
  if (!first.includes('name="generator"')) throw new Error("root-layout meta missing");
  if (!about.includes("<title>About - oj</title>") || !about.includes('name="description"')) {
    throw new Error("/about did not render its title + meta");
  }
  if (!user.includes("<title>User 42 - oj</title>")) throw new Error("/users/42 did not render its param title");
  if (!first.includes('property="og:site_name" content="oj"')) throw new Error("layout og:site_name missing on /");
  if (!about.includes('property="og:title" content="About oj"')) throw new Error("/about og:title missing");
  if (!user.includes('property="og:title" content="User 42"') || !user.includes('property="og:site_name"')) {
    throw new Error("/users/42 did not merge page og:title with layout og:site_name");
  }
  console.log("ssr-dev: per-route head/meta + Open Graph ok (title/name/property, page overrides layout)");

  if (!first.includes("window.__OJ_DATA__=") || !first.includes('"likes":')) {
    throw new Error(`route data not serialized into the document:\n${first}`);
  }
  if (!first.includes('data-likes="0"')) throw new Error("home not rendered with loader data");
  const loaderRes = await fetch(`${base}/about`, { headers: { "oj-loader": "1" } });
  if (loaderRes.headers.get("content-type")?.includes("application/json") !== true) {
    throw new Error("loader fetch did not return JSON");
  }
  const loaderMap = await loaderRes.json();
  if (typeof loaderMap["about"]?.likes !== "number" || loaderMap["layout"]?.app !== "oj") {
    throw new Error(`loader map missing page or root-layout data: ${JSON.stringify(loaderMap)}`);
  }
  console.log("ssr-dev: route data loaded server-side + serialized + chain loader map");

  fs.writeFileSync(counter, baseline.replace("useState<number>(0)", "useState<number>(41)"));
  const edited = await waitFor((h) => /ssr[^0-9]*41/.test(stripTags(h)));
  if (!/ssr[^0-9]*41/.test(stripTags(edited))) throw new Error(`edit not reflected:\n${edited}`);
  fs.writeFileSync(counter, baseline.replace("useState<number>(0)", "useState<number>(8)"));
  await waitFor((h) => /ssr[^0-9]*8[^0-9]/.test(stripTags(h) + " "));
  fs.writeFileSync(counter, baseline);
  const runnerScript = path.join(repo, "playground", ".oj-cache", "ssr", "runner.mjs");
  if (!fs.existsSync(runnerScript)) throw new Error("module runner script was not spawned");
  console.log("ssr-dev: module runner re-eval ok (ssr: 41, then 8)");

  let pushed = false;
  for (let i = 0; i < 20 && !pushed; i++) {
    pushed = fs.readFileSync(errLog, "utf8").includes("hmr push -> invalidated");
    if (!pushed) await new Promise((r) => setTimeout(r, 250));
  }
  if (!pushed) throw new Error("runner did not receive a server-side HMR push");
  console.log("ssr-dev: server-side HMR push ok (runner invalidated on change event)");

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

  let pw = null;
  try {
    pw = await import("playwright");
  } catch {}

  if (pw) {
    const browser = await pw.chromium.launch();
    try {
      const sp = await browser.newPage();
      const stub = await (await fetch(`${base}/src/greeting.server.ts`)).text();
      if (!stub.includes("__ojServerCall") || stub.includes("process.versions")) {
        throw new Error("server module was not replaced by a client stub:\n" + stub);
      }
      await sp.goto(`${base}/`, { waitUntil: "domcontentloaded" });
      const result = await sp.evaluate(async () => {
        const m = await import("/src/greeting.server.ts");
        return m.greet("oj");
      });
      if (result !== "hello, oj (server=true)") {
        throw new Error("server function RPC returned unexpected result: " + JSON.stringify(result));
      }
      console.log("ssr-dev: server functions ok (client stub -> /__oj_fn -> server exec)");
    } finally {
      await browser.close();
    }
  }

  if (pw) {
    const browser = await pw.chromium.launch();
    try {
      const sp = await browser.newPage();
      await sp.goto(`${base}/`, { waitUntil: "networkidle" });
      await sp.evaluate(() => (window.__marker = 5));
      await sp.locator('a[href="/about"]').click();
      await sp.waitForSelector('[data-pending="loading"]', { timeout: 3000 });
      await sp.waitForSelector('[data-page="about"]');
      if ((await sp.locator("[data-pending]").count()) !== 0) throw new Error("pending state did not clear");
      await sp.goBack();
      await sp.waitForSelector('[data-page="home"]');
      await sp.locator('a[href="/boom"]').click();
      await sp.waitForSelector('[data-error]', { timeout: 3000 });
      if (!(await sp.locator("[data-error] p").textContent()).includes("request failed")) {
        throw new Error("loader error not shown as route error");
      }
      await sp.locator('[data-error] a[href="/"]').click();
      await sp.waitForSelector('[data-page="home"]');
      await sp.locator('a[href="/crash"]').click();
      await sp.waitForSelector('[data-error]', { timeout: 3000 });
      if (!(await sp.locator("[data-error] p").textContent()).includes("render threw")) {
        throw new Error("render error not caught by ErrorBoundary");
      }
      if ((await sp.evaluate(() => window.__marker)) !== 5) throw new Error("an error state caused a full reload");
      await sp.close();
      console.log("ssr-dev: pending + error UI states ok (loading, loader error, render error, no reload)");

      const lp = await browser.newPage();
      await lp.goto(`${base}/users/42`, { waitUntil: "networkidle" });
      if ((await lp.title()) !== "User 42 - oj") throw new Error("SSR document title wrong for /users/42");
      await lp.locator("[data-layout-inc]").click();
      await lp.locator("[data-layout-inc]").click();
      if ((await lp.locator("[data-layout-count]").textContent()) !== "2") throw new Error("layout state not set");
      await lp.locator('a[href="/users/43"]').click();
      await lp.waitForFunction(() => document.querySelector("[data-user-id]")?.textContent === "43", { timeout: 5000 });
      if ((await lp.locator("[data-layout-count]").textContent()) !== "2") {
        throw new Error("section layout remounted (state lost) on intra-section navigation");
      }
      await lp.waitForFunction(() => document.title === "User 43 - oj", { timeout: 3000 });
      await lp.waitForFunction(
        () => document.querySelector('meta[property="og:title"]')?.content === "User 43",
        { timeout: 3000 },
      );
      await lp.locator('a[href="/"]').click();
      await lp.waitForSelector('[data-page="home"]');
      await lp.waitForFunction(() => document.title === "Home - oj", { timeout: 3000 });
      if ((await lp.locator('[data-layout="users"]').count()) !== 0) throw new Error("section layout did not unmount");
      if ((await lp.locator('[data-layout="root"]').count()) !== 1) throw new Error("root layout did not persist");
      await lp.close();
      console.log("ssr-dev: nested-layout persistence + per-nav title ok (state kept, title 42->43->home)");

      const pf = await browser.newPage();
      const reqs = [];
      pf.on("request", (r) => reqs.push(r.url()));
      await pf.goto(`${base}/`, { waitUntil: "networkidle" });
      const hasChunk = (name) => reqs.some((u) => u.includes(`routes/${name}.tsx`));
      await pf.waitForFunction(
        () => performance.getEntriesByType("resource").some((e) => e.name.includes("routes/crash.tsx")),
        { timeout: 3000 },
      );
      if (!hasChunk("crash")) throw new Error("visible link was not viewport-prefetched");
      if (hasChunk("about")) throw new Error("data-no-prefetch link was prefetched");
      if (hasChunk("deep")) throw new Error("below-fold link was prefetched before entering the viewport");
      await pf.locator("[data-deep-link]").hover();
      await pf.waitForFunction(
        () => performance.getEntriesByType("resource").some((e) => e.name.includes("routes/deep.tsx")),
        { timeout: 3000 },
      );
      if (!hasChunk("deep")) throw new Error("hovering the below-fold link did not prefetch it");
      await pf.close();
      console.log("ssr-dev: prefetch ok (viewport warms visible, hover warms below-fold, opt-out respected)");

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
      await cg.evaluate(() => {
        window.__conn.saveData = false;
        window.__fireConn();
      });
      await cg.waitForFunction(
        () => performance.getEntriesByType("resource").some((e) => e.name.includes("routes/crash.tsx")),
        { timeout: 3000 },
      );
      if (!crashChunk()) throw new Error("prefetch did not resume after the connection improved");
      await cg.locator('a[href="/about"]').click();
      await cg.waitForSelector('[data-page="about"]');
      await cg.close();
      console.log("ssr-dev: connection gating + change ok (Data Saver suppresses, improve resumes, click navigates)");

      const page = await browser.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e)));
      page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
      await page.goto(`${base}/`, { waitUntil: "networkidle" });
      await page.waitForSelector("button");
      const counterBtn = page.locator("button", { hasText: "ssr" });
      const likeBtn = page.locator("button", { hasText: "like" });
      const deferred = await page.locator("[data-deferred]").textContent();
      if (!deferred.includes("deferred-streamed")) throw new Error(`deferred content lost: ${deferred}`);
      await page.evaluate(() => (window.__marker = 7));

      if ((await page.locator("[data-likes]").getAttribute("data-likes")) !== "0") throw new Error("likes did not start at 0");
      await likeBtn.click();
      await page.waitForSelector('[data-pending="submitting"]', { timeout: 3000 });
      await page.waitForSelector('[data-likes="1"]', { timeout: 5000 });
      console.log("ssr-dev: action ok (pending 'submitting' -> likes 1, no reload)");

      await page.locator('a[href="/about"]').click();
      await page.waitForSelector('[data-page="about"]');
      if ((await page.evaluate(() => location.pathname)) !== "/about") throw new Error("pushState did not update the URL");
      await page.waitForSelector('[data-likes="1"]', { timeout: 5000 });
      await page.goBack();
      await page.waitForSelector('[data-page="home"]');
      if ((await page.evaluate(() => window.__marker)) !== 7) throw new Error("SPA navigation caused a full reload");
      console.log("ssr-dev: SPA routing + server loader ok (link -> /about, back -> /, no reload)");

      for (let i = 0; i < 3; i++) await counterBtn.click();
      const clicked = (await counterBtn.textContent()).trim();
      if (!clicked.includes(": 3")) throw new Error(`hydration did not attach handler: ${clicked}`);

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
  cleanup();
  process.exit(process.exitCode ?? 0);
}
