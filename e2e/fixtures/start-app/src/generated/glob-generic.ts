// A .ts module with a NESTED generic import.meta.glob, the exact shape that
// broke oj's start SSR (data.ts's `import.meta.glob<Record<...>>`). It must be
// expanded at transform time, not reach the SSR runtime as a real call.
const mods = import.meta.glob<Record<string, { default: { title: string } }>>(
  "../content/*.json",
  { eager: true },
);
export const genericGlobTitles =
  "tsglob:" +
  Object.values(mods)
    .map((m) => m.default.title)
    .sort()
    .join("|");
