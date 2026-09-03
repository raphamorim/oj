// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `server.proxy` parity: a `ws: true` entry tunnels WebSocket connections to
// the target (subprotocol negotiated by the upstream), and HTTP responses are
// streamed through chunk by chunk (server-sent events, long polls) instead of
// being buffered until the upstream finishes.

import { spawn, execSync } from "node:child_process";
import http from "node:http";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const UPSTREAM = 5494;
const PORT = 5495;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// ---- upstream: a slow chunked route and a minimal WebSocket echo server ----
function wsFrame(op, payload) {
  const len = payload.length;
  let head;
  if (len < 126) head = Buffer.from([0x80 | op, len]);
  else { head = Buffer.alloc(4); head[0] = 0x80 | op; head[1] = 126; head.writeUInt16BE(len, 2); }
  return Buffer.concat([head, payload]);
}
function wsParse(buf) {
  const out = []; let off = 0;
  while (off + 2 <= buf.length) {
    const op = buf[off] & 0x0f; const masked = buf[off + 1] & 0x80; let len = buf[off + 1] & 0x7f; off += 2;
    if (len === 126) { len = buf.readUInt16BE(off); off += 2; } else if (len === 127) { len = Number(buf.readBigUInt64BE(off)); off += 8; }
    let mask; if (masked) { mask = buf.subarray(off, off + 4); off += 4; }
    if (off + len > buf.length) break;
    let payload = buf.subarray(off, off + len); off += len;
    if (masked) payload = Buffer.from(payload.map((c, i) => c ^ mask[i % 4]));
    out.push({ op, payload });
  }
  return out;
}
const upstream = http.createServer((req, res) => {
  if (req.url.startsWith("/api/slow")) {
    res.writeHead(200, { "content-type": "text/plain", "x-upstream": "yes" });
    res.write("first");
    setTimeout(() => { res.write("second"); res.end(); }, 800);
    return;
  }
  if (req.url.startsWith("/api/echo")) {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ got: body, host: req.headers.host })); });
    return;
  }
  res.writeHead(404); res.end();
});
upstream.on("upgrade", (req, socket) => {
  const key = req.headers["sec-websocket-key"];
  const accept = crypto.createHash("sha1").update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
  const protos = String(req.headers["sec-websocket-protocol"] || "").split(",").map((s) => s.trim()).filter(Boolean);
  const head = ["HTTP/1.1 101 Switching Protocols", "Upgrade: websocket", "Connection: Upgrade", `Sec-WebSocket-Accept: ${accept}`];
  if (protos.includes("chat")) head.push("Sec-WebSocket-Protocol: chat");
  socket.write(head.join("\r\n") + "\r\n\r\n");
  socket.write(wsFrame(1, Buffer.from("hello from upstream " + req.url)));
  socket.on("data", (buf) => {
    for (const f of wsParse(buf)) {
      if (f.op === 1) socket.write(wsFrame(1, Buffer.from("echo:" + f.payload.toString())));
      else if (f.op === 8) { socket.write(wsFrame(8, f.payload)); socket.end(); }
      else if (f.op === 9) socket.write(wsFrame(10, f.payload));
    }
  });
  socket.on("error", () => {});
});
await new Promise((r) => upstream.listen(UPSTREAM, r));

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-proxyws-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "proxyws-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ server: { proxy: { "/api": { target: `http://localhost:${UPSTREAM}`, ws: true, changeOrigin: true } } } }));
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__OK = 1;\n`);
fs.writeFileSync(path.join(app, "index.html"), `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`);

let failed = false;
const logPath = path.join(os.tmpdir(), "oj-proxyws.log");
const logFd = fs.openSync(logPath, "w");
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", logFd, logFd] });
try {
  for (let i = 0; i < 100; i++) { try { if ((await fetch(`http://localhost:${PORT}/`)).ok) break; } catch {} await sleep(200); }

  // streamed response: the first chunk arrives long before the upstream ends
  const t0 = Date.now();
  const res = await fetch(`http://localhost:${PORT}/api/slow`);
  assert.equal(res.headers.get("x-upstream"), "yes");
  const reader = res.body.getReader();
  const first = await reader.read();
  const tFirst = Date.now() - t0;
  let text = new TextDecoder().decode(first.value);
  for (;;) { const { value, done } = await reader.read(); if (done) break; text += new TextDecoder().decode(value); }
  assert.equal(text, "firstsecond");
  assert.ok(tFirst < 500, `first chunk took ${tFirst}ms: response was buffered`);

  // plain request/response still works, changeOrigin rewrites Host
  const echo = await (await fetch(`http://localhost:${PORT}/api/echo`, { method: "POST", body: "payload" })).json();
  assert.equal(echo.got, "payload");
  assert.equal(echo.host, `localhost:${UPSTREAM}`);

  // handshake diagnostics: the upgrade must be answered with 101
  const hs = await new Promise((resolve) => {
    const r = http.request({ host: "127.0.0.1", port: PORT, path: "/api/ws", headers: { Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==", "Sec-WebSocket-Protocol": "chat" } });
    r.on("upgrade", (res, socket) => { socket.destroy(); resolve({ status: res.statusCode, proto: res.headers["sec-websocket-protocol"] }); });
    r.on("response", (res) => { let b = ""; res.on("data", (c) => (b += c)); res.on("end", () => resolve({ status: res.statusCode, body: b })); });
    r.on("error", (e) => resolve({ error: String(e) }));
    r.end();
  });
  assert.equal(hs.status, 101, `ws handshake through the proxy: ${JSON.stringify(hs)}`);
  // websocket tunnel with subprotocol negotiation
  const got = await new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://localhost:${PORT}/api/ws?x=1`, ["chat", "other"]);
    const msgs = [];
    const timer = setTimeout(() => reject(new Error(`ws timeout; got ${JSON.stringify(msgs)}`)), 10000);
    ws.onopen = () => ws.send("ping-me");
    ws.onmessage = (e) => { msgs.push(String(e.data)); if (msgs.length === 2) { clearTimeout(timer); resolve({ msgs, protocol: ws.protocol }); ws.close(); } };
    ws.onerror = () => { clearTimeout(timer); reject(new Error("ws error")); };
  });
  assert.deepEqual(got.msgs, ["hello from upstream /api/ws?x=1", "echo:ping-me"]);
  assert.equal(got.protocol, "chat", "upstream-selected subprotocol is relayed");
  console.log("PROXY-WS-STREAM E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PROXY-WS-STREAM E2E FAILED:", err.message);
  try { console.error(fs.readFileSync(logPath, "utf8").split("\n").slice(-15).join("\n")); } catch {}
} finally {
  srv.kill("SIGKILL");
  upstream.close();
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
