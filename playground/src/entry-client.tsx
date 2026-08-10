import { useEffect, useState } from "react";
import { hydrateRoot } from "react-dom/client";
import { App, ErrorBoundary, NavContext, type NavState, type RouteData } from "@/router";

declare global {
  interface Window {
    __OJ_DATA__?: RouteData;
  }
}

type Route = { path: string; data: RouteData; error: string | null };

// Fetch server-authoritative route data; a failed loader/action becomes an
// `error` the route renders instead of crashing.
async function fetchData(path: string, init?: RequestInit): Promise<{ data: RouteData; error: string | null }> {
  try {
    const res = await fetch(path, { headers: { "oj-loader": "1" }, ...init });
    if (!res.ok) return { data: null, error: `request failed (${res.status})` };
    return { data: (await res.json()) as RouteData, error: null };
  } catch (e) {
    return { data: null, error: String((e as Error)?.message ?? e) };
  }
}

function Router() {
  const [route, setRoute] = useState<Route>(() => ({
    path: location.pathname,
    data: window.__OJ_DATA__ ?? null,
    error: null,
  }));
  const [nav, setNav] = useState<NavState>("idle");

  useEffect(() => {
    const navigate = async (path: string) => {
      setNav("loading");
      try {
        const { data, error } = await fetchData(path);
        setRoute({ path, data, error });
      } finally {
        setNav("idle");
      }
    };
    const onPop = () => navigate(location.pathname);

    const onClick = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
        return;
      }
      const anchor = (e.target as Element).closest?.("a");
      const href = anchor?.getAttribute("href");
      if (!anchor || anchor.target || !href || !href.startsWith("/")) return;
      e.preventDefault();
      if (href !== location.pathname + location.search) {
        history.pushState(null, "", href);
        void navigate(location.pathname);
      }
    };

    const onSubmit = async (e: SubmitEvent) => {
      const form = (e.target as Element).closest?.("form");
      if (!form || form.method.toLowerCase() !== "post") return;
      e.preventDefault();
      const body = new URLSearchParams(new FormData(form) as unknown as Record<string, string>).toString();
      setNav("submitting");
      try {
        const { data, error } = await fetchData(location.pathname, {
          method: "POST",
          body,
          headers: { "oj-loader": "1", "content-type": "application/x-www-form-urlencoded" },
        });
        setRoute((r) => ({ ...r, data, error }));
      } finally {
        setNav("idle");
      }
    };

    addEventListener("popstate", onPop);
    document.addEventListener("click", onClick);
    document.addEventListener("submit", onSubmit);
    return () => {
      removeEventListener("popstate", onPop);
      document.removeEventListener("click", onClick);
      document.removeEventListener("submit", onSubmit);
    };
  }, []);

  return (
    <NavContext.Provider value={nav}>
      <ErrorBoundary resetKey={route.path}>
        <App url={route.path} data={route.data} error={route.error} />
      </ErrorBoundary>
    </NavContext.Provider>
  );
}

hydrateRoot(document.getElementById("app")!, <Router />);
