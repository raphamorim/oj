import { createRouter } from "@tanstack/react-router";

import { routeTree } from "./routeTree";

// oj aliases `#tanstack-router-entry` to this module and TanStack Start's
// handler imports getRouter() from it.
export function getRouter() {
  return createRouter({
    routeTree,
    defaultPreload: "intent",
    scrollRestoration: true,
  });
}
