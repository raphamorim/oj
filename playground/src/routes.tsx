import { SsrApp } from "@/ssr-app";

// Route-level data. `loadedOn` records where the loader ran ("server" on the
// initial SSR load, "client" after an SPA navigation) so the flow is visible.
export type RouteData = { loadedOn: "server" | "client"; label: string } | null;

// The loader is isomorphic: the server runs it before rendering and serializes
// the result; the client runs it on SPA navigation. Real loaders fetch/query —
// this one just stamps where it ran, after an async tick.
export async function loadRoute(url: string): Promise<RouteData> {
  const path = url.split("?")[0];
  const loadedOn = typeof window === "undefined" ? "server" : "client";
  await Promise.resolve();
  if (path === "/") return { loadedOn, label: "home data" };
  if (path === "/about") return { loadedOn, label: "about data" };
  return null;
}

function Loaded({ data }: { data: RouteData }) {
  return (
    <p data-loaded={data?.loadedOn ?? "none"}>
      {data ? `${data.label} (loaded on ${data.loadedOn})` : "no data"}
    </p>
  );
}

function Home({ data }: { data: RouteData }) {
  return (
    <main data-page="home">
      <SsrApp />
      <Loaded data={data} />
      <a href="/about">about</a>
    </main>
  );
}

function About({ data }: { data: RouteData }) {
  return (
    <main data-page="about">
      <h1>About</h1>
      <Loaded data={data} />
      <a href="/">home</a>
    </main>
  );
}

export function App({ url = "/", data = null }: { url?: string; data?: RouteData }) {
  const path = url.split("?")[0];
  if (path === "/about") return <About data={data} />;
  if (path === "/") return <Home data={data} />;
  return <h1 data-page="notfound">404: {path}</h1>;
}
