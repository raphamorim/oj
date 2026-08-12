// SPDX-License-Identifier: MIT
// Resolve a package from an app that may use pnpm's strict, non-hoisted layout,
// where a transitive dep (router-generator, esbuild, ...) isn't reachable from
// the app root. Cascade: the app root (Node walks up node_modules), then
// explicit anchor packages, then any of the app's direct deps as an anchor
// (a transitive dep is reachable from whichever direct dep pulls it in).
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname } from "node:path";
import { readFileSync } from "node:fs";

export function makeResolver(root) {
  const appRequire = createRequire(pathToFileURL(root + "/package.json").href);
  let directDeps = [];
  try {
    const pkg = JSON.parse(readFileSync(root + "/package.json", "utf8"));
    directDeps = Object.keys({ ...pkg.dependencies, ...pkg.devDependencies });
  } catch {}
  const anchorDir = (a) => {
    try { return dirname(appRequire.resolve(a + "/package.json")); } catch { return null; }
  };
  return function resolvePkg(spec, preferred = []) {
    try { return appRequire.resolve(spec); } catch {}
    for (const a of [...preferred, ...directDeps]) {
      const dir = anchorDir(a);
      if (!dir) continue;
      try { return appRequire.resolve(spec, { paths: [dir] }); } catch {}
    }
    throw new Error(`oj: cannot resolve '${spec}' from ${root}`);
  };
}

// Resolve + dynamic-import a package; return the usable namespace (CJS default
// unwrapped so callers can destructure named exports).
export async function importPkg(root, spec, preferred = []) {
  const p = makeResolver(root)(spec, preferred);
  const m = await import(pathToFileURL(p).href);
  return m.default ?? m;
}

// esbuild `define` for Vite's `import.meta.env`: the standard MODE/DEV/PROD/SSR/
// BASE_URL plus every VITE_* var in the environment. Defining the whole object
// (not just dotted keys) keeps `const { VITE_X } = import.meta.env` working and
// makes unknown keys read as undefined instead of throwing.
export function viteEnvDefine({ ssr = false, mode = "development" } = {}) {
  const env = { MODE: mode, DEV: mode !== "production", PROD: mode === "production", SSR: !!ssr, BASE_URL: "/" };
  for (const [k, v] of Object.entries(process.env)) if (k.startsWith("VITE_")) env[k] = v;
  return { "import.meta.env": JSON.stringify(env) };
}
