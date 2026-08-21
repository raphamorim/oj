// SPDX-License-Identifier: MIT

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { importPkg, viteEnvDefine } from "./resolve-pkg.mjs";
import { assetsPlugin, makeVitePlugins, nodeBuiltinShims, workspaceRoot } from "./rolldown-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const { build } = await importPkg(APP, "rolldown", ["vite", "@tanstack/react-start"]);
const WORKSPACE = workspaceRoot(APP);
const SERVER_FN_BASE = process.env.TSS_SERVER_FN_BASE ?? "/_serverFn/";

function rewriteServerFns(code, id) {
  if (!code.includes("createServerFn")) return null;
  const rel = relative(APP, id);
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
  if (!edits.length) return null;
  let out = code;
  for (const e of edits.reverse()) {
    const id2 = Buffer.from(`${rel}#${e.name}`).toString("base64url");
    out = out.slice(0, e.open) + `createClientRpc(${JSON.stringify(id2)})` + out.slice(e.close);
  }
  return `import { createClientRpc } from "@tanstack/react-start/client-rpc";\n${out}`;
}

function routerEntry() {
  for (const ext of [".tsx", ".ts", ".jsx", ".js"]) {
    const p = resolve(APP, "src/router" + ext);
    if (existsSync(p)) return p;
  }
  return resolve(APP, "src/router.tsx");
}

const container = await loadPluginContainer(APP, { command: "serve", environment: "client" });

const cssUrls = [];

const serverFnClient = {
  name: "server-fn-client",
  transform: {
    filter: { id: { include: /\.(ts|tsx)$/, exclude: [/\/node_modules\//, /^\0/] } },
    async handler(code, id) {
      if (id.includes("/node_modules/") || id.startsWith("\0") || !/\.(ts|tsx)$/.test(id)) return null;
      let out = code;
      if (container) {
        const t = await container.transformUserCode(out, id);
        if (t != null) out = t;
      }
      out = transformGlob(out, id);
      const rpc = rewriteServerFns(out, id);
      if (rpc != null) return rpc;
      return out === code ? null : out;
    },
  },
};

const result = await build({
  input: join(HERE, "client-entry.tsx"),
  platform: "browser",
  transform: {
    jsx: { runtime: "automatic" },
    define: {
      "process.env": JSON.stringify({ NODE_ENV: "development", TSS_SERVER_FN_BASE: SERVER_FN_BASE }),
      global: "globalThis",
      ...viteEnvDefine({ ssr: false }),
    },
  },
  resolve: {
    conditionNames: ["browser", "module", "import"],
    alias: {
      "#tanstack-router-entry": routerEntry(),
      "#tanstack-start-entry": join(HERE, "start-entry.ts"),
      "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
      "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
      "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
    },
  },
  plugins: [
    makeVitePlugins({ container, appRoot: APP, mode: "dev" }),
    assetsPlugin({ mode: "dev", cssUrls }),
    serverFnClient,
    nodeBuiltinShims,
  ],
  output: {
    format: "esm",
    banner:
      `globalThis.process=globalThis.process||{env:{NODE_ENV:"development",TSS_SERVER_FN_BASE:${JSON.stringify(SERVER_FN_BASE)}}};` +
      "globalThis.global=globalThis.global||globalThis;",
  },
  write: false,
});

const chunk = result.output.find((o) => o.type === "chunk" && o.isEntry) ?? result.output[0];
writeFileSync(join(HERE, "client-entry.js"), chunk.code);
// Module count for the editor's update-progress narration ("(N modules)").
writeFileSync(join(HERE, "client-entry.modules"), String(chunk.modules ? Object.keys(chunk.modules).length : 0));

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
