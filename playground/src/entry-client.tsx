import { useEffect, useState } from "react";
import { hydrateRoot } from "react-dom/client";
import { App } from "@/routes";

// Client-side SPA routing over the SSR-rendered initial page: track the path in
// state (seeded from the URL the server rendered, so hydration matches),
// intercept plain internal-link clicks into history.pushState, and follow
// back/forward via popstate — all without a full reload. The server still
// renders each route for the initial load and no-JS.
function Router() {
  const [path, setPath] = useState(() => location.pathname);
  useEffect(() => {
    const sync = () => setPath(location.pathname);
    const onClick = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
        return;
      }
      const anchor = (e.target as Element).closest?.("a");
      const href = anchor?.getAttribute("href");
      if (!anchor || anchor.target || !href || !href.startsWith("/")) return; // external / new-tab
      e.preventDefault();
      if (href !== location.pathname + location.search) {
        history.pushState(null, "", href);
        sync();
      }
    };
    addEventListener("popstate", sync);
    document.addEventListener("click", onClick);
    return () => {
      removeEventListener("popstate", sync);
      document.removeEventListener("click", onClick);
    };
  }, []);
  return <App url={path} />;
}

hydrateRoot(document.getElementById("app")!, <Router />);
