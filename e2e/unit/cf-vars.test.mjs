// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { __test } from "../../crates/oj_server/src/assets/start/cf-server.mjs";

const { stripJsonc, parseWranglerJsonVars, parseWranglerTomlVars, parseDevVars } = __test;

test("stripJsonc removes // and /* */ but keeps // inside strings", () => {
  const src = `{
    // a line comment
    "url": "https://example.com/path", /* trailing */
    "n": 1
  }`;
  const cleaned = stripJsonc(src);
  assert.ok(!cleaned.includes("line comment"));
  assert.ok(!cleaned.includes("trailing"));
  assert.ok(cleaned.includes("https://example.com/path"));
});

test("parseWranglerJsonVars pulls the vars table (jsonc + trailing commas)", () => {
  const src = `{
    // wrangler config
    "name": "worker",
    "vars": {
      "API_URL": "https://api.example.com", // prod api
      "FLAG": "on",
    },
  }`;
  assert.deepEqual(parseWranglerJsonVars(src), { API_URL: "https://api.example.com", FLAG: "on" });
});

test("parseWranglerJsonVars returns {} when there are no vars or on bad json", () => {
  assert.deepEqual(parseWranglerJsonVars('{"name":"worker"}'), {});
  assert.deepEqual(parseWranglerJsonVars("not json"), {});
});

test("parseWranglerTomlVars reads only the [vars] section", () => {
  const src = [
    'name = "worker"',
    "",
    "[vars]",
    'API_URL = "https://api.example.com"',
    'FLAG = "on"',
    "",
    "[env.production]",
    'OTHER = "ignored"',
  ].join("\n");
  assert.deepEqual(parseWranglerTomlVars(src), { API_URL: "https://api.example.com", FLAG: "on" });
});

test("parseDevVars handles quotes, comments, blank lines, and = in values", () => {
  const src = [
    "# a comment",
    "",
    "PLAIN=value",
    'QUOTED="quoted value"',
    "SINGLE='single'",
    "TOKEN=abc=def==",
    "IGNORED_NO_EQ",
  ].join("\n");
  assert.deepEqual(parseDevVars(src), {
    PLAIN: "value",
    QUOTED: "quoted value",
    SINGLE: "single",
    TOKEN: "abc=def==",
  });
});
