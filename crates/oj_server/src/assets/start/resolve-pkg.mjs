// SPDX-License-Identifier: MIT

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { readFileSync, readdirSync } from "node:fs";

function depsOf(pkgJsonPath) {
  try {
    const pkg = JSON.parse(readFileSync(pkgJsonPath, "utf8"));
    return Object.keys({ ...pkg.dependencies, ...pkg.devDependencies, ...pkg.optionalDependencies });
  } catch { return []; }
}

// Locate a dependency's own package.json from `req`'s vantage point. The
// "<name>/package.json" subpath is the fast path; some packages don't expose it
// through their exports map, so fall back to resolving the entry and walking up.
function pkgJsonOf(req, name) {
  try { return req.resolve(name + "/package.json"); } catch {}
  try {
    let dir = dirname(req.resolve(name));
    for (let i = 0; i < 16; i++) {
      const cand = join(dir, "package.json");
      try { if (JSON.parse(readFileSync(cand, "utf8")).name === name) return cand; } catch {}
      const parent = dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
  } catch {}
  return null;
}

export function makeResolver(root) {
  const appRequire = createRequire(pathToFileURL(root + "/package.json").href);
  const directDeps = depsOf(root + "/package.json");
  return function resolvePkg(spec, preferred = []) {
    try { return appRequire.resolve(spec); } catch {}
    // A transitive dependency is not resolvable from the app root under a
    // strict (pnpm) layout, so walk the dependency graph breadth-first,
    // re-anchoring resolution at each package we reach until `spec` resolves.
    const seen = new Set();
    let frontier = [];
    for (const a of [...preferred, ...directDeps]) {
      const pj = pkgJsonOf(appRequire, a);
      if (pj) frontier.push(pj);
    }
    for (let depth = 0; depth < 8 && frontier.length; depth++) {
      const next = [];
      for (const pj of frontier) {
        if (seen.has(pj)) continue;
        seen.add(pj);
        const req = createRequire(pathToFileURL(pj).href);
        try { return req.resolve(spec); } catch {}
        for (const d of depsOf(pj)) {
          const dpj = pkgJsonOf(req, d);
          if (dpj && !seen.has(dpj)) next.push(dpj);
        }
      }
      frontier = next;
    }
    throw new Error(`oj: cannot resolve '${spec}' from ${root}`);
  };
}

export async function importPkg(root, spec, preferred = []) {
  const p = makeResolver(root)(spec, preferred);
  const m = await import(pathToFileURL(p).href);
  return m.default ?? m;
}

/// JSX transform options for rolldown / oxc-transform from `OJ_JSX` (the config's
/// `oxc.jsx` / `esbuild.jsx*`, serialized by oj). Defaults to the automatic React
/// runtime; a file's own `@jsx*` pragma comments still win inside oxc.
export function jsxTransformOptions(development) {
  let cfg = {};
  try { cfg = JSON.parse(process.env.OJ_JSX || "{}") || {}; } catch {}
  const classic = cfg.runtime === "classic";
  const out = { runtime: classic ? "classic" : "automatic" };
  if (development != null) out.development = development;
  if (!classic && cfg.importSource) out.importSource = cfg.importSource;
  if (classic && cfg.pragma) out.pragma = cfg.pragma;
  if (classic && cfg.pragmaFrag) out.pragmaFrag = cfg.pragmaFrag;
  return out;
}

export function viteEnvDefine({ ssr = false, mode = "development", env: envSource = process.env } = {}) {
  // Vite: DEV/PROD follow NODE_ENV (isProduction), MODE is the mode itself.
  const nodeEnv = envSource.NODE_ENV || (mode === "production" ? "production" : "development");
  const env = { MODE: mode, DEV: nodeEnv !== "production", PROD: nodeEnv === "production", SSR: !!ssr, BASE_URL: "/" };
  for (const [k, v] of Object.entries(envSource)) if (k.startsWith("VITE_")) env[k] = v;
  return { "import.meta.env": JSON.stringify(env) };
}

const SRC_EXT = /\.(ts|tsx|js|jsx|mjs|cjs|mts|cts)$/;
export function emptyVirtualStub(appRoot, resolvedId) {
  const original = resolvedId.replace(/^\0/, "");
  const re = new RegExp(
    `import\\s+(?:type\\s+)?[^;{]*\\{([^}]*)\\}[^;]*?from\\s*["']${
      original.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
    }["']`,
    "g",
  );
  const names = new Set();
  const scan = (dir) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (e.name === "node_modules" || e.name.startsWith(".")) continue;
      const p = join(dir, e.name);
      if (e.isDirectory()) {
        scan(p);
        continue;
      }
      if (!SRC_EXT.test(e.name)) continue;
      let src;
      try {
        src = readFileSync(p, "utf8");
      } catch {
        continue;
      }
      if (!src.includes(original)) continue;
      let m;
      while ((m = re.exec(src))) {
        for (let spec of m[1].split(",")) {
          spec = spec.trim();
          if (!spec || spec.startsWith("type ")) continue;
          const name = spec.split(/\s+as\s+/)[0].trim();
          if (name && name !== "default" && /^[A-Za-z_$][\w$]*$/.test(name)) names.add(name);
        }
      }
    }
  };
  scan(join(appRoot, "src"));
  let out = "export default undefined;\n";
  for (const n of names) out += `export const ${n} = undefined;\n`;
  return out;
}
