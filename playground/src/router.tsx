import type { ComponentType } from "react";
import { RouteError, type RouteData } from "@/ui";

export { ErrorBoundary, NavContext } from "@/ui";
export type { NavState, RouteData } from "@/ui";

export type LoaderArgs = { params: Record<string, string>; url: string; body?: string };
type RouteModule = {
  default: ComponentType<{ data: RouteData; params: Record<string, string> }>;
  loader?: (args: LoaderArgs) => unknown;
  action?: (args: LoaderArgs) => unknown;
};

// File-based routes: every module under src/routes/ is a route. The URL pattern
// comes from the file path — `index` -> the parent dir, `$param` -> a dynamic
// segment. This is eager so the route table exists synchronously on both the
// server and the client (no per-route code split needed for the fixture).
const modules = import.meta.glob("./routes/**/*.tsx", { eager: true }) as Record<string, RouteModule>;

type Route = { segments: string[]; mod: RouteModule };

function fileToSegments(key: string): string[] {
  const rel = key
    .replace(/^.*\/routes\//, "") // strip everything up to routes/
    .replace(/\.tsx$/, "")
    .replace(/\/?index$/, ""); // index -> parent
  return rel.split("/").filter(Boolean); // "" -> [], "users/$id" -> ["users","$id"]
}

const routes: Route[] = Object.entries(modules).map(([key, mod]) => ({
  segments: fileToSegments(key),
  mod,
}));

export function matchRoute(url: string): { mod: RouteModule; params: Record<string, string> } | null {
  const parts = url.split("?")[0].split("/").filter(Boolean);
  // Static routes win over dynamic ones.
  const ranked = [...routes].sort(
    (a, b) => a.segments.filter((s) => s.startsWith("$")).length - b.segments.filter((s) => s.startsWith("$")).length,
  );
  for (const r of ranked) {
    if (r.segments.length !== parts.length) continue;
    const params: Record<string, string> = {};
    let ok = true;
    for (let i = 0; i < r.segments.length; i++) {
      const seg = r.segments[i];
      if (seg.startsWith("$")) params[seg.slice(1)] = decodeURIComponent(parts[i]);
      else if (seg !== parts[i]) {
        ok = false;
        break;
      }
    }
    if (ok) return { mod: r.mod, params };
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
  if (error) return <RouteError error={error} />;
  const m = matchRoute(url);
  if (!m) return <h1 data-page="notfound">404: {url}</h1>;
  const Component = m.mod.default;
  return <Component data={data} params={m.params} />;
}
