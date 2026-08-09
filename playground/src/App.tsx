import { Counter } from "./Counter";

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
    </main>
  );
}
