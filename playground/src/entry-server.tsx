import { renderToString } from "react-dom/server";
import { Counter } from "@/Counter";

export function render(): string {
  return renderToString(<Counter label="ssr" />);
}
