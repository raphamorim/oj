import { useEffect, useState } from "react";
import { hydrateRoot } from "react-dom/client";
import { App, ErrorBoundary, NavContext, preloadRoute, type DataMap, type NavState } from "@/router";

declare global {
  interface Window {
    __OJ_DATA__?: DataMap;
  }
}

type Route = { path: string; data: DataMap; error: string | null };

// Fetch server-authoritative route data (the whole chain's loader map); a
// failed loader/action becomes an `error` the route renders instead of crashing.
async function fetchData(path: string, init?: RequestInit): Promise<{ data: DataMap; error: string | null }> {
  try {
    const res = await fetch(path, { headers: { "oj-loader": "1" }, ...init });
    if (!res.ok) return { data: {}, error: `request failed (${res.status})` };
    return { data: (await res.json()) as DataMap, error: null };
  } catch (e) {
    return { data: {}, error: String((e as Error)?.message ?? e) };
  }
}

function Router() {
  const [route, setRoute] = useState<Route>(() => ({
    path: location.pathname,
    data: window.__OJ_DATA__ ?? {},
    error: null,
  }));
  const [nav, setNav] = useState<NavState>("idle");

  useEffect(() => {
    // Prefetched loader data by path (the chunk cache lives in preloadRoute).
    const prefetchedData = new Map<string, Promise<{ data: DataMap; error: string | null }>>();

    const navigate = async (path: string) => {
      setNav("loading");
      try {
        // Reuse hover-prefetched data if present; load the chunk (cached if
        // prefetched) alongside.
        const dataPromise = prefetchedData.get(path) ?? fetchData(path);
        const [{ data, error }] = await Promise.all([dataPromise, preloadRoute(path)]);
        setRoute({ path, data, error });
      } finally {
        setNav("idle");
      }
    };
    const onPop = () => navigate(location.pathname);

    // Prefetch on intent (hover/focus): warm the route's chunk and loader data
    // so the click navigates instantly.
    const hrefOf = (target: EventTarget | null): string | null => {
      const anchor = (target as Element)?.closest?.("a");
      const href = anchor?.getAttribute("href");
      if (!anchor || anchor.target || !href || !href.startsWith("/")) return null;
      return href === location.pathname + location.search ? null : href;
    };
    const prefetch = (target: EventTarget | null) => {
      const anchor = (target as Element)?.closest?.("a");
      if (anchor?.hasAttribute("data-no-prefetch")) return; // opt out (prefetch="none")
      const href = hrefOf(anchor ?? null);
      if (!href || prefetchedData.has(href)) return;
      void preloadRoute(href).catch(() => {}); // chunk
      prefetchedData.set(href, fetchData(href)); // data
    };
    const onOver = (e: Event) => prefetch(e.target);

    // Viewport prefetch: warm links as they scroll into view (a little early
    // via rootMargin). One-shot per link; re-scan on DOM changes so links added
    // by a navigation get observed too.
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) {
            prefetch(e.target);
            io.unobserve(e.target);
          }
        }
      },
      { rootMargin: "200px" },
    );
    const observeLinks = () => document.querySelectorAll('a[href^="/"]').forEach((a) => io.observe(a));
    const mo = new MutationObserver(observeLinks);

    const onClick = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
        return;
      }
      const href = hrefOf(e.target);
      if (!href) return;
      e.preventDefault();
      history.pushState(null, "", href);
      void navigate(location.pathname);
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
        prefetchedData.clear(); // the mutation may have invalidated prefetched data
      } finally {
        setNav("idle");
      }
    };

    addEventListener("popstate", onPop);
    document.addEventListener("click", onClick);
    document.addEventListener("submit", onSubmit);
    document.addEventListener("pointerover", onOver);
    document.addEventListener("focusin", onOver);
    observeLinks();
    mo.observe(document.body, { childList: true, subtree: true });
    return () => {
      removeEventListener("popstate", onPop);
      document.removeEventListener("click", onClick);
      document.removeEventListener("submit", onSubmit);
      document.removeEventListener("pointerover", onOver);
      document.removeEventListener("focusin", onOver);
      io.disconnect();
      mo.disconnect();
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

// Load the initial route's chunk before hydrating so the tree matches the SSR
// output (the router renders from the module cache synchronously).
preloadRoute(location.pathname).then(() => {
  hydrateRoot(document.getElementById("app")!, <Router />);
});
