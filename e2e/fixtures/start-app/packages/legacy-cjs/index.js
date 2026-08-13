// A CommonJS package whose exports are assigned via `module.exports = {...}`.
// cjs-module-lexer cannot see these statically, so a naive `import { badge }`
// would throw -- this exercises the SSR loader's require()-backed CJS facade.
function badge(name) {
  return "[" + String(name).toUpperCase() + "]";
}
module.exports = { badge, LABEL: "legacy-cjs" };
