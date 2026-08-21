// SPDX-License-Identifier: MIT

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve, relative } from "node:path";
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

// Records every module the build traverses. chunk.modules alone is not enough:
// tree-shaking drops most of the graph from the rendered chunk, but traversal
// still shapes the output (e.g. manifest cssUrls come from css imports found
// anywhere in the graph), so the cache key must cover all visited files.
const visitedIds = new Set();
const closureRecorder = {
  name: "oj-closure-recorder",
  transform(code, id) {
    visitedIds.add(id);
    return null;
  },
};

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
    closureRecorder,
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

// Persist every output file. The build code-splits on dynamic imports
// (route components), so the entry is a small glue chunk whose static and
// dynamic imports reference sibling chunks; discarding them leaves the
// entry unable to load and the client never hydrates.
const entryChunk = result.output.find((o) => o.type === "chunk" && o.isEntry) ?? result.output[0];
const chunksDir = join(HERE, "client-chunks");
// Retries: recursive removal of a few thousand fresh files can hit a
// transient ENOTEMPTY on macOS while the indexer still holds entries.
rmSync(chunksDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
mkdirSync(chunksDir, { recursive: true });
const chunkIndex = [];
let moduleCount = 0;
for (const o of result.output) {
  const dest = join(chunksDir, o.fileName);
  mkdirSync(dirname(dest), { recursive: true });
  writeFileSync(dest, o.type === "chunk" ? o.code : o.source);
  chunkIndex.push({ name: o.fileName, size: statSync(dest).size });
  if (o.type === "chunk") moduleCount += Object.keys(o.modules ?? {}).length;
}
writeFileSync(
  join(HERE, "client-chunks.json"),
  JSON.stringify({ entry: entryChunk.fileName, files: chunkIndex }),
);
// Module count for the editor's update-progress narration ("(N modules)").
writeFileSync(join(HERE, "client-entry.modules"), String(moduleCount));

// Closure manifest for the Rust-side whole-bundle cache: every real file
// whose content shaped this build.
const closure = new Set([routerEntry(), ...visitedIds, ...(container?.configDependencies ?? []), ...(container?.watchFiles ?? [])]);
for (const o of result.output) {
  if (o.type !== "chunk") continue;
  for (const id of Object.keys(o.modules ?? {})) closure.add(id);
}
const closureFiles = [...closure]
  .map((id) => String(id).split("?")[0])
  .filter((p) => !p.startsWith("\0"))
  // config dependencies come back root-relative from loadConfigFromFile
  .map((p) => (isAbsolute(p) ? p : resolve(APP, p)))
  // Derived files poison the key. HERE holds oj-generated files: static
  // assets are covered by the oj version in the cache salt, and manifest.ts
  // is this build's own output. The route tree is regenerated on every boot
  // and rewritten again mid-run by the runner's generator with different
  // formatting; its real inputs (route file names and contents) are already
  // in the key.
  .filter((p) => !p.startsWith(HERE) && !/(^|\/)routeTree\.gen\.[jt]sx?$/.test(p))
  .filter((p) => {
    try { return statSync(p).isFile(); } catch { return false; }
  })
  .sort();
writeFileSync(join(HERE, "closure.json"), JSON.stringify(closureFiles));

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
// Raw stylesheet-URL list for the store's generation manifest; write-then-
// rename so a concurrent read never sees a partial JSON.
writeFileSync(join(HERE, "css-urls.json.tmp"), JSON.stringify(cssUrls));
renameSync(join(HERE, "css-urls.json.tmp"), join(HERE, "css-urls.json"));
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} client bundled (${result.output.length} files)\n`);
