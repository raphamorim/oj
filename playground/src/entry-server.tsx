import { renderToReadableStream, renderToString } from "react-dom/server";
import { App, actionRoute, loadRouteData, preloadRoute, type DataMap } from "@/router";

// Loader: preloads the matched chunks, then runs every loader in the chain.
export function load(url = "/"): Promise<DataMap> {
  return loadRouteData(url);
}

export function action(url = "/", body = ""): Promise<void> {
  return actionRoute(url, body);
}

// render/renderStream are async so they can load the route's code-split chunks
// before rendering (App renders from the module cache synchronously).
export async function renderStream(url = "/", data: DataMap = {}): Promise<ReadableStream<Uint8Array>> {
  await preloadRoute(url);
  return renderToReadableStream(<App url={url} data={data} />);
}

export async function render(url = "/", data: DataMap = {}): Promise<string> {
  await preloadRoute(url);
  return renderToString(<App url={url} data={data} />);
}
