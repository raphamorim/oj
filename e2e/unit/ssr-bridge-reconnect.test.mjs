// SPDX-License-Identifier: MIT
//
// The Start SSR plugin bridge (container-bridge.mjs) talks to the plugin-host
// container over named-pipe request/reply fifos. When the container process
// dies/restarts mid-session the bridge must RECONNECT to the restarted container
// and recover (return the real result), not crash on EPIPE and not degrade to a
// permanent "down". And when the container is truly gone it must report `down`
// so callers never persist a null into the loader cache. This drives mock
// containers over real fifos through restarts, a stale-byte desync, and a
// later heal.
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

// A mock that serves ONE request, then writes GARBAGE (an oversized frame
// header + junk) into rep.fifo and LINGERS holding the rep write end open while
// closing only its req read end. Closing the req read end makes the bridge's
// next write EPIPE (a detected death); keeping the rep write end open means the
// fifo buffer (with the garbage) survives the bridge's closeFds()+reopen, so the
// bridge MUST drain it on reconnect or the next reply misframes.
const MOCK_LINGER = [
  'import { openSync, readSync, writeSync, closeSync } from "node:fs";',
  "const [,, reqPath, repPath] = process.argv;",
  "const sl = new Int32Array(new SharedArrayBuffer(4));",
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
  "const head = readExact(reqFd, 4);",
  "const req = JSON.parse(readExact(reqFd, head.readUInt32LE(0)).toString());",
  "const rep = Buffer.from(JSON.stringify({ id: req.id, value: `OK:A:${req.method}:${req.args?.[0] ?? \"\"}` }));",
  "const frame = Buffer.allocUnsafe(4 + rep.length);",
  "frame.writeUInt32LE(rep.length, 0); rep.copy(frame, 4);",
  "let off = 0; while (off < frame.length) off += writeSync(repFd, frame, off, frame.length - off);",
  "// leave a bogus oversized frame header + junk in rep.fifo",
  "const junk = Buffer.from([0xff, 0xff, 0xff, 0xff, 1, 2, 3]);",
  "off = 0; while (off < junk.length) off += writeSync(repFd, junk, off, junk.length - off);",
  "closeSync(reqFd); // req reader gone -> bridge's next write EPIPEs (death)",
  "Atomics.wait(sl, 0, 0, 5000); // linger, keeping rep write end open",
  "process.exit(0);",
].join("\n");

function setup() {
  const tmp = mkdtempSync(join(tmpdir(), "oj-bridge-rc-"));
  const app = join(tmp, "app");
  const bdir = join(tmp, "bridge");
  mkdirSync(app);
  mkdirSync(bdir);
  writeFileSync(join(app, "vite.config.mjs"), "export default {};\n"); // findConfig
  const reqFifo = join(bdir, "req.fifo");
  const repFifo = join(bdir, "rep.fifo");
  execFileSync("mkfifo", [reqFifo, repFifo]);
  const mocks = [];
  const spawnMock = (script, args) => {
    const m = spawn(process.execPath, [script, reqFifo, repFifo, ...args], { stdio: "ignore" });
    mocks.push(m);
    return m;
  };
  return { tmp, app, bdir, reqFifo, repFifo, mocks, spawnMock };
}

async function withEnv(bdir, reconnectMs, run) {
  const prevDir = process.env.OJ_SSR_BRIDGE_DIR;
  const prevRc = process.env.OJ_SSR_BRIDGE_RECONNECT_MS;
  process.env.OJ_SSR_BRIDGE_DIR = bdir;
  process.env.OJ_SSR_BRIDGE_RECONNECT_MS = String(reconnectMs);
  try {
    return await run();
  } finally {
    if (prevDir === undefined) delete process.env.OJ_SSR_BRIDGE_DIR; else process.env.OJ_SSR_BRIDGE_DIR = prevDir;
    if (prevRc === undefined) delete process.env.OJ_SSR_BRIDGE_RECONNECT_MS; else process.env.OJ_SSR_BRIDGE_RECONNECT_MS = prevRc;
  }
}

test("bridge reconnects to a restarted container, reports down, then heals a later restart", async () => {
  const { tmp, app, bdir, mocks, spawnMock } = setup();
  const mockPath = join(tmp, "mock.mjs");
  writeFileSync(mockPath, MOCK);
  const mock = (tag, maxReqs, delayMs = 0) => spawnMock(mockPath, [tag, String(maxReqs), String(delayMs)]);
  await withEnv(bdir, 2500, async () => {
    try {
      // Container A: serves one call then exits (a restart).
      mock("A", 1);
      const { loadPluginContainerSync } = await import(bridgeUrl);
      const bridge = loadPluginContainerSync(app, { command: "serve", environment: "ssr" });
      assert.ok(bridge, "bridge constructed (config + fifo dir present)");

      // 1. First call reaches container A.
      assert.equal(bridge.resolveId("x", undefined), "OK:A:resolveId:x");
      assert.equal(bridge.down(), false);

      // 2. Container A has exited. Bring up container B AFTER a delay so the
      // bridge's next write hits the no-reader gap -> EPIPE -> reconnect -> retry
      // against B. Recovery must return B's real result, not null/crash.
      mock("B", 5, 400);
      assert.equal(bridge.resolveId("y", undefined), "OK:B:resolveId:y", "reconnected to the restarted container");
      assert.equal(bridge.load("z"), "OK:B:load:z", "the reconnected bridge keeps serving");
      assert.equal(bridge.down(), false);

      // 3. Container B killed and none replaces it: the call must return null (not
      // hang past the reconnect window, not crash) and the bridge must report down
      // so the loader won't persist this null. A second call while down is instant.
      for (const m of mocks) m.kill("SIGKILL");
      assert.equal(bridge.resolveId("q", undefined), null, "no container -> null, not a crash");
      assert.equal(bridge.down(), true, "down() true so callers skip caching");
      assert.equal(bridge.resolveId("q2", undefined), null, "stays down");

      // 4. "down" is NOT terminal: a container that comes up LATER heals on a
      // subsequent call, since each render re-probes. Poll as successive renders
      // would until C answers.
      mock("C", 5);
      let healed = null;
      const sl = new Int32Array(new SharedArrayBuffer(4));
      for (let i = 0; i < 300 && healed == null; i++) {
        healed = bridge.resolveId("r", undefined);
        if (healed == null) Atomics.wait(sl, 0, 0, 10);
      }
      assert.equal(healed, "OK:C:resolveId:r", "a later restart heals; down() is not terminal");
      assert.equal(bridge.down(), false);
    } finally {
      for (const m of mocks) { try { m.kill("SIGKILL"); } catch {} }
      rmSync(tmp, { recursive: true, force: true });
    }
  });
});

test("bridge drains stale bytes a dead container left in rep.fifo before retrying", async () => {
  const { tmp, app, bdir, mocks, spawnMock } = setup();
  const lingerPath = join(tmp, "mock-linger.mjs");
  const mockPath = join(tmp, "mock.mjs");
  writeFileSync(lingerPath, MOCK_LINGER);
  writeFileSync(mockPath, MOCK);
  await withEnv(bdir, 2500, async () => {
    try {
      // Container A serves one call, then leaves a bogus frame header in rep.fifo
      // and lingers holding the rep write end so the garbage survives the reopen.
      spawnMock(lingerPath, []);
      const { loadPluginContainerSync } = await import(bridgeUrl);
      const bridge = loadPluginContainerSync(app, { command: "serve", environment: "ssr" });
      assert.ok(bridge, "bridge constructed");

      assert.equal(bridge.resolveId("x", undefined), "OK:A:resolveId:x");

      // Container B comes up after a delay. The bridge's next write EPIPEs (A's
      // req reader is gone) -> reconnect -> drainRep discards A's leftover garbage
      // -> retry against B. Without the drain the bogus 0xffffffff header would
      // misframe the reply and this returns null (or throws), not B's result.
      spawnMock(mockPath, ["B", "5", "400"]);
      assert.equal(
        bridge.resolveId("y", undefined),
        "OK:B:resolveId:y",
        "drained the stale frame and recovered against the restarted container",
      );
      assert.equal(bridge.down(), false);
    } finally {
      for (const m of mocks) { try { m.kill("SIGKILL"); } catch {} }
      rmSync(tmp, { recursive: true, force: true });
    }
  });
});
