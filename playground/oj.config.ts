import { defineConfig } from "oj";
export default defineConfig({
  server: {
    proxy: {
      "/api": { target: "http://localhost:8899", changeOrigin: true, rewrite: { from: "^/api", to: "" } },
    },
  },
});
