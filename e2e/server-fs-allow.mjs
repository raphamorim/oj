// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies server.fs.allow: a file outside the project root (e.g. a monorepo
// shared package) is denied over /@fs unless the config allow-lists its dir.
// Without server.fs, oj only serves import-reached files — so a bare /@fs dial
// to an outside path 403s; with server.fs.allow it is served.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const OJ = path.join(process.cwd(), "target", "debug", "oj");

const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fsallow-"));
const app = path.join(workspace, "app");
const shared = path.join(workspace, "shared");
const sharedFile = path.join(shared, "util.js");

let failed = false;
let child;
const startApp = async (port, viteConfig) => {
  fs.rmSync(app, { recursive: true, force: true });
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"fsallow","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const v = 1;\n");
  if (viteConfig) fs.writeFileSync(path.join(app, "vite.config.mjs"), viteConfig);
  const c = spawn(OJ, ["dev", "--port", String(port)], { cwd: app, stdio: "ignore" });
  for (let i = 0; i < 300; i++) {
    try { if ((await fetch(`http://localhost:${port}/`)).ok) return c; } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  c.kill("SIGKILL");
  throw new Error("dev server did not start");
};

try {
  fs.mkdirSync(shared, { recursive: true });
  fs.writeFileSync(sharedFile, "export const shared = 42;\n");
  const fsUrl = (port) => `http://localhost:${port}/@fs${sharedFile}`;

  // Case A: no server.fs -> outside file denied.
  child = await startApp(5333, null);
  const denied = await fetch(fsUrl(5333));
  if (denied.status !== 403) throw new Error(`outside /@fs should be 403 without allow, got ${denied.status}`);
  child.kill("SIGKILL");
  child = null;
  console.log("without allow:  /@fs outside root -> 403");

  // Case B: server.fs.allow the shared dir -> served.
  const cfg = `export default { server: { fs: { allow: [${JSON.stringify(shared)}] } } };\n`;
  child = await startApp(5334, cfg);
  const ok = await fetch(fsUrl(5334));
  if (ok.status !== 200) throw new Error(`allow-listed /@fs should be 200, got ${ok.status}`);
  const body = await ok.text();
  if (!body.includes("42")) throw new Error(`served /@fs file missing its content:\n${body}`);
  console.log("with allow:     /@fs outside root -> 200");
  console.log("\nserver.fs.allow VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(workspace, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
