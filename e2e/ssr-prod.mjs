// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Check the streaming production SSR build: `oj build --ssr` emits a server
// bundle, a hashed client hydration bundle, and a `server.mjs` that streams
// renderToReadableStream over chunked HTTP and serves the client assets.
// Verifies, in order:
//   1. the build emits server bundle + client assets + server.mjs,
//   2. the running server streams (chunked) the shell + deferred Suspense
//      content and serves the hashed client bundle,
//   3. (browser, if Playwright is installed) the page hydrates and is
//      interactive with no console errors.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const playground = path.join(repo, "playground");
const out = path.join(playground, "dist-ssr");
const port = 5181;
const base = `http://localhost:${port}`;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
fs.rmSync(out, { recursive: true, force: true });
execSync(`${path.join(repo, "target", "debug", "oj")} build playground --ssr src/entry-server.tsx --out dist-ssr`, {
  cwd: repo,
  stdio: "inherit",
});

// 1. Build artifacts.
const serverJs = path.join(out, "server.mjs");
if (!fs.existsSync(path.join(out, "entry-server.mjs"))) throw new Error("no server bundle emitted");
if (!fs.existsSync(serverJs)) throw new Error("no server.mjs emitted");
// Prerender (SSG): build.prerender = ["/", "/about"] -> static HTML with the
// route content, the client hydration script, and (for "/") the resolved
// Suspense boundary so it hydrates without a mismatch.
{
  const home = path.join(out, "index.html");
  const about = path.join(out, "about", "index.html");
  if (!fs.existsSync(home) || !fs.existsSync(about)) throw new Error("prerender did not emit static HTML");
  const homeHtml = fs.readFileSync(home, "utf8");
  if (!homeHtml.includes('data-page="home"')) throw new Error("prerendered / missing home route content");
  if (!homeHtml.includes("deferred-streamed")) throw new Error("prerendered / did not resolve the Suspense boundary");
  if (!/src="\/assets\/entry-client[^"]*\.js"/.test(homeHtml)) throw new Error("prerendered / missing client hydration script");
  if (!fs.readFileSync(about, "utf8").includes('data-page="about"')) throw new Error("prerendered /about missing about content");
  console.log("ssr-prod: prerender (SSG) ok (/ and /about static HTML, Suspense resolved, hydration wired)");
}
// Server functions: the dispatch bundle carries the real server code; the
// client bundle carries only stubs (the server-only marker must not leak).
{
  if (!fs.existsSync(path.join(out, "_oj_server_fns.mjs"))) throw new Error("no server-fn dispatch bundle emitted");
  // The real server code is code-split into a sibling _oj_server_fns-*.mjs
  // (the glob produces lazy imports); it must carry the server-only marker.
  const serverHasReal = fs
    .readdirSync(out)
    .filter((f) => f.startsWith("_oj_server_fns") && f.endsWith(".mjs"))
    .some((f) => fs.readFileSync(path.join(out, f), "utf8").includes("process.versions"));
  if (!serverHasReal) throw new Error("server-fn dispatch bundle missing the real implementation");
  const clientLeak = fs
    .readdirSync(path.join(out, "assets"))
    .filter((f) => f.endsWith(".js"))
    .some((f) => fs.readFileSync(path.join(out, "assets", f), "utf8").includes("process.versions"));
  if (clientLeak) throw new Error("server-only code leaked into the client bundle");
  console.log("ssr-prod: server functions ok (dispatch bundle has real code, client has only stubs)");
}
// User plugins run in BOTH SSR-build environments: entry-server (ssr) and
// entry-client (client) each import a plugin virtual module, so its source must
// be bundled into both the server bundle and a client asset.
if (!fs.readFileSync(path.join(out, "entry-server.mjs"), "utf8").includes("hello from plugin")) {
  throw new Error("ssr build (ssr env) did not run plugin resolveId/load (virtual module absent from server bundle)");
}
const clientHasVirtual = fs
  .readdirSync(path.join(out, "assets"))
  .filter((f) => f.endsWith(".js"))
  .some((f) => fs.readFileSync(path.join(out, "assets", f), "utf8").includes("hello from plugin"));
if (!clientHasVirtual) {
  throw new Error("ssr build (client env) did not run plugin resolveId/load (virtual module absent from client bundle)");
}
// Per-environment define: the ssr build applies config + ssr-environment define.
if (!fs.readFileSync(path.join(out, "entry-server.mjs"), "utf8").includes("global-define|ssr-define")) {
  throw new Error("ssr build did not apply the ssr-environment define");
}
// Per-environment build output: environments.ssr.build.sourcemap=false means
// no server-bundle sourcemap, while the client bundle still emits one.
if (fs.existsSync(path.join(out, "entry-server.mjs.map"))) {
  throw new Error("ssr env sourcemap:false ignored (server bundle emitted a .map)");
}
if (!fs.readdirSync(path.join(out, "assets")).some((f) => f.endsWith(".js.map"))) {
  throw new Error("client env sourcemap missing (no client .map emitted)");
}
const assetFiles = fs.readdirSync(path.join(out, "assets"));
const clientAsset = assetFiles.find((f) => /^entry-client-.*\.js$/.test(f));
if (!clientAsset) throw new Error("no hashed client hydration bundle emitted");
console.log("ssr-prod: build emitted server bundle + client assets + server.mjs");

// Route-level code splitting: the entry chunk must NOT inline route bodies;
// each route is a separate chunk loaded on demand.
const entryCode = fs.readFileSync(path.join(out, "assets", clientAsset), "utf8");
if (entryCode.includes("boom: the loader failed")) throw new Error("boom route was bundled into the entry chunk (not split)");
const boomChunk = assetFiles.find(
  (f) => f !== clientAsset && f.endsWith(".js") && fs.readFileSync(path.join(out, "assets", f), "utf8").includes("boom: the loader failed"),
);
if (!boomChunk) throw new Error("boom route was not emitted as its own chunk");
console.log(`ssr-prod: route code splitting ok (${assetFiles.filter((f) => f.endsWith(".js")).length} js chunks; boom in ${boomChunk}, not entry)`);

// The server bundle's buffered render() (used as the fallback and by non-server
// consumers) still works — importing it also proves the Node bundle is valid.
const mod = await import(pathToFileURL(path.join(out, "entry-server.mjs")).href);
// render() is async (it preloads the route's code-split chunk first).
if (!String(await mod.render()).includes("ssr")) throw new Error("buffered render() missing ssr content");
console.log("ssr-prod: server bundle render() ok (buffered fallback)");

try {
  execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs kill`, { shell: "/bin/bash", stdio: "ignore" });
} catch {}
const server = spawn("node", [serverJs], {
  cwd: playground,
  env: { ...process.env, PORT: String(port) },
  stdio: ["ignore", "ignore", "inherit"],
});

const cleanup = () => {
  server.kill("SIGKILL");
  fs.rmSync(out, { recursive: true, force: true });
};

const up = async () => {
  for (let i = 0; i < 60; i++) {
    try {
      if ((await fetch(`${base}/`)).ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("prod SSR server did not start");
};

try {
  await up();

  // 2. Streaming transport + content.
  const headers = await new Promise((resolve, reject) => {
    http.get(`${base}/`, (res) => (res.resume(), resolve(res.headers))).on("error", reject);
  });
  if (headers["transfer-encoding"] !== "chunked" || headers["content-length"]) {
    throw new Error(`prod SSR was not streamed (chunked): ${JSON.stringify(headers)}`);
  }
  const html = await (await fetch(`${base}/`)).text();
  if (!/ssr[^0-9]*0/.test(html.replace(/<[^>]*>/g, ""))) throw new Error(`missing SSR content:\n${html}`);
  if (!html.includes("deferred-streamed")) throw new Error(`missing streamed Suspense content:\n${html}`);
  if (!html.includes(`/assets/${clientAsset}`)) throw new Error("client hydration bundle not referenced");
  if (!(await fetch(`${base}/assets/${clientAsset}`)).ok) throw new Error("client bundle not served");
  if (!html.includes('data-page="home"')) throw new Error("/ did not render the home route");
  const about = await (await fetch(`${base}/about`)).text();
  if (!about.includes('data-page="about"')) throw new Error("/about did not render the about route");
  // File-based dynamic route (src/routes/users/$id.tsx) with a param.
  const user = await (await fetch(`${base}/users/7`)).text();
  if (!user.includes('data-user-id="7"')) throw new Error("dynamic file-based route /users/7 did not render");
  // Nested layouts + per-layout loaders: root + users layouts compose, each
  // with its own loader data alongside the page's.
  if (!user.includes('data-layout="root"') || !user.includes('data-layout="users"')) {
    throw new Error("nested layouts did not compose on /users/7");
  }
  if (!user.includes('data-app-name="oj"') || !user.includes('data-users-count="3"')) {
    throw new Error("per-layout loader data missing on /users/7");
  }
  if (about.includes('data-layout="users"')) throw new Error("users layout leaked onto /about");
  // Per-route head/meta (incl. Open Graph) rendered into <head>.
  if (!user.includes("<title>User 7 - oj</title>")) throw new Error("/users/7 route title missing from head");
  if (!user.includes('property="og:title" content="User 7"') || !user.includes('property="og:site_name"')) {
    throw new Error("/users/7 Open Graph tags missing/unmerged");
  }
  if (!about.includes("<title>About - oj</title>") || !about.includes('name="description"')) {
    throw new Error("/about title/meta missing from head");
  }
  if (!html.includes("window.__OJ_DATA__=") || !html.includes('"likes":') || !html.includes('data-likes="0"')) {
    throw new Error(`route data not loaded/serialized server-side:\n${html}`);
  }
  // Action + revalidation over HTTP: POST mutates server state, GET reflects it.
  // The loader response is the chain's data map keyed by route/layout id.
  const acted = await (await fetch(`${base}/`, { method: "POST", headers: { "oj-loader": "1" } })).json();
  if (acted["index"]?.likes !== 1) throw new Error(`action did not mutate server state: ${JSON.stringify(acted)}`);
  const reloaded = await (await fetch(`${base}/about`, { headers: { "oj-loader": "1" } })).json();
  if (reloaded["about"]?.likes !== 1) throw new Error("mutation not visible via server-authoritative loader");
  if (reloaded["layout"]?.app !== "oj") throw new Error("root layout loader did not run in the chain");
  // A failing loader returns an error status (surfaced as route-error UI on the client).
  if ((await fetch(`${base}/boom`, { headers: { "oj-loader": "1" } })).status !== 500) {
    throw new Error("failing loader did not return an error status");
  }
  console.log("ssr-prod: streaming + per-route + data loading + action + loader error ok");

  // 3. Hydration.
  let pw = null;
  try {
    pw = await import("playwright");
  } catch {}
  if (pw) {
    const browser = await pw.chromium.launch();
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    page.on("console", (m) => {
      // The intentional /boom loader returns 500; the browser logs that failed
      // response as a console error — expected, not a bug.
      if (m.type() === "error" && !m.text().includes("Failed to load resource")) errors.push(m.text());
    });
    try {
      await page.goto(`${base}/`, { waitUntil: "networkidle" });
      const deferred = await page.locator("[data-deferred]").textContent();
      if (!deferred.includes("deferred-streamed")) throw new Error(`deferred content lost: ${deferred}`);
      // Server function: entry-client called a `*.server.ts` export on hydrate;
      // its RPC hit /__oj_fn and ran the real function server-side.
      const sfn = await page.waitForFunction(() => globalThis.__OJ_SFN, { timeout: 8000 }).then((h) => h.jsonValue());
      if (sfn !== "hello, prod (server=true)") throw new Error("server function RPC failed in prod: " + JSON.stringify(sfn));
      // A failing loader renders the route-error UI (no crash), then recovers.
      await page.locator('a[href="/boom"]').click();
      await page.waitForSelector('[data-error]', { timeout: 5000 });
      await page.locator('[data-error] a[href="/"]').click();
      await page.waitForSelector('[data-page="home"]');
      await page.locator("button", { hasText: "ssr" }).click(); // counter is interactive
      await page.waitForFunction(
        () => document.querySelector("button").textContent.includes(": 1"),
        { timeout: 5000 },
      );
      // Action + SPA routing, no full reload.
      await page.evaluate(() => (window.__spa = 1));
      const before = Number(await page.locator("[data-likes]").getAttribute("data-likes"));
      await page.locator("button", { hasText: "like" }).click();
      await page.waitForSelector(`[data-likes="${before + 1}"]`, { timeout: 5000 });
      await page.locator('a[href="/about"]').click();
      await page.waitForSelector('[data-page="about"]');
      if ((await page.evaluate(() => window.__spa)) !== 1) throw new Error("SPA navigation caused a full reload");
      if ((await page.evaluate(() => location.pathname)) !== "/about") throw new Error("URL did not update to /about");
      if (errors.length) throw new Error(`console errors: ${errors.join("; ")}`);
      console.log("ssr-prod: hydration + SPA + action ok (counter interactive, like mutation, link -> /about, no reload)");
    } finally {
      await browser.close();
    }
  } else {
    console.log("ssr-prod: playwright not installed, skipped browser hydration check");
  }

  console.log("\nSSR-PROD TEST PASSED");
} catch (e) {
  console.error(`\nSSR-PROD TEST FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  cleanup();
  process.exit(process.exitCode ?? 0);
}
