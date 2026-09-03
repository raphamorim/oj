// A plain .js module: the SSR loader must rewrite import.meta.env here too
// (only .ts/.tsx/.jsx used to be transformed, so this threw on `undefined`).
// The VITE_ var comes from `.env.<mode>` (start-define-mode.mjs runs --mode).
export const envProbe = `jsenv:${import.meta.env.MODE}:${import.meta.env.SSR}:${import.meta.env.VITE_FIXTURE_FLAVOR ?? "no-flavor"}`;
