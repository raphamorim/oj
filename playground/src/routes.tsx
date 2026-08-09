import { SsrApp } from "@/ssr-app";

// A minimal per-route SSR router: the same mapping runs on the server (given
// the request path) and on the client (given location.pathname), so each route
// server-renders its own tree and hydrates it. Client-side SPA transitions are
// a separate concern; navigating here is a full load that the server renders.
function About() {
  return (
    <main data-page="about">
      <h1>About</h1>
      <p>the about page, server-rendered per route</p>
      <a href="/">home</a>
    </main>
  );
}

export function App({ url = "/" }: { url?: string }) {
  const path = url.split("?")[0];
  if (path === "/about") return <About />;
  if (path === "/") {
    return (
      <main data-page="home">
        <SsrApp />
        <a href="/about">about</a>
      </main>
    );
  }
  return <h1 data-page="notfound">404: {path}</h1>;
}
