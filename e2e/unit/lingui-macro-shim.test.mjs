// SPDX-License-Identifier: MIT
//
// The @lingui/*/macro runtime identity shim oj serves when it cannot run
// @lingui/swc-plugin. It must keep an app rendering (source strings, with
// interpolation) without a build-time transform. These tests pin every export's
// degraded-but-usable behavior.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  t,
  msg,
  defineMessage,
  plural,
  selectOrdinal,
  select,
  useLingui,
  Trans,
  Plural,
  Select,
  SelectOrdinal,
} from "../../crates/oj_server/src/assets/lingui-macro-shim.mjs";

test("t as a tagged template interpolates values in source order", () => {
  const name = "World";
  assert.equal(t`Hello ${name}`, "Hello World");
  const a = 1,
    b = 2;
  assert.equal(t`${a} and ${b}!`, "1 and 2!");
  assert.equal(t`no interpolation`, "no interpolation");
});

test("t(i18n)`...` (the bound-instance macro form) still interpolates", () => {
  const i18n = { _: (x) => x };
  const bound = t(i18n);
  assert.equal(typeof bound, "function");
  const who = "Ada";
  assert.equal(bound`Hi ${who}`, "Hi Ada");
});

test("t accepts a plain string or a message descriptor", () => {
  assert.equal(t("literal"), "literal");
  assert.equal(t({ id: "some.id", message: "A message" }), "A message");
  assert.equal(t({ id: "only.id" }), "only.id");
});

test("msg and defineMessage return the interpolated source string", () => {
  const x = "there";
  assert.equal(msg`hey ${x}`, "hey there");
  assert.equal(defineMessage`static`, "static");
  assert.equal(defineMessage, msg);
});

test("plural picks one/other and replaces #", () => {
  const forms = { one: "# item", other: "# items" };
  assert.equal(plural(1, forms), "1 item");
  assert.equal(plural(5, forms), "5 items");
  assert.equal(plural(0, forms), "0 items");
});

test("plural falls back to other/one when a category is missing", () => {
  assert.equal(plural(1, { other: "# things" }), "1 things");
  assert.equal(plural(3, { one: "# thing" }), "3 thing");
  assert.equal(plural(2, {}), "");
});

test("selectOrdinal behaves like plural", () => {
  assert.equal(selectOrdinal, plural);
  assert.equal(selectOrdinal(1, { one: "#st", other: "#th" }), "1st");
});

test("select chooses by exact value, else other", () => {
  const forms = { male: "he", female: "she", other: "they" };
  assert.equal(select("male", forms), "he");
  assert.equal(select("nonbinary", forms), "they");
  assert.equal(select("x", {}), "");
});

test("useLingui returns a working t and an identity i18n", () => {
  const { t: ht, i18n, _ } = useLingui();
  const who = "You";
  assert.equal(ht`Welcome ${who}`, "Welcome You");
  assert.equal(i18n.locale, "en");
  assert.equal(i18n._("passthrough"), "passthrough");
  assert.equal(i18n._({ message: "from descriptor" }), "from descriptor");
  assert.equal(typeof _, "function");
});

test("i18n._ applies values object to a descriptor", () => {
  const { i18n } = useLingui();
  // Values are applied positionally to a template-like descriptor; a plain
  // string descriptor with a values map still returns the string.
  assert.equal(i18n._("static", { 0: "ignored" }), "static");
});

test("Trans renders its children, or message/id when childless", () => {
  assert.deepEqual(Trans({ children: ["Hello ", "World"] }), ["Hello ", "World"]);
  assert.equal(Trans({ message: "msg text" }), "msg text");
  assert.equal(Trans({ id: "the.id" }), "the.id");
  assert.equal(Trans(null), null);
  assert.equal(Trans({}), null);
});

test("Plural / SelectOrdinal component form replaces # and picks category", () => {
  assert.equal(Plural({ value: 1, one: "# file", other: "# files" }), "1 file");
  assert.equal(Plural({ value: 9, one: "# file", other: "# files" }), "9 files");
  assert.equal(SelectOrdinal, Plural);
});

test("Select component form chooses by value", () => {
  assert.equal(Select({ value: "a", a: "Apple", other: "Other" }), "Apple");
  assert.equal(Select({ value: "z", a: "Apple", other: "Other" }), "Other");
});
