import { defineConfig } from "oj";
export default defineConfig({
  virtualModules: {
    "virtual:oj-info": "export const tool = \"oj\"; export default { version: 9 };",
  },
  // Config-driven define: the top-level applies to every environment; the
  // per-environment overrides (Vite Environment API) apply only to that build.
  define: {
    __OJ_DEFINE_GLOBAL__: JSON.stringify("global-define"),
  },
  // Prerender (SSG): these routes are rendered to static HTML at build time
  // (hydrated by the client bundle). Applies to `oj build --ssr`.
  build: {
    prerender: ["/", "/about"],
  },
  environments: {
    client: { define: { __OJ_DEFINE_CLIENT__: JSON.stringify("client-define") } },
    ssr: {
      define: { __OJ_DEFINE_SSR__: JSON.stringify("ssr-define") },
      // Per-environment build output: the server bundle skips sourcemaps while
      // the client hydration bundle still emits them.
      build: { sourcemap: false },
    },
  },
  server: {
    proxy: {
      "/api": { target: "http://localhost:8899", changeOrigin: true, rewrite: { from: "^/api", to: "" } },
    },
  },
});
