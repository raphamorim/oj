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
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
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

function extractorDir(prefix) {
  const base = mkdtempSync(join(tmpdir(), prefix));
  copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
  writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module", dependencies: { vite: "*" } }));
  return base;
}

function runExtractor(base, config, args = [], opts = {}) {
  return execFileSync(
    process.execPath,
    [join(base, "vite-extract.mjs"), join(base, config), base, ...args],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: 30_000, ...opts },
  );
}

// A config/configEnvironment hook is real plugin code: one that starts an
// interval (a watcher, a server) must not keep the one-shot extractor alive —
// the result is emitted, then the process exits itself.
test("the extractor exits after emitting even when a hook keeps the event loop alive", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-exit-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    writeFileSync(
      join(base, "vite.config.mjs"),
      `export default {
        plugins: [{
          name: "keeps-loop-alive",
          config: () => { setInterval(() => {}, 1000); return null; },
        }],
        base: "/app/",
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs", [], { timeout: 15_000 }));
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/app/");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// Vite computes configEnv.isSsrBuild from the INLINE config before the file
// loads and never recomputes it, so a build.ssr set only by the config FILE is
// invisible to config hooks even under `vite build`. oj has no inline
// build.ssr at extraction time: hooks must see false for serve AND build.
test("config hooks see isSsrBuild false even when the file sets build.ssr under build", () => {
  const base = mkdtempSync(join(tmpdir(), "oj-vite-extract-ssrbuild-"));
  try {
    copyFileSync(asset("vite-extract.mjs"), join(base, "vite-extract.mjs"));
    writeFileSync(join(base, "package.json"), JSON.stringify({ name: "fx", type: "module" }));
    writeFileSync(
      join(base, "vite.config.mjs"),
      `import { writeFileSync } from "node:fs";
      export default {
        build: { ssr: "src/server.ts" },
        plugins: [{
          name: "env-probe",
          config: (conf, env) => { writeFileSync(new URL("./env.json", import.meta.url), JSON.stringify(env)); return null; },
        }],
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs", ["build", "production", "explicit"]));
    assert.equal(out.__ok, true);
    const env = JSON.parse(fs.readFileSync(join(base, "env.json"), "utf8"));
    assert.equal(env.command, "build");
    assert.equal(env.isSsrBuild, false, "Vite never recomputes isSsrBuild from the file's build.ssr");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// The single-eval flow through an installed vite: loadConfigFromFile runs
// first, its config goes back into resolveConfig via configFile:false, and the
// detection sentinel decides runner-backed inside resolveConfig's OWN hook run
// — the config file is evaluated once and no plugin's config hook runs twice.
test("with vite installed the config file is evaluated once and the sentinel decides runnerBacked", () => {
  const base = extractorDir("oj-vite-extract-single-");
  try {
    fs.mkdirSync(join(base, "node_modules", "vite"), { recursive: true });
    fs.writeFileSync(
      join(base, "node_modules", "vite", "package.json"),
      JSON.stringify({ name: "vite", version: "0.0.0-stub", type: "module", main: "index.mjs" }),
    );
    // A minimal vite honoring the contract the extractor relies on: config
    // hooks run hook-order-first inside resolveConfig, configEnvironment hooks
    // after, and the RESOLVED config default-fills every environment's
    // dev.createEnvironment (so reading the resolved config would always
    // false-positive — only the sentinel's pre-fill view is trustworthy).
    fs.writeFileSync(
      join(base, "node_modules", "vite", "index.mjs"),
      `import { pathToFileURL } from "node:url";
      export async function loadConfigFromFile(configEnv, configFile) {
        const m = await import(pathToFileURL(configFile).href);
        const config = typeof m.default === "function" ? await m.default(configEnv) : m.default;
        return { config, path: configFile, dependencies: [configFile] };
      }
      export function mergeConfig(a, b) { return { ...a, ...b }; }
      const merge = (a, b) => {
        const out = { ...a };
        for (const k of Object.keys(b ?? {})) {
          const v = b[k];
          if (v == null) continue;
          out[k] = out[k] && typeof out[k] === "object" && !Array.isArray(out[k]) && typeof v === "object" && !Array.isArray(v) ? merge(out[k], v) : v;
        }
        return out;
      };
      export async function resolveConfig(inline, command, mode) {
        if (inline.configFile !== false) throw new Error("stub expects the single-eval configFile:false flow");
        let conf = { ...inline };
        const plugins = (conf.plugins ?? []).flat(Infinity).filter(Boolean);
        const rank = (h) => (h && typeof h === "object" ? (h.order === "pre" ? -1 : h.order === "post" ? 1 : 0) : 0);
        const hooks = plugins.map((p) => ({ p, h: p.config })).filter((e) => e.h);
        hooks.sort((a, b) => rank(a.h) - rank(b.h));
        for (const { h } of hooks) {
          const res = await (typeof h === "object" ? h.handler : h).call({ error(m) { throw new Error(m); }, warn() {} }, conf, { command, mode, isSsrBuild: false, isPreview: false });
          if (res) conf = merge(conf, res);
        }
        conf.environments = { client: {}, ssr: {}, ...(conf.environments ?? {}) };
        for (const p of plugins) {
          if (!p.configEnvironment) continue;
          const h = typeof p.configEnvironment === "object" ? p.configEnvironment.handler : p.configEnvironment;
          for (const name of Object.keys(conf.environments)) {
            const r = await h.call({}, name, conf.environments[name], { command, mode });
            if (r) conf.environments[name] = merge(conf.environments[name], r);
          }
        }
        // The default factory fill: presence in the RESOLVED config says nothing.
        for (const name of Object.keys(conf.environments)) {
          conf.environments[name] = merge({ dev: { createEnvironment: () => ({}) } }, conf.environments[name]);
        }
        conf.configFileDependencies = [];
        return conf;
      }\n`,
    );
    // The config counts its evaluations and its hook runs on disk.
    fs.writeFileSync(
      join(base, "vite.config.mjs"),
      `import { appendFileSync } from "node:fs";
      appendFileSync(new URL("./evals.log", import.meta.url), "eval\\n");
      export default {
        base: "/one-eval/",
        plugins: [{
          name: "declares",
          config: () => {
            appendFileSync(new URL("./hooks.log", import.meta.url), "config\\n");
            return { environments: { worker: { dev: { createEnvironment: () => ({}) } } } };
          },
        }],
      };\n`,
    );
    const out = JSON.parse(runExtractor(base, "vite.config.mjs"));
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/one-eval/");
    assert.equal(out.ssr?.runnerBacked, true, "the sentinel saw the declaration inside resolveConfig");
    assert.equal(fs.readFileSync(join(base, "evals.log"), "utf8"), "eval\n", "the config file is evaluated exactly once");
    assert.equal(fs.readFileSync(join(base, "hooks.log"), "utf8"), "config\n", "no plugin's config hook runs twice");

    // A non-declaring config through the same flow: the sentinel's false is
    // authoritative (no fallback re-run of the hooks on the same instances).
    fs.writeFileSync(
      join(base, "plain.config.mjs"),
      `import { appendFileSync } from "node:fs";
      export default {
        plugins: [{
          name: "plain",
          config: () => { appendFileSync(new URL("./plain-hooks.log", import.meta.url), "config\\n"); return null; },
        }],
      };\n`,
    );
    const plain = JSON.parse(runExtractor(base, "plain.config.mjs"));
    assert.equal(plain.__ok, true);
    assert.equal(plain.ssr?.runnerBacked, undefined);
    assert.equal(fs.readFileSync(join(base, "plain-hooks.log"), "utf8"), "config\n");

    // configEnvironment-declared factories are seen by the sentinel too.
    fs.writeFileSync(
      join(base, "envhook.config.mjs"),
      `export default {
        plugins: [{
          name: "declares-via-configEnvironment",
          configEnvironment: (name) => (name === "ssr" ? { dev: { createEnvironment: () => ({}) } } : null),
        }],
      };\n`,
    );
    const envhook = JSON.parse(runExtractor(base, "envhook.config.mjs"));
    assert.equal(envhook.__ok, true);
    assert.equal(envhook.ssr?.runnerBacked, true, "a configEnvironment declaration is a runner declaration");
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

// When the raw config cannot be loaded but resolveConfig still succeeds, the
// degenerate path is LOUD and safe: a warning names the failure, and the
// resolved ssr.resolve sugar (possibly a foreign runtime's conditions) is
// withheld so Node consumers can never adopt it with detection unavailable.
test("a failed raw load warns and withholds the resolved ssr.resolve sugar", () => {
  const base = extractorDir("oj-vite-extract-rawnull-");
  try {
    fs.mkdirSync(join(base, "node_modules", "vite"), { recursive: true });
    fs.writeFileSync(
      join(base, "node_modules", "vite", "package.json"),
      JSON.stringify({ name: "vite", version: "0.0.0-stub", type: "module", main: "index.mjs" }),
    );
    fs.writeFileSync(
      join(base, "node_modules", "vite", "index.mjs"),
      `export async function loadConfigFromFile() { throw new Error("raw load exploded"); }
      export function mergeConfig(a, b) { return { ...a, ...b }; }
      export async function resolveConfig() {
        return {
          base: "/resolved/",
          ssr: { target: "webworker", resolve: { conditions: ["workerd"], externalConditions: ["workerd"] } },
          configFileDependencies: [],
        };
      }\n`,
    );
    fs.writeFileSync(join(base, "vite.config.mjs"), "export default {};\n");
    const child = spawnSync(
      process.execPath,
      [join(base, "vite-extract.mjs"), join(base, "vite.config.mjs"), base],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: 30_000 },
    );
    assert.match(
      child.stderr,
      /could not load the raw config .*raw load exploded.*ssr\.resolve conditions are unavailable/s,
      "the degenerate path is loud, never silent",
    );
    const out = JSON.parse(child.stdout);
    assert.equal(out.__ok, true);
    assert.equal(out.base, "/resolved/");
    assert.equal(out.ssr?.resolve, undefined, "foreign resolved conditions are withheld without detection");
    assert.equal(out.ssr?.target, "webworker", "the rest of the ssr block still travels");
    assert.equal(out.ssr?.runnerBacked, undefined);
    assert.equal(out.rawResolve, null);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
