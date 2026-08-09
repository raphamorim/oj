import { useEffect, useState } from "react";
import { hydrateRoot } from "react-dom/client";
import { App, loadRoute, type RouteData } from "@/routes";

declare global {
  interface Window {
    __OJ_DATA__?: RouteData;
  }
}

// Client-side SPA routing with route-level data loading over the SSR page. The
// initial render reuses the server-loaded data serialized into the document
// (so hydration matches, no refetch); each SPA navigation runs the loader on
// the client before rendering the new route.
function Router() {
  const [route, setRoute] = useState<{ path: string; data: RouteData }>(() => ({
    path: location.pathname,
    data: window.__OJ_DATA__ ?? null,
  }));
  useEffect(() => {
    const navigate = async (path: string) => setRoute({ path, data: await loadRoute(path) });
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
    addEventListener("popstate", onPop);
    document.addEventListener("click", onClick);
    return () => {
      removeEventListener("popstate", onPop);
      document.removeEventListener("click", onClick);
    };
  }, []);
  return <App url={route.path} data={route.data} />;
}

hydrateRoot(document.getElementById("app")!, <Router />);
