// A plain .js module: the SSR loader must rewrite import.meta.env here too
// (only .ts/.tsx/.jsx used to be transformed, so this threw on `undefined`).
// The VITE_ var comes from `.env.<mode>` (start-define-mode.mjs runs --mode);
// DEV follows NODE_ENV (`NODE_ENV=production oj dev` is not DEV, as in Vite);
// FIXTURE_EDITION reaches the app through the config's custom `envPrefix`.
export const envProbe = `jsenv:${import.meta.env.MODE}:${import.meta.env.SSR}:${import.meta.env.VITE_FIXTURE_FLAVOR ?? "no-flavor"}:${import.meta.env.DEV}:${import.meta.env.FIXTURE_EDITION ?? "no-edition"}`;
