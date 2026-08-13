import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { DocsLayout } from "../components/site";

function Features() {
  return (
    <DocsLayout>
      <p className="eyebrow eyebrow--accent">Reference</p>
      <h1>Features</h1>
      <p className="lede">
        A quick tour of what oj handles so a real app builds and runs without
        surprises.
      </p>

      <h2>Rust-native compilation</h2>
      <p>
        TypeScript and JSX are transformed by oxc. Types are erased, the
        automatic JSX runtime is used, and Fast Refresh is instrumented in
        development. Compiled output is cached content-addressed, keyed on the
        source, the URL, the mode, and the tool version.
      </p>

      <h2>Dev server and HMR</h2>
      <p>
        A fast on-demand compile server with React Fast Refresh: components
        hot-update with their state preserved, and edits that change rendering
        warm-reload the server rather than restarting it.
      </p>

      <h2>Server rendering and server functions</h2>
      <p>
        Streaming SSR through TanStack Start, with hydration and SPA navigation.{" "}
        <code>createServerFn().handler()</code> runs in process during a render
        and becomes an HTTP RPC in the browser, with a same-origin CSRF check.
      </p>

      <h2>Cloudflare on the edge</h2>
      <p>
        A single build emits a Worker and its static assets. Wrangler{" "}
        <code>vars</code> and <code>.dev.vars</code> are read in development
        through a Cloudflare context shim, so <code>getCloudflareContext()</code>
        resolves the same way it will in production.
      </p>

      <h2>Tailwind v4</h2>
      <p>
        Tailwind is compiled through <code>@tailwindcss/postcss</code> on demand
        in development and emitted with its <code>url()</code> references
        rewritten in production. No extra wiring beyond your stylesheet.
      </p>

      <h2>Asset conventions</h2>
      <ul>
        <li><code>?url</code>, <code>?raw</code>, and <code>?inline</code> imports.</li>
        <li><code>import.meta.glob</code> expanded to a static map, lazy or eager.</li>
        <li><code>import.meta.env</code> with your <code>VITE_</code> variables.</li>
        <li>svgr for <code>.svg</code> components, bare or via the <code>?react</code> query.</li>
        <li>MDX, CSS Modules, and Sass.</li>
        <li>Bare assets (images, fonts) hashed and emitted with matching URLs.</li>
      </ul>

      <h2>Resolution and interop</h2>
      <p>
        Extension probing, package <code>exports</code> and <code>imports</code>{" "}
        conditions, and tsconfig <code>paths</code> all resolve the way the
        bundler does. CommonJS dependencies are faceted into ES modules by
        requiring them and re-exporting their real runtime keys, so named imports
        work even when a static lexer cannot see them.
      </p>

      <h2>Plugin compatibility</h2>
      <p>
        oj loads your <code>vite.config</code> plugins and runs their hooks with
        Vite's gating: <code>apply</code>, id filters, <code>enforce</code>{" "}
        order, and per-environment application. Virtual modules, content
        emission, and framework plugins work as written.
      </p>
    </DocsLayout>
  );
}

export const featuresRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/features",
  component: Features,
});
