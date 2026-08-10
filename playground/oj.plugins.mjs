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

// Uses the full plugin context: this.resolve (resolve a specifier through oj's
// own resolver — here the tsconfig `@/` alias, proving it isn't plain Node
// resolution) and this.emitFile (emit an asset into the build output). The
// resolved module's basename is injected into App.tsx; the emitted file lands
// in dist/assets/ during the prod build.
function ctxPlugin() {
  return {
    name: "oj-ctx",
    buildStart() {
      this.emitFile({ type: "asset", name: "oj-plugin-emitted.txt", source: "emitted-by-plugin" });
    },
    async transform(code, id) {
      if (!id.endsWith("App.tsx")) return null;
      let out = code;
      if (out.includes("__RESOLVED__")) {
        const r = await this.resolve("@/Counter", id);
        const base = r && r.id ? r.id.split(/[\\/]/).pop() : "unresolved";
        out = out.replace("__RESOLVED__", base);
      }
      if (out.includes("__MODINFO__")) {
        // this.load fetches Counter's code + imports; getModuleInfo then reads
        // the cached info synchronously. Counter's imports post-compile are
        // react, its css module, and the auto-injected react/jsx-runtime (3).
        const r = await this.resolve("@/Counter", id);
        const loaded = r ? await this.load({ id: r.id }) : null;
        const info = r ? this.getModuleInfo(r.id) : null;
        const n = info ? info.importedIds.length : -1;
        const hasHook = loaded ? loaded.code.includes("useState") : false;
        out = out.replace("__MODINFO__", `${n}:${hasHook}`);
      }
      return out === code ? null : out;
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
  ctxPlugin(),
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

