// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
(async () => {
  if (process.env.OJ_E2E_MODE === "bundle") {
    // bundle workers are covered by e2e/worker-modes.mjs (all modes); this
    // shared-server playground test flakes on a mid-test full-reload navigation.
    console.log("SKIP worker (bundle covered by worker-modes.mjs)");
    return;
  }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    const result = await page.evaluate(async () => {
      const Worker = (await import("/src/worker-fixture.ts?worker")).default;
      const w = new Worker();
      return await new Promise((resolve) => {
        w.onmessage = (e) => resolve(e.data);
        w.postMessage(21);
      });
    });
    console.log("worker replied:", result, "| errors:", errors.length ? errors : "none");
    if (result !== 42) throw new Error("worker round-trip wrong: " + result);
    if (errors.length) throw new Error("console errors");
    console.log("WEB WORKER VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
