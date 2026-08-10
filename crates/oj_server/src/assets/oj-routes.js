// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// oj built-in: a file-based route manifest generated from src/routes/. Import
// it as `virtual:oj-routes`. oj compiles this module at the app root, so the
// glob resolves against <root>/src/routes and each match is a lazily-loaded
// (code-split) route.
//
// Convention: `index` -> "/", nested dirs -> nested paths, a dynamic segment
// written `$param` or `[param]` -> ":param", `[...rest]` -> "*". `layout` files
// are excluded (they are not pages).
const mods = import.meta.glob("./src/routes/**/*.tsx");

export const routes = Object.entries(mods)
  .map(([file, load]) => ({ rel: file.replace(/^.*\/routes\//, "").replace(/\.tsx$/, ""), load }))
  .filter(({ rel }) => rel !== "layout" && !rel.endsWith("/layout"))
  .map(({ rel, load }) => {
    let path = "/" + rel.replace(/\/?index$/, "");
    path = path
      .replace(/\[\.\.\.[^\]]+\]/g, "*")
      .replace(/\[([^\]]+)\]/g, ":$1")
      .replace(/\$([A-Za-z0-9_]+)/g, ":$1");
    if (path.length > 1) path = path.replace(/\/+$/, "");
    return { path: path || "/", load };
  })
  .sort((a, b) => a.path.localeCompare(b.path));

export default routes;
