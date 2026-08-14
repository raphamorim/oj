// SPDX-License-Identifier: MIT
// Persistent CSS compiler for dev. Runs the app's PostCSS with
// @tailwindcss/postcss so Tailwind v4 stylesheets (`@import "tailwindcss"`,
// `@plugin`, `@theme`, and the `@import` graph) compile to real CSS instead of
// being served raw. Protocol: JSON lines over stdio, {id, path} in, {id, css}
// or {id, error} out. A fresh processor per file lets Tailwind rescan the
// project's sources so newly used classes appear after a reload.
//
// stdout is the protocol channel; PostCSS/Tailwind (and their deps) may log, so
// divert stdout to stderr and keep a private writer for protocol frames.
import { importPkg } from "./resolve-pkg.mjs";
import { readFileSync } from "node:fs";
import readline from "node:readline";

const send = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const postcss = await importPkg(APP, "postcss", []);
const twMod = await importPkg(APP, "@tailwindcss/postcss", ["tailwindcss"]);
const tailwind = twMod.default ?? twMod;

async function compile(path) {
  const src = readFileSync(path, "utf8");
  const result = await postcss([tailwind()]).process(src, { from: path });
  return result.css;
}

const OJ = process.stderr.isTTY && !process.env.NO_COLOR ? "\x1b[1;38;2;42;51;212moj\x1b[0m" : "oj";
process.stderr.write(`${OJ} css host: ready\n`);
const rl = readline.createInterface({ input: process.stdin });
for await (const line of rl) {
  let msg;
  try { msg = JSON.parse(line); } catch { continue; }
  try {
    send(JSON.stringify({ id: msg.id, css: await compile(msg.path) }) + "\n");
  } catch (e) {
    send(JSON.stringify({ id: msg.id, error: String((e && e.stack) || e) }) + "\n");
  }
}
