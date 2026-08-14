import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { BenchRace } from "../components/BenchRace";

const GITHUB = "https://github.com/raphamorim/oj";

const STEPS = [
  { title: "Install", body: "One binary from crates.io, with Node on your PATH for the app's own toolchain.", cmd: "cargo install oj" },
  { title: "Develop", body: "A fast on-demand dev server with Fast Refresh and warm server rendering.", cmd: "oj dev ." },
  { title: "Build", body: "A minified client, a bundled SSR server, and an edge worker in one pass.", cmd: "oj build ." },
  { title: "Deploy", body: "The build emits a Cloudflare Worker and its assets. Ship it with wrangler.", cmd: "wrangler deploy" },
];

const FEATURES = [
  { title: "Rust-native compile", body: "TypeScript and JSX are transformed by oxc: types erased, the automatic JSX runtime used, Fast Refresh instrumented in dev, and every output cached content-addressed." },
  { title: "SSR & server functions", body: "Streaming server rendering through TanStack Start, with hydration and SPA navigation. createServerFn() runs in process during a render and becomes an HTTP RPC in the browser, behind a same-origin CSRF check." },
  { title: "Cloudflare on the edge", body: "One build emits a Worker and its static assets. Wrangler vars and .dev.vars are read in dev through a context shim, so getCloudflareContext() resolves the same way it will in production." },
  { title: "Tailwind v4, no wiring", body: "Compiled through @tailwindcss/postcss on demand in dev and emitted in prod, linked into the SSR head so the first paint is already styled." },
  { title: "The conventions you write", body: "?url, ?raw and ?inline imports, import.meta.glob and import.meta.env, svgr for .svg components, MDX, CSS Modules, and hashed bare assets." },
  { title: "Resolution & interop", body: "Extension probing, package exports and imports conditions, and tsconfig paths resolve the way a bundler does. CommonJS deps are faceted into ES modules so named imports work." },
  { title: "Your vite.config, honored", body: "oj loads your plugins and runs their hooks with Vite's gating: apply, id filters, enforce order, and per-environment application. Virtual modules and framework plugins work as written." },
];

const CRATES = [
  { name: "oj", desc: "the CLI and the dev-server request loop" },
  { name: "oj_server", desc: "the dev server and the framework adapters" },
  { name: "oj_compiler", desc: "TS/JSX transform, Fast Refresh, glob, CJS interop, on oxc" },
  { name: "oj_resolver", desc: "extensions, exports/imports conditions, tsconfig paths" },
  { name: "oj_css", desc: "CSS, CSS Modules, and Sass compilation" },
  { name: "oj_graph", desc: "the module graph and HMR update propagation" },
  { name: "oj_config", desc: "the config schema and per-environment resolution" },
  { name: "oj_cache", desc: "a content-addressed cache for compiled output" },
];

const STACK = [
  "TanStack Start", "React 19", "Tailwind v4", "Cloudflare Workers", "TypeScript",
  "Server functions", "import.meta.glob", "CSS Modules", "svgr", "MDX", "CommonJS interop",
];

function Home() {
  return (
    <div className="wrap" id="top">
      <h1 className="display">oj</h1>

      <p className="lede">
        A Rust-native build tool for React apps. Your build tool should be{" "}
        <em>invisible</em> — oj puts a Rust core under the app you already have.
      </p>
      <p className="body">
        A fast dev server, server rendering with TanStack Start, Tailwind, and
        one-command Cloudflare deploys. Point it at your app and run: it reads
        your <code>vite.config</code>, honors your plugins, and compiles
        TypeScript and JSX through oxc, caching every output. It optimizes for
        memory and cold start, where running many builds under Vite gets
        expensive.
      </p>
      <p className="intro-links">
        <a href="#start">Get started</a>
        <span className="sep">·</span>
        <a href={GITHUB} target="_blank" rel="noreferrer">GitHub</a>
        <span className="sep">·</span>
        Built by{" "}
        <a href="https://github.com/raphamorim" target="_blank" rel="noreferrer">Raphael Amorim</a>
      </p>

      {/* how it works */}
      <section id="how">
        <h2 className="head">How it works</h2>
        <p className="section__sub">Point oj at your app and run. Four commands, start to ship.</p>
        <div className="rows">
          {STEPS.map((s) => (
            <div key={s.title} className="row">
              <div className="row__key">{s.title}</div>
              <div className="row__val">
                {s.body}
                <span className="cmd"><b>$</b> {s.cmd}</span>
              </div>
            </div>
          ))}
        </div>
      </section>

      {/* benchmark */}
      <section id="benchmark">
        <h2 className="head">Benchmark</h2>
        <p className="section__sub">
          A 10,000-component app: cold start, warm start, and full reload (p50),
          on an M-series Mac. oj --bundle against Vite 8.2's default dev.
        </p>
        <BenchRace />
        <table className="benchtable">
          <thead>
            <tr>
              <th>Tool</th><th>Cold</th><th>Warm</th><th>HMR</th><th>Reload</th><th>Server RAM</th>
            </tr>
          </thead>
          <tbody>
            <tr data-oj="true">
              <td>oj --bundle</td><td>1570ms</td><td>1231ms</td><td>94ms</td><td>220ms</td><td>121MB</td>
            </tr>
            <tr>
              <td>vite (bundled dev)</td><td>1376ms</td><td>1385ms</td><td>66ms</td><td>272ms</td><td>1739MB</td>
            </tr>
            <tr>
              <td>vite (default)</td><td>5333ms</td><td>4914ms</td><td>58ms</td><td>1585ms</td><td>1499MB</td>
            </tr>
          </tbody>
        </table>
        <p className="bench__note">
          p50 on a generated 10k-component app, M-series Mac, Vite 8.2 (default
          and experimental bundled dev). oj wins cold, warm, and reload against
          default Vite by 3–7×, and holds 12–14× less server memory than either
          Vite mode; Vite keeps a slight edge on raw HMR. Reproducible:{" "}
          <code>node bench/run.mjs 10000</code>.
        </p>
      </section>

      {/* features */}
      <section id="features">
        <h2 className="head">Features</h2>
        <p className="section__sub">Everything a real app needs, none of the wiring.</p>
        <div className="rows">
          {FEATURES.map((f) => (
            <div key={f.title} className="row">
              <div className="row__key">{f.title}</div>
              <div className="row__val">{f.body}</div>
            </div>
          ))}
        </div>
      </section>

      {/* architecture */}
      <section id="architecture">
        <h2 className="head">Architecture</h2>
        <p className="section__sub">
          A small Rust workspace owns the hot path; a thin JavaScript adapter runs
          your vite.config plugins and speaks the framework's protocols. The same
          app builds in dev and prod and deploys to a Worker with no separate
          runtime.
        </p>
        <div className="rows">
          {CRATES.map((c) => (
            <div key={c.name} className="row">
              <div className="row__key row__key--mono">{c.name}</div>
              <div className="row__val">{c.desc}</div>
            </div>
          ))}
        </div>
        <div className="chips">
          {STACK.map((s) => <span key={s} className="chip">{s}</span>)}
        </div>
      </section>

      {/* get started */}
      <section id="start">
        <h2 className="head">Get started</h2>
        <p className="section__sub">Install, develop, ship.</p>
        <pre className="code-block"><code>{`cargo install oj      # the CLI, from crates.io
oj dev .              # dev server with SSR + Fast Refresh
oj build .            # dist/ : client, SSR server, edge worker`}</code></pre>
        <p className="code-note">
          For Cloudflare, add a <code>wrangler.jsonc</code> pointing at the
          emitted worker and serving the client assets from the edge:
        </p>
        <pre className="code-block"><code>{`{
  "name": "my-app",
  "main": "dist/worker.mjs",
  "compatibility_date": "2024-11-01",
  "compatibility_flags": ["nodejs_compat"],
  "assets": { "directory": "dist/client", "binding": "ASSETS" }
}`}</code></pre>
        <p className="code-note">Then build and ship in one step:</p>
        <pre className="code-block"><code>oj build . &amp;&amp; wrangler deploy</code></pre>
        <p className="intro-links">
          <a href={GITHUB} target="_blank" rel="noreferrer">Read the source</a>
        </p>
      </section>
    </div>
  );
}

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Home,
});
