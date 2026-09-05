// SPDX-License-Identifier: MIT
//
// Two Vite resolver/loader behaviors in the TanStack SSR loader: a JSON module
// exposes named exports for its identifier keys (json.namedExports, on by
// default), and the config's `resolve.conditions` (OJ_RESOLVE_CONDITIONS from
// oj) join Node's export conditions, so a package `exports` entry behind a
// custom condition resolves the way Vite's resolver picks it.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
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

test("SSR loader: linked (workspace) packages take the environment's conditions, not externalConditions", () => {
  const app = realpathSync(mkdtempSync(join(tmpdir(), "oj-ssr-linked-cond-")));
  const source = join(app, "src");
  const linkedsrc = join(app, "linkedsrc");
  const rolldown = join(app, "node_modules", "rolldown");
  for (const directory of [source, linkedsrc, rolldown]) mkdirSync(directory, { recursive: true });

  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-linked-cond-app", type: "module" }));
  writeFileSync(join(rolldown, "package.json"), JSON.stringify({
    name: "rolldown", type: "module", exports: { "./experimental": "./experimental.mjs" },
  }));
  writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");
  // A workspace-style package: symlinked into node_modules, real files outside it.
  writeFileSync(join(linkedsrc, "package.json"), JSON.stringify({
    name: "linkedpkg", type: "module",
    exports: { ".": { custom: "./custom.js", default: "./plain.js" } },
  }));
  writeFileSync(join(linkedsrc, "custom.js"), 'export const which = "custom";\n');
  writeFileSync(join(linkedsrc, "plain.js"), 'export const which = "plain";\n');
  symlinkSync(linkedsrc, join(app, "node_modules", "linkedpkg"), "dir");

  const entry = join(source, "entry.ts");
  writeFileSync(entry, ['import { which } from "linkedpkg";', "export default { which };"].join("\n"));

  const runner = [
    'import { registerHooks } from "node:module";',
    `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
    "registerHooks({ resolve: loader.resolve, load: loader.load });",
    `process.stdout.write(JSON.stringify((await import(${JSON.stringify(pathToFileURL(entry).href)})).default));`,
  ].join("\n");
  try {
    const result = spawnSync(process.execPath, ["--input-type=module", "--eval", runner], {
      encoding: "utf8",
      timeout: 10_000,
      env: {
        ...process.env,
        OJ_APP_ROOT: app,
        OJ_CACHE_ROOT: join(app, "cache"),
        OJ_SSR_LOADER_CACHE: "off",
        OJ_RESOLVE_CONDITIONS: JSON.stringify(["custom"]),
      },
    });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
    // Vite's isExternalizable: resolved outside node_modules (via the symlink's
    // realpath) means NOT external, so the environment's `custom` applies.
    assert.equal(JSON.parse(result.stdout).which, "custom");
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});

// This loader executes modules in Node: a workerd-shaped condition list (the
// Cloudflare plugin sets `browser` alongside `workerd`/`worker` for its own
// runtime) must never let `browser` win here — a browser conditional export
// runs DOM code at module scope (`document is not defined`). The package
// mirrors decode-named-character-reference, browser-first so only the strip
// (not condition order) saves it.
test("SSR loader: a workerd-shaped condition list never matches browser builds in Node", () => {
  const app = realpathSync(mkdtempSync(join(tmpdir(), "oj-ssr-workerd-cond-")));
  const source = join(app, "src");
  const dompkg = join(app, "node_modules", "dompkg");
  const rolldown = join(app, "node_modules", "rolldown");
  for (const directory of [source, dompkg, rolldown]) mkdirSync(directory, { recursive: true });

  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-workerd-cond-app", type: "module" }));
  writeFileSync(join(rolldown, "package.json"), JSON.stringify({
    name: "rolldown", type: "module", exports: { "./experimental": "./experimental.mjs" },
  }));
  writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");
  writeFileSync(join(dompkg, "package.json"), JSON.stringify({
    name: "dompkg", type: "module",
    exports: { ".": { browser: "./index.dom.js", worker: "./index.js", default: "./index.js" } },
  }));
  writeFileSync(join(dompkg, "index.js"), 'export const build = "node-safe";\n');
  writeFileSync(join(dompkg, "index.dom.js"), 'document.createElement("i");\nexport const build = "browser";\n');

  const entry = join(source, "entry.ts");
  writeFileSync(entry, ['import { build } from "dompkg";', "export default { build };"].join("\n"));

  const runner = [
    'import { registerHooks } from "node:module";',
    `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
    "registerHooks({ resolve: loader.resolve, load: loader.load });",
    `process.stdout.write(JSON.stringify((await import(${JSON.stringify(pathToFileURL(entry).href)})).default));`,
  ].join("\n");
  const run = (extraEnv) => spawnSync(process.execPath, ["--input-type=module", "--eval", runner], {
    encoding: "utf8",
    timeout: 10_000,
    env: { ...process.env, OJ_APP_ROOT: app, OJ_CACHE_ROOT: join(app, "cache"), OJ_SSR_LOADER_CACHE: "off", ...extraEnv },
  });
  const workerdList = JSON.stringify(["workerd", "worker", "module", "browser", "development"]);
  // The Cloudflare ssr environment sets `resolve.noExternal: true`, so every
  // bare import takes the environment's conditions.
  const noExternalAll = JSON.stringify({ noExternalAll: true });
  try {
    // Environment conditions path (noExternal): browser is stripped, worker wins.
    const viaConditions = run({ OJ_RESOLVE_CONDITIONS: workerdList, OJ_SSR_EXTERNALS: noExternalAll });
    assert.equal(viaConditions.status, 0, viaConditions.stderr || viaConditions.error?.message);
    assert.equal(JSON.parse(viaConditions.stdout).build, "node-safe");

    // Externalized path: a workerd-shaped externalConditions list is stripped too.
    const viaExternal = run({ OJ_EXTERNAL_CONDITIONS: workerdList });
    assert.equal(viaExternal.status, 0, viaExternal.stderr || viaExternal.error?.message);
    assert.equal(JSON.parse(viaExternal.stdout).build, "node-safe");

    // A browser list without workerd stays honored (Vite honors user conditions).
    const honored = run({ OJ_RESOLVE_CONDITIONS: JSON.stringify(["browser"]), OJ_SSR_EXTERNALS: noExternalAll });
    assert.equal(honored.status, 1, "a plain browser user condition still picks the browser build");
    assert.match(honored.stderr ?? "", /document is not defined/);
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});

// The runtime markers themselves are stripped too, not just `browser`: Node
// activating `workerd` resolves a workerd-only exports key to code written for
// that runtime; it must fall through to node/default instead. And any non-Node
// runtime marker (edge-light here) marks the list, not workerd alone.
test("SSR loader: runtime markers are stripped — workerd-only keys fall through, edge-light lists lose browser", () => {
  const app = realpathSync(mkdtempSync(join(tmpdir(), "oj-ssr-marker-cond-")));
  const source = join(app, "src");
  const cfonly = join(app, "node_modules", "cfonly");
  const dompkg = join(app, "node_modules", "dompkg");
  const rolldown = join(app, "node_modules", "rolldown");
  for (const directory of [source, cfonly, dompkg, rolldown]) mkdirSync(directory, { recursive: true });

  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-marker-cond-app", type: "module" }));
  writeFileSync(join(rolldown, "package.json"), JSON.stringify({
    name: "rolldown", type: "module", exports: { "./experimental": "./experimental.mjs" },
  }));
  writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");
  // A workerd-only conditional export ahead of default (no worker/node key).
  writeFileSync(join(cfonly, "package.json"), JSON.stringify({
    name: "cfonly", type: "module",
    exports: { ".": { workerd: "./workerd.js", default: "./index.js" } },
  }));
  writeFileSync(join(cfonly, "index.js"), 'export const build = "node-safe";\n');
  writeFileSync(join(cfonly, "workerd.js"), 'throw new Error("workerd-only build executed in Node");\n');
  // Browser-first alongside worker/default, as in the workerd-shaped test.
  writeFileSync(join(dompkg, "package.json"), JSON.stringify({
    name: "dompkg", type: "module",
    exports: { ".": { browser: "./index.dom.js", worker: "./index.js", default: "./index.js" } },
  }));
  writeFileSync(join(dompkg, "index.js"), 'export const build = "node-safe";\n');
  writeFileSync(join(dompkg, "index.dom.js"), 'document.createElement("i");\nexport const build = "browser";\n');

  const entry = join(source, "entry.ts");
  writeFileSync(entry, [
    'import { build as cf } from "cfonly";',
    'import { build as dom } from "dompkg";',
    "export default { cf, dom };",
  ].join("\n"));

  const runner = [
    'import { registerHooks } from "node:module";',
    `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
    "registerHooks({ resolve: loader.resolve, load: loader.load });",
    `process.stdout.write(JSON.stringify((await import(${JSON.stringify(pathToFileURL(entry).href)})).default));`,
  ].join("\n");
  const run = (extraEnv) => spawnSync(process.execPath, ["--input-type=module", "--eval", runner], {
    encoding: "utf8",
    timeout: 10_000,
    env: { ...process.env, OJ_APP_ROOT: app, OJ_CACHE_ROOT: join(app, "cache"), OJ_SSR_LOADER_CACHE: "off", ...extraEnv },
  });
  const noExternalAll = JSON.stringify({ noExternalAll: true });
  try {
    // A workerd-shaped list: the workerd marker itself is stripped, so the
    // workerd-only key falls through to default (environment conditions path).
    const workerdList = JSON.stringify(["workerd", "worker", "module", "browser", "development"]);
    const viaConditions = run({ OJ_RESOLVE_CONDITIONS: workerdList, OJ_SSR_EXTERNALS: noExternalAll });
    assert.equal(viaConditions.status, 0, viaConditions.stderr || viaConditions.error?.message);
    assert.deepEqual(JSON.parse(viaConditions.stdout), { cf: "node-safe", dom: "node-safe" });

    // Externalized path too.
    const viaExternal = run({ OJ_EXTERNAL_CONDITIONS: workerdList });
    assert.equal(viaExternal.status, 0, viaExternal.stderr || viaExternal.error?.message);
    assert.deepEqual(JSON.parse(viaExternal.stdout), { cf: "node-safe", dom: "node-safe" });

    // edge-light marks the list like workerd does: browser is stripped, worker
    // (runtime-neutral) is kept and wins over the browser build.
    const edgeList = JSON.stringify(["edge-light", "worker", "module", "browser"]);
    const viaEdge = run({ OJ_RESOLVE_CONDITIONS: edgeList, OJ_SSR_EXTERNALS: noExternalAll });
    assert.equal(viaEdge.status, 0, viaEdge.stderr || viaEdge.error?.message);
    assert.equal(JSON.parse(viaEdge.stdout).dom, "node-safe");
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});
