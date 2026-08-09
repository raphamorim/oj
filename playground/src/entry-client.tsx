import { useEffect, useState } from "react";
import { hydrateRoot } from "react-dom/client";
import { App, type RouteData } from "@/routes";

declare global {
  interface Window {
    __OJ_DATA__?: RouteData;
  }
}

// Fetch server-authoritative route data. `oj-loader` tells the server to return
// JSON loader data instead of an HTML document. A POST body runs the route's
// action first (server-side mutation), then the revalidated loader data.
async function fetchData(path: string, init?: RequestInit): Promise<RouteData> {
  const res = await fetch(path, { headers: { "oj-loader": "1" }, ...init });
  return res.ok ? ((await res.json()) as RouteData) : null;
}

// Client-side SPA routing with route data + actions over the SSR page. The
// initial render reuses the server-loaded data serialized into the document (no
// refetch); navigations fetch loader data; form submits run the action then
// revalidate — all without a full reload.
function Router() {
  const [route, setRoute] = useState<{ path: string; data: RouteData }>(() => ({
    path: location.pathname,
    data: window.__OJ_DATA__ ?? null,
  }));

  useEffect(() => {
    const navigate = async (path: string) => setRoute({ path, data: await fetchData(path) });
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
      const data = await fetchData(location.pathname, {
        method: "POST",
        body,
        headers: { "oj-loader": "1", "content-type": "application/x-www-form-urlencoded" },
      });
      setRoute((r) => ({ ...r, data }));
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

  return <App url={route.path} data={route.data} />;
}

hydrateRoot(document.getElementById("app")!, <Router />);
