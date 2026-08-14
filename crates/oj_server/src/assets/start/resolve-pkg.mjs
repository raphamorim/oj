// SPDX-License-Identifier: MIT

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { readFileSync, readdirSync } from "node:fs";

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

export async function importPkg(root, spec, preferred = []) {
  const p = makeResolver(root)(spec, preferred);
  const m = await import(pathToFileURL(p).href);
  return m.default ?? m;
}

export function viteEnvDefine({ ssr = false, mode = "development" } = {}) {
  const env = { MODE: mode, DEV: mode !== "production", PROD: mode === "production", SSR: !!ssr, BASE_URL: "/" };
  for (const [k, v] of Object.entries(process.env)) if (k.startsWith("VITE_")) env[k] = v;
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
