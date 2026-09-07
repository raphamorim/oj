// SPDX-License-Identifier: MIT
//
// The Start SSR plugin bridge (container-bridge.mjs) talks to the plugin-host
// container over named-pipe request/reply fifos. When the container process
// dies/restarts mid-session the bridge must RECONNECT to the restarted container
// and recover (return the real result), not crash on EPIPE and not degrade to a
// permanent "down". And when the container is truly gone it must report `down`
// so callers never persist a null into the loader cache. This drives a mock
// container over real fifos through a restart.
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const bridgeUrl = pathToFileURL(join(repo, "crates/oj_server/src/assets/start/container-bridge.mjs")).href;

// A mock container: opens both fifos "r+" like plugin-host.mjs, reads framed
// requests ({id,method,args}), replies {id, value:"OK:<tag>:<method>:<arg0>"},
// exits after `maxReqs`. `delayMs` waits before opening (to force the bridge's
// write into the no-reader gap so the reconnect path runs).
const MOCK = [
  'import { openSync, readSync, writeSync } from "node:fs";',
  "const [,, reqPath, repPath, tag, maxReqs, delayMs] = process.argv;",
  "const sl = new Int32Array(new SharedArrayBuffer(4));",
  "if (Number(delayMs)) Atomics.wait(sl, 0, 0, Number(delayMs));",
  'const repFd = openSync(repPath, "r+");',
  'const reqFd = openSync(reqPath, "r+");',
  "function readExact(fd, len) {",
  "  const b = Buffer.allocUnsafe(len); let off = 0;",
  "  while (off < len) {",
  "    let n; try { n = readSync(fd, b, off, len - off, null); }",
  '    catch (e) { if (e.code === "EAGAIN" || e.code === "EINTR") { Atomics.wait(sl,0,0,1); continue; } throw e; }',
  "    if (n === 0) process.exit(0);",
  "    off += n;",
  "  }",
  "  return b;",
  "}",
  "let handled = 0;",
  "while (handled < Number(maxReqs)) {",
  "  const head = readExact(reqFd, 4);",
  "  const req = JSON.parse(readExact(reqFd, head.readUInt32LE(0)).toString());",
  "  const rep = Buffer.from(JSON.stringify({ id: req.id, value: `OK:${tag}:${req.method}:${req.args?.[0] ?? \"\"}` }));",
  "  const frame = Buffer.allocUnsafe(4 + rep.length);",
  "  frame.writeUInt32LE(rep.length, 0); rep.copy(frame, 4);",
  "  let off = 0; while (off < frame.length) off += writeSync(repFd, frame, off, frame.length - off);",
  "  handled++;",
  "}",
  "process.exit(0);",
].join("\n");

test("bridge reconnects to a restarted container, recovers, and reports down when it is gone", async () => {
  const tmp = mkdtempSync(join(tmpdir(), "oj-bridge-rc-"));
  const app = join(tmp, "app");
  const bdir = join(tmp, "bridge");
  mkdirSync(app);
  mkdirSync(bdir);
  writeFileSync(join(app, "vite.config.mjs"), "export default {};\n"); // findConfig
  const mockPath = join(tmp, "mock.mjs");
  writeFileSync(mockPath, MOCK);
  const reqFifo = join(bdir, "req.fifo");
  const repFifo = join(bdir, "rep.fifo");
  execFileSync("mkfifo", [reqFifo, repFifo]);

  const mocks = [];
  const spawnMock = (tag, maxReqs, delayMs = 0) => {
    const m = spawn(process.execPath, [mockPath, reqFifo, repFifo, tag, String(maxReqs), String(delayMs)], {
      stdio: "ignore",
    });
    mocks.push(m);
    return m;
  };

  const prevDir = process.env.OJ_SSR_BRIDGE_DIR;
  const prevRc = process.env.OJ_SSR_BRIDGE_RECONNECT_MS;
  process.env.OJ_SSR_BRIDGE_DIR = bdir;
  process.env.OJ_SSR_BRIDGE_RECONNECT_MS = "2500";
  try {
    // Container A: serves one call then exits (a restart).
    spawnMock("A", 1);
    const { loadPluginContainerSync } = await import(bridgeUrl);
    const bridge = loadPluginContainerSync(app, { command: "serve", environment: "ssr" });
    assert.ok(bridge, "bridge constructed (config + fifo dir present)");

    // 1. First call reaches container A.
    assert.equal(bridge.resolveId("x", undefined), "OK:A:resolveId:x");
    assert.equal(bridge.down(), false);

    // 2. Container A has exited. Bring up container B AFTER a delay so the
    // bridge's next write hits the no-reader gap -> EPIPE -> reconnect -> retry
    // against B. Recovery must return B's real result, not null/crash.
    spawnMock("B", 5, 400);
    assert.equal(bridge.resolveId("y", undefined), "OK:B:resolveId:y", "reconnected to the restarted container");
    assert.equal(bridge.load("z"), "OK:B:load:z", "the reconnected bridge keeps serving");
    assert.equal(bridge.down(), false);

    // 3. Container B killed and none replaces it: the call must return null (not
    // hang past the reconnect window, not crash) and the bridge must report down
    // so the loader won't persist this null.
    for (const m of mocks) m.kill("SIGKILL");
    assert.equal(bridge.resolveId("q", undefined), null, "no container -> null, not a crash");
    assert.equal(bridge.down(), true, "down() true so callers skip caching");
    assert.equal(bridge.resolveId("q2", undefined), null, "stays down");
  } finally {
    for (const m of mocks) { try { m.kill("SIGKILL"); } catch {} }
    if (prevDir === undefined) delete process.env.OJ_SSR_BRIDGE_DIR; else process.env.OJ_SSR_BRIDGE_DIR = prevDir;
    if (prevRc === undefined) delete process.env.OJ_SSR_BRIDGE_RECONNECT_MS; else process.env.OJ_SSR_BRIDGE_RECONNECT_MS = prevRc;
    rmSync(tmp, { recursive: true, force: true });
  }
});
