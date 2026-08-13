// Imported two ways by the app: via the package.json "imports" map (#lib/format)
// and via the tsconfig paths alias (@app/lib/format). Both must resolve on the
// SSR side, which Node does not do natively -- oj's loader mirrors them.
export function shout(text: string): string {
  return text.toUpperCase() + "!";
}
