import { Suspense } from "react";
import { Counter } from "@/Counter";
import { Deferred } from "@/Deferred";

// The SSR app tree, shared by the server entry and the client hydration entry
// so both render (and hydrate) exactly the same thing. The Suspense boundary
// makes streaming observable: the Counter + fallback flush immediately, the
// Deferred content streams in once its promise resolves.
export function SsrApp() {
  return (
    <>
      <Counter label="ssr" />
      <Suspense fallback={<span data-loading>loading…</span>}>
        <Deferred />
      </Suspense>
    </>
  );
}
