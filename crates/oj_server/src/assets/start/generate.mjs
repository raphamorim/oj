// SPDX-License-Identifier: MIT
// Generate routeTree.gen.ts from src/routes via @tanstack/router-generator.
// The generator is build tooling that ships as a transitive dep, so under
// pnpm's strict layout it isn't resolvable from the app root; importPkg
// resolves it through the direct deps that depend on it.
import { importPkg } from "./resolve-pkg.mjs";

const root = process.env.OJ_APP_ROOT ?? process.cwd();

// Quiet the router-generator's noise: a Node process warning about
// `replaceRouteChunk` (a harmless circular-dependency access in its internals),
// and a verbose multi-line block for every src/routes file that exports no
// Route (common when an app wires its router manually or colocates helpers).
// Collapse the per-file warnings into one concise summary; keep everything else.
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

const config = getConfig(
  {
    target: "react",
    routesDirectory: "./src/routes",
    generatedRouteTree: "./src/routeTree.gen.ts",
    autoCodeSplitting: false,
    // Colocated tests/stories/type-decls under src/routes are not routes;
    // ignore them so the generator neither includes nor warns about them.
    routeFileIgnorePattern: "\\.(test|spec|stories|bench)\\.|\\.d\\.ts$",
  },
  root,
);
await new Generator({ config, root }).run();
for (const k of Object.keys(origConsole)) console[k] = origConsole[k];

if (skipped.length) {
  process.stderr.write(
    `oj: route tree generated (${skipped.length} file(s) under src/routes export no Route, skipped)\n`,
  );
} else {
  process.stderr.write("oj: route tree generated\n");
}
