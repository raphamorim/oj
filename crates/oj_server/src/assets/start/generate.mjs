// SPDX-License-Identifier: MIT

import { readFileSync } from "node:fs";
import { join as pathJoin } from "node:path";

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

// getConfig merges as { ...tsr.config.json, ...inline }, so anything passed
// inline cannot be configured by the app. Only `target` is genuinely ours to
// decide; the rest are the generator's own documented settings, and an app that
// keeps its route tree somewhere other than src/routeTree.gen.ts has to be able
// to say so. Pass them as defaults under the file rather than overrides above it.
const OJ_DEFAULTS = {
  routesDirectory: "./src/routes",
  generatedRouteTree: "./src/routeTree.gen.ts",
  autoCodeSplitting: false,
  routeFileIgnorePattern: "\\.(test|spec|stories|bench)\\.|\\.d\\.ts$",
};
const configured = (() => {
  try {
    return JSON.parse(readFileSync(pathJoin(root, "tsr.config.json"), "utf8"));
  } catch {
    return {};
  }
})();
const config = getConfig(
  {
    ...Object.fromEntries(
      Object.entries(OJ_DEFAULTS).filter(([k]) => !(k in configured)),
    ),
    target: "react",
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
