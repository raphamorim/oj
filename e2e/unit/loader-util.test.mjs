// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  probe, isCjsFile, hasEsmSyntax, nearestPkgType, cjsFacade, stripJsonc, readJsonc,
  rewriteServerFns, substituteAlias, parseImportsField, mergeTsConfig,
} from "../../crates/oj_server/src/assets/start/loader-util.mjs";
import { sep } from "node:path";

const mk = (p) => mkdtempSync(join(tmpdir(), "oj-loader-" + p + "-"));

test("probe finds an exact file, else a .ts for a .js import, else an index", () => {
  const dir = mk("probe");
  try {
    mkdirSync(join(dir, "src", "mod"), { recursive: true });
    writeFileSync(join(dir, "src", "exact.ts"), "");
    writeFileSync(join(dir, "src", "comp.tsx"), "");
    writeFileSync(join(dir, "src", "mod", "index.ts"), "");
    assert.equal(probe(join(dir, "src", "exact.ts")), join(dir, "src", "exact.ts"));
    assert.equal(probe(join(dir, "src", "comp.js")), join(dir, "src", "comp.tsx"));
    assert.equal(probe(join(dir, "src", "exact")), join(dir, "src", "exact.ts"));
    assert.equal(probe(join(dir, "src", "mod")), join(dir, "src", "mod", "index.ts"));
    assert.equal(probe(join(dir, "src", "missing")), null);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("probe resolves extensionless JSON modules and directory indexes", () => {
  const dir = mk("json-probe");
  try {
    mkdirSync(join(dir, "config"), { recursive: true });
    writeFileSync(join(dir, "settings.json"), "{}");
    writeFileSync(join(dir, "config", "index.json"), "{}");

    assert.equal(probe(join(dir, "settings")), join(dir, "settings.json"));
    assert.equal(probe(join(dir, "config")), join(dir, "config", "index.json"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("hasEsmSyntax detects import/export at statement position", () => {
  assert.ok(hasEsmSyntax(fileWith("export const x = 1;")));
  assert.ok(hasEsmSyntax(fileWith("import x from 'y';")));
  assert.ok(!hasEsmSyntax(fileWith("const x = require('y'); module.exports = x;")));
  assert.ok(!hasEsmSyntax(fileWith("const p = import('y');")));
});

let _tmpFiles = mk("hasesm");
function fileWith(src) {
  const p = join(_tmpFiles, "f" + Math.abs(hashish(src)) + ".js");
  writeFileSync(p, src);
  return p;
}
function hashish(s) {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return h;
}

test("nearestPkgType walks up to the nearest package.json type", () => {
  const dir = mk("pkgtype");
  try {
    mkdirSync(join(dir, "esm", "deep"), { recursive: true });
    writeFileSync(join(dir, "esm", "package.json"), '{"type":"module"}');
    assert.equal(nearestPkgType(join(dir, "esm", "deep")), "module");
    mkdirSync(join(dir, "cjs"), { recursive: true });
    writeFileSync(join(dir, "cjs", "package.json"), '{"name":"x"}');
    assert.equal(nearestPkgType(join(dir, "cjs")), "commonjs");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("isCjsFile: .cjs yes, .mjs no, .js by pkg type + syntax", () => {
  const dir = mk("iscjs");
  try {
    writeFileSync(join(dir, "package.json"), '{"name":"x"}');
    const cjs = join(dir, "a.cjs"); writeFileSync(cjs, "module.exports={}");
    const mjs = join(dir, "a.mjs"); writeFileSync(mjs, "export const a=1");
    const jsCjs = join(dir, "b.js"); writeFileSync(jsCjs, "module.exports={}");
    const jsEsm = join(dir, "c.js"); writeFileSync(jsEsm, "export const c=1");
    assert.ok(isCjsFile(cjs));
    assert.ok(!isCjsFile(mjs));
    assert.ok(isCjsFile(jsCjs));
    assert.ok(!isCjsFile(jsEsm));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("cjsFacade re-exports actual runtime keys of a required CJS module", async () => {
  const dir = mk("facade");
  try {
    writeFileSync(join(dir, "package.json"), '{"name":"x"}');
    const modPath = join(dir, "dep.cjs");
    writeFileSync(modPath, "module.exports = { alpha: 1, beta: 2, 'weird-key': 3 };");
    const facade = cjsFacade(modPath);
    assert.match(facade, /export const alpha = _m\["alpha"\];/);
    assert.match(facade, /export const beta = _m\["beta"\];/);
    assert.match(facade, /export default _m;/);
    assert.doesNotMatch(facade, /weird-key/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("cjsFacade unwraps default for __esModule (transpiled ESM) modules", () => {
  const dir = mk("facade-esm");
  try {
    writeFileSync(join(dir, "package.json"), '{"name":"x"}');
    const modPath = join(dir, "dep.cjs");
    writeFileSync(modPath, "Object.defineProperty(exports,'__esModule',{value:true}); exports.default = {x:1}; exports.named = 2;");
    const facade = cjsFacade(modPath);
    assert.match(facade, /_m\.default !== undefined \? _m\.default : _m/);
    assert.match(facade, /export const named = _m\["named"\];/);
    assert.doesNotMatch(facade, /export const default /);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("cjsFacade omits strict-mode reserved identifiers from named exports", () => {
  const dir = mk("facade-reserved");
  try {
    const file = join(dir, "legacy.cjs");
    writeFileSync(file, "module.exports = { interface: 1, implements: 2, private: 3, valid: 4 };");

    const facade = cjsFacade(file);
    assert.match(facade, /export const valid =/);
    assert.doesNotMatch(facade, /export const (?:interface|implements|private) =/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("parseImportsField flattens string and conditional-object targets", () => {
  const rules = parseImportsField({
    "#lib/*": "./src/lib/*",
    "#env": { node: "./env.node.ts", default: "./env.ts" },
    "#pick-import": { import: "./esm.js", require: "./cjs.cjs" },
    "#node-only": { node: "./env.node.ts" },
    "#no-string": { require: "./only-cjs.cjs" },
  });
  const map = Object.fromEntries(rules);
  assert.equal(map["#lib/*"], "./src/lib/*");
  assert.equal(map["#pick-import"], "./esm.js");
  // Node takes the first supported condition in object order; the SSR loader
  // runs in Node, so `node` listed first wins over `default`.
  assert.equal(map["#env"], "./env.node.ts");
  assert.equal(map["#node-only"], "./env.node.ts");
  assert.ok(!("#no-string" in map));
});

test("parseImportsField tolerates an empty/absent map", () => {
  assert.deepEqual(parseImportsField(), []);
  assert.deepEqual(parseImportsField({}), []);
});

test("parseImportsField resolves fallback arrays and nested import conditions", () => {
  const rules = Object.fromEntries(parseImportsField({
    "#fallback": [null, "./src/fallback.ts"],
    "#nested": { import: { default: "./src/nested.ts" } },
    "#array-condition": [{ require: "./ignored.cjs" }, { import: "./src/module.ts" }],
  }));

  assert.equal(rules["#fallback"], "./src/fallback.ts");
  assert.equal(rules["#nested"], "./src/nested.ts");
  assert.equal(rules["#array-condition"], "./src/module.ts");
});

test("parseImportsField takes conditions in the object's own order, like Node", () => {
  const rules = Object.fromEntries(parseImportsField({
    "#node-first": { node: "./src/node.ts", import: "./src/import.ts", default: "./src/default.ts" },
    "#import-first": { import: "./src/import.ts", node: "./src/node.ts" },
    "#unsupported-first": { browser: "./src/browser.ts", default: "./src/default.ts" },
    "#only-unsupported": { browser: "./src/browser.ts", require: "./src/x.cjs" },
  }));
  assert.equal(rules["#node-first"], "./src/node.ts");
  assert.equal(rules["#import-first"], "./src/import.ts");
  assert.equal(rules["#unsupported-first"], "./src/default.ts");
  assert.equal(rules["#only-unsupported"], undefined);
});

test("probe resolves extensionless .mts modules (Vite's resolve.extensions)", () => {
  const dir = mk("mts-probe");
  try {
    mkdirSync(join(dir, "lib"), { recursive: true });
    writeFileSync(join(dir, "util.mts"), "export const x = 1;");
    writeFileSync(join(dir, "lib", "index.mts"), "export const y = 2;");
    assert.equal(probe(join(dir, "util")), join(dir, "util.mts"));
    assert.equal(probe(join(dir, "lib")), join(dir, "lib", "index.mts"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("mergeTsConfig merges paths across an extends chain, later wins", () => {
  const base = "/app";
  const chain = [
    { cfg: { compilerOptions: { paths: { "@a/*": ["./base-a/*"], "@keep/*": ["./keep/*"] } } }, dir: "/app" },
    { cfg: { compilerOptions: { paths: { "@a/*": ["./over-a/*"] } } }, dir: "/app" },
  ];
  const { rules, baseDir } = mergeTsConfig(chain, base);
  const map = Object.fromEntries(rules);
  assert.deepEqual(map["@a/*"], ["./over-a/*"]);
  assert.deepEqual(map["@keep/*"], ["./keep/*"]);
  assert.equal(baseDir, "/app");
});

test("mergeTsConfig resolves baseUrl against its config dir and arrays targets", () => {
  const chain = [
    { cfg: { compilerOptions: { baseUrl: "./src", paths: { "@x": "./x.ts" } } }, dir: "/app" },
  ];
  const { rules, baseDir } = mergeTsConfig(chain, "/app");
  assert.equal(baseDir, ["/app", "src"].join(sep));
  assert.deepEqual(Object.fromEntries(rules)["@x"], ["./x.ts"]);
});

test("mergeTsConfig preserves an inherited baseUrl when the child omits it", () => {
  const chain = [
    {
      cfg: { compilerOptions: { baseUrl: "./shared", paths: { "@shared/*": ["./*"] } } },
      dir: "/workspace/config",
    },
    {
      cfg: { compilerOptions: { paths: { "@app/*": ["./app/*"] } } },
      dir: "/workspace/apps/web",
    },
  ];

  const { rules, baseDir } = mergeTsConfig(chain, "/workspace/apps/web");

  assert.equal(baseDir, ["/workspace", "config", "shared"].join(sep));
  assert.deepEqual(Object.fromEntries(rules)["@shared/*"], ["./*"]);
  assert.deepEqual(Object.fromEntries(rules)["@app/*"], ["./app/*"]);
});

test("substituteAlias fills a trailing-star pattern (tsconfig + imports style)", () => {
  assert.equal(substituteAlias("@app/*", "./src/*", "@app/lib/format"), "./src/lib/format");
  assert.equal(substituteAlias("#lib/*", "./src/lib/*", "#lib/format"), "./src/lib/format");
  assert.equal(substituteAlias("@app/*", "./src/*", "@app/a/b/c"), "./src/a/b/c");
});

test("substituteAlias handles a star in the middle of pattern and target", () => {
  assert.equal(substituteAlias("@ui/*/styles", "./comp/*/css", "@ui/button/styles"), "./comp/button/css");
  assert.equal(substituteAlias("@ui/*/styles", "./comp/*/css", "@ui/button/other"), null);
});

test("substituteAlias allows an empty star match (maps to the directory)", () => {
  assert.equal(substituteAlias("@app/*", "./src/*", "@app/"), "./src/");
  assert.equal(substituteAlias("@app/*", "./src/*", "other/x"), null);
});

test("substituteAlias without a star matches only the exact specifier", () => {
  assert.equal(substituteAlias("#env", "./env.ts", "#env"), "./env.ts");
  assert.equal(substituteAlias("#env", "./env.ts", "#env/extra"), null);
  assert.equal(substituteAlias("#env", "./env.ts", "#en"), null);
});

test("rewriteServerFns leaves code without createServerFn untouched", () => {
  const code = "export const x = 1;\nconst f = () => 2;";
  assert.equal(rewriteServerFns(code, "src/x.ts"), code);
});

test("rewriteServerFns rewrites a handler to the provider shape", () => {
  const rel = "src/server/data.ts";
  const code = 'const getGreeting = createServerFn({ method: "GET" }).handler(async () => ({ ok: true }));';
  const out = rewriteServerFns(code, rel);
  const id = Buffer.from(`${rel}#getGreeting`).toString("base64url");
  assert.match(out, /^import \{ createServerRpc \} from "@tanstack\/react-start\/server-rpc";/);
  assert.ok(out.includes(`export const getGreeting_createServerFn_handler = createServerRpc({ id: ${JSON.stringify(id)}, name: "getGreeting", filename: ${JSON.stringify(rel)} }, (opts) => getGreeting.__executeServer(opts));`));
  assert.match(out, /\.handler\(getGreeting_createServerFn_handler, async \(\) => \(\{ ok: true \}\)\)/);
});

test("rewriteServerFns handles an exported const and multiple functions", () => {
  const code = [
    'export const a = createServerFn().handler(() => 1);',
    'const b = createServerFn({ method: "POST" }).handler(() => 2);',
  ].join("\n");
  const out = rewriteServerFns(code, "src/fns.ts");
  assert.ok(out.includes("export const a_createServerFn_handler = createServerRpc("));
  assert.ok(out.includes("export const b_createServerFn_handler = createServerRpc("));
  assert.match(out, /export const a = createServerFn\(\)\.handler\(a_createServerFn_handler, /);
  assert.equal(out.match(/from "@tanstack\/react-start\/server-rpc"/g).length, 1);
});

test("rewriteServerFns adds no lines (source maps and stack traces stay aligned)", () => {
  const code = [
    'import { createServerFn } from "@tanstack/react-start";',
    "",
    "export const a = createServerFn({",
    '  method: "GET",',
    "}).handler(async () => {",
    '  throw new Error("boom");',
    "});",
  ].join("\n");
  const out = rewriteServerFns(code, "src/fns.ts");
  const lines = out.split("\n");
  assert.equal(lines.length, code.split("\n").length, out);
  assert.match(lines[0], /^import \{ createServerRpc \} from "@tanstack\/react-start\/server-rpc"; import \{ createServerFn \}/);
  assert.match(lines[2], /^export const a_createServerFn_handler = createServerRpc\(.*\); export const a = createServerFn\(\{$/);
  assert.equal(lines[5], '  throw new Error("boom");');
});

test("stripJsonc + readJsonc tolerate comments and trailing commas", () => {
  const dir = mk("jsonc");
  try {
    const p = join(dir, "tsconfig.json");
    writeFileSync(
      p,
      `{
        // comment with a "quote" and /* not a block */
        "compilerOptions": {
          "paths": { "@/*": ["./src/*"], }, /* trailing comma above */
        },
      }`,
    );
    const cfg = readJsonc(p);
    assert.deepEqual(cfg.compilerOptions.paths, { "@/*": ["./src/*"] });
    assert.ok(stripJsonc('{"u":"a//b"}').includes("a//b"));
    assert.equal(readJsonc(join(dir, "nope.json")), null);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("readJsonc preserves comma-and-bracket sequences inside string values", () => {
  const dir = mk("jsonc-string");
  try {
    const file = join(dir, "tsconfig.json");
    writeFileSync(file, '{"compilerOptions":{"custom":"literal,} and ,]",},}');

    assert.deepEqual(readJsonc(file), {
      compilerOptions: { custom: "literal,} and ,]" },
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("readJsonc accepts TypeScript configuration with a UTF-8 byte-order mark", () => {
  const dir = mk("jsonc-bom");
  try {
    const file = join(dir, "tsconfig.json");
    writeFileSync(file, '\ufeff{"compilerOptions":{"baseUrl":"./src"}}');

    assert.deepEqual(readJsonc(file), { compilerOptions: { baseUrl: "./src" } });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
