// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import vm from "node:vm";
import readline from "node:readline";
import { fstatSync, statSync } from "node:fs";

const [BASE, ENTRY] = process.argv.slice(2);

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

const registry = new Map();
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

async function entryNamespace() {
  invalidate();
  let rec = registry.get(ENTRY);
  if (!rec || !rec.evaluated) {
    const mod = await build(ENTRY);
    await mod.evaluate();
    rec = registry.get(ENTRY);
    rec.evaluated = true;
  }
  return rec.mod.namespace;
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

async function handleCall(emit, id, name, args) {
  const ns = await moduleNamespace(id);
  const fn = name === "default" ? ns.default : ns[name];
  if (typeof fn !== "function") {
    throw new Error(`server function "${name}" not found in ${id}`);
  }
  const result = await fn(...(Array.isArray(args) ? args : []));
  emit({ result: JSON.stringify(result ?? null) });
}

const serialize = (data) => JSON.stringify(data ?? null).replace(/</g, "\\u003c");

async function loadData(ns, url) {
  return typeof ns.load === "function" ? await ns.load(url) : null;
}

async function handleLoad(emit, url) {
  emit({ data: serialize(await loadData(await entryNamespace(), url)) });
}

async function handleAction(emit, url, body) {
  const ns = await entryNamespace();
  if (typeof ns.action === "function") await ns.action(url, body);
  emit({ data: serialize(await loadData(ns, url)) });
}

async function handleRender(emit, url) {
  const ns = await entryNamespace();
  const data = await loadData(ns, url);
  const head = typeof ns.head === "function" ? String(await ns.head(url, data)) : "";
  emit({ data: serialize(data), head });
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

let lock = Promise.resolve();
function withLock(fn) {
  const run = lock.then(fn, fn);
  lock = run.then(
    () => {},
    () => {},
  );
  return run;
}

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
    withLock(() => {
      const dropped = invalidate();
      if (dropped) process.stderr.write(`oj ssr: hmr push -> invalidated ${dropped} module(s)\n`);
    });
  });
  ws.addEventListener("close", () => setTimeout(connectHmr, 1000));
  ws.addEventListener("error", () => {});
}
connectHmr();

try {
  if (fstatSync(0).isFIFO()) {
    process.stdin.once("end", () => process.exit(0));
    process.stdin.once("close", () => process.exit(0));
  }
} catch {}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const emit = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");
  const url = typeof msg.url === "string" ? msg.url : "/";
  const body = typeof msg.body === "string" ? msg.body : "";
  const fail = (e) => emit({ error: String((e && e.stack) || e) });
  if (msg.cmd === "render") withLock(() => handleRender(emit, url)).catch(fail);
  else if (msg.cmd === "load") withLock(() => handleLoad(emit, url)).catch(fail);
  else if (msg.cmd === "action") withLock(() => handleAction(emit, url, body)).catch(fail);
  else if (msg.cmd === "call") withLock(() => handleCall(emit, msg.module, msg.name, msg.args)).catch(fail);
});
