// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Client runtime for server functions. A `*.server.{ts,tsx}` module is replaced
// on the client by stubs that call this: each call POSTs {module, name, args}
// to /__oj_fn, where the server executes the real function and returns JSON.
export async function __ojServerCall(module, name, args) {
  const res = await fetch("/__oj_fn", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ module, name, args }),
  });
  if (!res.ok) {
    throw new Error(`oj server function ${name} failed: ${res.status} ${await res.text()}`);
  }
  return res.json();
}
