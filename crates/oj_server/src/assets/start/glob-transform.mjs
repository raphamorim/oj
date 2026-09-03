// SPDX-License-Identifier: MIT

import { readdirSync, statSync } from "node:fs";
import { resolve, dirname, relative, sep } from "node:path";

const GLOB_HEAD = /import\.meta\.glob\b/g;

// Skip whitespace, then an optional balanced TypeScript type argument
// (`<...>`, nesting- and string-aware, e.g. `<Record<string, unknown>>`), then
// require `(`. Returns the index just past `(`, or -1 if this is not a call.
function callArgsStart(code, from) {
  let i = from;
  while (i < code.length && /\s/.test(code[i])) i++;
  if (code[i] === "<") {
    let depth = 0, str = "";
    for (; i < code.length; i++) {
      const c = code[i];
      if (str) { if (c === "\\") i++; else if (c === str) str = ""; continue; }
      if (c === '"' || c === "'" || c === "`") str = c;
      else if (c === "<") depth++;
      else if (c === ">") { depth--; if (depth === 0) { i++; break; } }
    }
    while (i < code.length && /\s/.test(code[i])) i++;
  }
  return code[i] === "(" ? i + 1 : -1;
}

function globToRegExp(absGlob) {
  let re = "";
  for (let i = 0; i < absGlob.length; i++) {
    const c = absGlob[i];
    if (c === "*") {
      if (absGlob[i + 1] === "*") {
        re += "(?:.*)";
        i++;
        if (absGlob[i + 1] === "/") i++;
      } else {
        re += "[^/]*";
      }
    } else if (c === "?") {
      re += "[^/]";
    } else if (c === "{") {
      const end = absGlob.indexOf("}", i);
      if (end === -1) { re += "\\{"; continue; }
      re += "(?:" + absGlob.slice(i + 1, end).split(",").map((s) => s.replace(/[.+^${}()|[\]\\]/g, "\\$&")).join("|") + ")";
      i = end;
    } else if ("+^$.()|[]\\".includes(c)) {
      re += "\\" + c;
    } else {
      re += c;
    }
  }
  return new RegExp("^" + re + "$");
}

function walk(dir, out) {
  let entries;
  try { entries = readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of entries) {
    const p = dir + sep + e.name;
    if (e.isDirectory()) walk(p, out);
    else if (e.isFile()) out.push(p);
  }
}

function matchPattern(fileDir, pattern, exhaustive = false) {
  const absGlob = (pattern.startsWith("/") ? pattern : resolve(fileDir, pattern)).split(sep).join("/");
  const starIdx = absGlob.search(/[*?{]/);
  const base = starIdx === -1 ? dirname(absGlob) : absGlob.slice(0, absGlob.lastIndexOf("/", starIdx));
  const re = globToRegExp(absGlob);
  const explicitHidden = absGlob.slice(base.length + 1).split("/")
    .filter((part) => part.startsWith("."))
    .map(globToRegExp);
  const all = [];
  walk(base.split("/").join(sep), all);
  return all.filter((f) => {
    const normalized = f.split(sep).join("/");
    if (!re.test(normalized)) return false;
    // Vite globs with `dot: !!exhaustive` and ignores node_modules unless
    // `exhaustive: true`; a literal dot segment in the pattern still matches.
    if (exhaustive) return true;
    const parts = normalized.slice(base.length + 1).split("/");
    if (parts.includes("node_modules")) return false;
    return parts.every((part) => !part.startsWith(".") || explicitHidden.some((pattern) => pattern.test(part)));
  });
}

const toRel = (fileDir, abs) => {
  let r = relative(fileDir, abs).split(sep).join("/");
  if (!r.startsWith(".")) r = "./" + r;
  return r;
};

export function transformGlob(code, filePath) {
  if (!code.includes("import.meta.glob")) return code;
  const fileDir = dirname(filePath);
  const prelude = [];
  let g = 0, out = "", last = 0, m;
  GLOB_HEAD.lastIndex = 0;
  while ((m = GLOB_HEAD.exec(code))) {
    const argsStart = callArgsStart(code, GLOB_HEAD.lastIndex);
    if (argsStart === -1) continue;
    let i = argsStart, depth = 1, str = "";
    for (; i < code.length && depth > 0; i++) {
      const c = code[i];
      if (str) { if (c === "\\") i++; else if (c === str) str = ""; continue; }
      if (c === '"' || c === "'" || c === "`") str = c;
      else if (c === "(") depth++;
      else if (c === ")") depth--;
    }
    const argsSrc = code.slice(argsStart, i - 1);
    let args;
    try { args = new Function("return [" + argsSrc + "]")(); } catch { continue; }
    GLOB_HEAD.lastIndex = i;
    const patterns = (Array.isArray(args[0]) ? args[0] : [args[0]]).filter((p) => typeof p === "string");
    const opts = args[1] && typeof args[1] === "object" ? args[1] : {};
    const includes = patterns.filter((p) => !p.startsWith("!"));
    const excludes = patterns.filter((p) => p.startsWith("!")).map((p) => p.slice(1));
    const exhaustive = opts.exhaustive === true;
    const exclude = new Set(excludes.flatMap((p) => matchPattern(fileDir, p, exhaustive)));
    const files = [...new Set(includes.flatMap((p) => matchPattern(fileDir, p, exhaustive)))]
      .filter((f) => !exclude.has(f))
      .sort();
    const query = typeof opts.query === "string"
      ? opts.query
      : opts.query && typeof opts.query === "object"
        ? `?${new URLSearchParams(opts.query)}`
        : "";
    const importName = typeof opts.import === "string" ? opts.import : null;
    const wantDefault = importName === "default";
    const entries = files.map((f, idx) => {
      const rel = toRel(fileDir, f);
      const spec = rel + query;
      const key = JSON.stringify(rel);
      if (opts.eager) {
        const id = `__oj_glob${g}_${idx}`;
        prelude.push(wantDefault
          ? `import ${id} from ${JSON.stringify(spec)};`
          : importName
            ? `import { ${importName} as ${id} } from ${JSON.stringify(spec)};`
            : `import * as ${id} from ${JSON.stringify(spec)};`);
        return `${key}: ${id}`;
      }
      const imp = wantDefault
        ? `() => import(${JSON.stringify(spec)}).then((m) => m.default)`
        : importName
          ? `() => import(${JSON.stringify(spec)}).then((m) => m[${JSON.stringify(importName)}])`
          : `() => import(${JSON.stringify(spec)})`;
      return `${key}: ${imp}`;
    });
    out += code.slice(last, m.index) + `{${entries.join(", ")}}`;
    last = i;
    g++;
  }
  out += code.slice(last);
  return prelude.length ? prelude.join("\n") + "\n" + out : out;
}
