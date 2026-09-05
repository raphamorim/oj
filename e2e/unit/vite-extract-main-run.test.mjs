// SPDX-License-Identifier: MIT

// The extractor only does anything when it is the entry module, and it decides
// that by comparing import.meta.url against argv[1]. Node canonicalizes the
// entry, so import.meta.url is symlink-free while argv[1] is whatever the
// caller typed. Any symlink in that path -- /var on macOS, a build system
// pointing the cache somewhere linked -- makes the two disagree.
//
// Getting it wrong is silent by construction: the body is skipped, nothing is
// written to stdout, and the process exits 0, which the caller cannot tell
// apart from a config that genuinely had nothing in it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { asset } from "./harness.mjs";

// argv[2] names a config that does not exist, so the run fails immediately --
// what is under test is whether it runs at all, not what it extracts.
function runVia(dir) {
  return execFileSync(
    process.execPath,
    [join(dir, "vite-extract.mjs"), join(dir, "no-such.config.mjs"), dir],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
}

test("the extractor runs when its path reaches it through a symlink", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-"));
  try {
    const real = join(base, "real");
    mkdirSync(real);
    copyFileSync(asset("vite-extract.mjs"), join(real, "vite-extract.mjs"));
    symlinkSync(real, join(base, "link"), "dir");

    // Through the real path it writes "{}" on a failed load; through the
    // symlink it has to do the same rather than exit silently.
    assert.equal(runVia(real), "{}");
    assert.equal(runVia(join(base, "link")), "{}");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// End to end through the extractor entry: a config whose plugin declares a
// dev-runtime environment from its `config` hook (Vite's declaration
// mechanism, as @cloudflare/vite-plugin does) marks the ssr environment
// runner-backed (`ssr.runnerBacked`), and the RAW top-level `resolve` block
// travels as `rawResolve` — the two inputs the Node SSR consumers select
// conditions from.
test("the extractor emits ssr.runnerBacked and rawResolve", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-rb-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "rb-app", type: "module" }));
    // A raw plugin list nests, as a plugin factory's return does.
    writeFileSync(
      join(base, "vite.config.mjs"),
      `export default {
        plugins: [[
          { name: "vite-plugin-cloudflare" },
          { name: "vite-plugin-cloudflare:config",
            config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }) },
        ]],
        resolve: { conditions: ["custom"], externalConditions: ["custom-ext"] },
        ssr: { target: "node" },
      };\n`,
    );
    const run = (config) => JSON.parse(execFileSync(
      process.execPath,
      [join(base, "vite-extract.mjs"), join(base, config), base],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    ));
    const out = run("vite.config.mjs");
    assert.equal(out.__ok, true);
    assert.equal(out.ssr.runnerBacked, true);
    assert.equal(out.ssr.target, "node");
    assert.deepEqual(out.rawResolve, { conditions: ["custom"], externalConditions: ["custom-ext"] });

    // Without the plugin (and no custom createEnvironment) nothing is marked.
    writeFileSync(
      join(base, "plain.config.mjs"),
      'export default { plugins: [{ name: "react" }], ssr: { target: "node" } };\n',
    );
    const plain = run("plain.config.mjs");
    assert.equal(plain.__ok, true);
    assert.equal(plain.ssr.runnerBacked, undefined);
    assert.equal(plain.rawResolve, null);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
