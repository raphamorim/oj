// SPDX-License-Identifier: MIT

import { readFileSync, statSync, truncateSync, unlinkSync } from "node:fs";
import { resolve as pathResolve, dirname } from "node:path";
import { pathToFileURL } from "node:url";
import { createRequire } from "node:module";
import { createHash } from "node:crypto";

export const EXTS = [
  ".ts", ".tsx", ".js", ".jsx", ".mjs",
  "/index.ts", "/index.tsx", "/index.js", "/index.jsx", "/index.mjs",
];

export const isFile = (p) => {
  try { return statSync(p).isFile(); } catch { return false; }
};

export const JS_TO_TS = { ".js": [".ts", ".tsx"], ".jsx": [".tsx"], ".mjs": [".mts"], ".cjs": [".cts"] };

export function probe(base) {
  if (isFile(base)) return base;
  for (const [js, tss] of Object.entries(JS_TO_TS)) {
    if (base.endsWith(js)) {
      const stem = base.slice(0, -js.length);
      for (const ts of tss) if (isFile(stem + ts)) return stem + ts;
    }
  }
  for (const ext of EXTS) if (isFile(base + ext)) return base + ext;
  return null;
}

export const RESERVED = new Set(
  ("break case catch class const continue debugger default delete do else enum export extends false finally " +
    "for function if import in instanceof new null return super switch this throw true try typeof var void " +
    "while with yield let static await implements interface package private protected public arguments eval").split(" "),
);

const pkgTypeCache = new Map();
export function nearestPkgType(startDir) {
  if (pkgTypeCache.has(startDir)) return pkgTypeCache.get(startDir);
  let d = startDir, type = "commonjs";
  for (let i = 0; i < 40 && d; i++) {
    const pj = pathResolve(d, "package.json");
    if (isFile(pj)) {
      try { type = JSON.parse(readFileSync(pj, "utf8")).type || "commonjs"; } catch {}
      break;
    }
    const parent = dirname(d);
    if (parent === d) break;
    d = parent;
  }
  pkgTypeCache.set(startDir, type);
  return type;
}

export function hasEsmSyntax(path) {
  try {
    const s = readFileSync(path, "utf8");
    return /(^|[\n;])\s*export\s/.test(s) || /(^|[\n;])\s*import\s[^(]/.test(s);
  } catch {
    return false;
  }
}

export function isCjsFile(path) {
  if (path.endsWith(".cjs")) return true;
  if (path.endsWith(".mjs")) return false;
  if (!path.endsWith(".js")) return false;
  if (nearestPkgType(dirname(path)) === "module") return false;
  return !hasEsmSyntax(path);
}

export function cjsFacade(path) {
  const url = pathToFileURL(path).href;
  const mod = createRequire(url)(path);
  const isEsm = !!(mod && mod.__esModule);
  const enumerable = mod && (typeof mod === "object" || typeof mod === "function") ? Object.keys(mod) : [];
  const names = enumerable.filter((k) => k !== "default" && /^[A-Za-z_$][\w$]*$/.test(k) && !RESERVED.has(k));
  return [
    `import { createRequire as _cr } from "node:module";`,
    `const _m = _cr(${JSON.stringify(url)})(${JSON.stringify(path)});`,
    `export default ${isEsm ? "(_m && _m.default !== undefined ? _m.default : _m)" : "_m"};`,
    ...names.map((n) => `export const ${n} = _m[${JSON.stringify(n)}];`),
  ].join("\n");
}

export function stripJsonc(s) {
  let out = "", i = 0, inStr = false, q = "";
  while (i < s.length) {
    const c = s[i], n = s[i + 1];
    if (inStr) {
      out += c;
      if (c === "\\") { out += n ?? ""; i += 2; continue; }
      if (c === q) inStr = false;
      i++; continue;
    }
    if (c === '"' || c === "'") { inStr = true; q = c; out += c; i++; continue; }
    if (c === "/" && n === "/") { while (i < s.length && s[i] !== "\n") i++; continue; }
    if (c === "/" && n === "*") { i += 2; while (i < s.length && !(s[i] === "*" && s[i + 1] === "/")) i++; i += 2; continue; }
    out += c; i++;
  }
  return out;
}

export function readJsonc(file) {
  try {
    return JSON.parse(stripJsonc(readFileSync(file, "utf8")).replace(/,(\s*[}\]])/g, "$1"));
  } catch {
    return null;
  }
}

export function parseImportsField(imports = {}) {
  return Object.entries(imports)
    .map(([pattern, target]) => [
      pattern,
      typeof target === "string" ? target : target?.import ?? target?.default ?? target?.node,
    ])
    .filter(([, t]) => typeof t === "string");
}

export function mergeTsConfig(chain, fallbackBaseDir) {
  let paths = {}, baseDir = fallbackBaseDir;
  for (const { cfg, dir } of chain) {
    const co = cfg.compilerOptions || {};
    if (co.paths) paths = { ...paths, ...co.paths };
    baseDir = co.baseUrl != null ? pathResolve(dir, co.baseUrl) : dir;
  }
  return { rules: Object.entries(paths).map(([k, v]) => [k, Array.isArray(v) ? v : [v]]), baseDir };
}

export function substituteAlias(pattern, target, spec) {
  if (pattern.includes("*")) {
    const [pre, post = ""] = pattern.split("*");
    if (!spec.startsWith(pre) || !spec.endsWith(post) || spec.length < pre.length + post.length) return null;
    return target.replace("*", spec.slice(pre.length, spec.length - post.length));
  }
  return spec === pattern ? target : null;
}

export function rewriteServerFns(code, rel) {
  if (!code.includes("createServerFn")) return code;
  const re =
    /(^|[\n;])([ \t]*)((?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\()/g;
  let changed = false;
  const out = code.replace(re, (_m, pre, indent, decl, name) => {
    changed = true;
    const id = JSON.stringify(Buffer.from(`${rel}#${name}`).toString("base64url"));
    const meta = `{ id: ${id}, name: ${JSON.stringify(name)}, filename: ${JSON.stringify(rel)} }`;
    const rpc =
      `${indent}export const ${name}_createServerFn_handler = createServerRpc(${meta}, ` +
      `(opts) => ${name}.__executeServer(opts));\n`;
    return `${pre}${rpc}${indent}${decl}${name}_createServerFn_handler, `;
  });
  if (!changed) return code;
  return `import { createServerRpc } from "@tanstack/react-start/server-rpc";\n${out}`;
}

export const PACK_FMT = 2;
export const PACK_PREFIX = 24;
const PACK_LEN_RE = /^[0-9a-f]{8}$/;

export const packHash = (payload) => createHash("sha256").update(payload).digest("hex").slice(0, 16);

export function packLine(obj) {
  const payload = JSON.stringify(obj);
  return Buffer.byteLength(payload).toString(16).padStart(8, "0") + packHash(payload) + payload + "\n";
}

export function packRecordAt(buf, off) {
  if (off + PACK_PREFIX > buf.length) return null;
  const lenHex = buf.toString("latin1", off, off + 8);
  if (!PACK_LEN_RE.test(lenHex)) return null;
  const len = parseInt(lenHex, 16);
  const end = off + PACK_PREFIX + len + 1;
  if (end > buf.length || buf[end - 1] !== 10) return null;
  return {
    off,
    end,
    len,
    hash: buf.toString("latin1", off + 8, off + PACK_PREFIX),
    payloadOff: off + PACK_PREFIX,
    payload: buf.subarray(off + PACK_PREFIX, off + PACK_PREFIX + len),
  };
}

export function packIntegrityFail(store, detail) {
  process.stderr.write(`oj: pack integrity: ${JSON.stringify({ store, ...detail })}\n`);
}

export function scanPack(file, store, epoch, verifyHashes, onRecord) {
  let buf;
  try { buf = readFileSync(file); } catch { return null; }
  const head = packRecordAt(buf, 0);
  let header = null;
  if (head && head.hash === packHash(head.payload)) {
    try { header = JSON.parse(head.payload.toString("utf8")); } catch {}
  }
  if (!header || header.fmt !== PACK_FMT) {
    packIntegrityFail(store, { action: "delete", reason: "format" });
    try { unlinkSync(file); } catch {}
    return null;
  }
  if (header.epoch !== epoch) return null;
  let off = head.end;
  while (off < buf.length) {
    const rec = packRecordAt(buf, off);
    if (!rec) {
      packIntegrityFail(store, { action: "truncate", offset: off, dropped: buf.length - off });
      try { truncateSync(file, off); } catch {}
      return off;
    }
    if ((verifyHashes && rec.hash !== packHash(rec.payload)) || onRecord(rec, buf) === false) {
      packIntegrityFail(store, { action: "skip", offset: off, bytes: rec.end - rec.off });
    }
    off = rec.end;
  }
  return buf.length;
}
