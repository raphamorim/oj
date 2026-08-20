// A real on-disk module a plugin's load() must override at dev time. It uses
// the arbitrary-string export form (`export { local as "name" }`, ES2022) that
// compiled i18n barrels emit and that callers reach as `m.name()` — the exact
// shape of the reported failure. If oj reads this from disk (or mangles the
// string export through its transform/bundle), the render shows STALE_ON_DISK
// or throws "m.freshMsg_cta is not a function".
export const LABEL = "STALE_ON_DISK";
const staleFn = () => "STALE_ON_DISK";
export { staleFn as "freshMsg_cta" };
