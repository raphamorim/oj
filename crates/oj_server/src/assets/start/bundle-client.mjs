// SPDX-License-Identifier: MIT
// Bundle the TanStack Start client hydration entry for the browser: esbuild
// with the framework aliases resolved (a browser can't run a Node loader hook),
// everything bundled, NODE_ENV defined, and node: builtins shimmed (the storage
// context imports node:async_hooks, which is a server concern). Output:
// client-entry.js.
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import esbuild from "esbuild";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();

function routerEntry() {
  for (const ext of [".tsx", ".ts", ".jsx", ".js"]) {
    const p = resolve(APP, "src/router" + ext);
    if (existsSync(p)) return p;
  }
  return resolve(APP, "src/router.tsx");
}

// Browser shims for node: builtins pulled in transitively. AsyncLocalStorage
// needs a working no-op (used by the storage context); the rest are empty.
const ALS =
  "export class AsyncLocalStorage{getStore(){return undefined}" +
  "run(_s,cb,...a){return cb(...a)}enterWith(){}exit(cb,...a){return cb(...a)}disable(){}}" +
  "export default {AsyncLocalStorage};";
const nodeShims = {
  name: "node-builtin-shims",
  setup(build) {
    build.onResolve({ filter: /^node:/ }, (args) => ({ path: args.path, namespace: "node-shim" }));
    build.onLoad({ filter: /.*/, namespace: "node-shim" }, (args) => ({
      contents: args.path === "node:async_hooks" ? ALS : "export default {};",
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
  },
  define: { "process.env.NODE_ENV": '"development"', global: "globalThis" },
  banner: { js: "globalThis.process=globalThis.process||{env:{NODE_ENV:\"development\"}};globalThis.global=globalThis.global||globalThis;" },
  plugins: [nodeShims],
  outfile: join(HERE, "client-entry.js"),
  logLevel: "silent",
});
process.stderr.write("oj: client entry bundled\n");
