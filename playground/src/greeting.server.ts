// A server function module. On the client it's replaced by RPC stubs; the real
// implementation runs only on the server (via the SSR module runner). The
// `process.versions.node` check proves it executed server-side, not in the
// browser. Imported only by e2e/ssr-dev.mjs (kept out of the app graph).
export async function greet(name: string): Promise<string> {
  const onServer = typeof process !== "undefined" && Boolean(process.versions?.node);
  return `hello, ${name} (server=${onServer})`;
}
