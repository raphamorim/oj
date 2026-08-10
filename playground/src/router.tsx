import type { ComponentType, ReactNode } from "react";
import { RouteError, type RouteData } from "@/ui";

export { ErrorBoundary, NavContext } from "@/ui";
export type { NavState, RouteData } from "@/ui";

export type LoaderArgs = { params: Record<string, string>; url: string; body?: string };
type PageModule = {
  default: ComponentType<{ data: RouteData; params: Record<string, string> }>;
  loader?: (args: LoaderArgs) => unknown;
  action?: (args: LoaderArgs) => unknown;
};
type LayoutModule = { default: ComponentType<{ children: ReactNode }> };

// File-based routes from src/routes/**. A `layout.tsx` in a directory wraps
// every route beneath it (composed outermost-first); a `$param` segment is
// dynamic; `index` maps to its parent directory.
const modules = import.meta.glob("./routes/**/*.tsx", { eager: true }) as Record<
  string,
  PageModule & LayoutModule
>;

const rel = (key: string) => key.replace(/^.*\/routes\//, "").replace(/\.tsx$/, "");

type Page = { segments: string[]; dir: string; mod: PageModule };
const pages: Page[] = [];
const layoutFor = new Map<string, LayoutModule>();
for (const [key, mod] of Object.entries(modules)) {
  const parts = rel(key).split("/");
  if (parts[parts.length - 1] === "layout") {
    layoutFor.set(parts.slice(0, -1).join("/"), mod); // "" = root, "users", …
  } else {
    const routePath = rel(key).replace(/\/?index$/, "");
    pages.push({ segments: routePath.split("/").filter(Boolean), dir: parts.slice(0, -1).join("/"), mod });
  }
}

// Layouts from the root down to (and including) the page's directory.
function layoutChain(dir: string): LayoutModule[] {
  const chain: LayoutModule[] = [];
  const acc: string[] = [];
  for (const seg of ["", ...dir.split("/").filter(Boolean)]) {
    if (seg) acc.push(seg);
    const layout = layoutFor.get(acc.join("/"));
    if (layout) chain.push(layout);
  }
  return chain;
}

const dynamicCount = (p: Page) => p.segments.filter((s) => s.startsWith("$")).length;

export function matchRoute(
  url: string,
): { mod: PageModule; params: Record<string, string>; layouts: LayoutModule[] } | null {
  const parts = url.split("?")[0].split("/").filter(Boolean);
  for (const p of [...pages].sort((a, b) => dynamicCount(a) - dynamicCount(b))) {
    if (p.segments.length !== parts.length) continue;
    const params: Record<string, string> = {};
    let ok = true;
    for (let i = 0; i < p.segments.length; i++) {
      const seg = p.segments[i];
      if (seg.startsWith("$")) params[seg.slice(1)] = decodeURIComponent(parts[i]);
      else if (seg !== parts[i]) {
        ok = false;
        break;
      }
    }
    if (ok) return { mod: p.mod, params, layouts: layoutChain(p.dir) };
  }
  return null;
}

export async function loadRoute(url: string): Promise<RouteData> {
  const m = matchRoute(url);
  if (!m?.mod.loader) return null;
  return (await m.mod.loader({ params: m.params, url })) as RouteData;
}

export async function actionRoute(url: string, body: string): Promise<void> {
  const m = matchRoute(url);
  await m?.mod.action?.({ params: m.params, url, body });
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
  const m = matchRoute(url);
  if (!m) return <h1 data-page="notfound">404: {url}</h1>;
  const Page = m.mod.default;
  const content = error ? <RouteError error={error} /> : <Page data={data} params={m.params} />;
  // Wrap the page in its layout chain, innermost first.
  return m.layouts.reduceRight((child, layout) => {
    const Layout = layout.default;
    return <Layout>{child}</Layout>;
  }, content);
}
