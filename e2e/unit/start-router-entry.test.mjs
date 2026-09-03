// SPDX-License-Identifier: MIT

// Where `#tanstack-router-entry` points. The framework makes it configurable
// (`router.entry`, resolved against `srcDirectory`), and the adapter resolves it
// by convention at src/router. An app that moved its router entry has to be
// able to say so, and package.json "imports" is the mechanism the loader
// already implements -- so a declaration has to beat the convention.

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const fixture = join(repo, "e2e/fixtures/start-app");
const installed = existsSync(join(fixture, "node_modules/rolldown"));

// The app declares its router entry somewhere the convention would never look,
// and also has a src/router.ts, so a pass cannot come from the convention
// happening to find the same file.
const app = mkdtempSync(join(tmpdir(), "oj-router-entry-"));
mkdirSync(join(app, "src", "lib"), { recursive: true });
mkdirSync(join(app, "src", "routes"), { recursive: true });
writeFileSync(join(app, "src", "lib", "router.ts"), "export function getRouter() {}\n");
writeFileSync(join(app, "src", "router.ts"), "export function getRouter() {}\n");
writeFileSync(
  join(app, "package.json"),
  JSON.stringify({
    name: "app",
    type: "module",
    imports: { "#tanstack-router-entry": "./src/lib/router.ts" },
  }),
);
if (installed) symlinkSync(join(fixture, "node_modules"), join(app, "node_modules"), "dir");
process.env.OJ_APP_ROOT = app;
// The loader keeps its resolve cache under OJ_CACHE_ROOT; without it, importing the
// module from the source tree would write that cache next to the sources.
process.env.OJ_CACHE_ROOT = join(app, "cache");
process.env.OJ_SSR_LOADER_CACHE = "off";
process.on("exit", () => rmSync(app, { recursive: true, force: true }));

const maybe = installed ? test : test.skip;
const loader = installed
  ? await import(pathToFileURL(join(repo, "crates/oj_server/src/assets/start/loader.mjs")).href)
  : null;

maybe("a declared #tanstack-router-entry beats the src/router convention", () => {
  const resolved = loader.resolve("#tanstack-router-entry", { parentURL: undefined }, () => {
    throw new Error("the loader deferred a specifier it aliases");
  });
  assert.equal(resolved.url.split("?")[0], pathToFileURL(join(app, "src", "lib", "router.ts")).href);
});
