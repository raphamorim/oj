// SPDX-License-Identifier: MIT

// Synchronous facade over the async Vite plugin container, for use inside
// module.registerHooks() hooks (which must return values, not promises).
// The container itself runs in a worker (container-host.mjs); each call here
// posts a request and blocks on Atomics.wait until the worker replies. The
// main thread never waits for worker bootstrap itself — only a call issued
// before the container is ready blocks until bootstrap (incl. buildStart)
// completes, so cache-hit boots pay nothing for config eval.

import { Worker, MessageChannel, receiveMessageOnPort } from "node:worker_threads";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { findConfig } from "./vite-plugin-bridge.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));

export function loadPluginContainerSync(app, opts) {
  if (!findConfig(app)) return null;
  const { port1, port2 } = new MessageChannel();
  const sab = new SharedArrayBuffer(4);
  const flag = new Int32Array(sab);
  const worker = new Worker(join(HERE, "container-host.mjs"), {
    workerData: { app, opts, port: port2, sab },
    transferList: [port2],
  });
  // Neither the worker nor the port may keep the runner alive after stdin
  // closes; requests are driven synchronously so no listener is needed.
  worker.unref();
  port1.unref();
  let seq = 0;
  function call(method, args) {
    const id = ++seq;
    port1.postMessage({ id, method, args });
    // buildStart can legitimately block on the cross-process lock in
    // vite-plugin-bridge.mjs (stolen after 300s), so match that horizon.
    const deadline = Date.now() + 300_000;
    for (;;) {
      Atomics.wait(flag, 0, 0, 200);
      Atomics.store(flag, 0, 0);
      const m = receiveMessageOnPort(port1);
      if (m) {
        if (m.message.id !== id) continue;
        if (m.message.error != null) throw new Error(m.message.error);
        return m.message.value;
      }
      if (Date.now() > deadline) throw new Error(`oj: SSR plugin container timed out (${method})`);
    }
  }
  return {
    resolveId: (id, importer) => call("resolveId", [id, importer]),
    load: (id) => call("load", [id]),
    transform: (code, id) => call("transform", [code, id]),
    transformUserCode: (code, id) => call("transformUserCode", [code, id]),
  };
}
