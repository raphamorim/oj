import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { DocsLayout } from "../components/site";

function GettingStarted() {
  return (
    <DocsLayout>
      <p className="eyebrow eyebrow--accent">Start</p>
      <h1>Getting started</h1>
      <p className="lede">
        oj is a single binary. Install it, point it at a React app, and run. It
        detects the shape of your project and does the right thing.
      </p>

      <h2>Install</h2>
      <p>The CLI installs from crates.io with Cargo:</p>
      <pre><code>cargo install oj</code></pre>
      <p>
        That gives you the <code>oj</code> command. It needs Node on your PATH
        for the parts of the pipeline that run your app's own toolchain (esbuild,
        your <code>vite.config</code> plugins).
      </p>

      <h2>Develop</h2>
      <p>
        Run the dev server against your app directory. oj compiles on demand,
        serves modules and assets, and keeps a warm process for server rendering:
      </p>
      <pre><code>oj dev .</code></pre>
      <p>
        Edits are reflected instantly. React components hot-update with Fast
        Refresh and their state preserved; a change that affects rendering
        warm-reloads the server without dropping the process.
      </p>

      <h2>Build</h2>
      <p>Produce a production build:</p>
      <pre><code>oj build .</code></pre>
      <p>
        For a TanStack Start app this emits a minified, code-split client bundle
        under <code>dist/client</code>, a bundled SSR server, and two runtime
        entrypoints: <code>dist/server.mjs</code> for Node and{" "}
        <code>dist/worker.mjs</code> for the edge.
      </p>
      <pre><code>node dist/server.mjs</code></pre>

      <h2>Deploy to Cloudflare</h2>
      <p>
        The build already emits a Cloudflare Worker. Add a{" "}
        <code>wrangler.jsonc</code> that points at it and serves the client
        assets from the edge:
      </p>
      <pre><code>{`{
  "name": "my-app",
  "main": "dist/worker.mjs",
  "compatibility_date": "2024-11-01",
  "compatibility_flags": ["nodejs_compat"],
  "assets": { "directory": "dist/client", "binding": "ASSETS" }
}`}</code></pre>
      <p>Then build and ship:</p>
      <pre><code>oj build . && wrangler deploy</code></pre>
      <p>
        This very site is a TanStack Start app built by oj and deployed exactly
        this way.
      </p>

      <h2>Project shape</h2>
      <p>oj works with a conventional TanStack Start layout:</p>
      <pre><code>{`my-app/
  src/
    router.tsx        # exports getRouter()
    routes/           # your routes
  vite.config.ts      # tanstackStart() + react() + your plugins
  wrangler.jsonc      # Cloudflare config
  package.json`}</code></pre>
      <p>
        You keep writing a normal app. oj reads your <code>vite.config</code>,
        your <code>tsconfig</code> paths, and your package <code>imports</code>,
        so nothing about your source has to change to build with it.
      </p>
    </DocsLayout>
  );
}

export const gettingStartedRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/getting-started",
  component: GettingStarted,
});
