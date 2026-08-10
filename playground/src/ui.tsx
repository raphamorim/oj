import { Component, createContext, useContext, type ReactNode } from "react";

// Route data is whatever a route's loader returns (or null).
export type RouteData = Record<string, unknown> | null;
export type NavState = "idle" | "loading" | "submitting";

// Navigation lifecycle, provided by the client router; the server default
// "idle" matches the client's first render.
export const NavContext = createContext<NavState>("idle");

export function PendingBar() {
  const state = useContext(NavContext);
  if (state === "idle") return null;
  return <div data-pending={state}>{state === "submitting" ? "saving…" : "loading…"}</div>;
}

export function Likes({ data }: { data: RouteData }) {
  const state = useContext(NavContext);
  const likes = Number(data?.likes ?? 0);
  return (
    <form method="post" data-likes-form>
      <span data-likes={likes}>likes: {likes}</span>
      <button type="submit">{state === "submitting" ? "saving…" : "like"}</button>
    </form>
  );
}

// Route-level error UI, shown for loader/action failures and render throws.
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
