import { rootRoute } from "./routes/__root";
import { indexRoute } from "./routes/index";
import { gettingStartedRoute } from "./routes/getting-started";
import { architectureRoute } from "./routes/architecture";
import { featuresRoute } from "./routes/features";

export const routeTree = rootRoute.addChildren([
  indexRoute,
  gettingStartedRoute,
  architectureRoute,
  featuresRoute,
]);
