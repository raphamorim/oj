// SPDX-License-Identifier: MIT
// Generate routeTree.gen.ts from src/routes via @tanstack/router-generator.
// The generator is build tooling that ships as a transitive dep, so under
// pnpm's strict layout it isn't resolvable from the app root; importPkg
// resolves it through the direct deps that depend on it.
import { importPkg } from "./resolve-pkg.mjs";

const root = process.env.OJ_APP_ROOT ?? process.cwd();
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
  },
  root,
);
await new Generator({ config, root }).run();
process.stderr.write("oj: route tree generated\n");
