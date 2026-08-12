// SPDX-License-Identifier: MIT
// esbuild plugin for Vite-style asset conventions used by TanStack Start apps:
//   import u from "./x.png?url"      -> a URL string
//   import s from "./x.txt?raw"      -> the file contents as a string
//   import d from "./x.svg?inline"   -> a data: URI
//   import "./x.css"                 -> injects the stylesheet (side effect)
// In dev, ?url and plain-css resolve to `${fsBase}${absPath}` so the dev server
// streams the real file (relative url() refs inside CSS resolve against the
// same directory). In prod, esbuild's file/css loaders emit hashed assets.
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join } from "node:path";

const SUFFIX = /\?(raw|url|inline)$/;
// Bare asset imports (Vite treats these as a URL by default, e.g. `import logo
// from "./logo.svg"`). `.svg?react` (svgr) is deliberately not covered here.
const ASSET_EXT = /\.(svg|png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf|eot|mp4|webm|wasm)$/;

export function assetsPlugin({ mode = "dev", fsBase = "/@oj-start/fs" } = {}) {
  const devUrl = (abs) => `export default ${JSON.stringify(fsBase + abs)};`;
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

      build.onLoad({ filter: /.*/, namespace: "oj-url" }, (a) =>
        mode === "dev"
          ? { contents: devUrl(a.path), loader: "js" }
          : { contents: readFileSync(a.path), loader: "file" });

      build.onLoad({ filter: /.*/, namespace: "oj-inline" }, (a) => ({
        contents: readFileSync(a.path),
        loader: "dataurl",
      }));

      build.onLoad({ filter: /.*/, namespace: "oj-css" }, (a) => {
        if (mode !== "dev") return { contents: readFileSync(a.path, "utf8"), loader: "css" };
        // Dev: inject a <link> to the dev-served stylesheet (keeps relative
        // url() refs resolving, and lets the server process CSS if it needs to).
        const href = fsBase + a.path;
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

// Resolve phantom dependencies (a package importing something it doesn't
// declare, e.g. @babel/runtime helpers) by giving esbuild pnpm's virtual store
// as NODE_PATH-style fallback dirs. esbuild consults `nodePaths` only when
// normal resolution fails, natively, with no per-import JS cost -- unlike a
// resolver plugin, which would wrap every bare import and stall a large graph.
// Vite's dev server never hits phantom deps in an unvisited route because it
// serves modules lazily; our client is one eager bundle over the whole graph.
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
