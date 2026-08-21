// SPDX-License-Identifier: MIT
//
// Runtime shim for @lingui/{core,react}/macro and @lingui/macro.
//
// The real macros are compiled away at build time by @lingui/swc-plugin, which
// runs inside @vitejs/plugin-react-swc. oj reimplements plugin-react on oxc and
// does not execute SWC WASM plugins, so those macros are never transformed. Left
// as runtime imports they pull the entire babel macro toolchain into the browser
// and the app never mounts.
//
// This shim keeps the app running by degrading i18n to identity: messages render
// in their source language (typically English) with interpolation applied, but
// no catalog lookup happens. It is NOT a substitute for the real transform; it
// is the dev-time fallback that lets an app that hard-depends on the lingui macro
// still load under oj.

function __oj_interp(strings, values) {
  if (strings && strings.raw && typeof strings.length === "number") {
    let out = strings[0] ?? "";
    for (let i = 0; i < values.length; i++) out += String(values[i]) + (strings[i + 1] ?? "");
    return out;
  }
  if (typeof strings === "string") return strings;
  if (strings && typeof strings === "object") return strings.message ?? strings.id ?? "";
  return "";
}

// `t` works as a tagged template (t`Hello ${x}`) and as t(i18n)`...` (the macro
// form that binds an i18n instance); both degrade to the interpolated string.
export function t(strings, ...values) {
  if (strings && typeof strings._ === "function" && !strings.raw) {
    return (s, ...v) => __oj_interp(s, v);
  }
  return __oj_interp(strings, values);
}

export function msg(strings, ...values) {
  return __oj_interp(strings, values);
}
export const defineMessage = msg;

export function plural(value, forms) {
  forms = forms || {};
  const cat = value === 1 ? "one" : "other";
  const form = forms[cat] ?? forms.other ?? forms.one ?? "";
  return String(form).replace(/#/g, String(value));
}
export const selectOrdinal = plural;

export function select(value, forms) {
  forms = forms || {};
  return String(forms[value] ?? forms.other ?? "");
}

const __oj_i18n = {
  _: (descriptor, values) => __oj_interp(descriptor, values ? Object.values(values) : []),
  locale: "en",
  t,
};

export function useLingui() {
  return { i18n: __oj_i18n, t, _: __oj_i18n._ };
}

// Component macros. Returning children/strings from a function component is
// valid React, so these render without importing React here.
export function Trans(props) {
  if (!props) return null;
  if (props.children !== undefined) return props.children;
  return props.message ?? props.id ?? null;
}

export function Plural(props) {
  props = props || {};
  const cat = props.value === 1 ? "one" : "other";
  const form = props[cat] ?? props.other ?? "";
  return String(form).replace(/#/g, String(props.value));
}
export const SelectOrdinal = Plural;

export function Select(props) {
  props = props || {};
  return String(props[props.value] ?? props.other ?? "");
}
