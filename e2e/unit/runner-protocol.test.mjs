// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import readline from "node:readline";

const here = dirname(fileURLToPath(import.meta.url));
const RUNNER = join(here, "..", "..", "crates", "oj_server", "src", "assets", "start", "runner.mjs");

const STUB_LOADER = "export async function initialize() {}\n";

const STUB_ENTRY = `
export default {
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/boom") throw new Error("kaboom");
    console.log("APP_LOG_MUST_NOT_CORRUPT_PROTOCOL");
    const echo = request.method === "GET" ? "" : await request.text();
    return new Response(
      JSON.stringify({ path: url.pathname, host: url.host, hostHeader: request.headers.get("host"), forwardedHost: request.headers.get("x-forwarded-host"), method: request.method, echo }),
      {
        status: url.pathname === "/missing" ? 404 : 200,
        headers: { "content-type": "application/json", "x-custom": "runner-ok" },
      },
    );
  },
};
`;

function startRunner(dir) {
  writeFileSync(join(dir, "loader.mjs"), STUB_LOADER);
  writeFileSync(join(dir, "entry.mjs"), STUB_ENTRY);
  const child = spawn("node", [RUNNER], {
    env: {
      ...process.env,
      OJ_RUNNER_ENTRY: join(dir, "entry.mjs"),
      OJ_RUNNER_LOADER: join(dir, "loader.mjs"),
    },
    stdio: ["pipe", "pipe", "pipe"],
  });
  const frames = [];
  let waiter = null;
  readline.createInterface({ input: child.stdout }).on("line", (line) => {
    if (waiter) { const w = waiter; waiter = null; w(line); } else frames.push(line);
  });
  let stderr = "";
  child.stderr.on("data", (d) => { stderr += d.toString(); });

  const nextFrame = () =>
    new Promise((res) => (frames.length ? res(frames.shift()) : (waiter = res)));
  const ready = () =>
    new Promise((res, rej) => {
      const t = setInterval(() => {
        if (stderr.includes("oj start runner: ready")) { clearInterval(t); clearTimeout(to); res(); }
      }, 20);
      const to = setTimeout(() => { clearInterval(t); rej(new Error("runner never became ready; stderr:\n" + stderr)); }, 10000);
    });
  // Control commands still travel over stdin as JSON lines...
  const cmd = async (msg) => {
    child.stdin.write(JSON.stringify(msg) + "\n");
    return JSON.parse(await nextFrame());
  };
  // ...while requests go to the loopback HTTP server the runner announces on
  // its first stdout line (`{ port }`), so bodies stay binary and responses stream.
  let port = null;
  const req = async ({ method = "GET", url, headers = {}, body }) => {
    if (port == null) port = JSON.parse(await nextFrame()).port;
    const res = await fetch(`http://127.0.0.1:${port}${url}`, { method, headers, body });
    const h = {};
    res.headers.forEach((v, k) => { h[k] = v; });
    return { status: res.status, headers: h, body: await res.text() };
  };
  return { child, ready, req, cmd, getStderr: () => stderr };
}

test("runner serves requests over loopback http and commands over stdin", async () => {
  const dir = mkdtempSync(join(tmpdir(), "oj-runner-"));
  const r = startRunner(dir);
  try {
    await r.ready();

    // The browser's Host arrives as x-oj-host (hyper owns the loopback Host);
    // a proxy's x-forwarded-host is the app's to read, as under Vite (srvx
    // only consults it behind a trusted proxy).
    const g = await r.req({ method: "GET", url: "/hello", headers: { "x-oj-host": "example.test", "x-forwarded-host": "proxy.example.test" } });
    assert.equal(g.status, 200);
    assert.equal(g.headers["x-custom"], "runner-ok");
    const gbody = JSON.parse(g.body);
    assert.equal(gbody.path, "/hello");
    assert.equal(gbody.host, "example.test");
    assert.equal(gbody.hostHeader, "example.test");
    assert.equal(gbody.forwardedHost, "proxy.example.test");
    assert.equal(gbody.method, "GET");

    // Node keeps only the first of duplicate Host headers; a joined value is
    // cut the same way instead of failing URL parsing.
    const joined = await r.req({ url: "/hello", headers: { "x-oj-host": "first.test, second.test" } });
    assert.equal(joined.status, 200);
    assert.equal(JSON.parse(joined.body).host, "first.test");

    const p = await r.req({ method: "POST", url: "/submit", headers: { "x-oj-host": "h" }, body: "payload" });
    assert.equal(JSON.parse(p.body).echo, "payload");

    const reloaded = await r.cmd({ cmd: "reload" });
    assert.equal(reloaded.reloaded, true);
    const after = await r.req({ url: "/hello", headers: { "x-oj-host": "h" } });
    assert.equal(after.status, 200);

    // An exception escaping the handler is a 500 HTML page with message and
    // stack, like Vite's errorMiddleware fallback body.
    const boom = await r.req({ url: "/boom", headers: { "x-oj-host": "h" } });
    assert.equal(boom.status, 500);
    assert.match(boom.headers["content-type"], /text\/html/);
    assert.match(boom.body, /<h1>Internal Server Error<\/h1><h2>kaboom<\/h2><pre>Error: kaboom/);

    const missing = await r.req({ url: "/missing", headers: { "x-oj-host": "h" } });
    assert.equal(missing.status, 404);

    assert.match(r.getStderr(), /APP_LOG_MUST_NOT_CORRUPT_PROTOCOL/);
  } finally {
    r.child.kill("SIGKILL");
    rmSync(dir, { recursive: true, force: true });
  }
});
