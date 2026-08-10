import { Counter } from "@/Counter";
import { HmrDemo } from "@/hmr-demo";
import meta, { appName } from "./data.json";
import "./theme.scss";

const pages = import.meta.glob("./pages/*.ts", { eager: true });

export function App() {
  return (
    <main>
      <h1 className="underline">oj playground</h1>
      <p>
        Click the counter a few times, then edit this paragraph in{" "}
        <code>src/App.tsx</code> and save — the text updates while the count
        survives. That is Fast Refresh doing its job.
      </p>
      <Counter label="Clicks" />
      <div
        data-env={`${import.meta.env.VITE_GREETING}|${import.meta.env.MODE}|${
          import.meta.env.SECRET_KEY ?? "none"
        }`}
        hidden
      />
      <div data-pages={Object.keys(pages).length} hidden />
      <div data-json={`${appName}|${meta.version}`} hidden />
      {/* Replaced by a Vite-style transform plugin (oj.plugins.mjs). */}
      <div data-plugin="__OJ_PLUGIN_UNTRANSFORMED__" hidden />
      {/* Filled by a plugin from the config()/configResolved() handshake. */}
      <div data-plugin-config="__OJ_CONFIG_MARKER__" hidden />
      {/* Appended to by enforce-ordered plugins: expect base-pre-post. */}
      <div data-order="base" hidden />
      {/* Filled by whichever apply-gated plugin is active (serve vs build). */}
      <div data-apply="__APPLY__" hidden />
      <HmrDemo />
    </main>
  );
}
