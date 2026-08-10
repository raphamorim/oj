import { writeFileSync } from "node:fs";

// buildStart / buildEnd lifecycle hooks. Captures the command from the resolved
// config, then drops marker files at each phase so the runtime can prove they
// fired: buildStart at dev-server start and at the top of the prod build,
// buildEnd once the prod build's module graph is complete. The host runs with
// the app root as its cwd, so .oj-cache/ (created for the host script) is here.
function lifecyclePlugin() {
  let command = "";
  return {
    name: "oj-lifecycle",
    configResolved(config) {
      command = config.command;
    },
    buildStart() {
      writeFileSync(".oj-cache/plugin-buildstart", command);
    },
    buildEnd() {
      writeFileSync(".oj-cache/plugin-buildend", command);
    },
  };
}

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

// Uses config() to contribute a value and configResolved() to capture the
// merged config, then injects a config-derived string via transform — the
// standard "capture config for later hooks" pattern.
function configPlugin() {
  let resolved = null;
  return {
    name: "oj-config",
    config(_config, env) {
      // Contribute to the config; the merged result is what configResolved sees.
      return { define: { __OJ_BUILT_BY__: "oj-plugin" } };
    },
    configResolved(config) {
      resolved = config; // { root, base, mode, command, define, server }
    },
    transform(code, id) {
      if (!id.endsWith(".tsx") || !code.includes("__OJ_CONFIG_MARKER__")) return null;
      const value = `${resolved.mode}:${resolved.define.__OJ_BUILT_BY__}`;
      return code.replace("__OJ_CONFIG_MARKER__", value);
    },
  };
}

// Uses handleHotUpdate to override HMR: this file self-accepts (Fast Refresh)
// by default, but the plugin forces a full reload for it.
function hmrPlugin() {
  return {
    name: "oj-hmr",
    handleHotUpdate({ file }) {
      if (file.endsWith("hmr-demo.tsx")) return "full-reload";
      return undefined;
    },
  };
}

// Uses transformIndexHtml to inject a meta tag into the document head (the
// tag-descriptor form), plus a string rewrite.
function htmlPlugin() {
  return {
    name: "oj-html",
    transformIndexHtml(html) {
      return {
        html: html.replace("<title>oj playground</title>", '<title>oj playground (plugin)</title>'),
        tags: [{ tag: "meta", attrs: { name: "oj-plugin-injected", content: "yes" }, injectTo: "head" }],
      };
    },
  };
}

// enforce ordering: appends to data-order. Listed post-then-pre below, but
// enforce sorts them pre -> post, so the result is "base-pre-post".
function orderPlugin(tag, enforce) {
  return {
    name: `oj-order-${tag}`,
    enforce,
    transform(code, id) {
      if (!id.endsWith("App.tsx") || !code.includes('data-order="')) return null;
      return code.replace(/(data-order="[^"]*)/, `$1-${tag}`);
    },
  };
}

// apply gating: only the plugin matching the command ("serve"/"build") runs, so
// data-apply is "serve-only" in dev and "build-only" in the prod build.
function applyPlugin(tag, apply) {
  return {
    name: `oj-apply-${tag}`,
    apply,
    transform(code, id) {
      if (!id.endsWith("App.tsx")) return null;
      return code.replace("__APPLY__", tag);
    },
  };
}

export default [
  lifecyclePlugin(),
  markerPlugin(),
  virtualPlugin(),
  configPlugin(),
  hmrPlugin(),
  htmlPlugin(),
  orderPlugin("post", "post"),
  orderPlugin("pre", "pre"),
  applyPlugin("serve-only", "serve"),
  applyPlugin("build-only", "build"),
];

