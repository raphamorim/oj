// SPDX-License-Identifier: MIT
// Bundle the TanStack Start client hydration entry for the browser: esbuild
// with the framework aliases resolved (a browser can't run a Node loader hook),
// everything bundled, NODE_ENV defined, and node: builtins shimmed (the storage
// context imports node:async_hooks, which is a server concern). Output:
// client-entry.js.
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { importPkg } from "./resolve-pkg.mjs";
import { assetsPlugin, pnpmStorePaths } from "./esbuild-assets.mjs";

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
// (`import "url"` in older deps like source-map). Shim both; async_hooks gets a
// working synchronous AsyncLocalStorage, everything else an empty module.
const BARE_BUILTINS =
  /^(assert|buffer|child_process|cluster|console|constants|crypto|dgram|dns|domain|events|fs|http|http2|https|module|net|os|path|perf_hooks|process|punycode|querystring|readline|repl|stream|string_decoder|sys|timers|tls|tty|url|util|v8|vm|worker_threads|zlib|async_hooks)$/;
const nodeShims = {
  name: "node-builtin-shims",
  setup(build) {
    build.onResolve({ filter: /^node:/ }, (args) => ({ path: args.path, namespace: "node-shim" }));
    build.onResolve({ filter: BARE_BUILTINS }, (args) => ({ path: args.path, namespace: "node-shim" }));
    build.onLoad({ filter: /.*/, namespace: "node-shim" }, (args) => ({
      contents: /(^|:)async_hooks$/.test(args.path) ? ALS : "export default {};",
      loader: "js",
    }));
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
  },
  banner: {
    js:
      `globalThis.process=globalThis.process||{env:{NODE_ENV:"development",TSS_SERVER_FN_BASE:${JSON.stringify(SERVER_FN_BASE)}}};` +
      "globalThis.global=globalThis.global||globalThis;",
  },
  plugins: [assetsPlugin({ mode: "dev" }), serverFnClient, nodeShims],
  nodePaths: pnpmStorePaths(WORKSPACE),
  outfile: join(HERE, "client-entry.js"),
  logLevel: "silent",
});
process.stderr.write("oj: client entry bundled\n");
