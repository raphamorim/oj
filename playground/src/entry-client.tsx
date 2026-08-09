import { hydrateRoot } from "react-dom/client";
import { App } from "@/routes";

hydrateRoot(document.getElementById("app")!, <App url={location.pathname} />);
