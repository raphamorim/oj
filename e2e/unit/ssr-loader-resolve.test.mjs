// SPDX-License-Identifier: MIT
//
// The SSR loader resolves a specifier through one shared ladder (relative,
// aliases, #imports, tsconfig paths, Node) and only then classifies the
// resolved path as asset / svg-react / plain module: Vite's ordering, where
// the alias plugin rewrites specifiers before the asset plugin sees them.
// These tests pin that ordering: asset tagging must apply to *every*
// resolution route, and version tagging must survive the shared ladder.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const loader = resolve(here, "../../crates/oj_server/src/assets/start/loader.mjs");

function makeApp() {
  // realpath: on macOS the tmpdir is a symlink (/var -> /private/var) and
  // Node's resolver reports realpathed URLs for node_modules hits.
  const app = realpathSync(mkdtempSync(join(tmpdir(), "oj-ssr-resolve-")));
  const source = join(app, "src");
  const icons = join(source, "icons");
  const lib = join(source, "lib");
  const assetpkg = join(app, "node_modules", "assetpkg");
  const csspkg = join(app, "node_modules", "csspkg");
  const rolldown = join(app, "node_modules", "rolldown");

  for (const directory of [icons, lib, assetpkg, csspkg, rolldown]) {
    mkdirSync(directory, { recursive: true });
  }

  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-resolve-app", type: "module" }));
  writeFileSync(join(app, "tsconfig.json"), JSON.stringify({
    compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
  }));
  writeFileSync(join(rolldown, "package.json"), JSON.stringify({
    name: "rolldown",
    type: "module",
    exports: { "./experimental": "./experimental.mjs" },
  }));
  writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");

  // A dep whose exports map lands a non-asset-looking subpath on an asset
  // file: only resolved-path classification can tag it.
  writeFileSync(join(assetpkg, "package.json"), JSON.stringify({
    name: "assetpkg",
    exports: { "./logo": "./logo.png" },
  }));
  writeFileSync(join(assetpkg, "logo.png"), "synthetic-png");

  // A dep whose legacy main is a stylesheet: same story for bare specifiers.
  writeFileSync(join(csspkg, "package.json"), JSON.stringify({ name: "csspkg", main: "./styles.css" }));
  writeFileSync(join(csspkg, "styles.css"), ".pkg { color: blue; }\n");

  writeFileSync(join(icons, "logo.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>\n");
  writeFileSync(join(source, "local.png"), "synthetic-local-png");
  writeFileSync(join(source, "track.mp3"), "synthetic-audio");
  writeFileSync(join(lib, "util.ts"), "export const moduleUrl = import.meta.url;\n");

  const entry = join(source, "entry.mjs");
  writeFileSync(entry, [
    'import logoUrl from "assetpkg/logo";',
    'import styles from "csspkg";',
    'import iconSvg from "@/icons/logo.svg?react";',
    'import localUrl from "./local.png";',
    'import { moduleUrl } from "@/lib/util";',
    'import { transformSync } from "rolldown/experimental";',
    "export default { logoUrl, styles, iconSvg, localUrl, moduleUrl, bareModuleOk: typeof transformSync === \"function\" };",
  ].join("\n"));

  return { app, source, icons, lib, assetpkg, csspkg, entry };
}

test("SSR loader resolves through one ladder, then classifies the resolved path", () => {
  const { app, source, icons, assetpkg, csspkg, entry } = makeApp();

  try {
    const runner = [
      'import { registerHooks, createRequire } from "node:module";',
      'import { pathToFileURL } from "node:url";',
      `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
      "loader.setVersion(9);",
      "registerHooks({ resolve: loader.resolve, load: loader.load });",
      `const entryUrl = ${JSON.stringify(pathToFileURL(entry).href)};`,
      `const values = (await import(entryUrl)).default;`,
      "",
      // Direct resolve() calls to assert URL shapes the imported values can't
      // distinguish (tag kind, version query).
      "const ctx = { parentURL: entryUrl, conditions: [] };",
      'const notFound = () => { const e = new Error("Cannot find module"); e.code = "ERR_MODULE_NOT_FOUND"; throw e; };',
      "const req = createRequire(entryUrl);",
      "const nodeNext = (s) => ({ url: pathToFileURL(req.resolve(s)).href });",
      "const direct = {};",
      'direct.svgReact = loader.resolve("@/icons/logo.svg?react", ctx, notFound).url;',
      'direct.aliasAsset = loader.resolve("@/local.png", ctx, notFound).url;',
      // Extension list matches the Rust client-side classifier (audio & co).
      'direct.audioAsset = loader.resolve("@/track.mp3", ctx, notFound).url;',
      // Root-relative ids resolve against the project root (Vite asSrc).
      'direct.rootRelAsset = loader.resolve("/src/local.png", ctx, notFound).url;',
      'direct.rootRelModule = loader.resolve("/src/lib/util", ctx, notFound).url;',
      'direct.cssPkg = loader.resolve("csspkg", ctx, nodeNext).url;',
      'direct.exportsMapAsset = loader.resolve("assetpkg/logo", ctx, nodeNext).url;',
      'direct.versionedModule = loader.resolve("@/lib/util", ctx, notFound).url;',
      'try { loader.resolve("@/lib/util?react", ctx, notFound); direct.nonSvgReactThrew = false; } catch { direct.nonSvgReactThrew = true; }',
      // An already-tagged URL (module runner re-import) must pass through
      // with its query intact, never be re-classified from the extension.
      'const tagged = direct.aliasAsset.replace("?ojasset=url", "?ojasset=inline");',
      "direct.taggedPassthrough = loader.resolve(tagged, ctx, (s) => ({ url: s })).url;",
      "process.stdout.write(JSON.stringify({ values, direct }));",
    ].join("\n");

    const result = spawnSync(process.execPath, ["--input-type=module", "--eval", runner], {
      encoding: "utf8",
      timeout: 10_000,
      env: { ...process.env, OJ_APP_ROOT: app, OJ_CACHE_ROOT: join(app, "cache"), OJ_SSR_LOADER_CACHE: "off" },
    });

    assert.equal(result.status, 0, result.stderr || result.error?.message);
    const { values, direct } = JSON.parse(result.stdout);

    // Exports-map subpath landing on an asset file: classified from the
    // resolved path even though the specifier never looked like an asset.
    assert.equal(values.logoUrl, "/@oj-start/fs" + join(assetpkg, "logo.png"));
    assert.match(direct.exportsMapAsset, /\?ojasset=url$/);

    // Bare package whose main is a stylesheet: tagged css, loads as {}.
    assert.deepEqual(values.styles, {});
    assert.match(direct.cssPkg, /\?ojasset=css$/);
    assert.equal(direct.cssPkg, pathToFileURL(join(csspkg, "styles.css")).href + "?ojasset=css");

    // svg?react through a tsconfig alias goes down the svg-react route
    // (without a plugin container it falls back to a URL default export).
    assert.match(direct.svgReact, /\?ojsvg=react$/);
    assert.equal(direct.svgReact, pathToFileURL(join(icons, "logo.svg")).href + "?ojsvg=react");
    assert.equal(values.iconSvg, "/@oj-start/fs" + join(icons, "logo.svg"));

    // Relative and aliased asset paths still tag through the same ladder.
    assert.equal(values.localUrl, "/@oj-start/fs" + join(source, "local.png"));
    assert.match(direct.aliasAsset, /\?ojasset=url$/);
    assert.match(direct.audioAsset, /\/track\.mp3\?ojasset=url$/);
    assert.match(direct.rootRelAsset, /\/src\/local\.png\?ojasset=url$/);
    assert.match(direct.rootRelModule, /\/src\/lib\/util\.ts\?ojv=9$/);

    // Version tagging survives: local (user-code) hits carry ?ojv, asset
    // URLs never do.
    assert.match(direct.versionedModule, /\?ojv=9$/);
    assert.match(values.moduleUrl, /\/src\/lib\/util\.ts\?ojv=9$/);
    assert.doesNotMatch(direct.aliasAsset, /ojv=/);

    // ?react is only an intent on .svg specifiers; anything else still fails
    // resolution instead of being silently rewritten.
    assert.equal(direct.nonSvgReactThrew, true);

    // A re-imported tagged URL keeps its original tag (inline stays inline,
    // it is not re-classified to url from the .png extension).
    assert.match(direct.taggedPassthrough, /\?ojasset=inline$/);

    // Bare specifiers that resolve to plain modules pass through untouched.
    assert.equal(values.bareModuleOk, true);
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});
