// A server function module. On the client it's replaced by RPC stubs; the real
// implementation runs only on the server (via the SSR module runner). The
// `process.versions.node` check proves it executed server-side, not in the
// browser. Imported only by e2e/ssr-dev.mjs (kept out of the app graph).
let calls = 0;

export async function greet(name: string): Promise<string> {
  calls += 1;
  const onServer = typeof process !== "undefined" && Boolean(process.versions?.node);
  return `hello, ${name} (server=${onServer}, call=${calls})`;
}
