import { renderToReadableStream, renderToString } from "react-dom/server";
import { App, loadRoute, type RouteData } from "@/routes";

// The runner calls load() first, then passes the data to the render so the
// route renders with it and the transport can serialize it for the client.
export function load(url = "/"): Promise<RouteData> {
  return loadRoute(url);
}

export function renderStream(url = "/", data: RouteData = null): Promise<ReadableStream<Uint8Array>> {
  return renderToReadableStream(<App url={url} data={data} />);
}

export function render(url = "/", data: RouteData = null): string {
  return renderToString(<App url={url} data={data} />);
}
