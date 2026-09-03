// SPDX-License-Identifier: MIT

// `getCloudflareContext()` inside the Worker bundle: the bindings and execution
// context the runtime handed to `fetch(request, env, ctx)` (captured by oj's
// server entry), no filesystem, no process.

const NOOP_CTX = { waitUntil() {}, passThroughOnException() {} };

export async function getCloudflareContext() {
  return {
    env: globalThis.__OJ_CF_ENV ?? {},
    cf: {},
    ctx: globalThis.__OJ_CF_CTX ?? NOOP_CTX,
  };
}

export default { getCloudflareContext };
