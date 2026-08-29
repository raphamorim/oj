// A .jsx file (JSX + import.meta.glob) exercised through the start SSR loader:
// the loader must run the glob transform for .jsx, not fall through to Node.
const mods = import.meta.glob("../content/*.json", { eager: true });
const titles = Object.values(mods)
  .map((m) => m.default.title)
  .sort()
  .join("|");
export function GlobWidget() {
  return <span>{"jsxglob:" + titles}</span>;
}
