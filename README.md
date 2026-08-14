# oj

0. If you like the idea and project please consider sponsor, working on it in my free timr.
1. This is a research project, use on your own risk.
2. It was created out of frustration with agents running vite build when I was working on rioterm-js repository. Each build process was carrying 2gb.
3. If anyone want to move the project upstream, please contact me via email. Currently working mostly to fix my own problems.

A Rust-native build tool for React apps. One oxc parse per file does the TS
strip, JSX, and Fast Refresh instrumentation; on top of that: React Fast
Refresh, CJS to ESM interop, content-addressed persistent caching, an
experimental registry-runtime dev bundle mode (`--bundle`), production builds
via embedded Rolldown, CSS Modules on Lightning CSS, and a Tailwind v4 sidecar.

It optimizes for memory and cold start, where running many builds (CI, agents,
multi-tenant) under Vite gets expensive.

## Server rendering

There is an SSR mode: `oj dev --ssr src/entry-server.tsx` in dev, and
`oj build --ssr` for production. It streams the HTML out with
`renderToReadableStream` instead of buffering, and the client hydrates through
the normal dev pipeline, so Fast Refresh and HMR keep working over a
server-rendered page. In dev the server modules run in a small persistent Node
process (a module runner using `vm.SourceTextModule`) that re-evaluates only
what changed, instead of rebuilding a bundle per request.

The playground app also carries a small file-based router built on top of all
this: a `src/routes/` directory, nested `layout.tsx` files, `$param` segments,
per-route data loaders and form actions, error and pending states, route code
splitting, and link prefetching. Most of that is example-app code, not the tool
itself. It is there to show the primitives compose, not to be a framework.

## Plugins

Vite/Rollup-style plugins run through a persistent Node plugin host. Drop an
`oj.plugins.mjs` at the app root that default-exports a plugin array, or let oj
read an app's `vite.config.{ts,js,mjs}` and pick up its `plugins` array
directly. A TypeScript config (including one that imports local `.ts` files) is
loaded via Vite's own config loader when Vite is installed, or bundled with the
app's esbuild otherwise. From a `vite.config` oj also adopts the app's `base`,
`server.port`/`host`, `define`, and `resolve.alias` for any field its own config
leaves unset; alias entries resolve alongside tsconfig `paths`, in both `oj dev`
and `oj build`. The dev
server and `oj build` run `transform`, `resolveId`, `load`, `config`,
`configResolved`, `transformIndexHtml`, `handleHotUpdate`, `buildStart`,
`buildEnd`, `renderStart`, `renderChunk`, `generateBundle`, `writeBundle`,
`closeBundle`, and `configureServer`; honor `enforce`,
`apply`, and `applyToEnvironment` ordering; and give hooks a plugin context with
`this.resolve`, `this.load`, `this.emitFile`, `this.getModuleInfo`,
`this.getModuleIds`, and `this.addWatchFile`. Plugins run in both the client and
SSR environments, in dev and in the production build. Still missing are
whole-graph module info and per-environment build outputs, so a plugin that
drives several build environments at once will not work yet.

## Goals

oj is meant to run real production React apps without changes to their source.
The work ahead, roughly in priority order:

- More of the multi-environment build model. Per-environment `define`, resolve
  conditions (browser for the client, node for SSR), build output (`minify`,
  `sourcemap`), and plugin gating (`applyToEnvironment`) work today; still
  missing are per-environment `outDir`/`target`/rollup options and
  `builder.sharedPlugins`, which modern meta-frameworks build on.
- More of the router-driven framework layer on top of the streaming SSR that
  already exists. File-based route discovery ships as `virtual:oj-routes` (a
  `src/routes/` manifest with the index/`$param`/layout conventions), and
  `oj build --ssr` prerenders configured routes to static HTML (`build.prerender`,
  hydrated). Server functions work in dev and in the production build: a
  `*.server.ts` module is replaced on the client by RPC stubs (its code never
  ships to the browser), and the real functions run on the module runner in dev
  and a bundled dispatch behind `server.mjs` in production.
- Deeper edge support. `oj build --ssr` already emits a `worker.mjs` Web
  `fetch` handler (Workers/`workerd`-style) beside the Node `server.mjs`; still
  ahead is bundling for a specific edge runtime's constraints and asset story.
- More of the module-graph plugin API. The `moduleParsed` and `watchChange`
  hooks fire today; still ahead is a synchronous whole-graph `getModuleInfo`
  (oj's plugin host is out of process, so cross-graph lookups are async).

## Quickstart

Install the CLI from crates.io, then run `oj` in any app:

```sh
cargo install oj                                # install the `oj` CLI
oj dev                                           # dev server for the current app on :5199
oj build                                         # production build into ./dist
```

Or run it from a checkout of this repo:

```sh
cargo run -p oj -- dev                          # dev server for ./playground on :5199
cargo run -p oj -- dev --bundle                 # registry-runtime bundle mode
cargo run -p oj -- dev --ssr src/entry-server.tsx  # streaming SSR + hydration
cargo run -p oj -- build playground             # production build into playground/dist
cargo test --workspace                          # rust unit tests
node --test e2e/unit/*.test.mjs                 # js unit tests (adapter helpers)
node e2e/run.mjs                                # browser e2e suite (add --bundle for bundle mode)
node e2e/ssr-dev.mjs                            # SSR dev e2e; e2e/ssr-prod.mjs for the built server
node e2e/start.mjs                              # tanstack start integration (see e2e/fixtures/start-app)
node bench/generate.mjs 1000                    # generate a benchmark app (then npm i inside it)
node bench/run.mjs 1000                         # p50/p95 benchmark vs vite
node bench/card.mjs                             # render bench/card.html to oj-benchmarks.png
```

## Benchmarks

Generated fanout-10 React component trees, measured save-to-paint with
Playwright against Vite 8.2.1 (Rolldown-based) on an M-series Mac, in both
its default dev mode (vite) and its experimental bundled dev mode
(vite-fbm). p50/p95 over 5 cold+warm restart cycles and 10 HMR edits.

**1,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **354/354ms** | **354/359ms** | **41/42ms** | **76/94ms** | **46MB** |
| oj (unbundled) | 438/966ms | 458/463ms | 174/175ms | 78/95ms | 46MB |
| vite | 696/707ms | 644/668ms | 172/174ms | 56/94ms | 420MB |
| vite-fbm | 302/329ms | 315/323ms | 49/52ms | 58/61ms | 354MB |

**5,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **855/871ms** | **711/726ms** | **128/129ms** | **82/85ms** | **87MB** |
| oj (unbundled) | 1286/1304ms | 1159/1166ms | 763/811ms | 81/86ms | 67MB |
| vite | 2604/2643ms | 2399/2424ms | 747/764ms | 58/136ms | 904MB |
| vite-fbm | 971/976ms | 973/976ms | 176/180ms | 61/65ms | 974MB |

**10,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **1570/1597ms** | **1231/1278ms** | **220/222ms** | **94/186ms** | **121MB** |
| oj (unbundled) | 2438/2499ms | 2080/2132ms | 1484/1509ms | 88/194ms | 96MB |
| vite | 5333/5395ms | 4914/4945ms | 1585/1598ms | 58/176ms | 1499MB |
| vite-fbm | 1376/1402ms | 1385/1390ms | 272/291ms | 66/68ms | 1739MB |

Bundle-mode oj wins cold start, warm start, and reload against Vite's default
dev at every size (3-7x at 10k), and beats Vite's experimental bundled dev on
warm start and reload. The two are close on cold start (vite-fbm edges oj at
10k), and Vite keeps a small lead on raw HMR latency — where oj also runs the
app's `@vitejs/plugin-react`. oj's decisive, consistent win is memory: 46-121MB
against Vite's 354MB-1.7GB, an 8-14x gap that widens with app size.

Production builds (`oj build` vs `vite build`) land at parity: same engine
(Rolldown), byte-identical output sizes.

Caveats: one machine, one app shape. Reproduce with `bench/`.

## Reference reading

- [oxc_transformer/examples/transformer.rs](https://github.com/oxc-project/oxc/blob/main/crates/oxc_transformer/examples/transformer.rs): the pipeline oj's compiler is based on
- [oxc_transformer/src/jsx](https://github.com/oxc-project/oxc/tree/main/crates/oxc_transformer/src/jsx): JSX + ReactRefresh transform internals
- [vitejs/vite-plugin-react](https://github.com/vitejs/vite-plugin-react): the Fast Refresh glue semantics oj replicates
- [vite/packages/vite/src/node/server](https://github.com/vitejs/vite/tree/main/packages/vite/src/node/server): HMR propagation, `import.meta.hot` protocol
- [rolldown/rolldown](https://github.com/rolldown/rolldown): plugin hook filters, the prod linker oj embeds
