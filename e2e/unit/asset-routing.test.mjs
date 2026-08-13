// SPDX-License-Identifier: MIT
// Build-level harness for the shared esbuild plugins (esbuild-assets.mjs). It
// drives assetsPlugin / makeVitePlugins / nodeBuiltinShims through a real
// esbuild bundle over temp files, with a STUB plugin container so no vite
// config or svgr install is needed. This covers the asset/svg/virtual/mdx
// routing that only runs inside a build, which the unit tests otherwise cannot
// reach. esbuild comes from the start fixture; the whole suite skips when the
// fixture is not installed.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, basename, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";
import {
  assetsPlugin,
  makeVitePlugins,
  nodeBuiltinShims,
} from "../../crates/oj_server/src/assets/start/esbuild-assets.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = resolve(here, "..", "fixtures", "start-app");

let esbuild = null;
try {
  const req = createRequire(pathToFileURL(join(fixture, "package.json")).href);
  const m = await import(pathToFileURL(req.resolve("esbuild")).href);
  esbuild = m.default ?? m;
} catch {}

// Register real tests only when esbuild is present; otherwise a single skip.
const it = esbuild ? test : (name) => test(name, { skip: "fixture esbuild not installed" }, () => {});

// A fake asset emitter: deterministic, no hashing, so assertions are stable.
const emit = async (abs) => "/assets/" + basename(abs);

// A stub plugin container standing in for the app's vite plugins (svgr, mdx, a
// virtual module). Returns null for anything it does not own, exactly like the
// real container, so the URL/asset fallbacks are exercised too.
function stubContainer() {
  return {
    async resolveId(id) {
      return id === "virtual:info" ? "\0virtual:info" : null;
    },
    async load(id) {
      if (id === "\0virtual:info") return `export const info = "VIRTUAL_INFO_OK";`;
      if (id.endsWith(".svg?react")) return `export default () => "SVG_REACT_COMPONENT";`;
      if (id.endsWith("icon.svg")) return `export default () => "SVG_BARE_COMPONENT";`;
      return null; // e.g. plain.svg -> caller falls back to a URL
    },
    async transform(code, id) {
      if (id.endsWith(".mdx")) return `export default () => ${JSON.stringify("MDX:" + code.trim())};`;
      return null;
    },
    async generateBundle() {},
    publicDir: null,
    pluginCount: 1,
  };
}

// Write `files` into a fresh temp dir, bundle `entry.js` with `plugins`, and
// return the concatenated output text. Cleans up on the way out.
async function bundle(files, plugins) {
  const dir = mkdtempSync(join(tmpdir(), "oj-route-"));
  try {
    for (const [name, contents] of Object.entries(files)) {
      const p = join(dir, name);
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, contents);
    }
    const res = await esbuild.build({
      entryPoints: [join(dir, "entry.js")],
      bundle: true, write: false, format: "esm", logLevel: "silent",
      plugins: plugins(dir),
    });
    return { text: res.outputFiles.map((f) => f.text).join("\n"), dir };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

it("routes ?raw, ?url, ?inline, and bare-asset imports (prod)", async () => {
  const files = {
    "notes.txt": "RAW_FILE_MARKER contents",
    "logo.png": "PNGDATA-logo",
    "tiny.png": "PNGDATA-tiny",
    "pic.png": "PNGDATA-pic",
    "entry.js": [
      'import raw from "./notes.txt?raw";',
      'import url from "./logo.png?url";',
      'import inline from "./tiny.png?inline";',
      'import bare from "./pic.png";',
      "export { raw, url, inline, bare };",
    ].join("\n"),
  };
  const { text } = await bundle(files, () => [assetsPlugin({ mode: "prod", server: false, emit })]);
  // ?raw inlines the file text
  assert.ok(text.includes("RAW_FILE_MARKER contents"), "?raw should inline the text");
  // ?url and bare assets become emitted /assets URLs
  assert.ok(text.includes("/assets/logo.png"), "?url should emit an /assets URL");
  assert.ok(text.includes("/assets/pic.png"), "bare asset should emit an /assets URL");
  // ?inline becomes a data URI (esbuild picks base64 or url-encoding by content)
  assert.match(text, /data:image\/png[;,]/, "?inline should be a data URI");
});

it("?url resolves to the dev fs route in dev mode", async () => {
  const files = {
    "logo.png": "PNGDATA",
    "entry.js": 'import url from "./logo.png?url"; export { url };',
  };
  const { text } = await bundle(files, () => [
    assetsPlugin({ mode: "dev", server: false, fsBase: "/@oj-start/fs" }),
  ]);
  // dev serves assets from the fs route at their absolute path
  assert.match(text, /\/@oj-start\/fs[^"']*logo\.png/, "dev ?url should point at the fs route");
  assert.doesNotMatch(text, /\/assets\/logo\.png/, "dev should not emit a hashed /assets url");
});

it("css import is a no-op on the server and a <link> on the dev client", async () => {
  const files = {
    "styles.css": ".a{color:red}",
    "entry.js": 'import "./styles.css"; export const ok = 1;',
  };
  const server = await bundle(files, () => [assetsPlugin({ mode: "prod", server: true, emit })]);
  assert.doesNotMatch(server.text, /createElement\("link"\)/, "server css import must not touch the DOM");

  const client = await bundle(files, () => [
    assetsPlugin({ mode: "dev", server: false, fsBase: "/@oj-start/fs" }),
  ]);
  assert.match(client.text, /createElement\("link"\)/, "dev client css import should inject a <link>");
  assert.match(client.text, /rel = "stylesheet"|rel="stylesheet"/, "the link should be a stylesheet");
});

it("prod client css import emits the stylesheet but injects no link", async () => {
  const files = {
    "styles.css": ".a{color:red}",
    "entry.js": 'import "./styles.css"; export const ok = 1;',
  };
  const emitted = [];
  const spyEmit = async (abs) => (emitted.push(abs), "/assets/" + basename(abs));
  const out = await bundle(files, () => [assetsPlugin({ mode: "prod", server: false, emit: spyEmit })]);
  // no runtime <link> injection: the manifest links it in the SSR head instead
  assert.doesNotMatch(out.text, /createElement\("link"\)/, "prod client must not inject a link");
  // but the stylesheet was still emitted (so it can be linked from the head)
  assert.ok(emitted.some((p) => p.endsWith("styles.css")), "css still emitted in prod");
});

it("routes virtual: ids and .mdx through the plugin container", async () => {
  const files = {
    "doc.mdx": "# Title\nmdx body",
    "entry.js": [
      'import { info } from "virtual:info";',
      'import Doc from "./doc.mdx";',
      "export { info, Doc };",
    ].join("\n"),
  };
  const { text } = await bundle(files, (dir) => [
    makeVitePlugins({ container: stubContainer(), appRoot: dir, mode: "prod", emit }),
    assetsPlugin({ mode: "prod", server: false, emit }),
  ]);
  assert.ok(text.includes("VIRTUAL_INFO_OK"), "virtual: id should load from the container");
  assert.ok(text.includes("MDX:# Title"), ".mdx should be transformed by the container");
});

it("routes bare .svg and .svg?react through the container, URL fallback otherwise", async () => {
  const files = {
    "icon.svg": "<svg><rect/></svg>",
    "plain.svg": "<svg><circle/></svg>",
    "entry.js": [
      'import Icon from "./icon.svg";',        // container claims -> component
      'import IconReact from "./icon.svg?react";', // container claims (?react id) -> component
      'import plain from "./plain.svg";',      // container returns null -> URL fallback
      "export { Icon, IconReact, plain };",
    ].join("\n"),
  };
  const { text } = await bundle(files, (dir) => [
    makeVitePlugins({ container: stubContainer(), appRoot: dir, mode: "prod", emit }),
    assetsPlugin({ mode: "prod", server: false, emit }),
  ]);
  assert.ok(text.includes("SVG_BARE_COMPONENT"), "bare .svg should reach the container");
  assert.ok(text.includes("SVG_REACT_COMPONENT"), ".svg?react should reach the container with the query");
  assert.ok(text.includes("/assets/plain.svg"), "an unclaimed svg should fall back to a URL");
});

it("shims node builtins for the browser bundle", async () => {
  const files = {
    "entry.js": [
      'import { AsyncLocalStorage } from "node:async_hooks";',
      'import stream from "stream";',
      "export const als = new AsyncLocalStorage();",
      "export { stream };",
    ].join("\n"),
  };
  const { text } = await bundle(files, () => [nodeBuiltinShims]);
  // async_hooks gets a working synchronous AsyncLocalStorage (esbuild may render
  // it as `var AsyncLocalStorage = class`), so assert on stable identifiers
  assert.match(text, /AsyncLocalStorage/, "async_hooks should be shimmed");
  assert.match(text, /getStore\(\)/, "the ALS shim should expose getStore");
  // bare 'stream' resolves to the shim rather than erroring as unresolved
  assert.match(text, /PassThrough/, "bare node builtins should resolve to the stream shim");
});
