// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The plugin host must pass a Vite-shaped HmrContext ({ file, timestamp, type,
// modules, read }) to handleHotUpdate and honor a returned filtered module list.
// Before, the ctx was only { file, timestamp } and a non-empty array was ignored.
test("plugin-host enriches the handleHotUpdate ctx and honors a filtered return", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [{
      name: "hmr",
      async handleHotUpdate(ctx) {
        const content = await ctx.read();
        const ok =
          ctx.type === "update" &&
          typeof ctx.timestamp === "number" &&
          Array.isArray(ctx.modules) &&
          ctx.modules[0] && ctx.modules[0].url === "/a.tsx" &&
          ctx.modules[0].isSelfAccepting === true &&
          content.includes("export const x");
        // Only narrow to the module set when the whole ctx was delivered; a
        // wrong ctx forces a full-reload, which the test detects as a failure.
        return ok ? ctx.modules : "full-reload";
      },
    }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const modules = JSON.stringify([{ url: "/a.tsx", isSelfAccepting: true, importers: ["/b.tsx"] }]);
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", modules],
    });
    assert.equal(res.id, 1);
    const parsed = JSON.parse(res.result);
    assert.equal(parsed.action, "filter", "the ctx was fully delivered and the return honored");
    assert.deepEqual(parsed.modules, ["/a.tsx"], "returned module urls forwarded as the filter set");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// A plugin returning an empty array suppresses HMR entirely (Vite: no modules
// to update). The host must surface that as "skip", not a filter or a reload.
test("plugin-host treats an empty handleHotUpdate array as a suppressed update", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [{
      name: "hmr-suppress",
      handleHotUpdate() {
        return [];
      },
    }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", "[]"],
    });
    assert.equal(res.result, "skip", "empty array suppresses the update");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// Vite folds the return across plugins: each returned array becomes the next
// plugin's ctx.modules and the FINAL set decides. An earlier empty array must
// not latch a suppression that a later plugin overrides.
test("plugin-host folds handleHotUpdate returns so the last plugin decides", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [
      { name: "empty-first", handleHotUpdate() { return []; } },
      { name: "narrow-last", handleHotUpdate() { return [{ url: "/a.tsx" }]; } },
    ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", "[]"],
    });
    const parsed = JSON.parse(res.result);
    assert.equal(parsed.action, "filter", "a later non-empty return overrides an earlier empty one");
    assert.deepEqual(parsed.modules, ["/a.tsx"]);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The mirror of the fold: a later empty array overrides an earlier filter and
// suppresses the update.
test("plugin-host lets a later empty handleHotUpdate return suppress an earlier filter", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [
      { name: "narrow-first", handleHotUpdate() { return [{ url: "/a.tsx" }]; } },
      { name: "empty-last", handleHotUpdate() { return []; } },
    ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", "[]"],
    });
    assert.equal(res.result, "skip", "the last plugin's empty array wins");
  } finally {
    host.close();
    fx.cleanup();
  }
});
