import type { ComponentType, ReactNode } from "react";
import { RouteError, type RouteData } from "@/ui";

export { ErrorBoundary, NavContext } from "@/ui";
export type { NavState, RouteData } from "@/ui";

// Loader data for a whole matched chain, keyed by route/layout id.
export type DataMap = Record<string, RouteData>;

export type LoaderArgs = { params: Record<string, string>; url: string; body?: string };
type PageModule = {
  default: ComponentType<{ data: RouteData; params: Record<string, string> }>;
  loader?: (args: LoaderArgs) => unknown;
  action?: (args: LoaderArgs) => unknown;
};
type LayoutModule = {
  default: ComponentType<{ children: ReactNode; data: RouteData }>;
  loader?: (args: LoaderArgs) => unknown;
};
type Node<M> = { id: string; mod: M };

// File-based routes from src/routes/**. `layout.tsx` wraps everything beneath
// it; `$param` is dynamic; `index` maps to its parent directory. Every node —
// each layout and the page — may export its own loader.
const modules = import.meta.glob("./routes/**/*.tsx", { eager: true }) as Record<
  string,
  PageModule & LayoutModule
>;

const rel = (key: string) => key.replace(/^.*\/routes\//, "").replace(/\.tsx$/, "");

type Page = { segments: string[]; dir: string; id: string; mod: PageModule };
const pages: Page[] = [];
const layoutFor = new Map<string, Node<LayoutModule>>();
for (const [key, mod] of Object.entries(modules)) {
  const id = rel(key);
  const parts = id.split("/");
  if (parts[parts.length - 1] === "layout") {
    layoutFor.set(parts.slice(0, -1).join("/"), { id, mod });
  } else {
    const routePath = id.replace(/\/?index$/, "");
    pages.push({ segments: routePath.split("/").filter(Boolean), dir: parts.slice(0, -1).join("/"), id, mod });
  }
}

function layoutChain(dir: string): Node<LayoutModule>[] {
  const chain: Node<LayoutModule>[] = [];
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
): { layouts: Node<LayoutModule>[]; page: Node<PageModule>; params: Record<string, string> } | null {
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
    if (ok) return { layouts: layoutChain(p.dir), page: { id: p.id, mod: p.mod }, params };
  }
  return null;
}

// Run every loader in the matched chain (layouts + page) in parallel; a failing
// loader anywhere rejects the whole request (surfaced as the route error UI).
export async function loadRouteData(url: string): Promise<DataMap> {
  const m = matchRoute(url);
  if (!m) return {};
  const nodes = [...m.layouts, m.page];
  const entries = await Promise.all(
    nodes.map(async (n) => [n.id, n.mod.loader ? ((await n.mod.loader({ params: m.params, url })) as RouteData) : null] as const),
  );
  return Object.fromEntries(entries);
}

export async function actionRoute(url: string, body: string): Promise<void> {
  const m = matchRoute(url);
  await m?.page.mod.action?.({ params: m.params, url, body });
}

export function App({ url = "/", data = {}, error = null }: { url?: string; data?: DataMap; error?: string | null }) {
  const m = matchRoute(url);
  if (!m) return <h1 data-page="notfound">404: {url}</h1>;
  const Page = m.page.mod.default;
  const content = error ? (
    <RouteError error={error} />
  ) : (
    <Page data={data[m.page.id] ?? null} params={m.params} />
  );
  // Wrap the page in its layout chain, innermost first; each layout gets its
  // own loader data.
  return m.layouts.reduceRight((child, layout) => {
    const Layout = layout.mod.default;
    return <Layout data={data[layout.id] ?? null}>{child}</Layout>;
  }, content);
}
