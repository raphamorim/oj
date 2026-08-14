// SPDX-License-Identifier: MIT

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

const serverFnClient = {
  name: "server-fn-client",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx)$/ }, async (args) => {
      if (args.path.includes("/node_modules/")) return null;
      const loader = args.path.endsWith("tsx") ? "tsx" : "ts";
      let code = readFileSync(args.path, "utf8");
      if (container) {
        const t = await container.transformUserCode(code, args.path);
        if (t != null) code = t;
      }
      code = transformGlob(code, args.path);
      if (!code.includes("createServerFn")) return { contents: code, loader };
      const rel = relative(APP, args.path);
      const re = /(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\(/g;
      const edits = [];
      let m;
      while ((m = re.exec(code))) {
        const open = m.index + m[0].length;
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

const container = await loadPluginContainer(APP, { command: "serve", environment: "client" });

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
    "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
    "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
  },
  define: {
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
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} client entry bundled\n`);
