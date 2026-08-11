// SPDX-License-Identifier: MIT
// Persistent TanStack Start SSR runner: registers the alias loader hook,
// imports the server entry once, then answers {id,url} render requests over
// stdio by calling the entry's fetch(Request) and returning the HTML.
import { register } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import readline from "node:readline";

// Server-function base path (server functions build their URL from it; a bare
// createServerRpc needs it defined even for the in-process SSR path).
process.env.TSS_SERVER_FN_BASE ??= "/_serverFn/";

const HERE = dirname(fileURLToPath(import.meta.url));
register(pathToFileURL(join(HERE, "loader.mjs")).href, pathToFileURL(HERE + "/").href);

const entry = await import(pathToFileURL(join(HERE, "server-entry.tsx")).href);
process.stderr.write("oj start runner: ready\n");

const rl = readline.createInterface({ input: process.stdin });
for await (const line of rl) {
  let msg;
  try { msg = JSON.parse(line); } catch { continue; }
  try {
    const res = await entry.default.fetch(new Request("http://localhost" + (msg.url ?? "/")));
    const body = await res.text();
    process.stdout.write(JSON.stringify({ id: msg.id, status: res.status, body }) + "\n");
  } catch (e) {
    process.stdout.write(JSON.stringify({ id: msg.id, status: 500, body: String((e && e.stack) || e) }) + "\n");
  }
}
