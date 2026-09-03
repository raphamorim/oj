// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { fstatSync } from "node:fs";
import { pathToFileURL } from "node:url";
import path from "node:path";
import readline from "node:readline";

const load = (base, spec) => {
  const req = createRequire(path.join(base, "package.json"));
  return import(pathToFileURL(req.resolve(spec)).href).then((m) => m.default ?? m);
};

// `opts` is css.preprocessorOptions.less / .stylus, passed through like Vite
// does (javascriptEnabled, globalVars, modifyVars, paths, define, ...).
async function less(base, css, from, opts = {}) {
  const less = await load(base, "less");
  const { paths = [], ...rest } = opts;
  const out = await less.render(css, { ...rest, filename: from, paths: [path.dirname(from), ...paths] });
  return out.css;
}

async function stylus(base, css, from, opts = {}) {
  const stylus = await load(base, "stylus");
  const { paths = [], define, imports, ...rest } = opts;
  return await new Promise((resolve, reject) => {
    const s = stylus(css).set("filename", from).set("paths", [path.dirname(from), ...paths]);
    for (const [k, v] of Object.entries(rest)) s.set(k, v);
    if (define && typeof define === "object") for (const [k, v] of Object.entries(define)) s.define(k, v);
    if (Array.isArray(imports)) for (const i of imports) s.import(i);
    s.render((err, out) => (err ? reject(err) : resolve(out)));
  });
}

const rl = readline.createInterface({ input: process.stdin });
let inflight = 0;
let stdinClosed = false;
const maybeExit = () => { if (stdinClosed && inflight === 0) process.exit(0); };
try {
  if (fstatSync(0, { bigint: true }).isFIFO()) rl.once("close", () => { stdinClosed = true; maybeExit(); });
} catch {}
rl.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const { id, base, css, from } = msg;
  const opts = msg.options && typeof msg.options === "object" ? msg.options : {};
  const ext = String(from || "").split("?")[0].split(".").pop().toLowerCase();
  inflight += 1;
  try {
    let out = css;
    if (ext === "less") out = await less(base, css, from, opts);
    else if (ext === "styl" || ext === "stylus") out = await stylus(base, css, from, opts);
    process.stdout.write(JSON.stringify({ id, css: out }) + "\n");
  } catch (e) {
    process.stdout.write(JSON.stringify({ id, error: String((e && e.message) || e) }) + "\n");
  } finally {
    inflight -= 1;
    maybeExit();
  }
});
