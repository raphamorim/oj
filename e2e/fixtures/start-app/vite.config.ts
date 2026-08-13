import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import mdx from "@mdx-js/rollup";
import svgr from "vite-plugin-svgr";
import { defineConfig } from "vite";
import type { Plugin } from "vite";

// A tiny plugin that owns a `virtual:build-info` module (id resolution + load).
// Exercises oj's plugin-container bridge on both the client bundle and the SSR
// loader, exactly as a real app's virtual modules do.
function buildInfoPlugin(): Plugin {
  const VID = "virtual:build-info";
  const RESOLVED = "\0" + VID;
  return {
    name: "fixture-build-info",
    resolveId(id) {
      if (id === VID) return RESOLVED;
    },
    load(id) {
      if (id === RESOLVED) return `export const buildTag = "fixture-virtual-ok";`;
    },
  };
}

export default defineConfig({
  // A non-default publicDir, to exercise oj adopting vite's publicDir.
  publicDir: "public",
  plugins: [
    buildInfoPlugin(),
    // .mdx before react/tanstack so its output is plain JSX for them to handle.
    mdx(),
    // exportType:"default" makes a bare `.svg` import a component; the second
    // include pattern also claims the explicit `foo.svg?react` query form.
    svgr({ svgrOptions: { exportType: "default" }, include: ["src/**/*.svg", "src/**/*.svg?react"] }),
    tanstackStart(),
    react(),
  ],
});
