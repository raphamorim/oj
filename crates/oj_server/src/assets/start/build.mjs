// SPDX-License-Identifier: MIT
// Production build for a TanStack Start app: a minified/hashed client bundle,
// a Node server bundle wrapping createStartHandler's fetch, a manifest wiring
// the hashed client entry, and a server.mjs. SSR + hydration; server-function
// HTTP dispatch and prerender are follow-ups.
import { existsSync, readFileSync, writeFileSync, mkdirSync, cpSync, rmSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import esbuild from "esbuild";

const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const DIST = join(APP, "dist");
const CLIENT = join(DIST, "client");
const sfid = (rel, name) => Buffer.from(`${rel}#${name}`).toString("base64url");

// Node production server: serve built client assets + public, else SSR fetch.
const SERVER = [
  'import { createServer } from "node:http";',
  'import { readFile } from "node:fs/promises";',
  'import { join, extname, dirname } from "node:path";',
  'import { fileURLToPath } from "node:url";',
  'import handler from "./server-bundle.mjs";',
  'const HERE = dirname(fileURLToPath(import.meta.url));',
  'const CLIENT = join(HERE, "client");',
  'const PORT = process.env.PORT || 3000;',
  'const MIME = { ".js":"text/javascript", ".css":"text/css", ".json":"application/json", ".wasm":"application/wasm", ".ico":"image/x-icon", ".png":"image/png", ".svg":"image/svg+xml", ".woff2":"font/woff2" };',
  'function readBody(req){ return new Promise((r)=>{ const c=[]; req.on("data",(d)=>c.push(d)); req.on("end",()=>r(Buffer.concat(c))); }); }',
  'createServer(async (req, res) => {',
  '  const url = new URL(req.url, "http://" + (req.headers.host || "localhost"));',
  '  if (req.method === "GET") {',
  '    const rel = url.pathname.replace(/^\\/+/, "");',
  '    if (rel && !rel.startsWith("_serverFn/")) {',
  '      try { const bytes = await readFile(join(CLIENT, rel)); res.writeHead(200, { "content-type": MIME[extname(rel)] || "application/octet-stream" }); res.end(bytes); return; } catch {}',
  '    }',
  '  }',
  '  const body = (req.method === "GET" || req.method === "HEAD") ? undefined : await readBody(req);',
  '  const response = await handler.fetch(new Request(url.href, { method: req.method, headers: req.headers, body }));',
  '  res.writeHead(response.status, Object.fromEntries(response.headers));',
  '  res.end(await response.text());',
  '}).listen(PORT, () => console.log("oj tanstack server on http://localhost:" + PORT));',
  '',
].join("\n");

function routerEntry() {
  for (const ext of [".tsx", ".ts", ".jsx", ".js"]) {
    const p = resolve(APP, "src/router" + ext);
    if (existsSync(p)) return p;
  }
  return resolve(APP, "src/router.tsx");
}

// Balanced-paren scan of each top-level `const NAME = createServerFn(...).handler(`.
function serverFns(code) {
  const re = /(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\(/g;
  const out = [];
  let m;
  while ((m = re.exec(code))) {
    let depth = 1, i = m.index + m[0].length;
    for (; i < code.length && depth > 0; i++) {
      if (code[i] === "(") depth++;
      else if (code[i] === ")") depth--;
    }
    out.push({ name: m[1], open: m.index + m[0].length, close: i - 1 });
  }
  return out;
}

// Client transform: replace `.handler(FN)` with `.handler(createClientRpc(id))`.
const clientFnPlugin = {
  name: "server-fn-client",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx)$/ }, (args) => {
      const code = readFileSync(args.path, "utf8");
      if (!code.includes("createServerFn")) return null;
      const rel = relative(APP, args.path);
      const fns = serverFns(code);
      if (!fns.length) return null;
      let out = code;
      for (const f of fns.reverse()) {
        out = out.slice(0, f.open) + `createClientRpc(${JSON.stringify(sfid(rel, f.name))})` + out.slice(f.close);
      }
      return {
        contents: `import { createClientRpc } from "@tanstack/react-start/client-rpc";\n${out}`,
        loader: args.path.endsWith("tsx") ? "tsx" : "ts",
      };
    });
  },
};

// Server transform: provider shape so handlers run in-process during SSR.
const serverFnPlugin = {
  name: "server-fn-server",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx)$/ }, (args) => {
      const code = readFileSync(args.path, "utf8");
      if (!code.includes("createServerFn")) return null;
      const rel = relative(APP, args.path);
      const re =
        /(^|[\n;])([ \t]*)((?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\()/g;
      let changed = false;
      const out = code.replace(re, (_m, pre, indent, decl, name) => {
        changed = true;
        const meta = `{ id: ${JSON.stringify(sfid(rel, name))}, name: ${JSON.stringify(name)}, filename: ${JSON.stringify(rel)} }`;
        return `${pre}${indent}const ${name}_h = createServerRpc(${meta}, (opts) => ${name}.__executeServer(opts));\n${indent}${decl}${name}_h, `;
      });
      if (!changed) return null;
      return {
        contents: `import { createServerRpc } from "@tanstack/react-start/server-rpc";\n${out}`,
        loader: args.path.endsWith("tsx") ? "tsx" : "ts",
      };
    });
  },
};

const ALS =
  "export class AsyncLocalStorage{getStore(){return this._s}run(s,cb,...a){const p=this._s;this._s=s;try{return cb(...a)}finally{this._s=p}}enterWith(s){this._s=s}exit(cb,...a){const p=this._s;this._s=undefined;try{return cb(...a)}finally{this._s=p}}disable(){this._s=undefined}}export default {AsyncLocalStorage};";
const nodeShims = {
  name: "node-shims",
  setup(build) {
    build.onResolve({ filter: /^node:/ }, (a) => ({ path: a.path, namespace: "node-shim" }));
    build.onLoad({ filter: /.*/, namespace: "node-shim" }, (a) => ({
      contents: a.path === "node:async_hooks" ? ALS : "export default {};",
      loader: "js",
    }));
  },
};

rmSync(DIST, { recursive: true, force: true });
mkdirSync(CLIENT, { recursive: true });

// 1. Client bundle (minified, hashed).
const client = await esbuild.build({
  entryPoints: { "assets/client": join(HERE, "client-entry.tsx") },
  bundle: true, format: "esm", platform: "browser", jsx: "automatic", minify: true, splitting: true,
  entryNames: "[dir]/[name]-[hash]", chunkNames: "assets/[name]-[hash]", assetNames: "assets/[name]-[hash]",
  outdir: CLIENT, metafile: true, conditions: ["browser", "module", "import"],
  alias: {
    "#tanstack-router-entry": routerEntry(),
    "#tanstack-start-entry": join(HERE, "start-entry.ts"),
    "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
    "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
  },
  define: { "process.env.NODE_ENV": '"production"', "process.env.TSS_SERVER_FN_BASE": '"/_serverFn/"', global: "globalThis" },
  banner: { js: 'globalThis.process=globalThis.process||{env:{NODE_ENV:"production",TSS_SERVER_FN_BASE:"/_serverFn/"}};globalThis.global=globalThis.global||globalThis;' },
  plugins: [clientFnPlugin, nodeShims],
  logLevel: "silent",
});
const entryOut = Object.entries(client.metafile.outputs).find(([, o]) => o.entryPoint);
const clientUrl = "/" + relative(CLIENT, join(APP, entryOut[0]));

// 2. Manifest pointing at the hashed client entry.
writeFileSync(
  join(HERE, "manifest.ts"),
  `export const tsrStartManifest = () => ({ routes: { __root__: { preloads: [${JSON.stringify(clientUrl)}], scripts: [{ attrs: { type: "module", async: true, src: ${JSON.stringify(clientUrl)} } }] } } });\n`,
);

// 3. Server bundle (Node, fully bundled so the framework `#`-import aliases
// resolve at build time; node: builtins stay external on platform:node).
await esbuild.build({
  entryPoints: [join(HERE, "server-entry.tsx")],
  bundle: true, format: "esm", platform: "node", jsx: "automatic",
  outfile: join(DIST, "server-bundle.mjs"),
  alias: {
    "#tanstack-router-entry": routerEntry(),
    "#tanstack-start-entry": join(HERE, "start-entry.ts"),
    "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
    "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
  },
  define: { "process.env.NODE_ENV": '"production"', "process.env.TSS_SERVER_FN_BASE": '"/_serverFn/"' },
  // CJS deps bundled into ESM need a working `require` for node builtins.
  banner: { js: "import { createRequire as ___cr } from 'node:module'; const require = ___cr(import.meta.url);" },
  plugins: [serverFnPlugin],
  logLevel: "silent",
});

// 4. server.mjs: Node http, serve client assets + public, else SSR fetch.
writeFileSync(join(DIST, "server.mjs"), SERVER);

// 5. Copy public/.
if (existsSync(join(APP, "public"))) cpSync(join(APP, "public"), CLIENT, { recursive: true });

process.stderr.write(`oj: built dist (client ${clientUrl})\n`);
