// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { mkdirSync, rmSync, readFileSync, existsSync } from "node:fs";
import path from "node:path";
import builtinModules from "node:module";

const { root, outDir, entries, include = [], exclude = [] } = JSON.parse(process.argv[2]);

function detectEntries() {
  const html = path.join(root, "index.html");
  if (!existsSync(html)) return [];
  const src = readFileSync(html, "utf8");
  const found = [];
  for (const tag of src.match(/<script\b[^>]*>/gi) || []) {
    if (!/type\s*=\s*["']module["']/i.test(tag)) continue;
    const m = tag.match(/\bsrc\s*=\s*["']([^"']+)["']/i);
    if (!m || /^https?:/i.test(m[1])) continue;
    const abs = path.join(root, m[1].replace(/^\//, ""));
    if (existsSync(abs)) found.push(abs);
  }
  return found;
}

const entryList = entries && entries.length ? entries : detectEntries();

const req = createRequire(path.join(root, "package.json"));
const esbuild = await import(pathToFileURL(req.resolve("esbuild")).href).then((m) => m.default ?? m);

const NODE_BUILTINS = new Set([...builtinModules.builtinModules, ...builtinModules.builtinModules.map((m) => "node:" + m)]);
const isBare = (id) => id && !id.startsWith(".") && !id.startsWith("/") && !id.startsWith("\0") && !NODE_BUILTINS.has(id);
const excludeSet = new Set(exclude);

async function scan() {
  const found = new Set();
  const collector = {
    name: "oj-scan",
    setup(build) {
      build.onResolve({ filter: /.*/ }, (args) => {
        if (args.kind === "entry-point") return null;
        if (isBare(args.path) && !excludeSet.has(args.path)) {
          found.add(args.path);
          return { path: args.path, external: true };
        }
        if (!args.path.startsWith(".") && !path.isAbsolute(args.path)) return { path: args.path, external: true };
        return null;
      });
    },
  };
  try {
    await esbuild.build({
      entryPoints: entryList,
      bundle: true,
      write: false,
      logLevel: "silent",
      platform: "browser",
      loader: { ".js": "jsx", ".ts": "ts", ".tsx": "tsx", ".jsx": "jsx" },
      jsx: "automatic",
      plugins: [collector],
      metafile: false,
    });
  } catch {}
  return found;
}

const scanned = await scan();
for (const inc of include) scanned.add(inc);
const deps = [...scanned].filter((d) => !excludeSet.has(d));

const entryPoints = {};
const nameOf = {};
for (const dep of deps) {
  let entry;
  try {
    entry = req.resolve(dep);
  } catch {
    continue;
  }
  const name = dep.replace(/^@/, "").replace(/[/@]/g, "_");
  entryPoints[name] = entry;
  nameOf[dep] = name;
}

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

const metadata = {};
if (Object.keys(entryPoints).length) {
  const result = await esbuild.build({
    entryPoints,
    bundle: true,
    splitting: true,
    format: "esm",
    outdir: outDir,
    outExtension: { ".js": ".mjs" },
    platform: "browser",
    define: { "process.env.NODE_ENV": JSON.stringify("development") },
    mainFields: ["browser", "module", "main"],
    conditions: ["browser", "module", "import", "development"],
    target: "esnext",
    logLevel: "silent",
    metafile: true,
    write: true,
  });
  const exportsOf = {};
  for (const [out, meta] of Object.entries(result.metafile.outputs)) {
    if (meta.entryPoint) exportsOf[path.basename(out)] = meta.exports || [];
  }
  for (const [dep, name] of Object.entries(nameOf)) {
    const file = `${name}.mjs`;
    const exports = exportsOf[file] || [];
    const needsInterop = exports.length === 0 || (exports.length === 1 && exports[0] === "default");
    metadata[dep] = { file, needsInterop, exports };
  }
}

process.stdout.write(JSON.stringify({ metadata }));
