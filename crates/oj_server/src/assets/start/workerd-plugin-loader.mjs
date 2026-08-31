// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import http from "node:http";
import { resolve as pathResolve } from "node:path";
import { existsSync, readFileSync } from "node:fs";

import { loadPluginContainer } from "./vite-plugin-bridge.mjs";

const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const PORT = Number(process.argv[2] || process.env.OJ_LOADER_PORT || 0);

function importerFor(referrer) {
  if (!referrer) return undefined;
  const rel = referrer.replace(/^\//, "");
  const abs = pathResolve(APP, rel);
  return existsSync(abs) ? abs : referrer.startsWith("/") ? referrer : undefined;
}

const container = await loadPluginContainer(APP, {
  command: "serve",
  mode: "development",
  environment: "ssr",
});

if (!container) {
  process.stderr.write("oj workerd-plugin-loader: no plugin container (no vite config?)\n");
  process.exit(0);
}

try {
  await container.buildStart?.();
} catch (e) {
  process.stderr.write(`oj workerd-plugin-loader: buildStart failed: ${(e && e.message) || e}\n`);
}

const server = http.createServer(async (req, res) => {
  try {
    const u = new URL(req.url, "http://loader");
    if (u.pathname === "/transform") {
      const file = u.searchParams.get("file") ?? "";
      let raw;
      try {
        raw = readFileSync(file, "utf8");
      } catch {
        res.writeHead(404);
        res.end();
        return;
      }
      const out = (await container.transformUserCode(raw, file)) ?? raw;
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ code: out }));
      return;
    }
    const specifier =
      u.searchParams.get("rawSpecifier") || u.searchParams.get("specifier") || "";
    const referrer = u.searchParams.get("referrer") ?? "";
    const rid = await container.resolveId(specifier, importerFor(referrer));
    if (rid == null) {
      res.writeHead(404);
      res.end();
      return;
    }
    const loaded = await container.load(rid);
    if (loaded == null) {
      res.writeHead(404);
      res.end();
      return;
    }
    const transformed = (await container.transform(loaded, rid)) ?? loaded;
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ code: transformed }));
  } catch (e) {
    process.stderr.write(`oj workerd-plugin-loader: ${(e && e.message) || e}\n`);
    res.writeHead(500);
    res.end();
  }
});

server.listen(PORT, "127.0.0.1", () => {
  const port = server.address().port;
  process.stdout.write(`OJ_LOADER_PORT=${port}\n`);
});
