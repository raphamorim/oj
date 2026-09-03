// The base fixture config plus Vite's `ssr.external`: the production server
// bundle must leave these dependencies as bare imports (resolved from
// node_modules at run time) instead of inlining them. Used through
// `oj build --config vite.ssr-external.config.ts` by e2e/start-ssr-external.mjs.
import base from "./vite.config";

export default {
  ...base,
  ssr: { external: ["react", "react-dom"] },
};
