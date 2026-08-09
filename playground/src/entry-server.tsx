import { renderToReadableStream, renderToString } from "react-dom/server";
import { App } from "@/routes";

// Streaming SSR, per route: the dev server and the production server pass the
// request path so each URL renders its own tree.
export function renderStream(url = "/"): Promise<ReadableStream<Uint8Array>> {
  return renderToReadableStream(<App url={url} />);
}

// Buffered fallback, also used by the production `oj build --ssr` bundle.
export function render(url = "/"): string {
  return renderToString(<App url={url} />);
}
