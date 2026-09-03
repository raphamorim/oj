// A plain .js module: the SSR loader must rewrite import.meta.env here too
// (only .ts/.tsx/.jsx used to be transformed, so this threw on `undefined`).
export const envProbe = `jsenv:${import.meta.env.MODE}:${import.meta.env.SSR}`;
