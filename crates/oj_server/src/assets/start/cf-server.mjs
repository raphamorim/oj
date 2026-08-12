// SPDX-License-Identifier: MIT
// Dev shim for `@cloudflare/vite-plugin/server`, which is a virtual module the
// Cloudflare Vite plugin injects (its package exports only "." and
// "./experimental-config"). The real getCloudflareContext() is backed by a
// workerd/miniflare instance; here we provide the `env` bindings apps read most
// -- the wrangler `vars` plus `.dev.vars` -- merged over process.env. Live KV /
// D1 / R2 / service bindings need workerd emulation and are out of scope; ctx
// is a no-op so `waitUntil`/`passThroughOnException` are safe to call.
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const APP = process.env.OJ_APP_ROOT ?? process.cwd();

// Strip // and /* */ comments outside strings, then trailing commas (wrangler
// config is JSONC and its string values contain `//` in URLs).
function stripJsonc(s) {
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

function wranglerVars() {
  for (const f of ["wrangler.jsonc", "wrangler.json"]) {
    const p = join(APP, f);
    if (!existsSync(p)) continue;
    try {
      const cfg = JSON.parse(stripJsonc(readFileSync(p, "utf8")).replace(/,(\s*[}\]])/g, "$1"));
      return cfg.vars && typeof cfg.vars === "object" ? cfg.vars : {};
    } catch {
      return {};
    }
  }
  // wrangler.toml: a minimal [vars] scan (KEY = "value").
  const toml = join(APP, "wrangler.toml");
  if (existsSync(toml)) {
    const vars = {};
    let inVars = false;
    for (const line of readFileSync(toml, "utf8").split("\n")) {
      const t = line.trim();
      if (t.startsWith("[")) { inVars = t === "[vars]"; continue; }
      const m = inVars && t.match(/^([A-Za-z_][\w]*)\s*=\s*"([^"]*)"/);
      if (m) vars[m[1]] = m[2];
    }
    return vars;
  }
  return {};
}

// .dev.vars is dotenv-style (KEY=VALUE, optional quotes, # comments).
function devVars() {
  const p = join(APP, ".dev.vars");
  if (!existsSync(p)) return {};
  const vars = {};
  for (const line of readFileSync(p, "utf8").split("\n")) {
    const t = line.trim();
    if (!t || t.startsWith("#")) continue;
    const eq = t.indexOf("=");
    if (eq === -1) continue;
    let v = t.slice(eq + 1).trim();
    if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) v = v.slice(1, -1);
    vars[t.slice(0, eq).trim()] = v;
  }
  return vars;
}

let cached;
export async function getCloudflareContext() {
  if (!cached) {
    cached = {
      env: { ...process.env, ...wranglerVars(), ...devVars() },
      cf: {},
      ctx: { waitUntil() {}, passThroughOnException() {} },
    };
  }
  return cached;
}

export default { getCloudflareContext };
