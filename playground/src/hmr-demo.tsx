// A self-accepting component: editing it is a Fast Refresh update (no reload)
// by default. The handleHotUpdate plugin in oj.plugins.mjs forces a full reload
// for this file instead, which is how the override is observable.
export function HmrDemo() {
  return <span data-hmr-demo="v1" hidden />;
}
