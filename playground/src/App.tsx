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
    </main>
  );
}
