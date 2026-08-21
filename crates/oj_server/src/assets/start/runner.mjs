// SPDX-License-Identifier: MIT

import module from "node:module";
import { fstatSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import readline from "node:readline";

process.env.TSS_SERVER_FN_BASE ??= "/_serverFn/";
// Vite dev sets this too: start-server-core then resolves the manifest per
// request instead of caching the first render's (possibly CSS-less) result.
process.env.TSS_DEV_SERVER ??= "true";
// oj serves no dev-styles endpoint, so keep that <link> injection off.
process.env.TSS_DEV_SSR_STYLES_ENABLED ??= "false";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
// Opt-in: measured on a large app, the bytecode cache costs more per boot
// (read + deserialize + produce for ~6k modules) than the ~0.7s compile it
// saves.
if (process.env.OJ_V8_COMPILE_CACHE === "on") {
  try {
    const v8Dir = join(APP, ".oj-cache", "v8");
    process.env.NODE_COMPILE_CACHE ??= v8Dir;
    module.enableCompileCache?.(v8Dir);
  } catch {}
}
const ENTRY = pathToFileURL(process.env.OJ_RUNNER_ENTRY || join(HERE, "server-entry.tsx")).href;
const LOADER = pathToFileURL(process.env.OJ_RUNNER_LOADER || join(HERE, "loader.mjs")).href;
// In-thread synchronous hooks (registerHooks, Node 22.15+/23.5+): no hooks
// worker, no sync-IPC round-trip per resolve/load. The loader itself is
// imported before registration, so its own dependency graph loads unhooked.
const loaderApi = await import(LOADER);
if (loaderApi.resolve || loaderApi.load) module.registerHooks({ resolve: loaderApi.resolve, load: loaderApi.load });

const send = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

const flushV8 = () => { try { module.flushCompileCache?.(); } catch {} };
let version = 0;
let statsSent = false;
let handler = (await import(ENTRY)).default;
flushV8();
loaderApi.flushCaches?.();
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
process.stderr.write(`${OJ} start runner: ready\n`);

// stdin EOF means oj died without tearing us down (SIGKILL skips
// kill_on_drop): exit instead of idling as an orphan.
const onParentGone = () => {
  try { module.flushCompileCache?.(); } catch {}
  process.exit(0);
};
try {
  // bigint keeps the FIFO mode out of the shared stat buffer fs.realpathSync reads mid-walk
  if (fstatSync(0, { bigint: true }).isFIFO()) {
    process.stdin.once("end", onParentGone);
    process.stdin.once("close", onParentGone);
  }
} catch {}

const rl = readline.createInterface({ input: process.stdin });
for await (const line of rl) {
  let msg;
  try { msg = JSON.parse(line); } catch { continue; }
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
  try {
    const init = { method: msg.method || "GET", headers: msg.headers || {} };
    if (init.method !== "GET" && init.method !== "HEAD" && msg.body != null) init.body = msg.body;
    const host = (msg.headers && msg.headers.host) || "localhost";
    const res = await handler.fetch(new Request("http://" + host + (msg.url ?? "/"), init));
    const body = await res.text();
    const headers = {};
    res.headers.forEach((v, k) => { headers[k] = v; });
    send(JSON.stringify({ id: msg.id, status: res.status, headers, body }) + "\n");
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
    send(JSON.stringify({ id: msg.id, status: 500, headers: {}, body: String((e && e.stack) || e) }) + "\n");
  }
}
