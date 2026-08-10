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

export default [markerPlugin()];
