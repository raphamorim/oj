// SPDX-License-Identifier: MIT

import module, { register } from "node:module";
import { fstatSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { MessageChannel } from "node:worker_threads";
import readline from "node:readline";

process.env.TSS_SERVER_FN_BASE ??= "/_serverFn/";

const HERE = dirname(fileURLToPath(import.meta.url));
const ENTRY = pathToFileURL(process.env.OJ_RUNNER_ENTRY || join(HERE, "server-entry.tsx")).href;
const LOADER = pathToFileURL(process.env.OJ_RUNNER_LOADER || join(HERE, "loader.mjs")).href;
const { port1, port2 } = new MessageChannel();
register(LOADER, {
  parentURL: pathToFileURL(HERE + "/").href,
  data: { port: port2 },
  transferList: [port2],
});

const send = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

let version = 0;
let handler = (await import(ENTRY)).default;
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
process.stderr.write(`${OJ} start runner: ready\n`);

const onParentGone = () => {
  try { module.flushCompileCache?.(); } catch {}
  process.exit(0);
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
  if (msg.cmd === "reload") {
    try {
      version += 1;
      port1.postMessage(version);
      handler = (await import(`${ENTRY}?ojv=${version}`)).default;
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
  } catch (e) {
    send(JSON.stringify({ id: msg.id, status: 500, headers: {}, body: String((e && e.stack) || e) }) + "\n");
  }
}
