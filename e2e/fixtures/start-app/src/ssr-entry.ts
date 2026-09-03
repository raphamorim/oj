import serverEntry from "@tanstack/react-start/server-entry";

// The app's own Start server entry (`tanstackStart({ server: { entry } })`), the
// shape an SSR error wrapper takes: it wraps Start's default handler, which the
// dev server and the prod bundle must run instead of the default (Vite imports
// the configured entry as the SSR handler). Every response is marked so the
// e2e can tell this entry answered.
type Handler = { fetch: (request: Request, env?: unknown, ctx?: unknown) => Promise<Response> | Response };

export default {
  async fetch(request: Request, env?: unknown, ctx?: unknown): Promise<Response> {
    const res = await (serverEntry as Handler).fetch(request, env, ctx);
    const headers = new Headers(res.headers);
    headers.set("x-server-entry", "fixture");
    return new Response(res.body, { status: res.status, statusText: res.statusText, headers });
  },
};
