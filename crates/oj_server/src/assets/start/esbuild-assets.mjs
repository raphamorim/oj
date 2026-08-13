// SPDX-License-Identifier: MIT
// Shared esbuild building blocks for the TanStack Start adapter, used by both
// the dev client bundle and the prod build so the two stay in lockstep:
//   - assetsPlugin: Vite-style ?url / ?raw / ?inline / bare-asset / css imports
//   - makeVitePlugins: routes virtual: ids, .mdx, and .svg through the app's
//     vite plugin container (svgr, mdx, virtual modules)
//   - nodeBuiltinShims: browser shims for node: (and bare) builtins
//   - pnpmStorePaths / contentHashEmitter: pnpm phantom-dep resolution + a
//     content-addressed asset emitter (client and server emit matching URLs
//     with no shared manifest, since the URL is a hash of the bytes)
// In dev, asset URLs point at the server's /@oj-start/fs route; in prod they are
// emitted into dist/client/assets and referenced by hash.
import { readFileSync, readdirSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join, dirname, extname, basename, resolve } from "node:path";
import { createHash } from "node:crypto";

const SUFFIX = /\?(raw|url|inline)$/;
// Bare asset imports (Vite treats these as a URL by default, e.g. `import logo
// from "./logo.png"`). `.svg` is excluded: svgr (a vite plugin) may turn it
// into a React component, so it is routed through the plugin container with a
// URL fallback for svgs svgr doesn't claim.
const ASSET_EXT = /\.(png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf|eot|mp4|webm|wasm)$/;

// dev: the file is streamed by the server's /@oj-start/fs route (relative url()
// refs resolve against the same dir). prod: the file is emitted into
// dist/client/assets under a content hash and referenced absolutely.
// Async because the prod emitter may compile the asset (Tailwind css). In dev
// it resolves immediately to the /@oj-start/fs URL.
const makeUrlFor = ({ mode, fsBase, emit }) => async (abs) => (mode === "dev" ? fsBase + abs : emit(abs));

export function assetsPlugin({ mode = "dev", server = false, fsBase = "/@oj-start/fs", emit } = {}) {
  const urlFor = makeUrlFor({ mode, fsBase, emit });
  const urlModule = async (abs) => `export default ${JSON.stringify(await urlFor(abs))};`;
  return {
    name: "oj-assets",
    setup(build) {
      // Route a specifier through esbuild's own resolver (so aliases, package
      // `imports`, and node_modules all work), then park it in our namespace.
      // pluginData guards the re-entrant resolve against infinite recursion.
      const route = (namespace) => async (args) => {
        if (args.pluginData?.ojAsset) return undefined;
        const clean = args.path.replace(SUFFIX, "");
        const r = await build.resolve(clean, {
          kind: args.kind,
          resolveDir: args.resolveDir,
          importer: args.importer,
          pluginData: { ojAsset: true },
        });
        if (r.errors.length) return { errors: r.errors };
        return { path: r.path, namespace };
      };

      build.onResolve({ filter: /\?raw$/ }, route("oj-raw"));
      build.onResolve({ filter: /\?url$/ }, route("oj-url"));
      build.onResolve({ filter: /\?inline$/ }, route("oj-inline"));
      build.onResolve({ filter: ASSET_EXT }, route("oj-url"));
      build.onResolve({ filter: /\.css$/ }, route("oj-css"));

      build.onLoad({ filter: /.*/, namespace: "oj-raw" }, (a) => ({
        contents: `export default ${JSON.stringify(readFileSync(a.path, "utf8"))};`,
        loader: "js",
      }));

      build.onLoad({ filter: /.*/, namespace: "oj-url" }, async (a) => ({
        contents: await urlModule(a.path),
        loader: "js",
      }));

      build.onLoad({ filter: /.*/, namespace: "oj-inline" }, (a) => ({
        contents: readFileSync(a.path),
        loader: "dataurl",
      }));

      build.onLoad({ filter: /.*/, namespace: "oj-css" }, async (a) => {
        // Styling is a client concern; on the server a css import is a no-op.
        if (server) return { contents: "export default {};", loader: "js" };
        // Inject a <link> to the stylesheet (dev-served or emitted), which keeps
        // relative url() refs resolving against the file's own directory.
        const href = await urlFor(a.path);
        return {
          contents:
            `const l=document.createElement("link");l.rel="stylesheet";` +
            `l.href=${JSON.stringify(href)};document.head.appendChild(l);`,
          loader: "js",
        };
      });
    },
  };
}

// Route virtual: ids, .mdx, and .svg through the app's vite plugin container.
// `.svg` is svgr (a component) when the container claims it, else an asset URL.
// `fallback` is a second container consulted when the primary can't `load` a
// virtual: the ssr build's plugins sometimes error expecting cross-environment
// state (Vite builds client first and shares it), so the client container's
// output is used instead.
export function makeVitePlugins({ container, fallback, appRoot, mode = "dev", fsBase = "/@oj-start/fs", emit } = {}) {
  const urlFor = makeUrlFor({ mode, fsBase, emit });
  return {
    name: "oj-vite-plugins",
    setup(build) {
      const svgModule = async (path, id) => {
        if (container) {
          const code = await container.load(id);
          if (code != null) return { contents: code, loader: "js", resolveDir: dirname(path) };
        }
        return { contents: `export default ${JSON.stringify(await urlFor(path))};`, loader: "js" };
      };
      // Bare `.svg` in the default (file) namespace. Registered unconditionally
      // so .svg always has a loader. The explicit namespace keeps it from also
      // claiming the ?react-tagged files parked in the oj-svg-react namespace.
      build.onLoad({ filter: /\.svg$/, namespace: "file" }, (args) => svgModule(args.path, args.path));
      // svgr's explicit-component query `foo.svg?react`: esbuild's resolver
      // can't find the queried path, so strip the query, resolve the real .svg,
      // and hand the ?react id to the container so svgr emits a component.
      build.onResolve({ filter: /\.svg\?react$/ }, async (args) => {
        if (args.pluginData?.ojSvg) return undefined;
        const r = await build.resolve(args.path.slice(0, -"?react".length), {
          kind: args.kind, resolveDir: args.resolveDir, importer: args.importer,
          pluginData: { ojSvg: true },
        });
        return r.errors.length ? { errors: r.errors } : { path: r.path, namespace: "oj-svg-react" };
      });
      build.onLoad({ filter: /.*/, namespace: "oj-svg-react" }, (args) =>
        svgModule(args.path, args.path + "?react"),
      );
      if (!container) return;
      const resolveVirtual = async (args) => {
        const rid = await container.resolveId(args.path, args.importer);
        return rid ? { path: rid, namespace: "oj-vite-virtual" } : undefined;
      };
      build.onResolve({ filter: /^virtual:/ }, resolveVirtual);
      build.onResolve({ filter: /^\0/ }, resolveVirtual);
      build.onLoad({ filter: /.*/, namespace: "oj-vite-virtual" }, async (args) => {
        let code = await container.load(args.path);
        if (code == null && fallback) code = await fallback.load(args.path);
        return code == null ? undefined : { contents: code, loader: "js", resolveDir: appRoot };
      });
      build.onLoad({ filter: /\.mdx?$/ }, async (args) => {
        const out = await container.transform(readFileSync(args.path, "utf8"), args.path);
        return out == null ? undefined : { contents: out, loader: "jsx", resolveDir: dirname(args.path) };
      });
    },
  };
}

// Node builtins reach the browser bundle `node:`-prefixed and bare (`import
// "url"` in older deps). SSR-only framework code (stream rendering) can land in
// the client graph and named-import from them, so a few need real named exports
// (browser globals where they exist; inert stubs otherwise -- that code is not
// reached client-side). async_hooks gets a working synchronous AsyncLocalStorage.
const ALS =
  "export class AsyncLocalStorage{getStore(){return this._s}" +
  "run(s,cb,...a){const p=this._s;this._s=s;try{return cb(...a)}finally{this._s=p}}" +
  "enterWith(s){this._s=s}exit(cb,...a){const p=this._s;this._s=undefined;try{return cb(...a)}finally{this._s=p}}" +
  "disable(){this._s=undefined}}export default {AsyncLocalStorage};";
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
const BARE_BUILTINS =
  /^(assert|buffer|child_process|cluster|console|constants|crypto|dgram|dns|domain|events|fs|http|http2|https|module|net|os|path|perf_hooks|process|punycode|querystring|readline|repl|stream|stream\/web|string_decoder|sys|timers|tls|tty|url|util|v8|vm|worker_threads|zlib|async_hooks)$/;
function shimSource(spec) {
  const name = spec.replace(/^node:/, "");
  if (name === "async_hooks") return ALS;
  if (name === "stream/web") return SHIM_STREAM_WEB;
  if (name === "stream") return SHIM_STREAM;
  if (name === "punycode") return SHIM_PUNYCODE;
  return "export default {};";
}
export const nodeBuiltinShims = {
  name: "node-builtin-shims",
  setup(build) {
    build.onResolve({ filter: /^node:/ }, (a) => ({ path: a.path, namespace: "node-shim" }));
    build.onResolve({ filter: BARE_BUILTINS }, (a) => ({ path: a.path, namespace: "node-shim" }));
    build.onLoad({ filter: /.*/, namespace: "node-shim" }, (a) => ({ contents: shimSource(a.path), loader: "js" }));
  },
};

// Resolve phantom dependencies (a package importing something it doesn't
// declare, e.g. @babel/runtime helpers) by giving esbuild pnpm's virtual store
// as NODE_PATH-style fallback dirs. esbuild consults `nodePaths` only when
// normal resolution fails, natively, with no per-import JS cost.
export function pnpmStorePaths(workspaceRoot) {
  const paths = [];
  const pnpmDir = join(workspaceRoot, "node_modules/.pnpm");
  try {
    for (const e of readdirSync(pnpmDir)) {
      const nm = join(pnpmDir, e, "node_modules");
      if (existsSync(nm)) paths.push(nm);
    }
  } catch {}
  return paths;
}

// Farthest ancestor with a node_modules (the pnpm/workspace root).
export function workspaceRoot(app) {
  let best = app;
  for (let cur = app; ; ) {
    const parent = dirname(cur);
    if (parent === cur) break;
    if (existsSync(join(parent, "node_modules"))) best = parent;
    cur = parent;
  }
  return best;
}

// A content-addressed asset emitter: copies a file into `${clientDir}/assets`
// under a name that includes an 8-char hash of its bytes and returns the
// absolute URL. Client and server builds independently produce identical URLs
// for identical bytes, so no shared manifest is needed. CSS is special-cased:
// Tailwind/PostCSS stylesheets are compiled first (via `compileCss`), then their
// relative url() references (fonts, images) are emitted too and rewritten to the
// hashed URLs, otherwise they'd 404 (the flat /assets layout doesn't preserve
// the source's relative directory structure). Async because compilation is.
export function contentHashEmitter(clientDir, compileCss) {
  const assetsDir = join(clientDir, "assets");
  const seen = new Set();
  const emitting = new Set();

  const write = (absPath, buf) => {
    const ext = extname(absPath);
    const hash = createHash("sha256").update(buf).digest("hex").slice(0, 8);
    const name = basename(absPath, ext).replace(/[^\w.-]+/g, "_") + "-" + hash + ext;
    if (!seen.has(name)) {
      mkdirSync(assetsDir, { recursive: true });
      writeFileSync(join(assetsDir, name), buf);
      seen.add(name);
    }
    return "/assets/" + name;
  };

  async function emit(absPath) {
    if (extname(absPath).toLowerCase() === ".css" && !emitting.has(absPath)) {
      emitting.add(absPath);
      try {
        let css = readFileSync(absPath, "utf8");
        if (compileCss && needsCssCompile(css)) css = await compileCss(absPath, css);
        return write(absPath, Buffer.from(await rewriteCss(css, dirname(absPath)), "utf8"));
      } finally {
        emitting.delete(absPath);
      }
    }
    return write(absPath, readFileSync(absPath));
  }

  // Rewrite url(...) references relative to the CSS file: emit each referenced
  // asset and substitute its hashed URL. Skips data:, absolute, and rooted refs.
  async function rewriteCss(css, dir) {
    const re = /url\(\s*(['"]?)([^'")]+)\1\s*\)/g;
    let out = "", last = 0, m;
    while ((m = re.exec(css))) {
      out += css.slice(last, m.index);
      last = m.index + m[0].length;
      const t = m[2].trim();
      if (/^(data:|https?:|\/\/|#|\/)/.test(t)) { out += m[0]; continue; }
      const clean = t.replace(/[?#].*$/, "");
      const suffix = t.slice(clean.length);
      const abs = resolve(dir, clean);
      if (!existsSync(abs)) { out += m[0]; continue; }
      out += `url(${JSON.stringify((await emit(abs)) + suffix)})`;
    }
    return out + css.slice(last);
  }

  return emit;
}

// A stylesheet needs Tailwind/PostCSS compilation (v4 `@import "tailwindcss"` or
// the `@tailwind` / `@plugin` / `@apply` at-rules).
export function needsCssCompile(src) {
  return src.includes("tailwindcss") || src.includes("@tailwind") || src.includes("@plugin") || src.includes("@apply");
}
