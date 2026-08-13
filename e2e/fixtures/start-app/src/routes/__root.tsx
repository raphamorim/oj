import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
// Side-effect: pulls the Tailwind stylesheet into the client graph.
import "../../styles/app.css";

export const rootRoute = createRootRoute({
  head: () => ({
    meta: [{ title: "oj start fixture" }],
  }),
  component: RootComponent,
});

function RootComponent() {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
