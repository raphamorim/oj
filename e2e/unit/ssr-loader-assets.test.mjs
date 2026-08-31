// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const loader = resolve(here, "../../crates/oj_server/src/assets/start/loader.mjs");

test("SSR loader resolves tsconfig-aliased static assets as URL modules", () => {
  const app = mkdtempSync(join(tmpdir(), "oj-ssr-asset-"));

  try {
    const source = join(app, "src");
    const images = join(source, "images");
    const fonts = join(source, "fonts");
    const rolldown = join(app, "node_modules", "rolldown");

    for (const directory of [images, fonts, rolldown]) {
      mkdirSync(directory, { recursive: true });
    }

    writeFileSync(join(app, "package.json"), JSON.stringify({ name: "synthetic-asset-app", type: "module" }));
    writeFileSync(join(app, "tsconfig.json"), JSON.stringify({
      compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } },
    }));
    writeFileSync(join(rolldown, "package.json"), JSON.stringify({
      name: "rolldown",
      type: "module",
      exports: { "./experimental": "./experimental.mjs" },
    }));
    writeFileSync(join(rolldown, "experimental.mjs"), "export const transformSync = (_path, code) => ({ code });\n");

    writeFileSync(join(images, "photo.jpg"), "synthetic-jpeg");
    writeFileSync(join(images, "icon.png"), "synthetic-png");
    writeFileSync(join(fonts, "ui.woff2"), "synthetic-font");
    writeFileSync(join(source, "notes.txt"), "synthetic notes");
    writeFileSync(join(source, "styles.css"), ".example { color: red; }\n");

    const entry = join(source, "entry.mjs");
    writeFileSync(entry, [
      'import photo from "@/images/photo.jpg";',
      'import icon from "@/images/icon.png";',
      'import font from "@/fonts/ui.woff2";',
      'import notes from "@/notes.txt?raw";',
      'import explicitUrl from "@/images/icon.png?url";',
      'import inline from "@/images/icon.png?inline";',
      'import styles from "@/styles.css";',
      "export default { photo, icon, font, notes, explicitUrl, inline, styles };",
    ].join("\n"));

    const runner = [
      'import { registerHooks } from "node:module";',
      `const loader = await import(${JSON.stringify(pathToFileURL(loader).href)});`,
      "registerHooks({ resolve: loader.resolve, load: loader.load });",
      `const result = await import(${JSON.stringify(pathToFileURL(entry).href)});`,
      "process.stdout.write(JSON.stringify(result.default));",
    ].join("\n");

    const result = spawnSync(process.execPath, ["--input-type=module", "--eval", runner], {
      encoding: "utf8",
      timeout: 10_000,
      env: { ...process.env, OJ_APP_ROOT: app, OJ_CACHE_ROOT: join(app, "cache"), OJ_SSR_LOADER_CACHE: "off" },
    });

    assert.equal(result.status, 0, result.stderr || result.error?.message);

    const assets = JSON.parse(result.stdout);
    assert.equal(assets.photo, "/@oj-start/fs" + join(images, "photo.jpg"));
    assert.equal(assets.icon, "/@oj-start/fs" + join(images, "icon.png"));
    assert.equal(assets.font, "/@oj-start/fs" + join(fonts, "ui.woff2"));
    assert.equal(assets.notes, "synthetic notes");
    assert.equal(assets.explicitUrl, assets.icon);
    assert.match(assets.inline, /^data:image\/png;base64,/);
    assert.deepEqual(assets.styles, {});
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});
