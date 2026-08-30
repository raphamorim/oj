// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const NODE_BUILTIN = /^(node:|bun:|cloudflare:)/;
const EXTERNAL_URL = /^(https?:)?\/\//;

export function createInvokeHandler(deps) {
  async function fetchModule(url, importer, options = {}) {
    if (typeof url !== "string") {
      throw new Error("fetchModule: url must be a string");
    }
    if (url.startsWith("data:")) {
      return { externalize: url, type: "builtin" };
    }
    if (NODE_BUILTIN.test(url) || (deps.isBuiltin && deps.isBuiltin(url))) {
      return { externalize: url, type: "builtin" };
    }
    if (EXTERNAL_URL.test(url) && !url.startsWith("file://")) {
      return { externalize: url, type: "network" };
    }
    const resolved = (await deps.resolveId(url, importer)) ?? null;
    if (!resolved) {
      throw new Error(`Cannot resolve "${url}"${importer ? ` from "${importer}"` : ""}`);
    }
    if (resolved.external) {
      return { externalize: resolved.id, type: resolved.type || "module" };
    }
    const code = await deps.load(resolved.id);
    if (code == null) {
      throw new Error(`No source for "${resolved.id}"`);
    }
    const transformed = await deps.transform(code, resolved.id);
    return {
      code: transformed,
      file: resolved.file ?? resolved.id,
      id: resolved.id,
      url,
      invalidate: !options.cached,
    };
  }

  async function dispatch(name, args) {
    if (name === "fetchModule") return fetchModule(args[0], args[1], args[2]);
    if (name === "getBuiltins") return deps.builtins ? deps.builtins() : [];
    throw new Error(`unknown invoke method: ${name}`);
  }

  return async function handleInvoke(payload) {
    const d = payload && payload.data;
    if (!d || typeof d.name !== "string") {
      return { error: { name: "TransportError", message: "invalid invoke payload" } };
    }
    try {
      const result = await dispatch(d.name, Array.isArray(d.data) ? d.data : []);
      return { result };
    } catch (e) {
      return {
        error: {
          name: (e && e.name) || "Error",
          message: (e && e.message) || String(e),
          stack: e && e.stack,
        },
      };
    }
  };
}
