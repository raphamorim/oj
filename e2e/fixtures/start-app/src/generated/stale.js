// A real on-disk module a plugin's load() must override at dev time. If oj
// reads this from disk instead of consulting the plugin (as it once did), the
// SSR render shows STALE_ON_DISK and the assertion fails.
export const LABEL = "STALE_ON_DISK";
