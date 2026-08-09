import { renderToReadableStream, renderToString } from "react-dom/server";
import { SsrApp } from "@/ssr-app";

// Streaming SSR: the dev server prefers this, flushing chunks as React produces
// them (shell first, deferred Suspense content later).
export function renderStream(): Promise<ReadableStream<Uint8Array>> {
  return renderToReadableStream(<SsrApp />);
}

// Buffered fallback, also used by the production `oj build --ssr` bundle.
export function render(): string {
  return renderToString(<SsrApp />);
}
