import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// A server route on a dotted path with a non-GET handler: the dev server must
// hand POST /api/export.csv to the SSR handler (Vite: every request nothing
// else owns reaches the app). It echoes the body back and sets two cookies, so
// the proxies (dev and the generated prod server) have to keep binary bodies
// and duplicate set-cookie headers intact.
export const exportRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/api/export.csv",
  server: {
    handlers: {
      POST: async ({ request }: { request: Request }) => {
        const bytes = new Uint8Array(await request.arrayBuffer());
        const headers = new Headers({
          "content-type": "application/octet-stream",
          "x-body-bytes": String(bytes.length),
        });
        headers.append("set-cookie", "first=1; Path=/");
        headers.append("set-cookie", "second=2; Path=/");
        return new Response(bytes, { status: 200, headers });
      },
    },
  },
});
