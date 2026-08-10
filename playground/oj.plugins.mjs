// A Vite/Rollup-style plugin: a factory returning a `{ name, transform }`
// object, exactly the shape npm plugins use. oj's plugin host loads this and
// runs the transform hook against the compile pipeline.
function markerPlugin() {
  return {
    name: "oj-marker",
    transform(code, id) {
      if (!id.endsWith(".tsx")) return null;
      if (!code.includes("__OJ_PLUGIN_UNTRANSFORMED__")) return null;
      return code.replace("__OJ_PLUGIN_UNTRANSFORMED__", "transformed-by-plugin");
    },
  };
}

// A virtual-module plugin using resolveId + load (the classic `virtual:` id
// pattern real plugins use).
function virtualPlugin() {
  const VIRTUAL_ID = "\0virtual:plugin-greeting";
  return {
    name: "oj-virtual",
    resolveId(source) {
      if (source === "virtual:plugin-greeting") return VIRTUAL_ID;
      return null;
    },
    load(id) {
      if (id === VIRTUAL_ID) return `export const info = "hello from plugin";`;
      return null;
    },
  };
}

export default [markerPlugin(), virtualPlugin()];

