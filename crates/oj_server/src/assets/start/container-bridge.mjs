// SPDX-License-Identifier: MIT

// Synchronous facade over the Vite plugin container, for use inside
// module.registerHooks() hooks (which must return values, not promises).
// The container lives in the oj plugin-host process (plugin-host.mjs), which
// evaluates the vite config once for all environments. Each call writes a
// length-prefixed JSON frame to a FIFO and blocks in fs.readSync for the
// framed reply. Connection is lazy: a full-cache-hit boot never touches the
// FIFOs; only the first plugin-served miss blocks until the host is up.

import { closeSync, constants, existsSync, openSync, readSync, writeSync } from "node:fs";
import { join } from "node:path";
import { findConfig } from "./vite-plugin-bridge.mjs";

const sleeper = new Int32Array(new SharedArrayBuffer(4));
const sleep = (ms) => { Atomics.wait(sleeper, 0, 0, ms); };

export function loadPluginContainerSync(app, _opts) {
  if (!findConfig(app)) return null;
  // Decided in Rust (ssr_bridge_dir: OS temp dir, or the OJ_SSR_BRIDGE_DIR
  // override) and passed down when the runner spawns. Never a path inside the
  // app tree: source scanners that walk it block forever opening a FIFO.
  const dir = process.env.OJ_SSR_BRIDGE_DIR;
  if (!dir) return null;
  const reqPath = join(dir, "req.fifo");
  const repPath = join(dir, "rep.fifo");
  let state = "idle"; // idle -> up | down
  let reqFd = -1;
  let repFd = -1;
  let seq = 0;
  const seen = new Set();

  function connect() {
    // Config eval + buildStart can legitimately take this long on a cold
    // cache (same horizon the worker-based bridge used).
    const deadline = Date.now() + 300_000;
    for (;;) {
      if (existsSync(join(dir, "disabled"))) {
        state = "down";
        return false;
      }
      // A nonblocking write-open succeeds only once the host holds the read
      // end, so this doubles as the host-liveness probe; the real descriptors
      // are then opened blocking (writes must not short-read large frames).
      try {
        closeSync(openSync(reqPath, constants.O_WRONLY | constants.O_NONBLOCK));
        break;
      } catch {}
      if (Date.now() > deadline) {
        state = "down";
        throw new Error("oj: SSR plugin bridge: plugin host not ready after 300s");
      }
      sleep(25);
    }
    reqFd = openSync(reqPath, "w");
    repFd = openSync(repPath, "r");
    state = "up";
    return true;
  }

  function readExact(len) {
    const buf = Buffer.allocUnsafe(len);
    let off = 0;
    while (off < len) {
      let n = 0;
      try {
        n = readSync(repFd, buf, off, len - off, null);
      } catch (e) {
        if (e.code === "EAGAIN" || e.code === "EINTR") { sleep(1); continue; }
        state = "down";
        throw e;
      }
      if (n === 0) {
        state = "down";
        throw new Error("oj: SSR plugin bridge closed (plugin host exited)");
      }
      off += n;
    }
    return buf;
  }

  function call(method, args) {
    if (state === "down") return null;
    const id = ++seq;
    const first = !!process.env.OJ_BOOT_PHASES
      && (process.env.OJ_BOOT_PHASES === "2" || !seen.has(method));
    if (first) {
      seen.add(method);
      process.stderr.write(`[oj-phase] ${Date.now()} bridge: ${method}#${id} (${String(args[0] ?? "").slice(-80)})\n`);
    }
    if (state === "idle" && !connect()) return null;
    const json = Buffer.from(JSON.stringify({ id, method, args }));
    const frame = Buffer.allocUnsafe(4 + json.length);
    frame.writeUInt32LE(json.length, 0);
    json.copy(frame, 4);
    let off = 0;
    while (off < frame.length) off += writeSync(reqFd, frame, off, frame.length - off);
    for (;;) {
      const head = readExact(4);
      const m = JSON.parse(readExact(head.readUInt32LE(0)).toString("utf8"));
      if (m.id !== id) continue;
      if (first) process.stderr.write(`[oj-phase] ${Date.now()} bridge: ${method}#${id} returned\n`);
      if (m.error != null) throw new Error(m.error);
      return m.value ?? null;
    }
  }

  return {
    resolveId: (id, importer) => call("resolveId", [id, importer]),
    load: (id) => call("load", [id]),
    transform: (code, id) => call("transform", [code, id]),
    transformUserCode: (code, id) => call("transformUserCode", [code, id]),
    env: () => call("__env", []),
    // Diagnostics must not force a connect (that would put host readiness on
    // the full-hit warm path when OJ_SSR_MEM_STATS is on).
    heap: () => (state === "up" ? call("__heap", []) : null),
    // Nonblocking bootstrap probe (cross-process analog of an in-worker
    // bootstrap-done flag): the host writes `ready` after the SSR container's
    // buildStart completes.
    bootstrapDone: () => existsSync(join(dir, "ready")),
  };
}
