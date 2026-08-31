# Changelog

All notable changes to oj are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Experimental foundation for running Cloudflare (workerd) SSR natively, with no Node, Miniflare, or Vite plugin. `oj_server::workerd` locates the `workerd` binary, generates its Cap'n Proto text config (a worker with a socket, a module-fallback address, and var/service bindings), and answers workerd's module-fallback requests with TS/JSX-stripped ESM served from oj's compiler. A bare `node_modules` import is resolved through oj's resolver (under workerd/worker export conditions) and answered with a 301 redirect to its absolute path, so a worker's whole module graph is served on demand; oj's Start module aliases (for example `tanstack-start-manifest:v`) redirect the same way to their target files, and a served module's `#`-subpath alias imports (which workerd resolves through package `imports` and never sends to the fallback) are rewritten to their target's absolute path before the module leaves the fallback. A CommonJS `node_modules` module (a `.cjs`, or a `.js` with no ES module syntax) is wrapped into ESM so workerd's ES module loader can evaluate it: its `require()` calls resolve to fallback-served paths (or a `node:` builtin under `nodejs_compat`) and `module.exports` becomes the default and named exports. The project's `wrangler.jsonc`/`.json`/`.toml` is parsed (`oj_server::wrangler`) for the worker's `compatibility_date`, `compatibility_flags` (such as `nodejs_compat`), `vars`, and service bindings, with `.dev.vars` overriding, and those feed the generated config so worker bindings resolve locally. The fallback resolves the specifier classes a real app produces: a directory import redirects to its `index` file (so the module's own relative imports resolve against the right directory), a `#`-subpath import resolves through the nearest `package.json` `imports` map, and a `.json` import is served as an ESM default export. A module the Rust tiers cannot resolve (a `virtual:` module synthesized by a Vite plugin, such as the TanStack router plugin's) is proxied to an optional plugin-loader endpoint (`workerd-plugin-loader.mjs`, which runs oj's JS plugin container) and returns native ESM, so plugin virtuals resolve without teaching Rust every plugin. On a Cloudflare project with a `workerd` binary present, `oj dev` now spawns a workerd session and the plugin loader, and routes document requests to the worker (falling back to the Node SSR runner when workerd or the loader is unavailable). `oj_server::workerd_dev::spawn` orchestrates a live session: it starts the module-fallback service, picks the worker's HTTP socket, writes the config, and spawns `workerd serve`, killing it on drop. End-to-end tests render a TypeScript route (including a `node_modules` import, and a `:`-scheme module alias) inside real workerd. Detection is gated on a Cloudflare project (`wrangler.jsonc`/`.json`/`.toml`); not yet wired into `oj dev`.
- `oj_compiler::ssr::ssr_transform` — a Vite-compatible SSR / module-runner transform. It rewrites ES module syntax into the `__vite_ssr_*` runtime contract a Vite module runner evaluates: static imports become `await __vite_ssr_import__(...)` with references rewritten to live member access, exports become `__vite_ssr_exportName__` getters, `export *` becomes `__vite_ssr_exportAll__`, dynamic `import()` becomes `__vite_ssr_dynamic_import__`, and `import.meta` becomes `__vite_ssr_import_meta__`. Reference rewriting is scope-correct through oxc symbol resolution (shadowed locals are left alone), and live bindings are preserved. `ssr_transform_module` composes it with oj's existing TS/JSX strip so a `.ts`/`.tsx` module becomes runner-ready in one call. A companion `ssr-fetch-module.mjs` implements the Vite `vite:invoke`/`fetchModule` RPC responder (resolve → externalize routing → load → transform, returning a Vite `FetchResult`), and the dev server can serve runner-transformed modules over `/@ssr-module?...&runner=1`. Together these are the module-transform and fetch-module halves a Vite module runner needs, so oj can serve runner-transformed modules to an out-of-process runner (for example a workerd/Cloudflare dev runtime).

## [0.1.8] - 2026-08-29

### Fixed
- `import.meta.glob` with a TypeScript type argument is now expanded in the TanStack Start SSR path even when the type argument is nested (`import.meta.glob<Record<string, unknown>>(...)`) or an object literal (`import.meta.glob<{ default: T }>(...)`). The Start transform located the call with a regex that stopped at the first `>`, so a nested generic slipped through unrewritten and reached the SSR runtime as a real `import.meta.glob(...)` call, throwing `TypeError: (intermediate value).glob is not a function` and 500ing the route. The detector now skips a balanced, string-aware type argument before the call, matching the AST-based compiler path.

## [0.1.7] - 2026-08-29

### Added
- `server.hmr: false` disables HMR, matching Vite: file edits stop broadcasting reloads (the page holds until a manual refresh) while the dev server and plugin WebSocket keep running. Honored from both a `vite.config` and an `oj.config`.

### Fixed
- `import.meta.glob` is now expanded in `.jsx` and plain `.js`/`.mjs` modules in the TanStack Start SSR path, not only `.ts`/`.tsx`. Previously a `.jsx` route/component (or an unclaimed on-disk `.js`) that used `import.meta.glob` reached the SSR runtime untransformed and threw `TypeError: (intermediate value).glob is not a function`, 500ing the route. The dev SSR loader and both the SSR-server and client production builds now run the glob transform for the full JS/TS family.
- oj now accepts the Vite HMR WebSocket. A Vite HMR client dials the origin root with the `vite-hmr` subprotocol, but oj served its HMR socket only at `/__ws`, so that dial never upgraded and the client could not connect. oj now intercepts a `vite-hmr` upgrade on any path (matching Vite's subprotocol-based dispatch), echoes the subprotocol, and sends `{ "type": "connected" }` first, so the client connects and receives oj's existing Vite-shaped reload frames. The `vite-ping` liveness-probe subprotocol is accepted and closed immediately, as Vite does. This runs entirely on oj's native Rust WebSocket path, with no Node plugin host on the HMR path.

## [0.1.6] - 2026-08-26

### Fixed
- A module a plugin resolves via `resolveId` but does not serve from a `load` hook is now read from disk in the dev server, matching Rollup's contract (a `resolveId` result naming a real file is that module; a `load` returning nothing means read it from disk). Previously such a module 404'd — which broke a pure resolver plugin, the natural way to reach a `node_modules` tree that is not above the importer.

## [0.1.5] - 2026-08-26

### Added
- `resolve.extensions`, `resolve.mainFields`, and `resolve.preserveSymlinks` are now honored by the resolver.
- `envPrefix` accepts an array of prefixes (`string | string[]`).
- `css.preprocessorOptions.scss.additionalData` (and `sass`) injects a preamble before Sass compilation.
- `server.strictPort` is honored; by default the dev and preview servers move to the next free port when the chosen one is busy (Vite parity), and error only when `strictPort` is set.
- `optimizeDeps.esbuildOptions` (`define`, `target`, `loader`, `jsx*`, `mainFields`, `conditions`, `keepNames`, ...) is now applied to dependency pre-bundling instead of being ignored.
- `?worker&inline` emits an inline Blob worker (with a `data:` URI fallback) in bundle mode, matching Vite; in dev it serves a working module worker instead of a broken data URI.
- Plugin `handleHotUpdate` now receives the full Vite `HmrContext` (`file`, `timestamp`, `type`, `modules`, `read()`) and its returned module list is honored to narrow (or, when empty, suppress) the update, folded across plugins as in Vite.
- `build.rollupOptions`/`rolldownOptions.input` is honored (string, array, or `{ name: path }` object), so multi-page projects and projects without a root `index.html` build. HTML entries under nested directories are emitted at their root-relative path and their page-relative `<script>` sources are resolved against the page, matching Vite's `input` resolution.
- Plugins can call `this.emitFile({ type: "chunk", id })` during `buildStart` or `transform`; the chunk is bundled as a build root and `this.getFileName(referenceId)` resolves to its final hashed name in `generateBundle`. Plugin `buildStart` now runs as a rolldown build hook and receives an `options` object with `input` (the emit-chunk mechanism CRXJS-style plugins use to emit their manifest root, content scripts, and pages).
- The `generateBundle` bundle now exposes the full Rollup chunk shape (`facadeModuleId`, `moduleIds`, `modules`, `imports`, `dynamicImports`, `exports`, `isDynamicEntry`, `sourcemapFileName`; assets carry `name`/`names`), and plugins can rename an output by setting its `fileName` and delete an output with `delete bundle[key]` (used by CRXJS to rename pages and drop its manifest JS chunk).
- A plugin emitting an HTML page as a chunk (`this.emitFile({ type: "chunk", id: "...page.html" })`) now gets the Vite HTML treatment: the page's `<script type=module>` are bundled as entries, the processed page is written at its root-relative output path, and `getFileName(referenceId)` resolves to that page path (how CRXJS emits the extension's popup/options pages from its manifest).

### Added
- oj provides a `vite:css-post` plugin shim during the build. Plugins that look one up in `config.plugins` and hand it their generated CSS in `renderChunk` (UnoCSS, and similar) now have that CSS folded into the build's stylesheet output. `renderChunk` hooks also receive the full chunk shape (`modules`, `moduleIds`, `facadeModuleId`, ...) and a `NormalizedOutputOptions`-style `options` with a resolved `dir`, and a plugin-served virtual `.css` module is kept in the graph as a side-effect stub so those hooks can find it.
- The Vite build manifest is now present in the `generateBundle` bundle as a `.vite/manifest.json` asset, so plugins that read it there (for example `@crxjs`'s web-accessible-resources) can find it.

### Changed
- Plugin `transform` hooks now run on CSS and preprocessor (`.css`/`.scss`/`.less`/`.stylus`) sources during the build before oj compiles them, matching Vite where CSS is a real module in the graph. This lets directive transformers such as UnoCSS's `@apply`/`@unocss-include` resolve before Sass/lightningcss run.
- Plugin `transform` sourcemaps are now composed with oj's own transform map, so dev sourcemaps trace back to the original source through plugins that rewrite code.
- CSS imported with `?inline` returns the compiled CSS string instead of a base64 `data:` URI.
- Dev-served CSS rewrites relative `url()` and `@import` references to server-absolute paths so injected styles resolve them.

### Fixed
- `config.publicDir` is now resolved to an absolute path in `configResolved` (default `<root>/public`, Vite parity), so plugins that read it to locate assets (for example `@crxjs` loading a manifest's icons/locales) find them instead of seeing `undefined`.
- The `config`/`configResolved` hooks now see only the command-applicable plugins in `config.plugins` (serve-only plugins are excluded from a build, matching Vite), and `config.plugins` is no longer duplicated by config hooks that return their own `plugins` (the array is pinned across the config-hook merge). A plugin's `generateBundle` error is now surfaced instead of silently swallowed. Together these let `@crxjs`'s build-time manifest plugins (`transformCrxManifest`/`renderCrxManifest` fan-outs) run once and without a serve-only `crx:hmr` crashing them.
- Sass now resolves a relative dotted `@use`/`@forward`/`@import` (e.g. `@use 'colors.module.scss'`) made from inside another dotted `.module.scss` stylesheet: the nested import resolves against the imported file's real directory instead of the phantom directory grass sees a dotted basename through, so cross-directory CSS-modules stylesheets compile.
- Plugin `configResolved` now runs before `applyToEnvironment`, matching Vite; plugins that populate state in `configResolved` and read it from `applyToEnvironment` (for example `@tanstack/router-plugin`) no longer crash the plugin host, and a throwing `applyToEnvironment` keeps the plugin active instead of failing config load ([#37](https://github.com/raphamorim/oj/issues/37)).
- The `config` and `configResolved` hooks now receive `config.plugins` (the flat plugin array) and a defaulted `config.build` (`outDir`, `assetsDir`, `rollupOptions`, ...), matching Vite's resolved config. Plugins that read these in their config hooks (`@crxjs/vite-plugin` reads `config.plugins`; UnoCSS resolves `config.build.outDir`) no longer throw and get skipped.
- A module returned from a plugin `load` hook in the build is now typed from its id's extension (`.jsx`/`.tsx`/`.ts`/`.json`) instead of always JavaScript, matching how Vite derives the transform language from the id. Virtual modules like unplugin-icons' `~icons/*.jsx` are parsed as JSX again instead of failing with "JSX syntax is disabled". Build errors also now render with their source file and location rather than an opaque diagnostic dump.

### Security
- `server.fs.deny` is enforced, and oj blocks `.env`, `.env.*`, `*.crt`, `*.pem`, and `**/.git/**` by default (Vite parity).

## [0.1.4] - 2026-08-22

### Changed
- Reuse the SIMD scan finder and take a single-parse dependency fast path in `compile_factory`.

## [0.1.3] - 2026-08-22

### Changed
- The persistent cache is now opt-in via `--enable-cache` / `OJ_ENABLE_CACHE` and off by default.

## [0.1.2] - 2026-08-22

### Added
- Partial bundling: a self-contained per-package CJS-in-ESM bundler gated by `OJ_PARTIAL_BUNDLE`, with a rolldown-crate fallback (`OJ_PB_ROLLDOWN`) and a force-rolldown trigger for known-hard packages. ESM packages are bundled too.
- `optimizeDeps` configuration (`include`, `exclude`, `needsInterop`, `force`, bundler options) and `server.warmup`.

### Changed
- Compile on demand by default; `--eager` opts into the up-front graph crawl.
- Dependency pre-bundling is include-only by default; full esbuild auto-discovery is gated behind `OJ_OPTIMIZE_SCAN`.

## [0.1.1] - 2026-08-21

### Added
- Excalidraw dev benchmark harness.

### Changed
- Convert monorepo regex `resolve.alias` entries to path aliases, and keep aliased TypeScript source outside the root treated as source.
- Pass `env.config.consumer` to `applyToEnvironment`, and resolve legacy directory-entry dependency subpaths in the SSR loader.

### Fixed
- Skip `vite-plugin-checker` and survive plugins that throw asynchronously; warn once when skipping an unsupported plugin.
- Resolve dotted-basename `.module.scss` Sass imports.
- Rebuild scoping before global-define replacement.

## [0.1.0] - 2026-08-21

- First 0.1 release.
