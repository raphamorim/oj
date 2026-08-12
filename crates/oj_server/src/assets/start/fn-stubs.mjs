// SPDX-License-Identifier: MIT
// Runtime replacement for @tanstack/start-fn-stubs. The published stubs are
// meant to be compiler-replaced per build and DEFAULT to the server impl, so
// on an untransformed client bundle createIsomorphicFn().client(a).server(b)
// wrongly picks `b`. This branches at runtime on `typeof document` instead.
const isServerRuntime = typeof document === "undefined";

function make(clientImpl, serverImpl) {
  const fn = (...args) => (isServerRuntime ? serverImpl : clientImpl)?.(...args);
  fn.server = (s) => make(clientImpl, s);
  fn.client = (c) => make(c, serverImpl);
  return fn;
}

export function createIsomorphicFn() {
  return make(undefined, undefined);
}
export const createServerOnlyFn = (fn) => (...a) => {
  if (isServerRuntime) return fn(...a);
  throw new Error("createServerOnlyFn: called on the client");
};
export const createClientOnlyFn = (fn) => (...a) => {
  if (!isServerRuntime) return fn(...a);
  throw new Error("createClientOnlyFn: called on the server");
};
