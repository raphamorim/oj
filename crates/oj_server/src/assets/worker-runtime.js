// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const g = globalThis;
const registry = new Map();
const instances = new Map();

g.window ??= g;
g.process ??= { env: { NODE_ENV: "development" } };
g.$RefreshReg$ = () => {};
g.$RefreshSig$ = () => (type) => type;

const makeHot = () => ({
  data: {},
  accept() {},
  dispose() {},
  invalidate() {},
  on() {},
  send() {},
});

g.__oj_register = (url, kind, deps, factory) => {
  registry.set(url, { kind, deps: deps || {}, factory });
};

function instantiate(url) {
  const reg = registry.get(url);
  if (!reg) throw new Error(`[oj] module not registered: ${url}`);
  const module = { exports: {}, hot: makeHot() };
  const record = { module, exports: module.exports, ns: null };
  instances.set(url, record);
  const localRequire = (spec) => requireRaw(reg.deps[spec] ?? spec, reg.kind);
  if (reg.kind === "cjs") {
    reg.factory.call(module.exports, module, module.exports, localRequire);
    record.exports = module.exports;
  } else {
    reg.factory.call(undefined, module, module.exports, localRequire);
  }
  return record;
}

function requireRaw(url, importerKind) {
  const record = instances.get(url) ?? instantiate(url);
  const target = registry.get(url);
  if (importerKind === "esm" && target && target.kind === "cjs") {
    if (!record.ns) record.ns = cjsNamespace(record);
    return record.ns;
  }
  return record.exports;
}

function cjsNamespace(record) {
  const ns = { __proto__: null };
  const raw = () => record.exports;
  Object.defineProperty(ns, "default", {
    enumerable: true,
    get: () => (raw().__esModule ? raw().default : raw()),
  });
  for (const key of Object.keys(record.exports)) {
    if (key !== "default") {
      Object.defineProperty(ns, key, { enumerable: true, get: () => raw()[key] });
    }
  }
  Object.defineProperty(ns, "__cjs_exports", { enumerable: true, get: raw });
  return ns;
}

g.__oj_esm = (exports, getters) => {
  Object.defineProperty(exports, "__esModule", { value: true });
  for (const name of Object.keys(getters)) {
    Object.defineProperty(exports, name, { enumerable: true, get: getters[name] });
  }
};

g.__oj_export_star = (from, exports) => {
  for (const key of Object.keys(from)) {
    if (key !== "default" && !Object.prototype.hasOwnProperty.call(exports, key)) {
      Object.defineProperty(exports, key, { enumerable: true, get: () => from[key] });
    }
  }
};

g.__oj_import_lazy = async (url) => {
  const clean = url.split("?")[0];
  if (!registry.has(url) && !registry.has(clean)) {
    const have = [...registry.keys()].map(encodeURIComponent).join(",");
    await import(`/@oj/lazy.js?id=${encodeURIComponent(url)}&have=${have}`);
  }
  return requireRaw(registry.has(url) ? url : clean, "esm");
};

g.__oj_inject_css = () => {};

g.__oj_start = (entry) => {
  requireRaw(entry, "esm");
};
