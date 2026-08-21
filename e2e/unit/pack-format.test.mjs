// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, readFileSync, statSync, existsSync, truncateSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  PACK_FMT, PACK_PREFIX, packHash, packLine, packRecordAt, scanPack,
} from "../../crates/oj_server/src/assets/start/loader-util.mjs";

const EPOCH = "e".repeat(64);
const header = () => packLine({ fmt: PACK_FMT, epoch: EPOCH });
const mk = () => join(mkdtempSync(join(tmpdir(), "oj-pack-")), "pack.jsonl");

function writePack(file, records) {
  writeFileSync(file, header() + records.map((r) => packLine(r)).join(""));
}

function collect(file, verifyHashes = true) {
  const records = [];
  const bytes = scanPack(file, "test", EPOCH, verifyHashes, (rec) => {
    let e;
    try { e = JSON.parse(rec.payload.toString("utf8")); } catch { return false; }
    records.push(e);
  });
  return { bytes, records };
}

test("packLine round-trips through packRecordAt", () => {
  const obj = { k: "abc", c: "export const x = 'µ✓';" };
  const line = Buffer.from(packLine(obj));
  const rec = packRecordAt(line, 0);
  assert.ok(rec);
  assert.equal(rec.end, line.length);
  assert.equal(rec.hash, packHash(rec.payload));
  assert.deepEqual(JSON.parse(rec.payload.toString("utf8")), obj);
});

test("scanPack reads every record of an intact pack", () => {
  const file = mk();
  writePack(file, [{ k: "a", v: 1 }, { k: "b", v: 2 }]);
  const { bytes, records } = collect(file);
  assert.equal(bytes, statSync(file).size);
  assert.deepEqual(records, [{ k: "a", v: 1 }, { k: "b", v: 2 }]);
});

test("torn tail truncates to the last good record", () => {
  const file = mk();
  writePack(file, [{ k: "a", v: 1 }, { k: "b", v: 2 }, { k: "c", v: 3 }]);
  const goodBytes = Buffer.byteLength(header() + packLine({ k: "a", v: 1 }) + packLine({ k: "b", v: 2 }));
  truncateSync(file, statSync(file).size - 5);
  const { bytes, records } = collect(file);
  assert.equal(bytes, goodBytes);
  assert.equal(statSync(file).size, goodBytes);
  assert.deepEqual(records.map((r) => r.k), ["a", "b"]);
  // The healed file reads back cleanly.
  assert.deepEqual(collect(file).records.map((r) => r.k), ["a", "b"]);
});

test("flipped payload byte skips only that record when verifying", () => {
  const file = mk();
  writePack(file, [{ k: "a", v: 1 }, { k: "b", v: 2 }, { k: "c", v: 3 }]);
  const buf = readFileSync(file);
  const second = Buffer.byteLength(header() + packLine({ k: "a", v: 1 }));
  buf[second + PACK_PREFIX + 2] ^= 0x01;
  writeFileSync(file, buf);
  const { bytes, records } = collect(file, true);
  assert.equal(bytes, buf.length);
  assert.equal(statSync(file).size, buf.length);
  assert.deepEqual(records.map((r) => r.k), ["a", "c"]);
  // Without hash verification the framing still parses, so the record is
  // delivered — the reader that consumes it lazily must check the hash itself.
  assert.equal(collect(file, false).records.length, 3);
});

test("onRecord false skips the record and keeps scanning", () => {
  const file = mk();
  writeFileSync(file, header() + packLine({ k: "a" }) + packLine("not-an-object-but-valid"));
  const seen = [];
  const bytes = scanPack(file, "test", EPOCH, true, (rec) => {
    const e = JSON.parse(rec.payload.toString("utf8"));
    if (typeof e !== "object") return false;
    seen.push(e.k);
  });
  assert.equal(bytes, statSync(file).size);
  assert.deepEqual(seen, ["a"]);
});

test("format-1 pack (no fmt field) is deleted", () => {
  const file = mk();
  writeFileSync(file, JSON.stringify({ epoch: EPOCH }) + "\n" + JSON.stringify({ k: "a", v: 1 }) + "\n");
  assert.equal(collect(file).bytes, null);
  assert.ok(!existsSync(file));
});

test("unknown fmt value is deleted", () => {
  const file = mk();
  writeFileSync(file, packLine({ fmt: PACK_FMT + 1, epoch: EPOCH }) + packLine({ k: "a", v: 1 }));
  assert.equal(collect(file).bytes, null);
  assert.ok(!existsSync(file));
});

test("torn header is deleted", () => {
  const file = mk();
  writeFileSync(file, header().slice(0, 10));
  assert.equal(collect(file).bytes, null);
  assert.ok(!existsSync(file));
});

test("epoch mismatch is ignored but the file is kept for overwrite", () => {
  const file = mk();
  writeFileSync(file, packLine({ fmt: PACK_FMT, epoch: "f".repeat(64) }) + packLine({ k: "a", v: 1 }));
  assert.equal(collect(file).bytes, null);
  assert.ok(existsSync(file));
});

test("missing file returns null without side effects", () => {
  const file = mk();
  assert.equal(collect(file).bytes, null);
  assert.ok(!existsSync(file));
});

test("integrity failures emit one stderr line each", () => {
  const file = mk();
  writePack(file, [{ k: "a", v: 1 }]);
  truncateSync(file, statSync(file).size - 2);
  const lines = [];
  const orig = process.stderr.write;
  process.stderr.write = (s) => { lines.push(String(s)); return true; };
  try { collect(file); } finally { process.stderr.write = orig; }
  assert.equal(lines.length, 1);
  assert.match(lines[0], /^oj: pack integrity: \{.*"action":"truncate".*\}\n$/);
});
