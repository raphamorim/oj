import { createServerFn } from "@tanstack/react-start";
// Resolved by oj's alias to a dev shim (cf-server.mjs) in dev, and bundled in
// prod. Exposes the wrangler `vars` from wrangler.jsonc as env bindings.
import { getCloudflareContext } from "@cloudflare/vite-plugin/server";

// A server function: runs server-side only. The client calls it over the wire;
// on SSR it runs inline. Returns a value the index route renders, so a plain
// GET of "/" proves the server-fn round-trip worked during SSR.
export const getGreeting = createServerFn({ method: "GET" }).handler(async () => {
  const { env } = await getCloudflareContext();
  const edition = (env as Record<string, unknown>).EDITION ?? "unknown";
  return { message: "server-fn-marker", edition: String(edition) };
});
