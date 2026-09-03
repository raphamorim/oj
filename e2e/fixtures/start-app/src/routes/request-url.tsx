import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// Echoes how a request reached the app: the URL the handler was given, the
// Host it carries and any x-forwarded-host a proxy in front added. The dev
// server must present them as Vite does (Host is the dev server's own,
// x-forwarded-host passes through untouched) even when a proxy already set
// x-forwarded-host; joining the two hosts used to break URL parsing.
export const requestUrlRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/api/request-url",
  server: {
    handlers: {
      GET: ({ request }: { request: Request }) =>
        Response.json({
          url: request.url,
          host: request.headers.get("host"),
          forwardedHost: request.headers.get("x-forwarded-host"),
        }),
    },
  },
});
