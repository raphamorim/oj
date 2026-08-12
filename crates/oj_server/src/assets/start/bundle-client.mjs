// SPDX-License-Identifier: MIT
// Bundle the TanStack Start client hydration entry for the browser: esbuild
// with the framework aliases resolved (a browser can't run a Node loader hook),
// everything bundled, NODE_ENV defined, and node: builtins shimmed (the storage
// context imports node:async_hooks, which is a server concern). Output:
// client-entry.js.
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { importPkg, viteEnvDefine } from "./resolve-pkg.mjs";
import { assetsPlugin, pnpmStorePaths } from "./esbuild-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const esbuild = await importPkg(APP, "esbuild", ["vite", "@tanstack/react-start"]);

// Farthest ancestor with a node_modules (the pnpm/workspace root).
function workspaceRoot(app) {
  let best = app;
  for (let cur = app; ; ) {
    const parent = dirname(cur);
    if (parent === cur) break;
    if (existsSync(join(parent, "node_modules"))) best = parent;
    cur = parent;
  }
  return best;
}
const WORKSPACE = workspaceRoot(APP);
const SERVER_FN_BASE = process.env.TSS_SERVER_FN_BASE ?? "/_serverFn/";

// Client server-fn transform: rewrite each top-level
// `const NAME = createServerFn(...).handler(FN)` to inject `createClientRpc(id)`
// as the first handler arg. The runtime uses arg 1 as the extractedFn, so the
// browser makes an HTTP RPC to /_serverFn/<id> instead of running the handler.
// `id` must match the server resolver's manifest (same `<relpath>#<name>`).
const serverFnClient = {
  name: "server-fn-client",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx)$/ }, (args) => {
      const code = readFileSync(args.path, "utf8");
      if (!code.includes("createServerFn")) return null;
      const rel = relative(APP, args.path);
      // For each top-level `const NAME = createServerFn(...).handler(FN)`,
      // REPLACE the handler args with `createClientRpc(id)` (single arg): a
      // trailing second arg makes the runtime treat it as a server build and
      // run the handler in the browser. Balanced-paren scan to strip FN.
      const re = /(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\(/g;
      const edits = [];
      let m;
      while ((m = re.exec(code))) {
        const open = m.index + m[0].length; // just after `.handler(`
        let depth = 1;
        let i = open;
        for (; i < code.length && depth > 0; i++) {
          if (code[i] === "(") depth++;
          else if (code[i] === ")") depth--;
        }
        edits.push({ name: m[1], open, close: i - 1 });
      }
      if (!edits.length) return null;
      let out = code;
      for (const e of edits.reverse()) {
        const id = Buffer.from(`${rel}#${e.name}`).toString("base64url");
        out = out.slice(0, e.open) + `createClientRpc(${JSON.stringify(id)})` + out.slice(e.close);
      }
      const src = `import { createClientRpc } from "@tanstack/react-start/client-rpc";\n${out}`;
      return { contents: src, loader: args.path.endsWith("tsx") ? "tsx" : "ts" };
    });
  },
};

function routerEntry() {
  for (const ext of [".tsx", ".ts", ".jsx", ".js"]) {
    const p = resolve(APP, "src/router" + ext);
    if (existsSync(p)) return p;
  }
  return resolve(APP, "src/router.tsx");
}

// Browser shims for node: builtins pulled in transitively. AsyncLocalStorage
// needs a working no-op (used by the storage context); the rest are empty.
// Best-effort synchronous AsyncLocalStorage for the browser: persists the
// store (enterWith) and scopes it (run). No cross-await propagation, but the
// client Start context is set persistently, so getStore() works.
const ALS =
  "export class AsyncLocalStorage{getStore(){return this._s}" +
  "run(s,cb,...a){const p=this._s;this._s=s;try{return cb(...a)}finally{this._s=p}}" +
  "enterWith(s){this._s=s}exit(cb,...a){const p=this._s;this._s=undefined;try{return cb(...a)}finally{this._s=p}}" +
  "disable(){this._s=undefined}}export default {AsyncLocalStorage};";
// Node builtins reach the browser bundle two ways: `node:`-prefixed, and bare
// (`import "url"` in older deps like source-map). SSR-only framework code
// (stream rendering) can also land in the client graph and named-import from
// them, so a few builtins need real named exports (browser globals where they
// exist; inert stubs otherwise, since that code is not reached client-side).
const BARE_BUILTINS =
  /^(assert|buffer|child_process|cluster|console|constants|crypto|dgram|dns|domain|events|fs|http|http2|https|module|net|os|path|perf_hooks|process|punycode|querystring|readline|repl|stream|stream\/web|string_decoder|sys|timers|tls|tty|url|util|v8|vm|worker_threads|zlib|async_hooks)$/;
// stream/web maps to the real browser globals; node:stream gets inert stream
// classes; punycode gets identity stubs. Each also default-exports its members.
const SHIM_STREAM_WEB =
  "export const ReadableStream=globalThis.ReadableStream;export const WritableStream=globalThis.WritableStream;" +
  "export const TransformStream=globalThis.TransformStream;export const ByteLengthQueuingStrategy=globalThis.ByteLengthQueuingStrategy;" +
  "export const CountQueuingStrategy=globalThis.CountQueuingStrategy;" +
  "export default {ReadableStream,WritableStream,TransformStream,ByteLengthQueuingStrategy,CountQueuingStrategy};";
const SHIM_STREAM =
  "class S{on(){return this}once(){return this}emit(){return false}pipe(t){return t}end(){}write(){return true}" +
  "removeListener(){return this}destroy(){}}export class Readable extends S{static from(){return new Readable()}}" +
  "export class Writable extends S{}export class Duplex extends S{}export class Transform extends S{}" +
  "export class PassThrough extends S{}export class Stream extends S{}" +
  "export default {Readable,Writable,Duplex,Transform,PassThrough,Stream};";
const SHIM_PUNYCODE =
  "export const toUnicode=(s)=>s;export const toASCII=(s)=>s;export const encode=(s)=>s;export const decode=(s)=>s;" +
  "export const ucs2={decode:()=>[],encode:()=>\"\"};export default {toUnicode,toASCII,encode,decode,ucs2};";
function shimSource(spec) {
  const name = spec.replace(/^node:/, "");
  if (name === "async_hooks") return ALS;
  if (name === "stream/web") return SHIM_STREAM_WEB;
  if (name === "stream") return SHIM_STREAM;
  if (name === "punycode") return SHIM_PUNYCODE;
  return "export default {};";
}
const nodeShims = {
  name: "node-builtin-shims",
  setup(build) {
    build.onResolve({ filter: /^node:/ }, (args) => ({ path: args.path, namespace: "node-shim" }));
    build.onResolve({ filter: BARE_BUILTINS }, (args) => ({ path: args.path, namespace: "node-shim" }));
    build.onLoad({ filter: /.*/, namespace: "node-shim" }, (args) => ({
      contents: shimSource(args.path),
      loader: "js",
    }));
  },
};

// Bridge the app's vite.config plugins so `virtual:*` ids they own resolve in
// the client bundle. Fallback-only: normal resolution runs first; the container
// is consulted for virtual specifiers (and \0-prefixed re-imports from within
// loaded virtual code). Absent config/Vite -> null -> plugin is a no-op.
const container = await loadPluginContainer(APP, { command: "serve", environment: "client" });
const vitePlugins = {
  name: "oj-vite-plugins",
  setup(build) {
    if (!container) return;
    const resolve = async (args) => {
      const rid = await container.resolveId(args.path, args.importer);
      return rid ? { path: rid, namespace: "oj-vite-virtual" } : undefined;
    };
    build.onResolve({ filter: /^virtual:/ }, resolve);
    build.onResolve({ filter: /^\0/ }, resolve);
    build.onLoad({ filter: /.*/, namespace: "oj-vite-virtual" }, async (args) => {
      const code = await container.load(args.path);
      return code == null ? undefined : { contents: code, loader: "js", resolveDir: APP };
    });
  },
};

await esbuild.build({
  entryPoints: [join(HERE, "client-entry.tsx")],
  bundle: true,
  format: "esm",
  platform: "browser",
  jsx: "automatic",
  conditions: ["browser", "module", "import"],
  alias: {
    "#tanstack-router-entry": routerEntry(),
    "#tanstack-start-entry": join(HERE, "start-entry.ts"),
    "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
    // The router manifest virtual (start-server-core can be pulled into the
    // client graph before tree-shaking drops it).
    "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
    // Runtime isomorphic-fn impl (the stubs default to the server impl).
    "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
  },
  define: {
    "process.env.NODE_ENV": '"development"',
    "process.env.TSS_SERVER_FN_BASE": JSON.stringify(SERVER_FN_BASE),
    global: "globalThis",
    ...viteEnvDefine({ ssr: false }),
  },
  banner: {
    js:
      `globalThis.process=globalThis.process||{env:{NODE_ENV:"development",TSS_SERVER_FN_BASE:${JSON.stringify(SERVER_FN_BASE)}}};` +
      "globalThis.global=globalThis.global||globalThis;",
  },
  plugins: [vitePlugins, assetsPlugin({ mode: "dev" }), serverFnClient, nodeShims],
  nodePaths: pnpmStorePaths(WORKSPACE),
  outfile: join(HERE, "client-entry.js"),
  logLevel: "silent",
});
process.stderr.write("oj: client entry bundled\n");
