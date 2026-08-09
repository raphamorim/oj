import { hydrateRoot } from "react-dom/client";
import { Counter } from "@/Counter";

hydrateRoot(document.getElementById("app")!, <Counter label="ssr" />);
