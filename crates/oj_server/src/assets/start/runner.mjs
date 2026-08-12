// SPDX-License-Identifier: MIT
// Persistent TanStack Start SSR runner. Registers the alias loader hook (with a
// MessagePort for warm reloads), imports the server entry, and answers render
// requests over stdio via the entry's fetch(Request). A {cmd:"reload"} line
// re-imports the entry fresh (app + @tanstack re-evaluate; React stays warm)
// instead of respawning the process.
import { register } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { MessageChannel } from "node:worker_threads";
import readline from "node:readline";

// Server-function base path (server functions build their URL from it; a bare
// createServerRpc needs it defined even for the in-process SSR path).
process.env.TSS_SERVER_FN_BASE ??= "/_serverFn/";

const HERE = dirname(fileURLToPath(import.meta.url));
const ENTRY = pathToFileURL(join(HERE, "server-entry.tsx")).href;
const { port1, port2 } = new MessageChannel();
register(pathToFileURL(join(HERE, "loader.mjs")).href, {
  parentURL: pathToFileURL(HERE + "/").href,
  data: { port: port2 },
  transferList: [port2],
});

// stdout is the JSON-lines protocol channel to oj. App code and its deps write
// to stdout via console.log / process.stdout.write, which would interleave with
// (and corrupt) the protocol. Capture the real writer for protocol frames, then
// divert stdout to stderr so app logs stay visible without breaking framing.
const send = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

let version = 0;
let handler = (await import(ENTRY)).default;
process.stderr.write("oj start runner: ready\n");

const rl = readline.createInterface({ input: process.stdin });
for await (const line of rl) {
  let msg;
  try { msg = JSON.parse(line); } catch { continue; }
  // Warm reload: bump the version, re-import the entry (loader re-evaluates app
  // + @tanstack under the new version), swap the handler, ack.
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
    // Build the URL from the real Host so the request origin matches the
    // client's Origin (server functions enforce a same-origin CSRF check).
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
