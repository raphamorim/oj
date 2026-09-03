// SPDX-License-Identifier: MIT

import module from "node:module";
import http from "node:http";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fstatSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import readline from "node:readline";

process.env.TSS_SERVER_FN_BASE ??= "/_serverFn/";
process.env.TSS_DEV_SERVER ??= "true";
process.env.TSS_DEV_SSR_STYLES_ENABLED ??= "false";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
if (process.env.OJ_V8_COMPILE_CACHE === "on") {
  try {
    const v8Dir = process.env.NODE_COMPILE_CACHE ?? join(HERE, "..", "v8");
    process.env.NODE_COMPILE_CACHE ??= v8Dir;
    module.enableCompileCache?.(v8Dir);
  } catch {}
}
const phase = (label) => {
  if (process.env.OJ_BOOT_PHASES) process.stderr.write(`[oj-phase] ${Date.now()} runner: ${label}\n`);
};
phase("main begin");
const ENTRY = pathToFileURL(process.env.OJ_RUNNER_ENTRY || join(HERE, "server-entry.tsx")).href;
const LOADER = pathToFileURL(process.env.OJ_RUNNER_LOADER || join(HERE, "loader.mjs")).href;
const loaderApi = await import(LOADER);
if (loaderApi.resolve || loaderApi.load) module.registerHooks({ resolve: loaderApi.resolve, load: loaderApi.load });
phase("loader registered");

const send = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

const flushV8 = () => { try { module.flushCompileCache?.(); } catch {} };
let version = 0;
let statsSent = false;
let handler = null;
// Requests arrive over a loopback HTTP server rather than JSON lines on stdin:
// bodies stay binary, responses stream (TanStack Start streams its dehydrated
// data into the HTML), and requests run concurrently (a loader that fetches the
// app's own route during SSR no longer deadlocks behind itself). stdin keeps
// the control commands (revalidate/reload).
let entryReady;
const server = http.createServer(async (req, res) => {
  try {
    await entryReady;
    const host = req.headers["x-forwarded-host"] || req.headers.host || "localhost";
    const method = req.method || "GET";
    const headers = new Headers();
    for (const [k, v] of Object.entries(req.headers)) {
      if (v == null || k === "x-forwarded-host") continue;
      if (Array.isArray(v)) for (const x of v) headers.append(k, x);
      else headers.set(k, v);
    }
    headers.set("host", host);
    const init = { method, headers };
    if (method !== "GET" && method !== "HEAD") {
      init.body = Readable.toWeb(req);
      init.duplex = "half";
    }
    const out = await handler.fetch(new Request("http://" + host + (req.url ?? "/"), init));
    const outHeaders = {};
    out.headers.forEach((v, k) => { if (k !== "set-cookie") outHeaders[k] = v; });
    const cookies = out.headers.getSetCookie?.() ?? [];
    if (cookies.length) outHeaders["set-cookie"] = cookies;
    delete outHeaders["content-length"];
    res.writeHead(out.status, outHeaders);
    if (out.body) await pipeline(Readable.fromWeb(out.body), res);
    else res.end();
    loaderApi.flushCaches?.();
    if (!statsSent) {
      statsSent = true;
      loaderApi.reportCacheStats?.();
      flushV8();
      if (process.env.OJ_SSR_MEM_STATS === "1") {
        const v8 = await import("node:v8");
        const stats = {
          rss: process.memoryUsage.rss(),
          heap: v8.default.getHeapStatistics(),
          loader: loaderApi.memStats?.() ?? null,
        };
        process.stderr.write(`oj ssr memstats: ${JSON.stringify(stats)}\n`);
      }
    }
  } catch (e) {
    const text = String((e && e.stack) || e);
    if (res.headersSent) { res.end(text); return; }
    // Vite's errorMiddleware: a 500 HTML page carrying the message and stack
    // (its overlay import falls back to exactly these h1/h2/pre elements).
    const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    res.writeHead(500, { "content-type": "text/html; charset=utf-8" });
    res.end(`<!DOCTYPE html>\n<html lang="en"><head><meta charset="UTF-8" /><title>Error</title></head>\n<body><h1>Internal Server Error</h1><h2>${esc((e && e.message) || e)}</h2><pre>${esc(text)}</pre></body></html>\n`);
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
send(JSON.stringify({ port: server.address().port }) + "\n");
entryReady = (async () => {
  handler = (await import(ENTRY)).default;
})();
await entryReady;
phase("entry evaluated");
flushV8();
loaderApi.flushCaches?.();
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
process.stderr.write(`${OJ} start runner: ready\n`);

let parentGone = false;
const onParentGone = () => {
  if (parentGone) return;
  parentGone = true;
  try { flushV8(); loaderApi.flushCaches(); } catch {}
  setTimeout(() => process.exit(0), 500);
};
try {
  if (fstatSync(0, { bigint: true }).isFIFO()) {
    process.stdin.once("end", onParentGone);
    process.stdin.once("close", onParentGone);
  }
} catch {}

const rl = readline.createInterface({ input: process.stdin });
for await (const line of rl) {
  let msg;
  try { msg = JSON.parse(line); } catch { continue; }
  if (msg.cmd === "revalidate") {
    let reloaded = false;
    try {
      if (loaderApi.revalidateSpeculation?.()) {
        version += 1;
        loaderApi.setVersion?.(version);
        handler = (await import(`${ENTRY}?ojv=${version}`)).default;
        flushV8();
        reloaded = true;
      }
    } catch {}
    loaderApi.flushCaches?.();
    phase(`revalidated (reloaded=${reloaded})`);
    send(JSON.stringify({ revalidated: true, reloaded }) + "\n");
    continue;
  }
  if (msg.cmd === "reload") {
    try {
      version += 1;
      loaderApi.setVersion?.(version);
      handler = (await import(`${ENTRY}?ojv=${version}`)).default;
      flushV8();
      loaderApi.flushCaches?.();
      send(JSON.stringify({ reloaded: true }) + "\n");
    } catch (e) {
      send(JSON.stringify({ reloaded: false, error: String((e && e.stack) || e) }) + "\n");
    }
    continue;
  }
  // Requests travel over the loopback HTTP server; anything else on stdin is
  // an unknown control command.
  if (msg.id != null) {
    send(JSON.stringify({ id: msg.id, status: 500, headers: {}, body: "oj start runner: requests moved to the loopback http server" }) + "\n");
  }
}
