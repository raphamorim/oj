# Changelog

All notable changes to oj are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
