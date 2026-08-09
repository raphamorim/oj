import { defineConfig } from "oj";
export default defineConfig({
  virtualModules: {
    "virtual:oj-info": "export const tool = \"oj\"; export default { version: 9 };",
  },
  server: {
    proxy: {
      "/api": { target: "http://localhost:8899", changeOrigin: true, rewrite: { from: "^/api", to: "" } },
    },
  },
});
