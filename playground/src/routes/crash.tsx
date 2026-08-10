// A render-time throw: caught by the router's ErrorBoundary.
export default function Crash() {
  throw new Error("crash: render threw");
}
