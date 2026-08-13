// SPDX-License-Identifier: MIT
// Unit tests for the SSR loader's pure helpers (loader-util.mjs): extension
// probing (incl. the .js->.ts bundler convention), CJS detection, the CJS->ESM
// interop facade, JSONC parsing, and package "type" resolution.
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
    writeFileSync(join(dir, "src", "comp.tsx"), ""); // imported as ./comp.js
    writeFileSync(join(dir, "src", "mod", "index.ts"), "");
    assert.equal(probe(join(dir, "src", "exact.ts")), join(dir, "src", "exact.ts"));
    // a .js specifier resolves to the .tsx on disk
    assert.equal(probe(join(dir, "src", "comp.js")), join(dir, "src", "comp.tsx"));
    // extensionless resolves via the extension list
    assert.equal(probe(join(dir, "src", "exact")), join(dir, "src", "exact.ts"));
    // directory resolves to index
    assert.equal(probe(join(dir, "src", "mod")), join(dir, "src", "mod", "index.ts"));
    assert.equal(probe(join(dir, "src", "missing")), null);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("hasEsmSyntax detects import/export at statement position", () => {
  assert.ok(hasEsmSyntax(fileWith("export const x = 1;")));
  assert.ok(hasEsmSyntax(fileWith("import x from 'y';")));
  assert.ok(!hasEsmSyntax(fileWith("const x = require('y'); module.exports = x;")));
  // dynamic import() alone is not ESM syntax
  assert.ok(!hasEsmSyntax(fileWith("const p = import('y');")));
});

let _tmpFiles = mk("hasesm");
function fileWith(src) {
  const p = join(_tmpFiles, "f" + Math.abs(hashish(src)) + ".js");
  writeFileSync(p, src);
  return p;
}
// tiny deterministic string hash (Math.random is unavailable)
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
    writeFileSync(join(dir, "cjs", "package.json"), '{"name":"x"}'); // no type -> commonjs
    assert.equal(nearestPkgType(join(dir, "cjs")), "commonjs");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("isCjsFile: .cjs yes, .mjs no, .js by pkg type + syntax", () => {
  const dir = mk("iscjs");
  try {
    writeFileSync(join(dir, "package.json"), '{"name":"x"}'); // commonjs
    const cjs = join(dir, "a.cjs"); writeFileSync(cjs, "module.exports={}");
    const mjs = join(dir, "a.mjs"); writeFileSync(mjs, "export const a=1");
    const jsCjs = join(dir, "b.js"); writeFileSync(jsCjs, "module.exports={}");
    const jsEsm = join(dir, "c.js"); writeFileSync(jsEsm, "export const c=1");
    assert.ok(isCjsFile(cjs));
    assert.ok(!isCjsFile(mjs));
    assert.ok(isCjsFile(jsCjs)); // commonjs pkg + no esm syntax
    assert.ok(!isCjsFile(jsEsm)); // esm syntax -> not cjs
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("cjsFacade re-exports actual runtime keys of a required CJS module", async () => {
  const dir = mk("facade");
  try {
    writeFileSync(join(dir, "package.json"), '{"name":"x"}');
    // module.exports = {...} : keys a static lexer would miss
    const modPath = join(dir, "dep.cjs");
    writeFileSync(modPath, "module.exports = { alpha: 1, beta: 2, 'weird-key': 3 };");
    const facade = cjsFacade(modPath);
    assert.match(facade, /export const alpha = _m\["alpha"\];/);
    assert.match(facade, /export const beta = _m\["beta"\];/);
    assert.match(facade, /export default _m;/);
    // non-identifier keys are not emitted as named exports
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
    // __esModule itself is a valid identifier and gets re-exported; default is excluded
    assert.doesNotMatch(facade, /export const default /);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("parseImportsField flattens string and conditional-object targets", () => {
  const rules = parseImportsField({
    "#lib/*": "./src/lib/*",
    "#env": { node: "./env.node.ts", default: "./env.ts" }, // no import -> default wins
    "#pick-import": { import: "./esm.js", require: "./cjs.cjs" },
    "#node-only": { node: "./env.node.ts" }, // falls through to node
    "#no-string": { require: "./only-cjs.cjs" }, // dropped: no import/default/node
  });
  const map = Object.fromEntries(rules);
  assert.equal(map["#lib/*"], "./src/lib/*");
  assert.equal(map["#pick-import"], "./esm.js"); // import wins (import ?? default ?? node)
  assert.equal(map["#env"], "./env.ts"); // default preferred over node
  assert.equal(map["#node-only"], "./env.node.ts"); // node used when it is the only one
  assert.ok(!("#no-string" in map)); // no usable string target -> dropped
});

test("parseImportsField tolerates an empty/absent map", () => {
  assert.deepEqual(parseImportsField(), []);
  assert.deepEqual(parseImportsField({}), []);
});

test("mergeTsConfig merges paths across an extends chain, later wins", () => {
  const base = "/app";
  const chain = [
    { cfg: { compilerOptions: { paths: { "@a/*": ["./base-a/*"], "@keep/*": ["./keep/*"] } } }, dir: "/app" },
    { cfg: { compilerOptions: { paths: { "@a/*": ["./over-a/*"] } } }, dir: "/app" }, // overrides @a
  ];
  const { rules, baseDir } = mergeTsConfig(chain, base);
  const map = Object.fromEntries(rules);
  assert.deepEqual(map["@a/*"], ["./over-a/*"]); // child overrides base
  assert.deepEqual(map["@keep/*"], ["./keep/*"]); // base-only survives
  assert.equal(baseDir, "/app"); // no baseUrl -> the config's own dir
});

test("mergeTsConfig resolves baseUrl against its config dir and arrays targets", () => {
  const chain = [
    { cfg: { compilerOptions: { baseUrl: "./src", paths: { "@x": "./x.ts" } } }, dir: "/app" },
  ];
  const { rules, baseDir } = mergeTsConfig(chain, "/app");
  assert.equal(baseDir, ["/app", "src"].join(sep)); // baseUrl joined to dir
  assert.deepEqual(Object.fromEntries(rules)["@x"], ["./x.ts"]); // string -> [string]
});

test("substituteAlias fills a trailing-star pattern (tsconfig + imports style)", () => {
  assert.equal(substituteAlias("@app/*", "./src/*", "@app/lib/format"), "./src/lib/format");
  assert.equal(substituteAlias("#lib/*", "./src/lib/*", "#lib/format"), "./src/lib/format");
  // deep subpaths keep the whole tail
  assert.equal(substituteAlias("@app/*", "./src/*", "@app/a/b/c"), "./src/a/b/c");
});

test("substituteAlias handles a star in the middle of pattern and target", () => {
  assert.equal(substituteAlias("@ui/*/styles", "./comp/*/css", "@ui/button/styles"), "./comp/button/css");
  // prefix or suffix mismatch does not match
  assert.equal(substituteAlias("@ui/*/styles", "./comp/*/css", "@ui/button/other"), null);
});

test("substituteAlias allows an empty star match (maps to the directory)", () => {
  // spec equal to the pattern minus `*` leaves an empty tail, mapping to the
  // target directory (probing then resolves its index).
  assert.equal(substituteAlias("@app/*", "./src/*", "@app/"), "./src/");
  // but the prefix must still be present
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
  // prepends the createServerRpc import
  assert.match(out, /^import \{ createServerRpc \} from "@tanstack\/react-start\/server-rpc";/);
  // emits the exported provider bound to the module-relative id + name + filename
  assert.ok(out.includes(`export const getGreeting_createServerFn_handler = createServerRpc({ id: ${JSON.stringify(id)}, name: "getGreeting", filename: ${JSON.stringify(rel)} }, (opts) => getGreeting.__executeServer(opts));`));
  // injects the provider as the first .handler() argument, keeping the original fn
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
  // the export keyword on `a` is preserved after the injected provider line
  assert.match(out, /export const a = createServerFn\(\)\.handler\(a_createServerFn_handler, /);
  // only one import is prepended regardless of function count
  assert.equal(out.match(/from "@tanstack\/react-start\/server-rpc"/g).length, 1);
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
    // a // inside a string is preserved by stripJsonc
    assert.ok(stripJsonc('{"u":"a//b"}').includes("a//b"));
    // unreadable / bad json -> null, not a throw
    assert.equal(readJsonc(join(dir, "nope.json")), null);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
