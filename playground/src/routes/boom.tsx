import type { LoaderArgs } from "@/router";

// A loader failure: surfaces as the route error UI (never renders the default).
export function loader(_args: LoaderArgs): never {
  throw new Error("boom: the loader failed");
}

export default function Boom() {
  return <h1>unreachable</h1>;
}
