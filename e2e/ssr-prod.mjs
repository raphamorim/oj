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
const clientAsset = fs.readdirSync(path.join(out, "assets")).find((f) => /^entry-client-.*\.js$/.test(f));
if (!clientAsset) throw new Error("no hashed client hydration bundle emitted");
console.log("ssr-prod: build emitted server bundle + client assets + server.mjs");

// The server bundle's buffered render() (used as the fallback and by non-server
// consumers) still works — importing it also proves the Node bundle is valid.
const mod = await import(pathToFileURL(path.join(out, "entry-server.mjs")).href);
if (!String(mod.render()).includes("ssr")) throw new Error("buffered render() missing ssr content");
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
  console.log("ssr-prod: streaming + per-route ok (/ -> home, /about -> about)");

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
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    try {
      await page.goto(`${base}/`, { waitUntil: "networkidle" });
      const deferred = await page.locator("[data-deferred]").textContent();
      if (!deferred.includes("deferred-streamed")) throw new Error(`deferred content lost: ${deferred}`);
      await page.locator("button").click();
      await page.waitForFunction(() => document.querySelector("button").textContent.includes(": 1"), {
        timeout: 5000,
      });
      // Client-side SPA routing: navigate to /about via a link, no full reload.
      await page.evaluate(() => (window.__spa = 1));
      await page.locator('a[href="/about"]').click();
      await page.waitForSelector('[data-page="about"]');
      if ((await page.evaluate(() => window.__spa)) !== 1) throw new Error("SPA navigation caused a full reload");
      if ((await page.evaluate(() => location.pathname)) !== "/about") throw new Error("URL did not update to /about");
      if (errors.length) throw new Error(`console errors: ${errors.join("; ")}`);
      console.log("ssr-prod: hydration + SPA routing ok (home interactive, link -> /about, no reload)");
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
