import { SsrApp } from "@/ssr-app";
import { addLike, getLikes } from "@/store";

// Route data is now backed by server state, so loaders are server-authoritative:
// the client fetches loader data on navigation rather than recomputing it, and
// a mutation (action) is visible everywhere afterward.
export type RouteData = { likes: number } | null;

export async function loadRoute(url: string): Promise<RouteData> {
  const path = url.split("?")[0];
  await Promise.resolve();
  if (path === "/" || path === "/about") return { likes: getLikes() };
  return null;
}

// The action mutates server state; the framework then revalidates the loader.
export async function actionRoute(_url: string, _body: string): Promise<void> {
  addLike();
}

function Likes({ data }: { data: RouteData }) {
  const likes = data?.likes ?? 0;
  return (
    <form method="post" data-likes-form>
      <span data-likes={likes}>likes: {likes}</span>
      <button type="submit">like</button>
    </form>
  );
}

function Home({ data }: { data: RouteData }) {
  return (
    <main data-page="home">
      <SsrApp />
      <Likes data={data} />
      <a href="/about">about</a>
    </main>
  );
}

function About({ data }: { data: RouteData }) {
  return (
    <main data-page="about">
      <h1>About</h1>
      <Likes data={data} />
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
