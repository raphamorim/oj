import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { PixelField } from "../components/PixelField";
import { BenchRace } from "../components/BenchRace";

const GITHUB = "https://github.com/raphamorim/oj";

const STEPS = [
  { n: "01", title: "Install", body: "One binary from crates.io. Node on your PATH for the app's own toolchain.", cmd: "cargo install oj" },
  { n: "02", title: "Develop", body: "A fast on-demand server with Fast Refresh and warm server rendering.", cmd: "oj dev ." },
  { n: "03", title: "Build", body: "A minified client, a bundled SSR server, and an edge worker in one pass.", cmd: "oj build ." },
  { n: "04", title: "Deploy", body: "The build emits a Cloudflare Worker and its assets. Ship it with wrangler.", cmd: "wrangler deploy" },
];

const FEATURES = [
  { title: "Rust-native compile", body: "TypeScript and JSX are transformed by oxc. Types erased, the automatic JSX runtime used, Fast Refresh instrumented in dev, and every output cached content-addressed." },
  { title: "SSR & server functions", body: "Streaming server rendering through TanStack Start, with hydration and SPA navigation. createServerFn() runs in process during a render and becomes an HTTP RPC in the browser, behind a same-origin CSRF check." },
  { title: "Cloudflare on the edge", body: "A single build emits a Worker and its static assets. Wrangler vars and .dev.vars are read in development through a context shim, so getCloudflareContext() resolves the same way it will in production." },
  { title: "Tailwind v4, no wiring", body: "Compiled through @tailwindcss/postcss on demand in dev and emitted in prod, linked into the SSR head so the first paint is styled." },
  { title: "The conventions you write", body: "?url, ?raw and ?inline imports, import.meta.glob and import.meta.env, svgr for .svg components, MDX, CSS Modules, and hashed bare assets." },
  { title: "Resolution & interop", body: "Extension probing, package exports and imports conditions, and tsconfig paths resolve the way the bundler does. CommonJS deps are faceted into ES modules so named imports work." },
  { title: "Your vite.config, honored", body: "oj loads your plugins and runs their hooks with Vite's gating: apply, id filters, enforce order, and per-environment application. Virtual modules and framework plugins work as written." },
];

const CRATES = [
  { name: "oj", desc: "the CLI and the dev-server request loop" },
  { name: "oj_server", desc: "the dev server and the framework adapters" },
  { name: "oj_compiler", desc: "TS/JSX transform, Fast Refresh, glob, CJS interop, on oxc" },
  { name: "oj_resolver", desc: "extensions, exports/imports conditions, tsconfig paths" },
  { name: "oj_css", desc: "CSS, CSS Modules, and Sass compilation" },
  { name: "oj_config", desc: "the config schema and per-environment resolution" },
  { name: "oj_env", desc: ".env loading and import.meta.env defines" },
  { name: "oj_cache", desc: "a content-addressed cache for compiled output" },
];

const STACK = [
  "TanStack Start", "React 19", "Tailwind v4", "Cloudflare Workers", "TypeScript",
  "Server functions", "import.meta.glob", "CSS Modules", "svgr", "MDX", "CommonJS interop",
];

function Home() {
  return (
    <>
      {/* masthead */}
      <section id="top" className="masthead">
        <div className="wrap masthead__grid">
          <h1 className="masthead__title rise">Craft,<br />compiled.</h1>
          <div className="masthead__aside rise rise-2">
            <p className="masthead__pitch">A Rust-native build tool for React apps.</p>
            <div>
              <p className="masthead__tag">
                A fast dev server, server rendering with TanStack Start, Tailwind,
                and one-command Cloudflare deploys — built with a Rust core.
              </p>
              <div className="masthead__actions">
                <a href="#start" className="btn btn--solid">Get started</a>
                <a className="btn btn--ghost" href={GITHUB} target="_blank" rel="noreferrer">GitHub</a>
              </div>
            </div>
          </div>
        </div>
      </section>

      {/* generative canvas */}
      <div className="field bleed rise rise-3">
        <PixelField height={440} />
      </div>

      {/* point of view */}
      <section className="wrap">
        <p className="lede">
          Your build tool should be <em>invisible</em>. oj puts a Rust core under
          the app you already have.
        </p>
      </section>

      {/* process */}
      <section id="process" className="section">
        <div className="wrap">
          <div className="section__head">
            <p className="label">How it works</p>
            <h2 className="section__title">Point oj at your app and run.</h2>
          </div>
          <div className="steps">
            {STEPS.map((s) => (
              <div key={s.n} className="step">
                <div className="step__num">{s.n}</div>
                <div className="step__title">{s.title}</div>
                <p className="step__body">{s.body}</p>
                <div className="step__cmd"><span>$</span> {s.cmd}</div>
              </div>
            ))}
          </div>
        </div>
      </section>

      {/* benchmark */}
      <section id="benchmark" className="section">
        <div className="wrap">
          <div className="section__head">
            <p className="label label--accent">Benchmark</p>
            <h2 className="section__title">Save to paint, measured.</h2>
            <p className="section__sub">
              A 10,000-component dev server, save-to-paint latency (p50), on an
              M-series Mac. Watch a real edit reach the screen — oj against Vite 8.2.
            </p>
          </div>
          <BenchRace />
          <table className="benchtable">
            <thead>
              <tr>
                <th>Tool</th><th>Cold</th><th>Warm</th><th>HMR</th><th>Reload</th><th>Server RAM</th>
              </tr>
            </thead>
            <tbody>
              <tr data-oj="true">
                <td>oj --bundle</td><td>1427ms</td><td>996ms</td><td>49ms</td><td>234ms</td><td>107MB</td>
              </tr>
              <tr>
                <td>vite (bundled dev)</td><td>1425ms</td><td>1427ms</td><td>69ms</td><td>277ms</td><td>1752MB</td>
              </tr>
              <tr>
                <td>vite (default)</td><td>5532ms</td><td>5045ms</td><td>153ms</td><td>1639ms</td><td>1520MB</td>
              </tr>
            </tbody>
          </table>
          <p className="bench__note">
            p50 save-to-paint on a generated 10k-component app, M-series Mac, Vite
            8.2 (default and experimental bundled dev). oj's fused oxc pipeline and
            persistent cache also hold roughly 16× less memory. Reproducible:{" "}
            <code>node bench/run.mjs 1000</code>.
          </p>
        </div>
      </section>

      {/* features */}
      <section id="features" className="section">
        <div className="wrap">
          <div className="section__head">
            <p className="label">Features</p>
            <h2 className="section__title">Everything a real app needs.</h2>
          </div>
          <div className="feature-wrap">
            {FEATURES.map((f) => (
              <div key={f.title} className="feature">
                <div className="feature__title">{f.title}</div>
                <div className="feature__body">{f.body}</div>
              </div>
            ))}
          </div>
        </div>
      </section>

      {/* architecture */}
      <section id="architecture" className="section">
        <div className="wrap">
          <div className="section__head">
            <p className="label">Architecture</p>
            <h2 className="section__title">A small Rust workspace, a thin adapter.</h2>
            <p className="section__sub">
              The Rust side owns the hot path. A JavaScript adapter runs your
              vite.config plugins and speaks the framework's protocols, so the
              same app builds in dev and prod and deploys to a Worker with no
              separate runtime.
            </p>
          </div>
          <div className="crates">
            {CRATES.map((c) => (
              <div key={c.name} className="crate">
                <div className="crate__name">{c.name}</div>
                <div className="crate__desc">{c.desc}</div>
              </div>
            ))}
          </div>
          <div className="chips">
            {STACK.map((s) => <span key={s} className="chip">{s}</span>)}
          </div>
        </div>
      </section>

      {/* get started */}
      <section id="start" className="section">
        <div className="wrap">
          <div className="section__head">
            <p className="label label--accent">Get started</p>
            <h2 className="section__title">Install, develop, ship.</h2>
          </div>
          <div style={{ maxWidth: "46rem", marginTop: "clamp(2rem, 5vw, 3rem)" }}>
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
            <pre className="code-block"><code>oj build . && wrangler deploy</code></pre>
            <div style={{ marginTop: "2rem" }}>
              <a className="btn btn--solid" href={GITHUB} target="_blank" rel="noreferrer">Read the source</a>
            </div>
          </div>
        </div>
      </section>
    </>
  );
}

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Home,
});
