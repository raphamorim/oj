// SPDX-License-Identifier: MIT
//
// Two Vite resolver/loader behaviors in the TanStack SSR loader: a JSON module
// exposes named exports for its identifier keys (json.namedExports, on by
// default), and the config's `resolve.conditions` (OJ_RESOLVE_CONDITIONS from
// oj) join Node's export conditions, so a package `exports` entry behind a
// custom condition resolves the way Vite's resolver picks it.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { jsonToEsm } from "../../crates/oj_server/src/assets/start/loader-util.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const loader = resolve(here, "../../crates/oj_server/src/assets/start/loader.mjs");

test("jsonToEsm emits named exports for identifier keys and keeps the default", () => {
  const out = jsonToEsm('{ "version": "1.2.3", "name": "pkg", "not-ident": 1, "default": 2, "class": 3 }');
  assert.match(out, /export const version = "1\.2\.3";/);
  assert.match(out, /export const name = "pkg";/);
  assert.doesNotMatch(out, /export const default\b/);
  assert.doesNotMatch(out, /export const class\b/);
  assert.match(out, /export default \{ version, name, "not-ident": 1, "default": 2, "class": 3 \};/);
  // Arrays and scalars: default export only.
  assert.equal(jsonToEsm("[1, 2]"), "export default [1, 2];");
  assert.equal(jsonToEsm("42"), "export default 42;");
  // Invalid JSON is left to the engine to report.
  assert.equal(jsonToEsm("{ nope"), "export default { nope;");
});

test("SSR loader: JSON named exports and resolve.conditions reach Node's resolver", () => {
  const app = realpathSync(mkdtempSync(join(tmpdir(), "oj-ssr-json-cond-")));
  const source = join(app, "src");
  const condpkg = join(app, "node_modules", "condpkg");
  const rolldown = join(app, "node_modules", "rolldown");
  for (const directory of [source, condpkg, rolldown]) mkdirSync(directory, { recursive: true });

  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-json-cond-app", type: "module" }));
  writeFileSync(join(rolldown, "package.json"), JSON.stringify({
    name: "rolldown", type: "module", exports: { "./experimental": "./experimental.mjs" },
  }));
  writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");
  // A dep whose exports map offers a custom condition ahead of the default.
  writeFileSync(join(condpkg, "package.json"), JSON.stringify({
    name: "condpkg", type: "module",
    exports: { ".": { custom: "./custom.js", default: "./plain.js" } },
  }));
  writeFileSync(join(condpkg, "custom.js"), 'export const which = "custom";\n');
  writeFileSync(join(condpkg, "plain.js"), 'export const which = "plain";\n');
  writeFileSync(join(source, "data.json"), JSON.stringify({ title: "Alpha", version: "1.0.0", "kebab-key": true }));
  const entry = join(source, "entry.ts");
  writeFileSync(entry, [
    'import data, { title, version } from "./data.json";',
    'import { which } from "condpkg";',
    "export default { title, version, data, which };",
  ].join("\n"));

  const runner = [
    'import { registerHooks } from "node:module";',
    `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
    "registerHooks({ resolve: loader.resolve, load: loader.load });",
    `process.stdout.write(JSON.stringify((await import(${JSON.stringify(pathToFileURL(entry).href)})).default));`,
  ].join("\n");
  const run = (extraEnv) => {
    const result = spawnSync(process.execPath, ["--input-type=module", "--eval", runner], {
      encoding: "utf8",
      timeout: 10_000,
      env: { ...process.env, OJ_APP_ROOT: app, OJ_CACHE_ROOT: join(app, "cache"), OJ_SSR_LOADER_CACHE: "off", ...extraEnv },
    });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
    return JSON.parse(result.stdout);
  };

  try {
    const plain = run({ OJ_RESOLVE_CONDITIONS: "" });
    assert.equal(plain.title, "Alpha");
    assert.equal(plain.version, "1.0.0");
    assert.deepEqual(plain.data, { title: "Alpha", version: "1.0.0", "kebab-key": true });
    // Without a configured condition Node's own set applies: the default entry.
    assert.equal(plain.which, "plain");

    // Vite parity: the environment's `resolve.conditions` never reach an
    // externalized dep (`externalize ? options.externalConditions :
    // options.conditions`) — condpkg is external, so `custom` must NOT apply.
    const notExternal = run({ OJ_RESOLVE_CONDITIONS: JSON.stringify(["custom"]) });
    assert.equal(notExternal.which, "plain");

    // `resolve.externalConditions` is the knob that reaches externals.
    const custom = run({ OJ_EXTERNAL_CONDITIONS: JSON.stringify(["custom"]) });
    assert.equal(custom.which, "custom");
    assert.equal(custom.title, "Alpha");

    // A noExternal dep resolves with the environment's conditions again.
    const noExt = run({
      OJ_RESOLVE_CONDITIONS: JSON.stringify(["custom"]),
      OJ_SSR_EXTERNALS: JSON.stringify({ noExternal: ["condpkg"] }),
    });
    assert.equal(noExt.which, "custom");
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});
