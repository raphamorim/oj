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
      JSON.stringify({ path: url.pathname, host: url.host, method: request.method, echo }),
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
  const req = async (msg) => {
    child.stdin.write(JSON.stringify(msg) + "\n");
    return JSON.parse(await nextFrame());
  };
  return { child, ready, req, getStderr: () => stderr };
}

test("runner speaks the json-lines stdio protocol", async () => {
  const dir = mkdtempSync(join(tmpdir(), "oj-runner-"));
  const r = startRunner(dir);
  try {
    await r.ready();

    const g = await r.req({ id: 1, method: "GET", url: "/hello", headers: { host: "example.test" } });
    assert.equal(g.id, 1);
    assert.equal(g.status, 200);
    assert.equal(g.headers["x-custom"], "runner-ok");
    const gbody = JSON.parse(g.body);
    assert.equal(gbody.path, "/hello");
    assert.equal(gbody.host, "example.test");
    assert.equal(gbody.method, "GET");

    const p = await r.req({ id: 2, method: "POST", url: "/submit", headers: { host: "h" }, body: "payload" });
    assert.equal(JSON.parse(p.body).echo, "payload");

    const reloaded = await r.req({ cmd: "reload" });
    assert.equal(reloaded.reloaded, true);
    const after = await r.req({ id: 3, url: "/hello", headers: { host: "h" } });
    assert.equal(after.status, 200);

    const boom = await r.req({ id: 4, url: "/boom", headers: { host: "h" } });
    assert.equal(boom.status, 500);
    assert.match(boom.body, /kaboom/);

    const missing = await r.req({ id: 5, url: "/missing", headers: { host: "h" } });
    assert.equal(missing.status, 404);

    assert.match(r.getStderr(), /APP_LOG_MUST_NOT_CORRUPT_PROTOCOL/);
  } finally {
    r.child.kill("SIGKILL");
    rmSync(dir, { recursive: true, force: true });
  }
});
