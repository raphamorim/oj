// SPDX-License-Identifier: MIT

import { existsSync, readFileSync } from "node:fs";
import { dirname, join as pathJoin, relative as pathRelative, resolve as pathResolve, sep } from "node:path";

import { importPkg } from "./resolve-pkg.mjs";

const root = process.env.OJ_APP_ROOT ?? process.cwd();
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";

process.removeAllListeners("warning");
process.on("warning", (w) => {
  const m = String(w?.message ?? w);
  if (!/replaceRouteChunk|circular dependency|non-existent property/.test(m)) {
    process.stderr.write(m + "\n");
  }
});
const skipped = [];
const ROUTE_NOISE =
  /does not export a Route|If this file is not intended|Current configuration:|routeFileIgnore|Rename the file|will not be included in the route tree|to be a route/;
const origConsole = { warn: console.warn, error: console.error, log: console.log, info: console.info };
const filterRouteNoise = (orig) => (...args) => {
  const msg = args.map((a) => (typeof a === "string" ? a : "")).join(" ");
  const m = msg.match(/Route file "([^"]+)" does not export a Route/);
  if (m) {
    skipped.push(m[1]);
    return;
  }
  if (ROUTE_NOISE.test(msg)) return;
  orig(...args);
};
for (const k of Object.keys(origConsole)) console[k] = filterRouteNoise(origConsole[k]);

const { Generator, getConfig } = await importPkg(root, "@tanstack/router-generator", [
  "@tanstack/react-router",
  "@tanstack/react-start",
]);

const GENERATED_ROUTE_TREE = "./src/routeTree.gen.ts";

// The framework's own Vite plugin appends a `declare module` block naming the
// router entry; the app's Register interface, and so every typed route, is
// keyed off it. Without the same block the tree written here is a different
// file from the one the framework's tooling produces.
function routerEntry() {
  try {
    const declared = JSON.parse(readFileSync(pathJoin(root, "package.json"), "utf8"))
      .imports?.["#tanstack-router-entry"];
    if (typeof declared === "string") {
      const p = pathResolve(root, declared);
      if (existsSync(p)) return p;
    }
  } catch {}
  for (const ext of [".tsx", ".ts", ".jsx", ".js"]) {
    const p = pathResolve(root, "src/router" + ext);
    if (existsSync(p)) return p;
  }
  return pathResolve(root, "src/router.tsx");
}

function moduleDeclaration() {
  let rel = pathRelative(dirname(pathResolve(root, GENERATED_ROUTE_TREE)), routerEntry());
  if (!rel.startsWith(".")) rel = "./" + rel;
  rel = rel.split(sep).join("/");
  return [
    `import type { getRouter } from '${rel}'`,
    "import type { createStart } from '@tanstack/react-start'",
    "declare module '@tanstack/react-start' {",
    "  interface Register {",
    "    ssr: true",
    "    router: Awaited<ReturnType<typeof getRouter>>",
    "  }",
    "}",
  ].join("\n");
}

const config = getConfig(
  {
    target: "react",
    routesDirectory: "./src/routes",
    generatedRouteTree: GENERATED_ROUTE_TREE,
    autoCodeSplitting: false,
    routeFileIgnorePattern: "\\.(test|spec|stories|bench)\\.|\\.d\\.ts$",
    routeTreeFileFooter: [moduleDeclaration()],
  },
  root,
);
await new Generator({ config, root }).run();
for (const k of Object.keys(origConsole)) console[k] = origConsole[k];

if (skipped.length) {
  process.stderr.write(
    `${OJ}${_ojTTY ? "" : ":"} route tree generated (${skipped.length} file(s) under src/routes export no Route, skipped)\n`,
  );
} else {
  process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} route tree generated\n`);
}
