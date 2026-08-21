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

  function call(method, args) {
    if (state === "down") return null;
    if (state === "idle" && !connect()) return null;
    const id = ++seq;
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
    heap: () => (state === "up" ? call("__heap", []) : null),
    bootstrapDone: () => existsSync(join(dir, "ready")),
  };
}
