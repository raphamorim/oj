// SPDX-License-Identifier: MIT
// Bundle the TanStack Start client hydration entry for the browser: esbuild
// with the framework aliases resolved (a browser can't run a Node loader hook),
// everything bundled, NODE_ENV defined, node: builtins shimmed, and the app's
// vite plugins / assets / import.meta.glob handled. Output: client-entry.js.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { importPkg, viteEnvDefine } from "./resolve-pkg.mjs";
import { assetsPlugin, makeVitePlugins, nodeBuiltinShims, pnpmStorePaths, workspaceRoot } from "./esbuild-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const esbuild = await importPkg(APP, "esbuild", ["vite", "@tanstack/react-start"]);
const WORKSPACE = workspaceRoot(APP);
const SERVER_FN_BASE = process.env.TSS_SERVER_FN_BASE ?? "/_serverFn/";

// App-source transform for the client: expand `import.meta.glob`, then rewrite
// each top-level `const NAME = createServerFn(...).handler(FN)` to inject
// `createClientRpc(id)` as the first handler arg. The runtime uses arg 1 as the
// extractedFn, so the browser makes an HTTP RPC to /_serverFn/<id> instead of
// running the handler. `id` must match the server resolver's manifest (same
// `<relpath>#<name>`). node_modules is left to esbuild's native loader.
const serverFnClient = {
  name: "server-fn-client",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx)$/ }, (args) => {
      if (args.path.includes("/node_modules/")) return null;
      const loader = args.path.endsWith("tsx") ? "tsx" : "ts";
      let code = transformGlob(readFileSync(args.path, "utf8"), args.path);
      if (!code.includes("createServerFn")) return { contents: code, loader };
      const rel = relative(APP, args.path);
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
      if (!edits.length) return { contents: code, loader };
      let out = code;
      for (const e of edits.reverse()) {
        const id = Buffer.from(`${rel}#${e.name}`).toString("base64url");
        out = out.slice(0, e.open) + `createClientRpc(${JSON.stringify(id)})` + out.slice(e.close);
      }
      return {
        contents: `import { createClientRpc } from "@tanstack/react-start/client-rpc";\n${out}`,
        loader,
      };
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

// The app's vite.config plugins, so `virtual:*`, .mdx (transform) and .svg
// (svgr load) resolve. Absent config/Vite -> null -> the plugin is a no-op.
const container = await loadPluginContainer(APP, { command: "serve", environment: "client" });

// Stylesheet urls the app imports; linked in the dev SSR head via the manifest.
const cssUrls = [];

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
    // Replace the whole `process.env` object so any framework read
    // (TSS_ROUTER_BASEPATH, TSS_INLINE_CSS_ENABLED, ...) inlines to a value.
    // A dotted define would leave unknown keys as a runtime `process.env`
    // access, which throws in a split chunk that ESM evaluates before the entry
    // banner runs. Missing keys become `undefined`, which the framework accepts.
    "process.env": JSON.stringify({ NODE_ENV: "development", TSS_SERVER_FN_BASE: SERVER_FN_BASE }),
    global: "globalThis",
    ...viteEnvDefine({ ssr: false }),
  },
  banner: {
    js:
      `globalThis.process=globalThis.process||{env:{NODE_ENV:"development",TSS_SERVER_FN_BASE:${JSON.stringify(SERVER_FN_BASE)}}};` +
      "globalThis.global=globalThis.global||globalThis;",
  },
  plugins: [
    makeVitePlugins({ container, appRoot: APP, mode: "dev" }),
    assetsPlugin({ mode: "dev", cssUrls }),
    serverFnClient,
    nodeBuiltinShims,
  ],
  nodePaths: pnpmStorePaths(WORKSPACE),
  outfile: join(HERE, "client-entry.js"),
  logLevel: "silent",
});

// Dev manifest: link the app's stylesheets in the SSR <head> (via the root
// route's css) so the first paint is styled and does not depend on hydration.
// The runner re-reads this on every warm reload, so edits stay styled.
const devManifest = {
  routes: {
    __root__: {
      preloads: ["/@oj-start/client-entry.js"],
      css: cssUrls,
      scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
    },
  },
};
writeFileSync(
  join(HERE, "manifest.ts"),
  `export const tsrStartManifest = () => (${JSON.stringify(devManifest)});\n`,
);
const OJ = process.stderr.isTTY && !process.env.NO_COLOR ? "\x1b[1;38;2;42;51;212moj\x1b[0m" : "oj";
process.stderr.write(`${OJ}: client entry bundled\n`);
