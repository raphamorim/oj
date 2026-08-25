// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The plugin host must accept `this.emitFile({ type: "chunk" })` from a transform
// hook, mint a reference id, report the request back to Rust (which forwards it
// to rolldown), and later answer `this.getFileName(refId)` with the hashed name
// seeded via `seedChunkNames`.
test("plugin-host forwards emitFile chunk requests and resolves getFileName after seeding", async () => {
  const fx = tmpProject({ prefix: "oj-chunk-" });
  fx.write(
    "oj.plugins.mjs",
    `let ref;
     export default [{
       name: "emits-chunk",
       transform(code, id) {
         if (id.endsWith("entry.js")) {
           ref = this.emitFile({ type: "chunk", id: "/src/content.js", name: "content" });
           return null;
         }
         if (id.endsWith("probe.js")) {
           return "export const name = " + JSON.stringify(this.getFileName(ref)) + ";";
         }
         return null;
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    // A chunk emitted during transform is reported back, with the id/name kept.
    const t1 = await host.send({
      id: 1,
      hook: "transform",
      args: ["let a = 1;", path.join(fx.root, "entry.js")],
    });
    const out1 = JSON.parse(t1.result);
    assert.equal(out1.emittedChunks.length, 1, "the emitted chunk is reported");
    assert.equal(out1.emittedChunks[0].id, "/src/content.js");
    assert.equal(out1.emittedChunks[0].name, "content");
    const ref = out1.emittedChunks[0].referenceId;
    assert.match(ref, /^oj-chunk-/, "a chunk reference id was minted");

    // Rust seeds the resolved hashed name for that reference id.
    await host.send({
      id: 2,
      hook: "seedChunkNames",
      args: [JSON.stringify({ [ref]: "assets/content-abc123.js" })],
    });

    // getFileName(refId) now resolves to the seeded name.
    const t2 = await host.send({
      id: 3,
      hook: "transform",
      args: ["", path.join(fx.root, "probe.js")],
    });
    const out2 = JSON.parse(t2.result);
    assert.match(out2.code, /assets\/content-abc123\.js/, "getFileName returns the seeded hashed name");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// Chunks emitted from buildStart (how @crxjs emits its manifest root) are
// reported back too, and buildStart receives an options object with `input`.
test("plugin-host reports chunks emitted from buildStart", async () => {
  const fx = tmpProject({ prefix: "oj-chunk-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "bs-emitter",
       buildStart(options) {
         if (typeof options.input !== "undefined") {
           this.emitFile({ type: "chunk", id: "/src/background.js", name: "background" });
         }
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "buildStart", args: [] });
    const out = JSON.parse(res.result);
    assert.equal(out.emittedChunks.length, 1, "the buildStart-emitted chunk is reported");
    assert.equal(out.emittedChunks[0].id, "/src/background.js");
    assert.equal(out.emittedChunks[0].name, "background");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// emitFile still rejects an unknown descriptor type, and an asset emit keeps
// working (returns a reference id resolvable by getFileName).
test("plugin-host still supports asset emits and rejects unknown emit types", async () => {
  const fx = tmpProject({ prefix: "oj-chunk-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "emits",
       transform(code, id) {
         if (id.endsWith("bad.js")) {
           try { this.emitFile({ type: "prefetch" }); return "export const ok = false;"; }
           catch { return "export const ok = true;"; }
         }
         if (id.endsWith("asset.js")) {
           const r = this.emitFile({ type: "asset", name: "note.txt", source: "hi" });
           return "export const name = " + JSON.stringify(this.getFileName(r)) + ";";
         }
         return null;
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const bad = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "bad.js")] });
    assert.match(JSON.parse(bad.result).code, /ok = true/, "unknown emit type throws");
    const asset = await host.send({ id: 2, hook: "transform", args: ["", path.join(fx.root, "asset.js")] });
    assert.match(JSON.parse(asset.result).code, /note\.txt/, "asset emit + getFileName still works");
  } finally {
    host.close();
    fx.cleanup();
  }
});
