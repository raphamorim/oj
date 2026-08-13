import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// A second route, to prove multi-route SSR + SPA navigation.
export const aboutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
});

function About() {
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
    </main>
  );
}
