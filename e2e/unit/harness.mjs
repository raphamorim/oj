// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Shared test harness for driving oj's Node sidecars from `node --test`.
// Two shapes, matching how the Rust side spawns them:
//   - runSidecar:  one-shot `node <sidecar> <jsonArg>` -> parsed stdout JSON
//                  (optimize-deps.mjs)
//   - rpcSidecar:  long-lived newline-delimited JSON RPC over stdin/stdout
//                  (plugin-host.mjs, runner.mjs)

import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import readline from "node:readline";

const here = path.dirname(fileURLToPath(import.meta.url));
export const repo = path.join(here, "..", "..");
export const asset = (rel) => path.join(repo, "crates/oj_server/src/assets", rel);

const esbuildSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild");
export const hasEsbuildFixture = () => fs.existsSync(esbuildSrc);

// Wrap `node:test`'s `test` so cases that need the pre-bundler's esbuild skip
// (rather than hard-fail) when the start-app fixture has no node_modules, the
// same convention optimize-deps.test.mjs uses.
export function testWithEsbuild(test) {
  return hasEsbuildFixture()
    ? test
    : (name, fn) => test(name, { skip: "fixture esbuild not installed" }, () => {});
}

// A throwaway project dir with a node_modules/ and package.json. `linkEsbuild`
// symlinks the fixture's real esbuild (+ @esbuild binary) in, so the optimizer
// resolves it without a per-test install.
export function tmpProject({ prefix = "oj-fx-", pkgJson = { name: "fx" }, linkEsbuild = false } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify(pkgJson));
  if (linkEsbuild) {
    fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
    const scoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
    if (fs.existsSync(scoped)) fs.symlinkSync(scoped, path.join(root, "node_modules", "@esbuild"));
  }
  return {
    root,
    // Write a fake dependency under node_modules/<name>/.
    pkg(name, main, files) {
      const dir = path.join(root, "node_modules", name);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ name, version: "1.0.0", main }));
      for (const [f, c] of Object.entries(files)) fs.writeFileSync(path.join(dir, f), c);
    },
    // Write a file relative to the project root, creating parent dirs.
    write(rel, content) {
      const p = path.join(root, rel);
      fs.mkdirSync(path.dirname(p), { recursive: true });
      fs.writeFileSync(p, content);
    },
    cleanup() {
      fs.rmSync(root, { recursive: true, force: true });
    },
  };
}

// Shape A: run a one-shot sidecar as `node <sidecar> <jsonConfig>` and return
// its parsed stdout JSON. Throws on a non-zero exit; the thrown error carries
// `.stdout`/`.stderr`/`.status` for error-path assertions.
export function runSidecar(sidecarRel, config, { cwd, timeout = 30_000 } = {}) {
  const out = execFileSync("node", [asset(sidecarRel), JSON.stringify(config)], {
    cwd,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout,
    maxBuffer: 64 * 1024 * 1024,
  });
  return JSON.parse(out);
}

// Shape B: spawn a long-lived sidecar and speak newline-delimited JSON. `send`
// writes one frame and resolves with the next stdout frame (parsed); a frame
// that isn't the reply you expect is returned as-is, so callers that trigger
// host->driver requests can dispatch on it. Always `close()` in a finally.
export function rpcSidecar(sidecarRel, { args = [], env, cwd } = {}) {
  const child = spawn("node", [asset(sidecarRel), ...args], {
    cwd,
    env: env ? { ...process.env, ...env } : process.env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const frames = [];
  let waiter = null;
  readline.createInterface({ input: child.stdout }).on("line", (line) => {
    if (!line.trim()) return;
    if (waiter) {
      const w = waiter;
      waiter = null;
      w(line);
    } else {
      frames.push(line);
    }
  });
  let stderr = "";
  child.stderr.on("data", (d) => {
    stderr += d.toString();
  });

  const nextLine = (ms) =>
    new Promise((res, rej) => {
      if (frames.length) return res(frames.shift());
      const to = setTimeout(() => rej(new Error(`sidecar rpc timeout after ${ms}ms; stderr:\n${stderr}`)), ms);
      waiter = (line) => {
        clearTimeout(to);
        res(line);
      };
    });

  return {
    child,
    stderr: () => stderr,
    async nextFrame(ms = 10_000) {
      return JSON.parse(await nextLine(ms));
    },
    async send(msg, ms = 10_000) {
      child.stdin.write(JSON.stringify(msg) + "\n");
      return JSON.parse(await nextLine(ms));
    },
    close() {
      try {
        child.stdin.end();
        child.kill("SIGKILL");
      } catch {
        // already gone
      }
    },
  };
}
