import { use } from "react";

// A component that suspends on a promise resolving after a short delay, so SSR
// streaming flushes the shell + fallback first and the resolved content later.
// The promise is module-scoped: one delay per process on the server, and a
// fresh (identically-resolving) one in the browser so hydration matches.
let deferred: Promise<string> | undefined;
function load(): Promise<string> {
  deferred ??= new Promise((resolve) => setTimeout(() => resolve("deferred-streamed"), 80));
  return deferred;
}

export function Deferred() {
  return <span data-deferred>{use(load())}</span>;
}
