// SPDX-License-Identifier: MIT


import { closeSync, constants, existsSync, openSync, readSync, writeSync } from "node:fs";
import { join } from "node:path";
import { findConfig } from "./vite-plugin-bridge.mjs";

const sleeper = new Int32Array(new SharedArrayBuffer(4));
const sleep = (ms) => { Atomics.wait(sleeper, 0, 0, ms); };

// A reply frame is a transformed module; cap the length we will trust so a
// desynced or garbage 4-byte header can't drive a multi-GB allocUnsafe. Real
// modules are orders of magnitude below this.
const MAX_FRAME = 256 * 1024 * 1024;

export function loadPluginContainerSync(app, _opts) {
  if (!findConfig(app)) return null;
  const dir = process.env.OJ_SSR_BRIDGE_DIR;
  if (!dir) return null;
  const reqPath = join(dir, "req.fifo");
  const repPath = join(dir, "rep.fifo");
  // idle -> up | down. "down" is NOT terminal: a later call re-probes and can
  // heal from a container that restarts after the reconnect window elapsed.
  let state = "idle";
  let reqFd = -1;
  let repFd = -1;
  let seq = 0;
  const seen = new Set();

  // Non-blocking liveness probe: opening the request fifo O_WRONLY|O_NONBLOCK
  // succeeds only when a container currently holds its read end (else ENXIO).
  // Instant either way, so it never stalls the synchronous render thread.
  function probe() {
    try {
      closeSync(openSync(reqPath, constants.O_WRONLY | constants.O_NONBLOCK));
      return true;
    } catch {
      return false;
    }
  }

  // Open both fifos NON-BLOCKING. The write-open succeeds only with a reader
  // present (we just probed); the read-open never blocks. This is what stops a
  // container that dies right after the probe from hanging the render thread on
  // a plain blocking openSync. Writes/reads then surface EAGAIN, which
  // sendRecv/readExact already spin on.
  function openFds() {
    reqFd = openSync(reqPath, constants.O_WRONLY | constants.O_NONBLOCK);
    repFd = openSync(repPath, constants.O_RDONLY | constants.O_NONBLOCK);
  }

  function closeFds() {
    try { if (reqFd >= 0) closeSync(reqFd); } catch {}
    try { if (repFd >= 0) closeSync(repFd); } catch {}
    reqFd = -1;
    repFd = -1;
  }

  // Discard any bytes a dead container left in rep.fifo before a retry, so a
  // stale or partial reply frame can't misframe the next read. Non-blocking:
  // repFd is O_NONBLOCK, so readSync returns EAGAIN (empty) and we stop.
  function drainRep() {
    const buf = Buffer.allocUnsafe(65536);
    for (;;) {
      let n = 0;
      try {
        n = readSync(repFd, buf, 0, buf.length, null);
      } catch (e) {
        if (e.code === "EINTR") continue;
        break; // EAGAIN => nothing buffered
      }
      if (n === 0) break;
    }
  }

  function connect() {
    const deadline = Date.now() + 300_000;
    for (;;) {
      if (existsSync(join(dir, "disabled"))) {
        state = "down";
        return false;
      }
      if (probe()) break;
      if (Date.now() > deadline) {
        state = "down";
        throw new Error("oj: SSR plugin bridge: plugin host not ready after 300s");
      }
      sleep(25);
    }
    openFds();
    state = "up";
    return true;
  }

  // Reopen the fifos to a RESTARTED container. Bounded: a healthy plugin-host
  // restart re-creates the fifos within a second or two, and every open here is
  // non-blocking, so a container that dies mid-window can't hang the request the
  // way connect()'s 300s boot deadline would. Returns false (caller fast-fails)
  // if no container appears in the window.
  const RECONNECT_MS = Number(process.env.OJ_SSR_BRIDGE_RECONNECT_MS) || 5_000;
  function reconnect() {
    const deadline = Date.now() + RECONNECT_MS;
    for (;;) {
      if (existsSync(join(dir, "disabled"))) return false;
      if (probe()) break;
      if (Date.now() > deadline) return false;
      sleep(25);
    }
    try {
      openFds();
      drainRep(); // toss any residue the dead container left before we retry
    } catch {
      closeFds();
      return false;
    }
    state = "up";
    return true;
  }

  function readExact(len) {
    if (len > MAX_FRAME) {
      // A bad length header means the stream is desynced; recover like a death
      // (reconnect+retry) rather than attempting a multi-GB allocation.
      state = "down";
      throw new Error("oj: SSR plugin bridge: framing desync (length " + len + ")");
    }
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

  // Send `frame` and read its reply. A container DEATH (write EPIPE, the reply
  // pipe closing, or a framing/JSON desync) sets state="down" and throws so
  // call() can reconnect+retry; a plugin error (`m.error`) throws WITHOUT
  // touching state, so callers can tell a recoverable death from a real error.
  function sendRecv(id, frame) {
    let off = 0;
    while (off < frame.length) {
      try {
        off += writeSync(reqFd, frame, off, frame.length - off);
      } catch (e) {
        if (e.code === "EAGAIN" || e.code === "EINTR") { sleep(1); continue; }
        state = "down";
        throw e;
      }
    }
    for (;;) {
      const head = readExact(4);
      const body = readExact(head.readUInt32LE(0)).toString("utf8");
      let m;
      try {
        m = JSON.parse(body);
      } catch (e) {
        // Garbage on the wire => the stream is out of frame; recover like a death.
        state = "down";
        throw e;
      }
      if (m.id !== id) continue;
      if (m.error != null) throw new Error(m.error);
      return m.value ?? null;
    }
  }

  function call(method, args) {
    const id = ++seq;
    const first = !!process.env.OJ_BOOT_PHASES
      && (process.env.OJ_BOOT_PHASES === "2" || !seen.has(method));
    if (first) {
      seen.add(method);
      process.stderr.write(`[oj-phase] ${Date.now()} bridge: ${method}#${id} (${String(args[0] ?? "").slice(-80)})\n`);
    }
    if (state === "idle" && !connect()) return null;
    if (state === "down") {
      // A prior call found the container gone. Re-probe cheaply (instant): if a
      // restarted container is back, reconnect and serve it (this heals a restart
      // that happened after the reconnect window elapsed); otherwise fast-fail
      // with null NOW instead of stalling the render thread on a dead container.
      if (!probe()) return null;
      closeFds();
      state = "idle";
      if (!reconnect()) { state = "down"; return null; }
    }
    const json = Buffer.from(JSON.stringify({ id, method, args }));
    const frame = Buffer.allocUnsafe(4 + json.length);
    frame.writeUInt32LE(json.length, 0);
    json.copy(frame, 4);
    // Reconnect ONCE to a restarted container on a death and retry, so a
    // transient plugin-host restart RECOVERS (returns the real result) instead
    // of crashing (the old unguarded EPIPE). Only a death (state -> "down") is
    // retried; a plugin `m.error` propagates unchanged. A container that stays
    // gone past reconnect()'s window lands "down" and returns null - and "down"
    // is not terminal, since the next call re-probes above.
    for (let reconnected = false; ; reconnected = true) {
      try {
        const v = sendRecv(id, frame);
        if (first) process.stderr.write(`[oj-phase] ${Date.now()} bridge: ${method}#${id} returned\n`);
        return v;
      } catch (e) {
        if (state !== "down") throw e;
        if (reconnected) return null;
        closeFds();
        state = "idle";
        if (!reconnect()) {
          state = "down";
          return null;
        }
      }
    }
  }

  return {
    resolveId: (id, importer) => call("resolveId", [id, importer]),
    load: (id) => call("load", [id]),
    transform: (code, id) => call("transform", [code, id]),
    transformUserCode: (code, id) => call("transformUserCode", [code, id]),
    env: () => call("__env", []),
    defines: () => call("__define", []),
    heap: () => (state === "up" ? call("__heap", []) : null),
    // The bridge could not serve this call (container gone past the reconnect
    // window): callers must NOT persist such a null into the loader cache, or a
    // one-off container blip poisons a module's transform across restarts.
    down: () => state === "down",
    bootstrapDone: () => existsSync(join(dir, "ready")),
  };
}
