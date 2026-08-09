// Server-side mutable state. It's module-scoped, so it persists across requests
// in the long-lived SSR runner (dev) and production server process. The client
// never mutates this directly — it submits an action, which runs here on the
// server.
let likes = 0;

export const getLikes = (): number => likes;
export const addLike = (): number => ++likes;
