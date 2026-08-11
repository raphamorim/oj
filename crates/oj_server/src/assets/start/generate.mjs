// SPDX-License-Identifier: MIT
// Generate routeTree.gen.ts from src/routes via @tanstack/router-generator.
import { Generator, getConfig } from "@tanstack/router-generator";
const root = process.env.OJ_APP_ROOT ?? process.cwd();
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
