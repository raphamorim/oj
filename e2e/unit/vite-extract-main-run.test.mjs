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
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
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
