// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import vm from "node:vm";
import http from "node:http";
import { fstatSync, statSync } from "node:fs";
import { SourceMap } from "node:module";
import path from "node:path";

const [BASE, ENTRY] = process.argv.slice(2);

const vmConsole = new console.Console(process.stderr, process.stderr);

// The web platform surface server code reaches for (Vite evaluates SSR modules
// in the main realm, where all of these exist). Shared by reference from the
// host realm so `instanceof` against a host-created Response/Request holds.
const HOST_GLOBALS = [
  "fetch", "Request", "Response", "Headers", "FormData", "Blob", "File",
  "ReadableStream", "WritableStream", "TransformStream", "TextEncoderStream", "TextDecoderStream",
  "crypto", "Crypto", "CryptoKey", "SubtleCrypto",
  "AbortController", "AbortSignal", "Event", "EventTarget", "CustomEvent", "DOMException",
  "structuredClone", "setImmediate", "clearImmediate", "MessageChannel", "MessagePort", "MessageEvent",
  "BroadcastChannel", "WebSocket", "atob", "btoa", "navigator", "Intl",
  "CompressionStream", "DecompressionStream", "BigInt", "Symbol", "WeakRef", "FinalizationRegistry",
];
const contextGlobals = {
  console: vmConsole,
  process,
  URL,
  URLSearchParams,
  TextEncoder,
  TextDecoder,
  Buffer,
  setTimeout,
  clearTimeout,
  setInterval,
  clearInterval,
  queueMicrotask,
  performance,
};
for (const name of HOST_GLOBALS) {
  if (typeof globalThis[name] !== "undefined") contextGlobals[name] = globalThis[name];
}
const context = vm.createContext(contextGlobals);

const registry = new Map();
const externals = new Map();
// Source maps of the modules served by /@ssr-module, by module id. vm modules
// get no source-map support from Node itself, so, like Vite's ssrFixStacktrace,
// the runner rewrites stack frames through these before an error leaves it.
const sourceMaps = new Map();
const MAP_RE = /\n\/\/# sourceMappingURL=data:application\/json(?:;charset=[\w-]+)?;base64,([A-Za-z0-9+/=]+)\s*$/;
function sourceMapOf(code) {
  const m = MAP_RE.exec(code);
  if (!m) return null;
  try {
    return new SourceMap(JSON.parse(Buffer.from(m[1], "base64").toString("utf8")));
  } catch {
    return null;
  }
}
const FRAME_RE = /^ {4}at (?:(\S.*?)\s\()?(.+?):(\d+)(?::(\d+))?\)?/;
function rewriteStack(stack) {
  return String(stack)
    .split("\n")
    .map((line) =>
      line.replace(FRAME_RE, (input, name, id, ln, col) => {
        const map = sourceMaps.get(id);
        if (!map) return input;
        // Stack positions are 1-based, the map's are 0-based.
        const pos = map.findEntry(Number(ln) - 1, Number(col ?? 1) - 1);
        if (!pos || pos.originalSource == null || pos.originalLine == null) return input;
        const file = path.resolve(path.dirname(id), pos.originalSource);
        const where = `${file}:${pos.originalLine + 1}:${pos.originalColumn + 1}`;
        const fn = name?.trim();
        return !fn || fn === "eval" ? `    at ${where}` : `    at ${fn} (${where})`;
      }),
    )
    .join("\n");
}

const enc = encodeURIComponent;

async function fetchText(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url} -> ${r.status}: ${await r.text()}`);
  return r.text();
}

async function resolve(importer, spec) {
  const r = await fetch(`${BASE}/@ssr-resolve?importer=${enc(importer)}&spec=${enc(spec)}`);
  if (!r.ok) throw new Error(`resolve "${spec}" from ${importer}: ${r.status}`);
  return r.json();
}

function mtimeOf(id) {
  try {
    return statSync(id).mtimeMs;
  } catch {
    return 0;
  }
}

async function externalModule(spec) {
  if (externals.has(spec)) return externals.get(spec);
  const ns = await import(spec);
  const keys = Array.from(new Set([...Object.keys(ns), "default"]));
  const syn = new vm.SyntheticModule(
    keys,
    function () {
      for (const k of keys) {
        try {
          this.setExport(k, ns[k]);
        } catch {}
      }
    },
    { context, identifier: `external:${spec}` },
  );
  await syn.link(() => {
    throw new Error("external module has no imports");
  });
  await syn.evaluate();
  externals.set(spec, syn);
  return syn;
}

async function linker(spec, referencing) {
  const r = await resolve(referencing.identifier, spec);
  if (r.external) return externalModule(r.spec);
  const dep = await build(r.id);
  registry.get(r.id).importers.add(referencing.identifier);
  return dep;
}

async function importDynamic(spec, referencing) {
  const r = await resolve(referencing.identifier, spec);
  if (r.external) return import(r.spec);
  const dep = await build(r.id);
  registry.get(r.id).importers.add(referencing.identifier);
  if (dep.status === "linked") await dep.evaluate();
  return dep;
}

const building = new Map();
async function build(id) {
  const existing = registry.get(id);
  if (existing) return existing.mod;
  const inflight = building.get(id);
  if (inflight) return inflight;
  const p = (async () => {
    const code = await fetchText(`${BASE}/@ssr-module?id=${enc(id)}`);
    const map = sourceMapOf(code);
    if (map) sourceMaps.set(id, map);
    else sourceMaps.delete(id);
    const mod = new vm.SourceTextModule(code, {
      context,
      identifier: id,
      initializeImportMeta(meta) {
        meta.url = `file://${id}`;
      },
      importModuleDynamically: (spec, ref) => importDynamic(spec, ref),
    });
    registry.set(id, { mod, mtime: mtimeOf(id), importers: new Set(), evaluated: false });
    await mod.link(linker);
    return mod;
  })();
  building.set(id, p);
  try {
    return await p;
  } finally {
    building.delete(id);
  }
}

function invalidate() {
  const dirty = new Set();
  for (const [id, rec] of registry) if (mtimeOf(id) !== rec.mtime) dirty.add(id);
  if (!dirty.size) return;
  const stack = [...dirty];
  while (stack.length) {
    const changed = stack.pop();
    const rec = registry.get(changed);
    if (!rec) continue;
    for (const importer of rec.importers) {
      if (!dirty.has(importer)) {
        dirty.add(importer);
        stack.push(importer);
      }
    }
  }
  for (const id of dirty) registry.delete(id);
  return dirty.size;
}

// Evaluation of the entry is shared by concurrent requests: the first one
// builds and evaluates, the rest await the same promise (a module runner's
// import cache), and none of them serializes behind another request.
let entryEval = null;
async function entryNamespace() {
  invalidate();
  const rec = registry.get(ENTRY);
  if (rec && rec.evaluated) return rec.mod.namespace;
  if (!entryEval) {
    entryEval = (async () => {
      const mod = await build(ENTRY);
      await mod.evaluate();
      registry.get(ENTRY).evaluated = true;
      return mod.namespace;
    })().finally(() => { entryEval = null; });
  }
  return entryEval;
}

async function moduleNamespace(id) {
  let rec = registry.get(id);
  if (!rec || !rec.evaluated) {
    const mod = await build(id);
    await mod.evaluate();
    rec = registry.get(id);
    rec.evaluated = true;
  }
  return rec.mod.namespace;
}

const serialize = (data) => JSON.stringify(data ?? null).replace(/</g, "\\u003c");

async function loadData(ns, url) {
  return typeof ns.load === "function" ? await ns.load(url) : null;
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

// Requests arrive over a loopback HTTP server, as they do for the Start runner:
// they run concurrently (a loader that fetches the app's own server during a
// render no longer waits behind itself), action bodies arrive as bytes, and a
// render streams its HTML. `/render` answers with one JSON line of metadata
// (`{ data, head }`) followed by the HTML; the dev server wraps that in the
// document shell.
const JSON_TYPE = { "content-type": "application/json; charset=utf-8" };
const server = http.createServer(async (req, res) => {
  const reqUrl = new URL(req.url ?? "/", "http://runner");
  const url = reqUrl.searchParams.get("url") ?? "/";
  let streaming = false;
  try {
    // Every reply is computed before its head goes out, so a throwing loader
    // or action is a 500 with the stack, never a 200 with an error body.
    if (reqUrl.pathname === "/load") {
      const data = serialize(await loadData(await entryNamespace(), url));
      res.writeHead(200, JSON_TYPE);
      res.end(data);
    } else if (reqUrl.pathname === "/action") {
      const bytes = await readBody(req);
      const ns = await entryNamespace();
      // The body as text (the historical signature) and as the raw bytes, so a
      // binary payload is not lost in the decode.
      if (typeof ns.action === "function") await ns.action(url, bytes.toString("utf8"), new Uint8Array(bytes));
      const data = serialize(await loadData(ns, url));
      res.writeHead(200, JSON_TYPE);
      res.end(data);
    } else if (reqUrl.pathname === "/call") {
      const msg = JSON.parse((await readBody(req)).toString("utf8") || "{}");
      const ns = await moduleNamespace(String(msg.module ?? ""));
      const name = String(msg.name ?? "");
      const fn = name === "default" ? ns.default : ns[name];
      if (typeof fn !== "function") throw new Error(`server function "${name}" not found in ${msg.module}`);
      const result = await fn(...(Array.isArray(msg.args) ? msg.args : []));
      res.writeHead(200, JSON_TYPE);
      res.end(JSON.stringify(result ?? null));
    } else if (reqUrl.pathname === "/render") {
      const ns = await entryNamespace();
      const data = await loadData(ns, url);
      const head = typeof ns.head === "function" ? String(await ns.head(url, data)) : "";
      if (typeof ns.renderStream !== "function" && typeof ns.render !== "function") {
        throw new Error(`SSR entry ${ENTRY} exports neither render() nor renderStream()`);
      }
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      streaming = true;
      res.write(JSON.stringify({ data: serialize(data), head }) + "\n");
      if (typeof ns.renderStream === "function") {
        const reader = (await ns.renderStream(url, data)).getReader();
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          if (value && value.length) res.write(Buffer.from(value));
        }
      } else {
        res.write(String(await ns.render(url, data)));
      }
      res.end();
    } else {
      res.writeHead(404, JSON_TYPE);
      res.end(JSON.stringify({ error: `unknown runner endpoint ${reqUrl.pathname}` }));
    }
  } catch (e) {
    const text = e && e.stack ? rewriteStack(e.stack) : String(e);
    if (streaming) {
      // Mid-stream: the document is already open, so the error lands in it.
      const esc = text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
      res.end(`<pre>[oj ssr] ${esc}</pre>`);
    } else {
      if (!res.headersSent) res.writeHead(500, { "content-type": "text/plain; charset=utf-8" });
      res.end(text);
    }
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
process.stdout.write(JSON.stringify({ port: server.address().port }) + "\n");

function connectHmr() {
  if (typeof WebSocket === "undefined") return;
  let ws;
  try {
    ws = new WebSocket(`${BASE.replace(/^http/, "ws")}/__ws`);
  } catch {
    return;
  }
  ws.addEventListener("message", (ev) => {
    let msg;
    try {
      msg = JSON.parse(String(ev.data));
    } catch {
      return;
    }
    if (msg.type === "error") return;
    const dropped = invalidate();
    if (dropped) process.stderr.write(`oj ssr: hmr push -> invalidated ${dropped} module(s)\n`);
  });
  ws.addEventListener("close", () => setTimeout(connectHmr, 1000));
  ws.addEventListener("error", () => {});
}
connectHmr();

// stdin is the parent's lifeline: when oj goes away the pipe closes and the
// runner follows.
try {
  if (fstatSync(0, { bigint: true }).isFIFO()) {
    process.stdin.once("end", () => process.exit(0));
    process.stdin.once("close", () => process.exit(0));
    process.stdin.resume();
  }
} catch {}
