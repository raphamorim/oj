import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// A dynamic route whose param may contain a dot (/users/john.doe): the dev
// server must hand such GETs to the SSR handler when no static file owns them.
export const userRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/users/$id",
  component: User,
});

function User() {
  const { id } = userRoute.useParams();
  return (
    <main>
      <h1 className="fixture-heading">{`user-${id}-marker`}</h1>
    </main>
  );
}
