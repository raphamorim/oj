// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The plugin host pushes `{ ojServeInfo: { middlewarePort, runnerEnvironments } }`
// on stdout the moment its top-level init completes. The RPC listener only
// registers after every top-level await, so on a slow boot (many plugins, a
// Miniflare boot inside configureServer) the Rust side's getServeInfo RPC times
// out — the push is what lets it activate the middleware path late instead of
// silently serving every document from the Node SSR runner. These tests pin:
// the push exists, it carries the middleware port, and it is emitted BEFORE any
// RPC is answered (so an answered RPC implies the push already arrived — the
// ordering Rust's serve_info fast path relies on).

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

test("a delayed configureServer still yields the ojServeInfo push, ahead of any RPC reply", async () => {
  const fx = tmpProject({ prefix: "oj-serveinfo-push-" });
  // configureServer stalls like a heavy boot (plugin fleets, Miniflare): the
  // host must not answer RPCs until it finishes, and the push must come first.
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "slow-middleware",
       async configureServer(server) {
         await new Promise((r) => setTimeout(r, 600));
         server.middlewares.use((req, res, next) => next());
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    // Sent while the host is still mid-init, like Rust's boot-time RPCs.
    const reply = await host.send({ id: 7, hook: "getServeInfo" }, 20_000);
    assert.equal(reply.id, 7);

    // The push landed before the RPC was answered (the harness consumes it
    // out-of-band, so an answered RPC with no recorded push means it never
    // came, or came late).
    const pushed = host.serveInfoPushed();
    assert.ok(pushed, "the ojServeInfo push must precede the first RPC reply");
    assert.equal(typeof pushed.middlewarePort, "number", "the push carries the middleware port");
    assert.equal(pushed.runnerEnvironments, false);
    assert.equal(JSON.parse(reply.result).middlewarePort, pushed.middlewarePort);
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("with no middleware registered the push still arrives, with a null port", async () => {
  const fx = tmpProject({ prefix: "oj-serveinfo-none-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{ name: "no-middleware", transform: (code) => null }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const pushed = await host.serveInfo();
    assert.equal(pushed.middlewarePort, null);
    assert.equal(pushed.runnerEnvironments, false);
  } finally {
    host.close();
    fx.cleanup();
  }
});
