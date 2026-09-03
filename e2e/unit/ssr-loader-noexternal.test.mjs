// SPDX-License-Identifier: MIT
//
// Vite runs `ssr.noExternal` dependencies through its plugin pipeline like app
// source (external.ts: noExternal -> Vite transforms). The TanStack SSR loader
// must therefore hand a noExternal dep's modules to the plugin container's
// load/transform hooks too, not only rewrite define/env/glob in them.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const loader = resolve(here, "../../crates/oj_server/src/assets/start/loader.mjs");

test("noExternal deps go through the plugin container's transform; externals do not", async () => {
  const app = realpathSync(mkdtempSync(join(tmpdir(), "oj-ssr-noext-")));
  let bridge;
  try {
    const source = join(app, "src");
    const channel = join(app, "bridge");
    const rolldown = join(app, "node_modules", "rolldown");
    const noext = join(app, "node_modules", "noext-dep");
    const ext = join(app, "node_modules", "ext-dep");
    for (const directory of [source, channel, rolldown, noext, ext]) mkdirSync(directory, { recursive: true });

    writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-noext-app", type: "module" }));
    // A config file is what makes the loader open the plugin bridge at all.
    writeFileSync(join(app, "vite.config.mjs"), "export default {};\n");
    writeFileSync(join(rolldown, "package.json"), JSON.stringify({
      name: "rolldown", type: "module", exports: { "./experimental": "./experimental.mjs" },
    }));
    writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");
    for (const [dir, name] of [[noext, "noext-dep"], [ext, "ext-dep"]]) {
      writeFileSync(join(dir, "package.json"), JSON.stringify({ name, type: "module", main: "./index.js" }));
      writeFileSync(join(dir, "index.js"), `export const name = ${JSON.stringify(name)};\nexport let viaPlugin = "untouched";\n`);
    }
    const entry = join(source, "entry.ts");
    writeFileSync(entry, [
      'import * as a from "noext-dep";',
      'import * as b from "ext-dep";',
      "export default { noext: a.viaPlugin, ext: b.viaPlugin, names: [a.name, b.name] };",
    ].join("\n"));

    // A synthetic plugin bridge: transformUserCode marks every module it sees
    // under node_modules; load/resolveId claim nothing.
    const bridgeScript = [
      'import { openSync, writeSync, createReadStream, writeFileSync } from "node:fs";',
      'import { join } from "node:path";',
      "const [channel] = process.argv.slice(1);",
      'const request = openSync(join(channel, "req.fifo"), "r+");',
      'const response = openSync(join(channel, "rep.fifo"), "r+");',
      "let pending = Buffer.alloc(0);",
      "createReadStream(null, { fd: request, autoClose: false }).on(\"data\", (chunk) => {",
      "  pending = Buffer.concat([pending, chunk]);",
      "  while (pending.length >= 4 && pending.length >= pending.readUInt32LE(0) + 4) {",
      "    const size = pending.readUInt32LE(0);",
      "    const message = JSON.parse(pending.subarray(4, size + 4).toString());",
      "    pending = pending.subarray(size + 4);",
      "    let value = null;",
      '    if (message.method === "__define" || message.method === "__env") value = {};',
      '    else if (message.method === "transformUserCode" && String(message.args[1]).includes("/node_modules/")) {',
      '      value = message.args[0].replace(\'"untouched"\', \'"plugin-transformed"\');',
      "    }",
      "    const body = Buffer.from(JSON.stringify({ id: message.id, value }));",
      "    const frame = Buffer.alloc(4 + body.length);",
      "    frame.writeUInt32LE(body.length, 0); body.copy(frame, 4); writeSync(response, frame);",
      "  }",
      "});",
      'writeFileSync(join(channel, "ready"), "1");',
    ].join("\n");
    spawnSync("mkfifo", [join(channel, "req.fifo"), join(channel, "rep.fifo")]);
    bridge = spawn(process.execPath, ["--input-type=module", "--eval", bridgeScript, channel], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    let bridgeErr = "";
    bridge.stderr.on("data", (d) => (bridgeErr += d));
    const deadline = Date.now() + 5_000;
    while (!existsSync(join(channel, "ready")) && Date.now() < deadline) {
      await new Promise((next) => setTimeout(next, 10));
    }
    assert.equal(existsSync(join(channel, "ready")), true, "synthetic plugin bridge did not start");

    const runner = [
      'import { registerHooks } from "node:module";',
      `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
      "registerHooks({ resolve: loader.resolve, load: loader.load });",
      `process.stdout.write(JSON.stringify((await import(${JSON.stringify(pathToFileURL(entry).href)})).default));`,
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
        OJ_SSR_EXTERNALS: JSON.stringify({ noExternal: ["noext-dep"] }),
      },
    });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
    assert.deepEqual(JSON.parse(result.stdout), {
      noext: "plugin-transformed",
      ext: "untouched",
      names: ["noext-dep", "ext-dep"],
    }, `loader stderr:\n${result.stderr}\nbridge stderr:\n${bridgeErr}`);
  } finally {
    bridge?.kill("SIGKILL");
    rmSync(app, { recursive: true, force: true });
  }
});
