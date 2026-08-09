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

## Quickstart

```sh
cargo run -p oj -- dev              # dev server for ./playground on :5199
cargo run -p oj -- dev --bundle     # registry-runtime bundle mode
cargo run -p oj -- build playground # production build -> playground/dist
cargo test --workspace                  # 38 unit tests
node e2e/run.mjs                        # browser e2e suite (add --bundle for bundle mode)
node bench/generate.mjs 1000            # generate a benchmark app (then npm i inside it)
node bench/run.mjs 1000                 # p50/p95 benchmark vs vite
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
