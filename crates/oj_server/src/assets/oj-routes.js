// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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
