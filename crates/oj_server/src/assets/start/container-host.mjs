// SPDX-License-Identifier: MIT

// Worker side of the sync plugin-container bridge (see container-bridge.mjs).
// Owns the async Vite plugin container: config eval, buildStart, and every
// plugin hook run here, off the main thread. Requests queue on the port until
// bootstrap finishes, so callers observe the same "buildStart before any
// load" ordering the in-worker loader had.

import { workerData } from "node:worker_threads";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";

const { app, opts, port, sab, envBase } = workerData;
const flag = new Int32Array(sab);

function respond(msg) {
  port.postMessage(msg);
  Atomics.store(flag, 0, 1);
  Atomics.notify(flag, 0);
}

let container = null;
try {
  container = await loadPluginContainer(app, opts);
} catch (e) {
  process.stderr.write(`oj: SSR plugin container failed: ${(e && (e.stack || e.message)) || e}\n`);
}
if (container) {
  try { await container.buildStart(); }
  catch (e) { process.stderr.write(`oj: SSR buildStart failed: ${(e && (e.stack || e.message)) || e}\n`); }
}

// Evaluating the config (and buildStart) mutates this worker's process.env
// copy — e.g. configs that derive VITE_* vars from git state. The loader
// fetches this delta to feed the same values into its define/env inlining.
const envDelta = {};
for (const [k, v] of Object.entries(process.env)) {
  if (envBase[k] !== v) envDelta[k] = v;
}

port.on("message", async ({ id, method, args }) => {
  if (method === "__env") return respond({ id, value: envDelta });
  if (method === "__heap") {
    const v8 = await import("node:v8");
    return respond({ id, value: v8.default.getHeapStatistics() });
  }
  if (!container) return respond({ id, value: null });
  try {
    respond({ id, value: await container[method](...args) });
  } catch (e) {
    respond({ id, error: String((e && e.stack) || e) });
  }
});
