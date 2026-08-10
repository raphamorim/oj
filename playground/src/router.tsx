import type { ComponentType, ReactNode } from "react";
import { RouteError, type RouteData } from "@/ui";

export { ErrorBoundary, NavContext } from "@/ui";
export type { NavState, RouteData } from "@/ui";

export type DataMap = Record<string, RouteData>;
export type LoaderArgs = { params: Record<string, string>; url: string; body?: string };
export type MetaArgs = { params: Record<string, string>; url: string; data: RouteData };
// A `<title>`, a named `<meta name>`, or an Open Graph `<meta property>` tag.
export type MetaDescriptor = { title?: string; name?: string; property?: string; content?: string };
type MetaFn = (args: MetaArgs) => MetaDescriptor[];
type PageModule = {
  default: ComponentType<{ data: RouteData; params: Record<string, string> }>;
  loader?: (args: LoaderArgs) => unknown;
  action?: (args: LoaderArgs) => unknown;
  meta?: MetaFn;
};
type LayoutModule = {
  default: ComponentType<{ children: ReactNode; data: RouteData }>;
  loader?: (args: LoaderArgs) => unknown;
  meta?: MetaFn;
};

// Route-level code splitting: a LAZY glob gives one dynamic import() per route,
// so each route (and layout) is a separate chunk loaded on demand — the initial
// bundle carries only the router, not every route. Matching uses the glob keys
// (static); modules are fetched via preloadRoute and cached.
const thunks = import.meta.glob("./routes/**/*.tsx") as Record<string, () => Promise<PageModule & LayoutModule>>;

const rel = (key: string) => key.replace(/^.*\/routes\//, "").replace(/\.tsx$/, "");

type Thunk<M> = { id: string; load: () => Promise<M> };
type Page = { segments: string[]; dir: string; id: string; load: () => Promise<PageModule> };
const pages: Page[] = [];
const layoutFor = new Map<string, Thunk<LayoutModule>>();
for (const [key, load] of Object.entries(thunks)) {
  const id = rel(key);
  const parts = id.split("/");
  if (parts[parts.length - 1] === "layout") {
    layoutFor.set(parts.slice(0, -1).join("/"), { id, load });
  } else {
    const routePath = id.replace(/\/?index$/, "");
    pages.push({ segments: routePath.split("/").filter(Boolean), dir: parts.slice(0, -1).join("/"), id, load });
  }
}

// Loaded modules, keyed by route/layout id (populated by preloadRoute).
const cache = new Map<string, PageModule & LayoutModule>();

function layoutChain(dir: string): Thunk<LayoutModule>[] {
  const chain: Thunk<LayoutModule>[] = [];
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
): { layouts: Thunk<LayoutModule>[]; page: Thunk<PageModule>; params: Record<string, string> } | null {
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
    if (ok) return { layouts: layoutChain(p.dir), page: { id: p.id, load: p.load }, params };
  }
  return null;
}

// Load (and cache) the chunks for the matched chain — called before rendering
// on the server and before committing a navigation on the client.
export async function preloadRoute(url: string): Promise<void> {
  const m = matchRoute(url);
  if (!m) return;
  await Promise.all(
    [...m.layouts, m.page].map(async (n) => {
      if (!cache.has(n.id)) cache.set(n.id, (await n.load()) as PageModule & LayoutModule);
    }),
  );
}

export async function loadRouteData(url: string): Promise<DataMap> {
  await preloadRoute(url);
  const m = matchRoute(url);
  if (!m) return {};
  const nodes = [...m.layouts, m.page];
  const entries = await Promise.all(
    nodes.map(async (n) => {
      const loader = cache.get(n.id)?.loader;
      return [n.id, loader ? ((await loader({ params: m.params, url })) as RouteData) : null] as const;
    }),
  );
  return Object.fromEntries(entries);
}

// Collect and merge the matched chain's head descriptors (each node's `meta`
// gets its own loader-data slice). Deeper routes win: last `title`, last of
// each named meta.
export function resolveMeta(url: string, data: DataMap): MetaDescriptor[] {
  const m = matchRoute(url);
  if (!m) return [];
  let title: string | undefined;
  const byName = new Map<string, string>();
  const byProperty = new Map<string, string>(); // Open Graph et al.
  for (const n of [...m.layouts, m.page]) {
    const meta = cache.get(n.id)?.meta;
    if (typeof meta !== "function") continue;
    for (const d of meta({ params: m.params, url, data: data[n.id] ?? null })) {
      if (d.title !== undefined) title = d.title;
      else if (d.name) byName.set(d.name, d.content ?? "");
      else if (d.property) byProperty.set(d.property, d.content ?? "");
    }
  }
  const out: MetaDescriptor[] = [];
  if (title !== undefined) out.push({ title });
  for (const [name, content] of byName) out.push({ name, content });
  for (const [property, content] of byProperty) out.push({ property, content });
  return out;
}

const escapeHtml = (s: string) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// Server: render descriptors to head HTML for the SSR shell.
export function metaToHtml(descriptors: MetaDescriptor[]): string {
  return descriptors
    .map((d) => {
      if (d.title !== undefined) return `<title>${escapeHtml(d.title)}</title>`;
      const attr = d.property ? `property="${escapeHtml(d.property)}"` : `name="${escapeHtml(d.name ?? "")}"`;
      return `<meta data-oj-meta ${attr} content="${escapeHtml(d.content ?? "")}">`;
    })
    .join("\n");
}

// Client: apply descriptors to the live document on navigation. oj-managed meta
// tags are replaced wholesale so stale ones from the previous route are cleared.
export function applyMeta(descriptors: MetaDescriptor[]): void {
  document.querySelectorAll("meta[data-oj-meta]").forEach((el) => el.remove());
  for (const d of descriptors) {
    if (d.title !== undefined) {
      document.title = d.title;
    } else if (d.name || d.property) {
      const el = document.createElement("meta");
      el.setAttribute("data-oj-meta", "");
      el.setAttribute(d.property ? "property" : "name", (d.property ?? d.name)!);
      el.setAttribute("content", d.content ?? "");
      document.head.appendChild(el);
    }
  }
}

export async function actionRoute(url: string, body: string): Promise<void> {
  await preloadRoute(url);
  const m = matchRoute(url);
  await cache.get(m?.page.id ?? "")?.action?.({ params: m!.params, url, body });
}

export function App({ url = "/", data = {}, error = null }: { url?: string; data?: DataMap; error?: string | null }) {
  const m = matchRoute(url);
  if (!m) return <h1 data-page="notfound">404: {url}</h1>;
  const Page = cache.get(m.page.id)?.default;
  if (!Page) return <RouteError error={`route chunk not loaded: ${url}`} />; // preloadRoute wasn't awaited
  const content = error ? <RouteError error={error} /> : <Page data={data[m.page.id] ?? null} params={m.params} />;
  return m.layouts.reduceRight((child, layout) => {
    const Layout = cache.get(layout.id)?.default;
    return Layout ? <Layout data={data[layout.id] ?? null}>{child}</Layout> : child;
  }, content);
}
