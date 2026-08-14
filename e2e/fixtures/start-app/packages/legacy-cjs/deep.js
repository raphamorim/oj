// A CJS subpath (no "exports" map), imported extensionless as "legacy-cjs/deep".
// Node's strict ESM resolver won't probe the .js extension; the Start SSR loader
// must recover it the way a bundler does. Regression for the lodash/isEqual class.
module.exports = function deep(x) {
  return "[deep:" + String(x) + "]";
};
