import { renderToReadableStream, renderToString } from "react-dom/server";
import { App, actionRoute, loadRoute, type RouteData } from "@/router";

export function load(url = "/"): Promise<RouteData> {
  return loadRoute(url);
}

export function action(url = "/", body = ""): Promise<void> {
  return actionRoute(url, body);
}

export function renderStream(url = "/", data: RouteData = null): Promise<ReadableStream<Uint8Array>> {
  return renderToReadableStream(<App url={url} data={data} />);
}

export function render(url = "/", data: RouteData = null): string {
  return renderToString(<App url={url} data={data} />);
}
