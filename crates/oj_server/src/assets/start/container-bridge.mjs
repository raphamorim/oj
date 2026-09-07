// SPDX-License-Identifier: MIT


import { closeSync, constants, existsSync, openSync, readSync, writeSync } from "node:fs";
import { join } from "node:path";
import { findConfig } from "./vite-plugin-bridge.mjs";

const sleeper = new Int32Array(new SharedArrayBuffer(4));
const sleep = (ms) => { Atomics.wait(sleeper, 0, 0, ms); };

export function loadPluginContainerSync(app, _opts) {
  if (!findConfig(app)) return null;
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
    const deadline = Date.now() + 300_000;
    for (;;) {
      if (existsSync(join(dir, "disabled"))) {
        state = "down";
        return false;
      }
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

  // Reopen the fifos to a RESTARTED container. Bounded (a healthy plugin-host
  // restart re-creates the fifos within seconds); returns false if the container
  // does not come back in the window, so a truly-dead container can't hang a
  // request the way connect()'s 300s boot deadline would.
  const RECONNECT_MS = Number(process.env.OJ_SSR_BRIDGE_RECONNECT_MS) || 30_000;
  function reconnect() {
    const deadline = Date.now() + RECONNECT_MS;
    for (;;) {
      if (existsSync(join(dir, "disabled"))) return false;
      try {
        closeSync(openSync(reqPath, constants.O_WRONLY | constants.O_NONBLOCK));
        break;
      } catch {}
      if (Date.now() > deadline) return false;
      sleep(25);
    }
    try {
      reqFd = openSync(reqPath, "w");
      repFd = openSync(repPath, "r");
    } catch {
      return false;
    }
    state = "up";
    return true;
  }

  function closeFds() {
    try { if (reqFd >= 0) closeSync(reqFd); } catch {}
    try { if (repFd >= 0) closeSync(repFd); } catch {}
    reqFd = -1;
    repFd = -1;
  }

  // Send `frame` and read its reply. A container DEATH (write EPIPE, or the
  // reply pipe closing in readExact) sets state="down" and throws; a plugin
  // error (`m.error`) throws WITHOUT touching state, so callers can tell a
  // recoverable death from a real error.
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
      const m = JSON.parse(readExact(head.readUInt32LE(0)).toString("utf8"));
      if (m.id !== id) continue;
      if (m.error != null) throw new Error(m.error);
      return m.value ?? null;
    }
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
    // Send/receive, reconnecting ONCE to a restarted container on a death and
    // retrying, so a transient plugin-host restart RECOVERS (returns the real
    // result) instead of crashing (the old unguarded EPIPE) or degrading forever
    // (a permanent "down" a later restart never heals). Only a death (state ->
    // "down") is retried; a plugin `m.error` propagates unchanged. A container
    // that stays gone past reconnect()'s window lands "down" and returns null.
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
