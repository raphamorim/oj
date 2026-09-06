// SPDX-License-Identifier: MIT
//
// The SSR loader must not crash on the `cloudflare:` URL scheme. In a worker
// build `cloudflare:*` stays external and workerd provides it; in oj's Node SSR
// loader there is no workerd, so Node's default ESM loader throws
// ERR_UNSUPPORTED_ESM_URL_SCHEME and takes the dev server down. The loader
// aliases `cloudflare:workers` to a stub whose `env` is backed by the app's
// wrangler vars, and stubs any other `cloudflare:*` (or unsupported scheme) as
// an empty module.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const loader = resolve(here, "../../crates/oj_server/src/assets/start/loader.mjs");

test("SSR loader stubs cloudflare:workers (env from wrangler) and never crashes on the cloudflare: scheme", () => {
  const app = mkdtempSync(join(tmpdir(), "oj-cf-scheme-"));
  try {
    writeFileSync(join(app, "package.json"), JSON.stringify({ name: "cf-scheme-app", type: "module" }));
    // The loader imports rolldown/experimental for its transform (see the
    // ssr-loader asset tests); provide a stub so the hooks can load.
    const rolldown = join(app, "node_modules", "rolldown");
    mkdirSync(rolldown, { recursive: true });
    writeFileSync(
      join(rolldown, "package.json"),
      JSON.stringify({ name: "rolldown", type: "module", exports: { "./experimental": "./experimental.mjs" } }),
    );
    writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");
    writeFileSync(
      join(app, "wrangler.jsonc"),
      JSON.stringify({ name: "cf-scheme", compatibility_date: "2025-09-01", vars: { EDITION: "unit-edition" } }),
    );

    const entry = join(app, "entry.mjs");
    writeFileSync(entry, [
      'import { env, WorkerEntrypoint } from "cloudflare:workers";',
      // An unhandled cloudflare:* module: must resolve to an empty stub, not
      // crash the process.
      'import sockets from "cloudflare:sockets";',
      "export default {",
      "  edition: env.EDITION,",
      "  isClass: typeof WorkerEntrypoint === 'function',",
      "  sockets: sockets && typeof sockets,",
      "};",
    ].join("\n"));

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
      env: { ...process.env, OJ_APP_ROOT: app, OJ_CACHE_ROOT: join(app, "cache"), OJ_SSR_LOADER_CACHE: "off" },
    });

    assert.equal(result.status, 0, result.stderr || result.error?.message);
    const out = JSON.parse(result.stdout);
    assert.equal(out.edition, "unit-edition");
    assert.equal(out.isClass, true);
    // The unknown cloudflare:* module resolved to an empty-object default.
    assert.equal(out.sockets, "object");
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});
