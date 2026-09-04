// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The native plugin seam in dev, driven through the in-tree example plugin
// (`cargo build -p oj --features example-plugin`): the AST pass rewrites a
// marker identifier in served output and JSX still compiles after it; the
// plugin's virtual sheet is served at /@oj/marker.css and its `@marker;`
// directive is expanded inside a stylesheet; editing a marked module pushes a
// css-update for the sheet and the sheet reflects the edit; and a warm restart
// with the persistent cache on replays `module_seen` from the cached side
// channel instead of retransforming (proved by editing the cached meta on
// disk and seeing that value in the sheet).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj --features example-plugin", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-native-plugin-"));
const write = (rel, content) => {
  const p = path.join(app, rel);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, content);
};
write("package.json", JSON.stringify({ name: "native-plugin-app", private: true, type: "module" }));
write("oj.config.json", JSON.stringify({ marker: { prefix: "mk" } }));
write(
  "index.html",
  `<!doctype html><html><head>
<link rel="stylesheet" href="/src/styles.css" />
<link rel="stylesheet" href="/@oj/marker.css" />
</head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);
write("src/main.tsx", `import { hello } from "./hello";\nimport { plain } from "./plain";\ndocument.getElementById("root").textContent = hello() + plain;\n`);
// Starts without a marker: the edit below gives it one, and the sheet must
// learn about it from the recompile, not from a plugin reading the disk.
write("src/plain.tsx", `export const plain = 1;\nif (import.meta.hot) import.meta.hot.accept();\n`);
write(
  "src/hello.tsx",
  `export function hello(): string {
  const tag: string = __MARKER__;
  const el = <span className={__MARKER__}>{tag}</span>;
  return tag + ":" + typeof el;
}
if (import.meta.hot) import.meta.hot.accept();
`,
);
write("src/styles.css", `.a { color: red }\n@marker;\n.b { color: blue }\n`);
// A stub React so the automatic JSX runtime import resolves without an install.
write(
  "node_modules/react/package.json",
  JSON.stringify({
    name: "react",
    version: "0.0.0-stub",
    type: "module",
    exports: { ".": "./index.js", "./jsx-dev-runtime": "./jsx-dev-runtime.js", "./jsx-runtime": "./jsx-runtime.js" },
  }),
);
write("node_modules/react/index.js", "export default {};\n");
write("node_modules/react/jsx-dev-runtime.js", "export const Fragment = 0;\nexport function jsxDEV(type, props) { return { type, props }; }\n");
write("node_modules/react/jsx-runtime.js", "export const Fragment = 0;\nexport function jsx(type, props) { return { type, props }; }\nexport const jsxs = jsx;\n");

async function waitFor(pred, label, ms = 10000) {
  for (let i = 0; i < ms / 100; i++) {
    if (pred()) return;
    await sleep(100);
  }
  throw new Error(`timeout: ${label}`);
}

async function startServer(port) {
  const srv = spawn(oj, ["dev", app, "--port", String(port), "--enable-cache"], { stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  srv.stdout.on("data", (d) => (log += d.toString()));
  srv.stderr.on("data", (d) => (log += d.toString()));
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`http://localhost:${port}/`)).ok) break;
    } catch {}
    await sleep(200);
  }
  return { srv, log: () => log };
}

const text = async (url, headers = {}) => {
  const res = await fetch(url, { headers });
  assert.equal(res.status, 200, `${url} -> ${res.status}`);
  return res.text();
};

const port = 6901;
let server = await startServer(port);
let failed = false;
try {
  assert.ok(server.log().includes("native plugins: example-marker"), `the plugin announces itself:\n${server.log()}`);

  // AST pass: the marker is rewritten to the module's class and JSX compiled after it.
  const hello = await text(`http://localhost:${port}/src/hello.tsx`);
  assert.ok(hello.includes('"mk-hello"'), `marker rewritten:\n${hello}`);
  assert.ok(!hello.includes("__MARKER__"), "no marker left in served output");
  assert.ok(hello.includes("jsx-dev-runtime"), `JSX still compiled after the pass:\n${hello}`);

  // Virtual sheet built from the registry the pass fed.
  const sheetRes = await fetch(`http://localhost:${port}/@oj/marker.css`);
  assert.equal(sheetRes.headers.get("content-type"), "text/css");
  const sheet = await sheetRes.text();
  assert.equal(sheet.trim(), ".mk-hello{--marker-count:2}", `virtual sheet: ${sheet}`);

  // Directive expanded in a stylesheet, in place, through Lightning CSS.
  const styles = await text(`http://localhost:${port}/src/styles.css`, { accept: "text/css" });
  const a = styles.indexOf(".a");
  const m = styles.indexOf(".mk-hello{--marker-count:2}");
  const b = styles.indexOf(".b");
  assert.ok(a >= 0 && m > a && b > m, `directive expanded between .a and .b:\n${styles}`);
  assert.ok(!styles.includes("@marker") && !styles.includes("@oj-directive"), "no directive or sentinel leaks");

  // Change: css-update for the virtual sheet (and the directive sheet), then
  // both reflect the new marker count.
  const frames = [];
  const ws = new WebSocket(`ws://localhost:${port}/__ws`);
  ws.addEventListener("message", (ev) => {
    try {
      frames.push(JSON.parse(ev.data));
    } catch {}
  });
  await new Promise((resolve, reject) => {
    const to = setTimeout(() => reject(new Error("socket did not open")), 8000);
    ws.addEventListener("open", () => {
      clearTimeout(to);
      resolve();
    });
  });
  const before = frames.length;
  fs.appendFileSync(path.join(app, "src", "hello.tsx"), "export const extra = __MARKER__;\n");
  await waitFor(
    () =>
      frames
        .slice(before)
        .some((f) => f.type === "update" && f.updates.some((u) => u.type === "css-update" && u.path === "/@oj/marker.css")),
    "css-update for /@oj/marker.css",
  );
  const update = frames.slice(before).find((f) => f.type === "update");
  const kinds = update.updates.map((u) => `${u.type} ${u.path}`);
  assert.ok(kinds.includes("css-update /src/styles.css"), `directive sheet repushed too: ${kinds}`);
  assert.ok(kinds.includes("js-update /src/hello.tsx"), `the module update still goes out: ${kinds}`);
  assert.ok(!frames.slice(before).some((f) => f.type === "full-reload"), "no full reload");
  const sheet2 = await text(`http://localhost:${port}/@oj/marker.css`);
  assert.equal(sheet2.trim(), ".mk-hello{--marker-count:3}", `sheet reflects the edit: ${sheet2}`);
  const styles2 = await text(`http://localhost:${port}/src/styles.css`, { accept: "text/css" });
  assert.ok(styles2.includes("--marker-count:3"), `directive sheet was not served from cache:\n${styles2}`);

  // A module the plugin never saw gains a marker: the host recompiles it
  // before asking the plugins, so the sheet gets the new module in the same
  // css-update.
  const before2 = frames.length;
  fs.appendFileSync(path.join(app, "src", "plain.tsx"), "export const p2 = __MARKER__;\n");
  await waitFor(
    () =>
      frames
        .slice(before2)
        .some((f) => f.type === "update" && f.updates.some((u) => u.type === "css-update" && u.path === "/@oj/marker.css")),
    "css-update for /@oj/marker.css after a plain module gains a marker",
  );
  const sheet3 = await text(`http://localhost:${port}/@oj/marker.css`);
  assert.equal(
    sheet3.trim(),
    ".mk-hello{--marker-count:3}\n.mk-plain{--marker-count:1}",
    `sheet has the newly marked module: ${sheet3}`,
  );
  ws.close();

  // Warm restart: the cached entry carries the side channel; edit it on disk so
  // a replay is distinguishable from a retransform (which would say 3).
  await sleep(500);
  server.srv.kill("SIGKILL");
  await sleep(300);
  const cacheRoot = path.join(app, ".oj-cache", "v1");
  const entries = [];
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith(".json")) entries.push(p);
    }
  };
  walk(cacheRoot);
  let patched = 0;
  for (const p of entries) {
    const raw = fs.readFileSync(p, "utf8");
    if (!raw.includes('"example-marker"')) continue;
    const json = JSON.parse(raw);
    for (const pair of json.meta ?? []) {
      if (pair[0] === "example-marker") {
        pair[1].count = 42;
        patched++;
      }
    }
    fs.writeFileSync(p, JSON.stringify(json));
  }
  // Both versions of hello.tsx (before and after the edit) were cached; each
  // carries the plugin's side channel.
  assert.ok(patched >= 1, `cached modules carry the plugin's side channel (${entries.length} entries)`);

  server = await startServer(port);
  // The crawl serves the module from the persistent cache and replays
  // module_seen from its meta; the sheet shows the patched value.
  await waitFor(() => false, "settle", 800).catch(() => {});
  const helloWarm = await text(`http://localhost:${port}/src/hello.tsx`);
  assert.ok(helloWarm.includes('"mk-hello"'), "warm output is the cached transform");
  const sheetWarm = await text(`http://localhost:${port}/@oj/marker.css`);
  assert.equal(
    sheetWarm.trim(),
    ".mk-hello{--marker-count:42}\n.mk-plain{--marker-count:42}",
    `registry rebuilt from cache meta, no retransform: ${sheetWarm}`,
  );

  console.log("native-plugin-dev: ok");
} catch (e) {
  failed = true;
  console.error(e);
  console.error(server.log());
} finally {
  server.srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
