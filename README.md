# oj

0. If you like the idea and project please consider sponsor, working on it in my free timr.
1. This is a research project, use on your own risk.
2. It was created out of frustration with agents running vite build when I was working on rioterm-js repository. Each build process was carrying 2gb.
3. If anyone want to move the project upstream, please contact me via email. Currently working mostly to fix my own problems.

A Rust-native build tool for React apps: an opinionated, zero-config,
low-memory toolchain. Fused oxc compile pipeline (one parse: TS strip, JSX,
Fast Refresh instrumentation), real React Fast Refresh, CJS to ESM interop,
content-addressed persistent caching, an experimental registry-runtime bundle
mode for dev (`--bundle`), production builds via embedded Rolldown, CSS
Modules on Lightning CSS, and a Tailwind v4 sidecar.

Aimed at the workloads where Vite 8 is still weak: memory, cold start, and
multi-tenant / agent-driven builds at scale.

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
`oj.plugins.mjs` at the app root that default-exports a plugin array. The dev
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
  hydrated). Still ahead: server functions (client-callable RPC with server-only
  code extraction).
- Edge and serverless server targets, so one app can build for a Node server or
  a worker runtime.
- A whole-graph module API (`getModuleInfo` across the full graph,
  `moduleParsed`, `watchChange`) for plugins that inspect the dependency graph.

## Quickstart

```sh
cargo run -p oj -- dev                          # dev server for ./playground on :5199
cargo run -p oj -- dev --bundle                 # registry-runtime bundle mode
cargo run -p oj -- dev --ssr src/entry-server.tsx  # streaming SSR + hydration
cargo run -p oj -- build playground             # production build into playground/dist
cargo test --workspace                          # unit tests
node e2e/run.mjs                                # browser e2e suite (add --bundle for bundle mode)
node e2e/ssr-dev.mjs                            # SSR dev e2e; e2e/ssr-prod.mjs for the built server
node bench/generate.mjs 1000                    # generate a benchmark app (then npm i inside it)
node bench/run.mjs 1000                         # p50/p95 benchmark vs vite
```

## Benchmarks

Generated fanout-10 React component trees, measured save-to-paint with
Playwright against Vite 8.2.1 (Rolldown-based) on an M-series Mac, in both
its default dev mode (vite) and its experimental bundled dev mode
(vite-fbm). p50/p95 over 5 cold+warm restart cycles and 10 HMR edits.

**1,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **185/193ms** | **162/164ms** | **43/43ms** | **38/41ms** | **45MB** |
| oj (unbundled) | 280/283ms | 269/272ms | 178/183ms | 37/41ms | 45MB |
| vite | 714/728ms | 667/675ms | 177/178ms | 55/91ms | 439MB |
| vite-fbm | 330/343ms | 325/334ms | 49/54ms | 56/58ms | 358MB |

**5,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **722/746ms** | **502/510ms** | **132/139ms** | **42/44ms** | **77MB** |
| oj (unbundled) | 1153/1186ms | 1003/1018ms | 783/792ms | 41/43ms | 62MB |
| vite | 2663/2686ms | 2450/2459ms | 766/794ms | 57/95ms | 966MB |
| vite-fbm | 962/975ms | 958/966ms | 176/179ms | 60/79ms | 980MB |

**10,000 components (p50/p95):**

| tool | cold start | warm start | reload | HMR | server RSS |
|---|---|---|---|---|---|
| **oj --bundle** | **1427/1444ms** | **1070/1081ms** | **234/241ms** | **49/51ms** | **107MB** |
| oj (unbundled) | 2403/2472ms | 2033/2057ms | 1560/1596ms | 46/157ms | 88MB |
| vite | 5532/5558ms | 5045/5088ms | 1639/1676ms | 42/176ms | 1520MB |
| vite-fbm | 1425/1437ms | 1427/1435ms | 277/287ms | 68/72ms | 1752MB |

Read honestly: bundle-mode oj wins or ties every column at every scale.
Cold start at 10k is a statistical tie with vite's bundled mode; everywhere
else oj leads outright, at 8-16x less memory. Bundled-mode vite uses more
RAM than its default mode; oj's bundle mode does not.

Production builds (`oj build` vs `vite build`) land at parity: same engine
(Rolldown), byte-identical output sizes.

Caveats: one machine, one app shape. Reproduce with `bench/`.

## Reference reading

- [oxc_transformer/examples/transformer.rs](https://github.com/oxc-project/oxc/blob/main/crates/oxc_transformer/examples/transformer.rs): the pipeline oj's compiler is based on
- [oxc_transformer/src/jsx](https://github.com/oxc-project/oxc/tree/main/crates/oxc_transformer/src/jsx): JSX + ReactRefresh transform internals
- [vitejs/vite-plugin-react](https://github.com/vitejs/vite-plugin-react): the Fast Refresh glue semantics oj replicates
- [vite/packages/vite/src/node/server](https://github.com/vitejs/vite/tree/main/packages/vite/src/node/server): HMR propagation, `import.meta.hot` protocol
- [rolldown/rolldown](https://github.com/rolldown/rolldown): plugin hook filters, the prod linker oj embeds
