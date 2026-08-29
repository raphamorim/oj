// A plain on-disk .js the fixture plugin does NOT claim; its import.meta.glob
// must still be expanded by the start SSR loader's unclaimed-.js branch.
const mods = import.meta.glob("../content/*.json", { eager: true });
export const plainGlobTitles =
  "jsglob:" +
  Object.values(mods)
    .map((m) => m.default.title)
    .sort()
    .join("|");
