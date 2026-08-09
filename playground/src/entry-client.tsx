import { hydrateRoot } from "react-dom/client";
import { SsrApp } from "@/ssr-app";

hydrateRoot(document.getElementById("app")!, <SsrApp />);
