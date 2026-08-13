import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

// Pulls the design system into the client graph (Tailwind v4 + tokens).
import "../../styles/app.css";
import { Nav, Footer } from "../components/site";

export const rootRoute = createRootRoute({
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      { title: "oj: Rust-native builds for React" },
      {
        name: "description",
        content:
          "oj is a Rust-native build tool for React apps: a fast dev server, SSR and TanStack Start, Tailwind, and one-command Cloudflare deploys.",
      },
      { name: "theme-color", content: "#fbfaf7" },
    ],
    links: [{ rel: "icon", type: "image/svg+xml", href: "/favicon.svg" }],
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
        <Nav />
        <Outlet />
        <Footer />
        <Scripts />
      </body>
    </html>
  );
}
