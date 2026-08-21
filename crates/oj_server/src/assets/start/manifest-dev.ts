// SPDX-License-Identifier: MIT

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

// Lazy CSS: bundle-client.mjs writes css-urls.json when the client bundle
// finishes; reading it per call lets the SSR runner boot before the bundle.
function cssUrls(): string[] {
  try {
    return JSON.parse(readFileSync(join(HERE, "css-urls.json"), "utf8"));
  } catch {
    return [];
  }
}

export const tsrStartManifest = () => ({
  routes: {
    __root__: {
      preloads: ["/@oj-start/client-entry.js"],
      css: cssUrls(),
      scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
    },
  },
});
