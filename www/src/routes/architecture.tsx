import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { DocsLayout } from "../components/site";

function Architecture() {
  return (
    <DocsLayout>
      <p className="eyebrow eyebrow--accent">Reference</p>
      <h1>Architecture</h1>
      <p className="lede">
        oj is a small Rust workspace with a thin JavaScript adapter for the parts
        of the ecosystem that only exist in JavaScript. The Rust side owns the
        hot path; the adapter keeps you compatible.
      </p>

      <h2>The workspace</h2>
      <p>The core is a set of focused Rust crates:</p>
      <ul>
        <li><strong>oj</strong> — the CLI and the dev-server request loop.</li>
        <li><strong>oj_server</strong> — the dev server and the framework adapters.</li>
        <li><strong>oj_compiler</strong> — TS and JSX transform, Fast Refresh, <code>import.meta</code>, glob expansion, and CommonJS interop, on oxc.</li>
        <li><strong>oj_resolver</strong> — module resolution: extensions, exports and imports conditions, tsconfig paths.</li>
        <li><strong>oj_css</strong> — CSS, CSS Modules, and Sass compilation.</li>
        <li><strong>oj_config</strong> — the config schema and per-environment resolution.</li>
        <li><strong>oj_env</strong> — <code>.env</code> loading and <code>import.meta.env</code> defines.</li>
        <li><strong>oj_cache</strong> — a content-addressed persistent cache for compiled output.</li>
      </ul>

      <h2>The dev flow</h2>
      <p>
        <code>oj dev</code> builds on a normal module-and-asset server and layers
        a single middleware over it. Document requests are server-rendered by a
        warm runner process; module and asset requests fall through to the
        compile pipeline. A file watcher regenerates only what changed, rebuilds
        the client, and warm-reloads the runner, then signals the page to reload.
      </p>

      <h2>The runner</h2>
      <p>
        Server rendering runs in a persistent Node process. oj and the runner
        speak a small newline-delimited JSON protocol over stdio: one request in,
        one response out. App output is diverted so it can never corrupt the
        protocol frames. On a rebuild the runner re-imports the entry under a new
        version query, so app modules re-evaluate while React stays warm.
      </p>

      <h2>The production build</h2>
      <p>
        <code>oj build</code> runs two esbuild passes sharing one plugin
        container. The client pass targets the browser: minified, hashed,
        code-split. The server pass targets Node and is fully bundled so the
        framework's internal imports resolve at build time. Both emit assets
        through one content-addressed emitter, so identical bytes get identical
        URLs with no shared manifest.
      </p>

      <h2>The TanStack Start adapter</h2>
      <p>
        When oj detects a TanStack Start app it loads your{" "}
        <code>vite.config</code> plugins into a container and runs their
        resolve, load, transform, and generateBundle hooks with the same gating
        Vite applies. That is what makes virtual modules, MDX, and svgr work.
        Server functions are transformed on both sides and tied together by a
        stable id, so a browser RPC finds its in-process handler.
      </p>
      <p>
        The result is a build that runs the same app in development and
        production, and deploys to a Cloudflare Worker without a separate
        framework runtime.
      </p>
    </DocsLayout>
  );
}

export const architectureRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/architecture",
  component: Architecture,
});
