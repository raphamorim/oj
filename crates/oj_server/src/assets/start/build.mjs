// SPDX-License-Identifier: MIT
// Production build for a TanStack Start app: a minified/hashed client bundle,
// a Node server bundle wrapping createStartHandler's fetch, a manifest wiring
// the hashed client entry, and a server.mjs. SSR + hydration; server-function
// HTTP dispatch and prerender are follow-ups.
import { existsSync, readFileSync, writeFileSync, mkdirSync, cpSync, rmSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { importPkg, viteEnvDefine } from "./resolve-pkg.mjs";
import {
  assetsPlugin, makeVitePlugins, nodeBuiltinShims, pnpmStorePaths, workspaceRoot, contentHashEmitter,
} from "./esbuild-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";

const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const esbuild = await importPkg(APP, "esbuild", ["vite", "@tanstack/react-start"]);

const HERE = dirname(fileURLToPath(import.meta.url));
const DIST = join(APP, "dist");
const CLIENT = join(DIST, "client");
const WORKSPACE = workspaceRoot(APP);
const sfid = (rel, name) => Buffer.from(`${rel}#${name}`).toString("base64url");

// Cloudflare Worker / workerd entry: a Web fetch handler. Static assets are
// served by the platform (Workers Assets binding / a CDN over dist/client);
// this worker handles SSR + server-function RPC. Needs the nodejs_compat flag.
const WORKER = [
  'import handler from "./server-bundle.mjs";',
  'export default { async fetch(request, env, ctx) { return handler.fetch(request); } };',
  '',
].join("\n");

// Node production server: serve built client assets + public, else SSR fetch.
const SERVER = [
  'import { createServer } from "node:http";',
  'import { readFile } from "node:fs/promises";',
  'import { join, extname, dirname } from "node:path";',
  'import { fileURLToPath } from "node:url";',
  'import { register } from "node:module";',
  // Resolve @cloudflare/vite-plugin/server (a virtual the CF plugin injects) to
  // the dev shim: the app imports it with a runtime, variable specifier, which
  // escapes the build-time alias, so a loader hook is needed to catch it.
  'register("./cf-loader.mjs", import.meta.url);',
  'const handler = (await import("./server-bundle.mjs")).default;',
  'const HERE = dirname(fileURLToPath(import.meta.url));',
  'const CLIENT = join(HERE, "client");',
  'const PORT = process.env.PORT || 3000;',
  'const MIME = { ".html":"text/html; charset=utf-8", ".js":"text/javascript", ".css":"text/css", ".json":"application/json", ".wasm":"application/wasm", ".ico":"image/x-icon", ".png":"image/png", ".svg":"image/svg+xml", ".woff2":"font/woff2" };',
  'function readBody(req){ return new Promise((r)=>{ const c=[]; req.on("data",(d)=>c.push(d)); req.on("end",()=>r(Buffer.concat(c))); }); }',
  'createServer(async (req, res) => {',
  '  const url = new URL(req.url, "http://" + (req.headers.host || "localhost"));',
  '  if (req.method === "GET") {',
  '    const rel = url.pathname.replace(/^\\/+/, "");',
  '    if (!rel.startsWith("_serverFn/")) {',
  '      const candidates = [];',
  '      if (rel) candidates.push(rel);',                          // exact asset
  '      candidates.push((rel ? rel.replace(/\\/+$/, "") + "/" : "") + "index.html");', // prerendered doc
  '      for (const c of candidates) {',
  '        try { const bytes = await readFile(join(CLIENT, c)); res.writeHead(200, { "content-type": MIME[extname(c)] || "application/octet-stream" }); res.end(bytes); return; } catch {}',
  '      }',
  '    }',
  '  }',
  '  const body = (req.method === "GET" || req.method === "HEAD") ? undefined : await readBody(req);',
  // No static index.html matched: render `.../index.html` as the `.../` document.
  '  if (url.pathname.endsWith("/index.html")) url.pathname = url.pathname.slice(0, -10);',
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

// Client app-source transform: expand import.meta.glob, then replace
// `.handler(FN)` with `.handler(createClientRpc(id))`.
const clientFnPlugin = {
  name: "server-fn-client",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx)$/ }, (args) => {
      if (args.path.includes("/node_modules/")) return null;
      const loader = args.path.endsWith("tsx") ? "tsx" : "ts";
      const code = transformGlob(readFileSync(args.path, "utf8"), args.path);
      if (!code.includes("createServerFn")) return { contents: code, loader };
      const rel = relative(APP, args.path);
      const fns = serverFns(code);
      if (!fns.length) return { contents: code, loader };
      let out = code;
      for (const f of fns.reverse()) {
        out = out.slice(0, f.open) + `createClientRpc(${JSON.stringify(sfid(rel, f.name))})` + out.slice(f.close);
      }
      return {
        contents: `import { createClientRpc } from "@tanstack/react-start/client-rpc";\n${out}`,
        loader,
      };
    });
  },
};

// Server app-source transform: expand import.meta.glob, then rewrite each
// createServerFn to the provider shape so handlers run in-process during SSR.
const serverFnPlugin = {
  name: "server-fn-server",
  setup(build) {
    build.onLoad({ filter: /\.(ts|tsx)$/ }, (args) => {
      if (args.path.includes("/node_modules/")) return null;
      const loader = args.path.endsWith("tsx") ? "tsx" : "ts";
      const code = transformGlob(readFileSync(args.path, "utf8"), args.path);
      if (!code.includes("createServerFn")) return { contents: code, loader };
      const rel = relative(APP, args.path);
      const re =
        /(^|[\n;])([ \t]*)((?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\()/g;
      let changed = false;
      const out = code.replace(re, (_m, pre, indent, decl, name) => {
        changed = true;
        const meta = `{ id: ${JSON.stringify(sfid(rel, name))}, name: ${JSON.stringify(name)}, filename: ${JSON.stringify(rel)} }`;
        // Exported under the resolver's expected name (matches loader.mjs).
        return `${pre}${indent}export const ${name}_createServerFn_handler = createServerRpc(${meta}, (opts) => ${name}.__executeServer(opts));\n${indent}${decl}${name}_createServerFn_handler, `;
      });
      if (!changed) return { contents: code, loader };
      return {
        contents: `import { createServerRpc } from "@tanstack/react-start/server-rpc";\n${out}`,
        loader,
      };
    });
  },
};

rmSync(DIST, { recursive: true, force: true });
mkdirSync(CLIENT, { recursive: true });

// Shared across both builds: a content-hash asset emitter (client and server
// produce matching /assets/<hash> URLs) and the app's vite plugin container
// (build command; client vs ssr environment changes plugin behavior).
const emit = contentHashEmitter(CLIENT);
const NODE_PATHS = pnpmStorePaths(WORKSPACE);
const clientContainer = await loadPluginContainer(APP, { command: "build", environment: "client" });
const serverContainer = await loadPluginContainer(APP, { command: "build", environment: "ssr" });

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
    "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
    "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
  },
  define: {
    "process.env.NODE_ENV": '"production"', "process.env.TSS_SERVER_FN_BASE": '"/_serverFn/"',
    global: "globalThis", ...viteEnvDefine({ ssr: false, mode: "production" }),
  },
  banner: { js: 'globalThis.process=globalThis.process||{env:{NODE_ENV:"production",TSS_SERVER_FN_BASE:"/_serverFn/"}};globalThis.global=globalThis.global||globalThis;' },
  plugins: [
    makeVitePlugins({ container: clientContainer, appRoot: APP, mode: "prod", emit }),
    clientFnPlugin,
    assetsPlugin({ mode: "prod", server: false, emit }),
    nodeBuiltinShims,
  ],
  nodePaths: NODE_PATHS,
  logLevel: "silent",
});
const entryOut = Object.entries(client.metafile.outputs).find(([, o]) => o.entryPoint);
const clientUrl = "/" + relative(CLIENT, join(APP, entryOut[0]));

// Run the client plugins' generateBundle hooks, writing any files they publish
// (e.g. content-assets' /__content/<collection>/<file>) into dist/client so the
// prod server's ASSETS binding can serve them.
if (clientContainer) {
  await clientContainer.generateBundle(({ type, fileName, source }) => {
    if (type !== "asset" || !fileName || source == null) return;
    const outFile = join(CLIENT, fileName);
    mkdirSync(dirname(outFile), { recursive: true });
    writeFileSync(outFile, source);
  });
}

// 2. Manifest pointing at the hashed client entry.
writeFileSync(
  join(HERE, "manifest.ts"),
  `export const tsrStartManifest = () => ({ routes: { __root__: { preloads: [${JSON.stringify(clientUrl)}], scripts: [{ attrs: { type: "module", async: true, src: ${JSON.stringify(clientUrl)} } }] } } });\n`,
);

// 3. Server bundle (Node, fully bundled so the framework `#`-import aliases
// resolve at build time; node: builtins stay external on platform:node).
await esbuild.build({
  entryPoints: [join(HERE, "server-entry.tsx")],
  bundle: true, format: "esm", platform: "node", jsx: "automatic", minify: true,
  // Code-split rather than emit one file: real ESM chunks preserve top-level
  // await (react-start's server module uses it), which esbuild's single-file
  // lazy-init wrappers cannot always propagate.
  outdir: DIST, entryNames: "server-bundle", chunkNames: "chunks/[name]-[hash]", splitting: true,
  outExtension: { ".js": ".mjs" },
  alias: {
    "#tanstack-router-entry": routerEntry(),
    "#tanstack-start-entry": join(HERE, "start-entry.ts"),
    "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
    "#tanstack-start-server-fn-resolver": join(HERE, "server-fn-resolver.mjs"),
    "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
    // Cloudflare context shim (the CF vite plugin injects this virtual module).
    "@cloudflare/vite-plugin/server": join(HERE, "cf-server.mjs"),
  },
  define: {
    "process.env.NODE_ENV": '"production"', "process.env.TSS_SERVER_FN_BASE": '"/_serverFn/"',
    ...viteEnvDefine({ ssr: true, mode: "production" }),
  },
  // CJS deps bundled into ESM need a working `require` for node builtins.
  banner: { js: "import { createRequire as ___cr } from 'node:module'; const require = ___cr(import.meta.url);" },
  plugins: [
    makeVitePlugins({ container: serverContainer, fallback: clientContainer, appRoot: APP, mode: "prod", emit }),
    serverFnPlugin,
    assetsPlugin({ mode: "prod", server: true, emit }),
  ],
  nodePaths: NODE_PATHS,
  logLevel: "silent",
});

// 4. server.mjs (Node http) + worker.mjs (edge Web fetch handler), plus the
// Cloudflare-context shim and the loader that maps the framework's virtual
// `@cloudflare/vite-plugin/server` specifier to it at runtime. (The edge worker
// gets real bindings from `cloudflare:workers`, so it needs neither.)
writeFileSync(join(DIST, "server.mjs"), SERVER);
writeFileSync(join(DIST, "worker.mjs"), WORKER);
cpSync(join(HERE, "cf-server.mjs"), join(DIST, "cf-server.mjs"));
writeFileSync(
  join(DIST, "cf-loader.mjs"),
  'export async function resolve(spec, ctx, next) {\n' +
    '  if (spec === "@cloudflare/vite-plugin/server")\n' +
    '    return { url: new URL("./cf-server.mjs", import.meta.url).href, shortCircuit: true };\n' +
    '  return next(spec, ctx);\n' +
    '}\n',
);

// 5. Copy public/.
if (existsSync(join(APP, "public"))) cpSync(join(APP, "public"), CLIENT, { recursive: true });

// 6. Prerender (SSG): render each configured route to a static HTML file the
// server serves before falling back to SSR. Routes come from build.prerender.
const prerender = (process.env.OJ_PRERENDER || "").split(",").map((s) => s.trim()).filter(Boolean);
if (prerender.length) {
  const handler = (await import(pathToFileURL(join(DIST, "server-bundle.mjs")).href)).default;
  for (const route of prerender) {
    const res = await handler.fetch(new Request("http://localhost" + route));
    const html = await res.text();
    const rel = route === "/" ? "index.html" : `${route.replace(/^\/+/, "").replace(/\/+$/, "")}/index.html`;
    const outFile = join(CLIENT, rel);
    mkdirSync(dirname(outFile), { recursive: true });
    writeFileSync(outFile, html);
  }
  process.stderr.write(`oj: prerendered ${prerender.length} route(s)\n`);
}

process.stderr.write(`oj: built dist (client ${clientUrl})\n`);
