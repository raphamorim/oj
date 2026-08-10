import { Component, createContext, useContext, type ReactNode } from "react";
import { SsrApp } from "@/ssr-app";
import { addLike, getLikes } from "@/store";

export type RouteData = { likes: number } | null;
export type NavState = "idle" | "loading" | "submitting";

// Navigation lifecycle, provided by the client router; components read it to
// show pending UI. The server has no in-flight navigation, so the default
// "idle" is correct there (and matches the client's first render).
export const NavContext = createContext<NavState>("idle");

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function loadRoute(url: string): Promise<RouteData> {
  const path = url.split("?")[0];
  if (path === "/boom") throw new Error("boom: the loader failed");
  await sleep(path === "/about" ? 200 : 0); // /about is slow enough to see "loading"
  if (path === "/" || path === "/about") return { likes: getLikes() };
  return null;
}

export async function actionRoute(_url: string, _body: string): Promise<void> {
  await sleep(100); // slow enough to see "saving…"
  addLike();
}

function PendingBar() {
  const state = useContext(NavContext);
  if (state === "idle") return null;
  return <div data-pending={state}>{state === "submitting" ? "saving…" : "loading…"}</div>;
}

function Likes({ data }: { data: RouteData }) {
  const state = useContext(NavContext);
  const likes = data?.likes ?? 0;
  return (
    <form method="post" data-likes-form>
      <span data-likes={likes}>likes: {likes}</span>
      <button type="submit">{state === "submitting" ? "saving…" : "like"}</button>
    </form>
  );
}

// Route-level error UI, shown for both loader/action failures (the router
// passes `error`) and render-time throws (ErrorBoundary below).
export function RouteError({ error }: { error: string }) {
  return (
    <div data-error>
      <p>route error: {error}</p>
      <a href="/">home</a>
    </div>
  );
}

// Catches render-time errors in the routed tree; the router remounts it on
// navigation (via `key`) so recovering to another route resets it.
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: string | null }> {
  state: { error: string | null } = { error: null };
  static getDerivedStateFromError(e: unknown) {
    return { error: String((e as Error)?.message ?? e) };
  }
  render() {
    return this.state.error ? <RouteError error={this.state.error} /> : this.props.children;
  }
}

function Crash(): ReactNode {
  throw new Error("crash: render threw");
}

function Home({ data }: { data: RouteData }) {
  return (
    <main data-page="home">
      <SsrApp />
      <Likes data={data} />
      <PendingBar />
      <nav>
        <a href="/about">about</a> <a href="/boom">boom</a> <a href="/crash">crash</a>
      </nav>
    </main>
  );
}

function About({ data }: { data: RouteData }) {
  return (
    <main data-page="about">
      <h1>About</h1>
      <Likes data={data} />
      <PendingBar />
      <a href="/">home</a>
    </main>
  );
}

export function App({
  url = "/",
  data = null,
  error = null,
}: {
  url?: string;
  data?: RouteData;
  error?: string | null;
}) {
  if (error) return <RouteError error={error} />;
  const path = url.split("?")[0];
  if (path === "/about") return <About data={data} />;
  if (path === "/") return <Home data={data} />;
  if (path === "/crash") return <Crash />;
  return <h1 data-page="notfound">404: {path}</h1>;
}
