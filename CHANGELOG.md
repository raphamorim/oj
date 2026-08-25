# Changelog

All notable changes to oj are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - Unreleased

### Added
- `resolve.extensions`, `resolve.mainFields`, and `resolve.preserveSymlinks` are now honored by the resolver.
- `envPrefix` accepts an array of prefixes (`string | string[]`).
- `css.preprocessorOptions.scss.additionalData` (and `sass`) injects a preamble before Sass compilation.
- `server.strictPort` is honored; by default the dev and preview servers move to the next free port when the chosen one is busy (Vite parity), and error only when `strictPort` is set.
- `optimizeDeps.esbuildOptions` (`define`, `target`, `loader`, `jsx*`, `mainFields`, `conditions`, `keepNames`, ...) is now applied to dependency pre-bundling instead of being ignored.

### Changed
- CSS imported with `?inline` returns the compiled CSS string instead of a base64 `data:` URI.
- Dev-served CSS rewrites relative `url()` and `@import` references to server-absolute paths so injected styles resolve them.

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
