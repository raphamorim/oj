export const tsrStartManifest = () => ({
  routes: {
    __root__: {
      preloads: ["/@oj-start/client-entry.js"],
      scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
    },
  },
});
