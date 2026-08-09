import { renderToReadableStream, renderToString } from "react-dom/server";
import { App, actionRoute, loadRoute, type RouteData } from "@/routes";

// Loader: runs on the server for the initial render and for client data fetches.
export function load(url = "/"): Promise<RouteData> {
  return loadRoute(url);
}

// Action: a server-side mutation (POST). The caller revalidates via load().
export function action(url = "/", body = ""): Promise<void> {
  return actionRoute(url, body);
}

export function renderStream(url = "/", data: RouteData = null): Promise<ReadableStream<Uint8Array>> {
  return renderToReadableStream(<App url={url} data={data} />);
}

export function render(url = "/", data: RouteData = null): string {
  return renderToString(<App url={url} data={data} />);
}
