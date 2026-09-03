// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const loader = resolve(here, "../../crates/oj_server/src/assets/start/loader.mjs");

test("TanStack SSR applies global and server-specific Vite defines", async () => {
  const app = mkdtempSync(join(tmpdir(), "oj-ssr-defines-"));
  let bridge;

  try {
    const source = join(app, "src");
    const channel = join(app, "bridge");
    const rolldown = join(app, "node_modules", "rolldown");
    for (const directory of [source, channel, rolldown]) mkdirSync(directory, { recursive: true });

    writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-define-app", type: "module" }));
    writeFileSync(join(app, "vite.config.mjs"), [
      "export default {",
      '  define: { __SYNTHETIC_GLOBAL__: JSON.stringify("global value"), __SYNTHETIC_OVERRIDE__: "false" },',
      '  environments: { ssr: { define: { __SYNTHETIC_SERVER__: "true", __SYNTHETIC_OVERRIDE__: "true" } } },',
      "};",
    ].join("\n"));
    writeFileSync(join(rolldown, "package.json"), JSON.stringify({
      name: "rolldown", type: "module", exports: { "./experimental": "./experimental.mjs" },
    }));
    writeFileSync(join(rolldown, "experimental.mjs"), [
      "export function transformSync(_filename, code, options = {}) {",
      "  for (const [key, value] of Object.entries(options.define ?? {})) {",
      "    code = code.replaceAll(key, value);",
      "  }",
      "  return { code };",
      "}",
    ].join("\n"));

    const entry = join(source, "entry.ts");
    writeFileSync(entry, [
      "export default {",
      "  global: __SYNTHETIC_GLOBAL__,",
      "  server: __SYNTHETIC_SERVER__,",
      "  override: __SYNTHETIC_OVERRIDE__,",
      "};",
    ].join("\n"));

    const request = join(channel, "req.fifo");
    const response = join(channel, "rep.fifo");
    const created = spawnSync("mkfifo", [request, response], { encoding: "utf8" });
    assert.equal(created.status, 0, created.stderr || created.error?.message);

    const bridgeScript = [
      'import { createReadStream, openSync, writeFileSync, writeSync } from "node:fs";',
      'import { join } from "node:path";',
      'import { pathToFileURL } from "node:url";',
      "const [app, channel] = process.argv.slice(1);",
      'const config = (await import(pathToFileURL(join(app, "vite.config.mjs")).href)).default;',
      'const request = openSync(join(channel, "req.fifo"), "r+");',
      'const response = openSync(join(channel, "rep.fifo"), "r+");',
      "let pending = Buffer.alloc(0);",
      "createReadStream(null, { fd: request, autoClose: false }).on(\"data\", (chunk) => {",
      "  pending = Buffer.concat([pending, chunk]);",
      "  while (pending.length >= 4 && pending.length >= pending.readUInt32LE(0) + 4) {",
      "    const size = pending.readUInt32LE(0);",
      "    const message = JSON.parse(pending.subarray(4, size + 4).toString());",
      "    pending = pending.subarray(size + 4);",
      '    const value = message.method === "__define"',
      "      ? { ...config.define, ...config.environments.ssr.define }",
      '      : message.method === "__env" ? {} : null;',
      "    const body = Buffer.from(JSON.stringify({ id: message.id, value }));",
      "    const frame = Buffer.alloc(4 + body.length);",
      "    frame.writeUInt32LE(body.length, 0); body.copy(frame, 4); writeSync(response, frame);",
      "  }",
      "});",
      'writeFileSync(join(channel, "ready"), "1");',
    ].join("\n");
    bridge = spawn(process.execPath, ["--input-type=module", "--eval", bridgeScript, app, channel], {
      stdio: ["ignore", "ignore", "pipe"],
    });

    const deadline = Date.now() + 5_000;
    while (!existsSync(join(channel, "ready")) && Date.now() < deadline) {
      await new Promise((next) => setTimeout(next, 10));
    }
    assert.equal(existsSync(join(channel, "ready")), true, "synthetic plugin bridge did not start");

    const runner = [
      'import { registerHooks } from "node:module";',
      `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
      "registerHooks({ resolve: loader.resolve, load: loader.load });",
      `const result = await import(${JSON.stringify(pathToFileURL(entry).href)});`,
      "process.stdout.write(JSON.stringify(result.default));",
    ].join("\n");
    const result = spawnSync(process.execPath, ["--input-type=module", "--eval", runner], {
      encoding: "utf8",
      timeout: 10_000,
      env: {
        ...process.env,
        OJ_APP_ROOT: app,
        OJ_CACHE_ROOT: join(app, "cache"),
        OJ_SSR_BRIDGE_DIR: channel,
        OJ_SSR_LOADER_CACHE: "off",
      },
    });

    assert.equal(result.status, 0, result.stderr || result.error?.message);
    assert.deepEqual(JSON.parse(result.stdout), { global: "global value", server: true, override: true });
  } finally {
    bridge?.kill("SIGKILL");
    rmSync(app, { recursive: true, force: true });
  }
});
