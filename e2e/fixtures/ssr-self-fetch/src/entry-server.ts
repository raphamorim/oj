// A dependency-free SSR entry for the generic `oj dev --ssr` runner.
//
// `/self` has a loader that fetches the app's OWN dev server (the `/about`
// loader) while it renders, the pattern a route that calls its own API takes.
// A runner that serializes requests deadlocks on it; a concurrent one (Vite's
// module runner, and oj's loopback runner) answers.
//
// `/echo` records the action body it received: the raw bytes must arrive
// intact (no text decode, no size cap) and the text form alongside them.

const port = () => process.env.OJ_E2E_SELF_PORT ?? "0";

let lastEcho: { text: string; bytes: number; first: number; last: number; sum: number } | null = null;

export async function load(url = "/") {
  if (url === "/about") return { page: "about", pid: process.pid };
  if (url === "/self") {
    const res = await fetch(`http://localhost:${port()}/about`, { headers: { "oj-loader": "1" } });
    return { page: "self", inner: await res.json() };
  }
  if (url === "/echo") return { page: "echo", echo: lastEcho };
  return { page: "home" };
}

export async function action(url = "/", body = "", bytes?: Uint8Array) {
  if (url !== "/echo") return;
  const raw = bytes ?? new TextEncoder().encode(body);
  let sum = 0;
  for (const b of raw) sum = (sum + b) % 65521;
  lastEcho = { text: body, bytes: raw.length, first: raw[0] ?? -1, last: raw[raw.length - 1] ?? -1, sum };
}

export function head(url = "/") {
  return `<title>${url}</title>`;
}

export async function render(url = "/", data: Record<string, unknown> = {}) {
  return `<main data-url="${url}">${JSON.stringify(data)}</main>`;
}
