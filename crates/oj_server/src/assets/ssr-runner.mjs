// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Persistent SSR module runner for `oj dev --ssr`.
//
// Instead of building a Rolldown bundle and spawning Node per render, oj spawns
// this once and drives it over stdin/stdout. It links the SSR module graph on
// demand: app source is fetched (already TS/JSX-compiled) from the dev server
// and evaluated as ES modules via `vm.SourceTextModule` with a custom linker;
// node_modules are imported natively in this realm (React elements interop
// across the vm realm via `Symbol.for`, so `renderToString` just works).
//
// A module's identity is its absolute file path, so invalidation is by mtime:
// any source whose file changed — plus every module that transitively imports
// it — is dropped and rebuilt, and `vm` re-evaluates only those. Unchanged
// subtrees and native imports are reused. No bundle step, one long-lived
// process.
//
// Invalidation is push-driven: the runner subscribes to the dev server's HMR
// WebSocket and drops stale modules the instant the server reports a change,
// so the SSR graph tracks edits in the background rather than only at the next
// render. A render-time mtime scan remains as a fallback.
//
// Usage: node --experimental-vm-modules ssr-runner.mjs <baseUrl> <entryAbsPath>

import vm from "node:vm";
import readline from "node:readline";
import { statSync } from "node:fs";

const [BASE, ENTRY] = process.argv.slice(2);

// Route module `console` output to stderr so it never corrupts the stdout
// JSON-lines render protocol.
const vmConsole = new console.Console(process.stderr, process.stderr);

const context = vm.createContext({
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
});

// id (abs path) -> { mod, mtime, importers:Set<id>, evaluated:bool }
const registry = new Map();
// bare specifier -> SyntheticModule wrapping the native import namespace
const externals = new Map();

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
        } catch {} // getters that throw off a namespace: skip
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

async function build(id) {
  const existing = registry.get(id);
  if (existing) return existing.mod;
  const code = await fetchText(`${BASE}/@ssr-module?id=${enc(id)}`);
  const mod = new vm.SourceTextModule(code, {
    context,
    identifier: id,
    initializeImportMeta(meta) {
      meta.url = `file://${id}`;
    },
  });
  // Register before linking so import cycles resolve to this instance.
  registry.set(id, { mod, mtime: mtimeOf(id), importers: new Set(), evaluated: false });
  await mod.link(linker);
  return mod;
}

// Drop every module whose file changed, plus everything that transitively
// imports it, so the next render rebuilds and re-evaluates only those.
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

async function entryNamespace() {
  invalidate();
  let rec = registry.get(ENTRY);
  if (!rec || !rec.evaluated) {
    const mod = await build(ENTRY);
    await mod.evaluate(); // evaluates only not-yet-evaluated modules in the graph
    rec = registry.get(ENTRY);
    rec.evaluated = true;
  }
  return rec.mod.namespace;
}

// Produce the render for `url`. First runs the route loader (if any) and emits
// `{data}` (JSON, `<` escaped so it can't close the inline <script>) for the
// transport to serialize into the document; then the render output — `{chunk}`…
// `{end}` for a streaming entry (renderStream), or a single `{html}`.
async function handleRender(emit, url) {
  const ns = await entryNamespace();
  const data = typeof ns.load === "function" ? await ns.load(url) : null;
  emit({ data: JSON.stringify(data ?? null).replace(/</g, "\\u003c") });
  if (typeof ns.renderStream === "function") {
    const stream = await ns.renderStream(url, data);
    const reader = stream.getReader();
    const decoder = new TextDecoder();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      const chunk = decoder.decode(value, { stream: true });
      if (chunk) emit({ chunk });
    }
    const tail = decoder.decode();
    if (tail) emit({ chunk: tail });
    emit({ end: true });
  } else if (typeof ns.render === "function") {
    emit({ html: String(await ns.render(url, data)) });
  } else {
    throw new Error(`SSR entry ${ENTRY} exports neither render() nor renderStream()`);
  }
}

// Serialize renders and push-invalidations so a change event can't delete a
// module mid-render (Node is single-threaded, but both are async).
let lock = Promise.resolve();
function withLock(fn) {
  const run = lock.then(fn, fn);
  lock = run.then(
    () => {},
    () => {},
  );
  return run;
}

// Server-side HMR push: subscribe to the dev server's HMR channel and drop
// stale SSR modules the moment the server reports a change, rather than waiting
// to poll mtimes at the next render. The message is just a "re-check now"
// trigger; invalidate() (mtime-precise) decides exactly what to drop.
function connectHmr() {
  if (typeof WebSocket === "undefined") return; // older Node: fall back to poll
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
    withLock(() => {
      const dropped = invalidate();
      if (dropped) process.stderr.write(`oj ssr: hmr push -> invalidated ${dropped} module(s)\n`);
    });
  });
  ws.addEventListener("close", () => setTimeout(connectHmr, 1000));
  ws.addEventListener("error", () => {}); // close handler retries
}
connectHmr();

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.cmd === "render") {
    const emit = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");
    const url = typeof msg.url === "string" ? msg.url : "/";
    withLock(() => handleRender(emit, url)).catch((e) =>
      emit({ error: String((e && e.stack) || e) }),
    );
  }
});
