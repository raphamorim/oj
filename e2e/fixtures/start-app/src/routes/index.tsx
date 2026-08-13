import { createRoute, useLoaderData } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { getGreeting } from "../server/data";

// tsconfig paths alias + package.json "imports" map -- both resolve to the
// same module, and both must work on the SSR side.
import { shout } from "#lib/format";
import { shout as shoutViaPaths } from "@app/lib/format";

// A CommonJS dep with `module.exports = {...}` (the loader's CJS facade).
import { badge } from "legacy-cjs";

// A plugin-owned virtual module (resolved via oj's plugin-container bridge).
import { buildTag } from "virtual:build-info";

// svgr: a bare .svg import yields a React component (exportType "default")...
import Logo from "../logo.svg";
// ...and the explicit `?react` query yields one regardless of exportType.
import Star from "../star.svg?react";

// MDX compiled to a component by @mdx-js/rollup (a plugin `transform` hook).
import Welcome from "../content/welcome.mdx";

// Asset conventions: ?raw inlines the text, ?url yields a served URL.
import notes from "../content/notes.txt?raw";
import heroUrl from "../hero.png?url";

// import.meta.glob (eager): a static map of the JSON content files.
const modules = import.meta.glob("../content/*.json", { eager: true }) as Record<
  string,
  { default: { title: string; slug: string } }
>;
const titles = Object.values(modules)
  .map((m) => m.default.title)
  .sort()
  .join(", ");

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  loader: async () => await getGreeting(),
  component: Index,
});

function Index() {
  const data = useLoaderData({ from: indexRoute.id });
  return (
    <main>
      <h1 className="fixture-heading">{shout("home")}</h1>
      <p data-testid="server-fn">{data.message} / edition={data.edition}</p>
      <p data-testid="paths-alias">{shoutViaPaths("alias")}</p>
      <p data-testid="cjs">{badge("interop")}</p>
      <p data-testid="virtual">{buildTag}</p>
      <p data-testid="glob">{titles}</p>
      <p data-testid="raw">{notes.trim()}</p>
      <img data-testid="url" src={heroUrl} alt="hero" />
      <span data-testid="svg"><Logo /></span>
      <span data-testid="svg-react"><Star /></span>
      <section data-testid="mdx"><Welcome /></section>
    </main>
  );
}
