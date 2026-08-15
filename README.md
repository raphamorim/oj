# oj

0. If you like the idea and project please consider sponsor, working on it in my free time.
1. This is a research project, use on your own risk.
2. It was created out of frustration with agents running vite build when I was working on rioterm-js repository. Each build process was carrying 2gb.
3. Working on it mostly to fix my own problems.

OJ is a Rust-native build tool for React apps.

It optimizes for memory and cold start, where running many builds (CI, agents, multi-tenant) under Vite gets expensive. OJ is meant to run real production React apps without changes to their source.

This project is alpha, so expect bugs. For real.

## Server rendering

There is an SSR mode: `oj dev --ssr src/entry-server.tsx` in dev, and `oj build --ssr` for production. It streams the HTML out with `renderToReadableStream` instead of buffering, and the client hydrates through
the normal dev pipeline, so Fast Refresh and HMR keep working over a server-rendered page.

In dev the server modules run in a small persistent Node process (a module runner using `vm.SourceTextModule`) that re-evaluates only what changed, instead of rebuilding a bundle per request.

## Plugins

Vite/Rollup-style plugins run through a persistent Node plugin host. Drop an `oj.plugins.mjs` at the app root that default-exports a plugin array, or let oj read an app's `vite.config.{ts,js,mjs}` and pick up its `plugins` array directly.

A TypeScript config (including one that imports local `.ts` files) is loaded via Vite's own config loader when Vite is installed, or bundled with the app's esbuild otherwise. From a `vite.config` oj also adopts the app's `base`, `server.port`/`host`, `define`, and `resolve.alias` for any field its own config
leaves unset, alias entries resolve alongside tsconfig `paths`, in both `oj dev` and `oj build`.

## TanStack Start

Point oj at a TanStack Start app and it runs the framework directly, no source changes. It detects Start from the app itself:

```sh
oj dev web                                       # dev server for the ./web Start app
oj build web                                     # production build
```

Dev serves file-based routes with streaming SSR and client hydration, keeps Fast Refresh working, runs server functions in the module runner, and applies the app's `vite.config` plugins (React and the app's own). `oj build` emits a Node `server.mjs`, a Cloudflare `worker.mjs` (`nodejs_compat`), content-hashed client assets, and prerendered routes. oj's own docs site (`www/`) is a TanStack Start app built by oj and served from a Cloudflare Worker.

## Quickstart

Install the CLI from crates.io, then run `oj` in any app:

```sh
cargo install oj                                # install the `oj` CLI
oj dev                                           # dev server for the current app on :5199
oj build                                         # production build into ./dist
```

## Benchmarks

Generated fanout-10 React component trees, measured save-to-paint with Playwright against Vite 8.2.1 (Rolldown-based) on an M-series Mac, in both its default dev mode (vite) and its experimental bundled dev mode (vite-fbm). p50/p95 over 5 cold+warm restart cycles and 10 HMR edits.

**1,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **343/346ms** | **334/350ms** | **41/42ms** | **57/59ms** | **47MB** |
| oj (unbundled) | 445/948ms | 452/459ms | 175/178ms | 57/60ms | 46MB |
| vite | 701/709ms | 653/660ms | 172/175ms | 54/94ms | 445MB |
| vite-fbm | 300/304ms | 301/371ms | 49/50ms | 55/58ms | 361MB |

**5,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **893/945ms** | **725/727ms** | **133/134ms** | **56/97ms** | **81MB** |
| oj (unbundled) | 1367/1508ms | 1188/1676ms | 792/945ms | 62/63ms | 63MB |
| vite | 2726/2774ms | 2490/2603ms | 791/856ms | 56/79ms | 910MB |
| vite-fbm | 917/934ms | 911/916ms | 168/169ms | 57/63ms | 973MB |

**10,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **1569/1618ms** | **1408/1429ms** | **231/241ms** | **69/216ms** | **122MB** |
| oj (unbundled) | 2589/2889ms | 2184/2238ms | 1576/1607ms | 64/170ms | 100MB |
| vite | 5468/5537ms | 4957/4980ms | 1604/1649ms | 114/173ms | 1504MB |
| vite-fbm | 1415/1442ms | 1417/1438ms | 277/291ms | 64/68ms | 1738MB |

Bundle-mode oj wins cold start, warm start, and reload against Vite's default dev at every size (3-7x at 10k), and beats Vite's experimental bundled dev on warm start and reload. On HMR oj now matches Vite's bundled dev (within a few ms) and is ~2x faster than Vite's default dev at 10k. The two bundled modes are
close on cold start (vite-fbm edges oj at 10k). oj's decisive, consistent win is memory: 47-122MB against Vite's 361MB-1.7GB, an 8-14x gap that widens with app size.

Production builds (`oj build` vs `vite build`) land at parity: same engine (Rolldown), byte-identical output sizes.

## Reference reading

- [oxc_transformer/examples/transformer.rs](https://github.com/oxc-project/oxc/blob/main/crates/oxc_transformer/examples/transformer.rs): the pipeline oj's compiler is based on
- [oxc_transformer/src/jsx](https://github.com/oxc-project/oxc/tree/main/crates/oxc_transformer/src/jsx): JSX + ReactRefresh transform internals
- [vitejs/vite-plugin-react](https://github.com/vitejs/vite-plugin-react): the Fast Refresh glue semantics oj replicates
- [vite/packages/vite/src/node/server](https://github.com/vitejs/vite/tree/main/packages/vite/src/node/server): HMR propagation, `import.meta.hot` protocol
- [rolldown/rolldown](https://github.com/rolldown/rolldown): plugin hook filters, the prod linker oj embeds

## Self note

```sh
cargo run -p oj -- dev                            # dev server for ./playground on :5199
cargo run -p oj -- dev --bundle                   # registry-runtime bundle mode
cargo run -p oj -- dev --ssr src/entry-server.tsx # streaming SSR + hydration
cargo run -p oj -- build playground               # production build into playground/dist
cargo test --workspace                            # rust unit tests
node --test e2e/unit/*.test.mjs                   # js unit tests (adapter helpers)
node e2e/run.mjs                                  # browser e2e suite (add --bundle for bundle mode)
node e2e/ssr-dev.mjs                              # SSR dev e2e, e2e/ssr-prod.mjs for the built server
node e2e/start.mjs                                # tanstack start integration (see e2e/fixtures/start-app)
node e2e/dep-optimize.mjs                         # dependency pre-bundle + cjs interop integration
node e2e/assets.mjs                               # asset url imports + new URL(import.meta.url)
node e2e/dynamic-import.mjs                       # dynamic import with variables (glob switch)
node e2e/wasm.mjs                                 # wasm ?init instantiation (dev + build)
node e2e/query-assets.mjs                         # ?url/?raw/?inline/?init in bundle mode
node e2e/rolldown-options.mjs                     # build.rollupOptions filenames + external
node bench/generate.mjs 1000                      # generate a benchmark app (then npm i inside it)
node bench/run.mjs 1000                           # p50/p95 benchmark vs vite
node bench/card.mjs                               # render bench/card.html to oj-benchmarks.png
```