import { renderToReadableStream, renderToString } from "react-dom/server";
import { App, actionRoute, loadRouteData, type DataMap } from "@/router";

// Loader: runs every loader in the matched chain (layouts + page) in parallel.
export function load(url = "/"): Promise<DataMap> {
  return loadRouteData(url);
}

export function action(url = "/", body = ""): Promise<void> {
  return actionRoute(url, body);
}

export function renderStream(url = "/", data: DataMap = {}): Promise<ReadableStream<Uint8Array>> {
  return renderToReadableStream(<App url={url} data={data} />);
}

export function render(url = "/", data: DataMap = {}): string {
  return renderToString(<App url={url} data={data} />);
}
