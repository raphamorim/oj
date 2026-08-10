// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Built-in file-based route manifest (virtual:oj-routes): oj globs src/routes/
// and derives route paths (index -> "/", $id -> :id, layouts excluded). The
// derivation runs in the browser, so a headless page imports the demo module
// and reads the computed paths. Unbundled only (the bundle-registry dev mode
// doesn't serve virtual ids — a documented limitation, like plugin virtuals).
const { chromium } = require("playwright");

(async () => {
  if (process.env.OJ_E2E_MODE === "bundle") {
    console.log("SKIP routes manifest (bundle registry does not serve virtual ids)");
    return;
  }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e.message || e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    const paths = await page.evaluate(async () => (await import("/src/route-demo.tsx")).paths);
    console.log("virtual:oj-routes paths:", paths, "| errors:", errors.length ? errors : "none");
    const expected = "/,/about,/boom,/crash,/deep,/users/:id";
    if (paths !== expected) throw new Error(`route manifest wrong:\n  got:      ${paths}\n  expected: ${expected}`);
    if (errors.length) throw new Error("console errors");
    console.log("FILE-BASED ROUTE MANIFEST VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
