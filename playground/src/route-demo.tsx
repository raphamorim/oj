// Exercises the built-in file-based route manifest (virtual:oj-routes). Kept
// out of the app graph (imported only by e2e/routes.test.js) so it doesn't
// affect the bundle-registry dev mode, which doesn't serve virtual ids.
import routes from "virtual:oj-routes";

export const paths = routes.map((r) => r.path).join(",");
