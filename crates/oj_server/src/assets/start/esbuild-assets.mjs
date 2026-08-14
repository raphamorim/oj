// SPDX-License-Identifier: MIT

import { readFileSync, readdirSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join, dirname, extname, basename, resolve } from "node:path";
import { createHash } from "node:crypto";
import { emptyVirtualStub } from "./resolve-pkg.mjs";

const SUFFIX = /\?(raw|url|inline)$/;
const ASSET_EXT = /\.(png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf|eot|mp4|webm|wasm)$/;

const makeUrlFor = ({ mode, fsBase, emit }) => async (abs) => (mode === "dev" ? fsBase + abs : emit(abs));

export function assetsPlugin({ mode = "dev", server = false, fsBase = "/@oj-start/fs", emit, cssUrls } = {}) {
  const urlFor = makeUrlFor({ mode, fsBase, emit });
  const urlModule = async (abs) => `export default ${JSON.stringify(await urlFor(abs))};`;
  return {
    name: "oj-assets",
    setup(build) {
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
        if (server) return { contents: "export default {};", loader: "js" };
        const href = await urlFor(a.path);
        if (cssUrls && !cssUrls.includes(href)) cssUrls.push(href);
        return { contents: "export default {};", loader: "js" };
      });
    },
  };
}

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
      build.onLoad({ filter: /\.svg$/, namespace: "file" }, (args) => svgModule(args.path, args.path));
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
      const warnedVirtual = new Set();
      const resolveVirtual = async (args) => {
        const rid = await container.resolveId(args.path, args.importer);
        return rid ? { path: rid, namespace: "oj-vite-virtual" } : undefined;
      };
      build.onResolve({ filter: /^virtual:/ }, resolveVirtual);
      build.onResolve({ filter: /^\0/ }, resolveVirtual);
      build.onLoad({ filter: /.*/, namespace: "oj-vite-virtual" }, async (args) => {
        let code = await container.load(args.path);
        if (code == null && fallback) code = await fallback.load(args.path);
        if (code == null) {
          if (!warnedVirtual.has(args.path)) {
            warnedVirtual.add(args.path);
            process.stderr.write(
              `oj: plugin virtual "${args.path}" produced no content in the dev client bundle; ` +
                `emitting an empty module. This virtual likely needs the full build graph oj does not run in dev.\n`,
            );
          }
          return { contents: emptyVirtualStub(appRoot, args.path), loader: "js", resolveDir: appRoot };
        }
        return { contents: code, loader: "js", resolveDir: appRoot };
      });
      build.onLoad({ filter: /\.mdx?$/ }, async (args) => {
        const out = await container.transform(readFileSync(args.path, "utf8"), args.path);
        return out == null ? undefined : { contents: out, loader: "jsx", resolveDir: dirname(args.path) };
      });
    },
  };
}

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

export function contentHashEmitter(clientDir, compileCss) {
  const assetsDir = join(clientDir, "assets");
  const seen = new Set();
  const emitting = new Set();
  const cssUrls = [];

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
        const url = write(absPath, Buffer.from(await rewriteCss(css, dirname(absPath)), "utf8"));
        if (!cssUrls.includes(url)) cssUrls.push(url);
        return url;
      } finally {
        emitting.delete(absPath);
      }
    }
    return write(absPath, readFileSync(absPath));
  }

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

  emit.cssUrls = () => cssUrls.slice();
  return emit;
}

export function needsCssCompile(src) {
  return src.includes("tailwindcss") || src.includes("@tailwind") || src.includes("@plugin") || src.includes("@apply");
}
