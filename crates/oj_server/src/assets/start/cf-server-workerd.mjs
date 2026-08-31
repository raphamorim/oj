// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

export async function getCloudflareContext() {
  const g = globalThis;
  return {
    env: g.__OJ_CF_ENV ?? {},
    cf: {},
    ctx: g.__OJ_CF_CTX ?? { waitUntil() {}, passThroughOnException() {} },
  };
}

export default { getCloudflareContext };
