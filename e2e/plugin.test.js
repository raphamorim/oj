// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// npm plugin-hook compatibility: playground/oj.plugins.mjs default-exports a
// Vite/Rollup-style plugin whose `transform` hook replaces a marker in App.tsx.
// The dev server runs it (via the Node plugin host) before compiling, so the
// rendered page shows the transformed value.
const { chromium } = require("playwright");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("[data-plugin]", { state: "attached" });
    const v = await page.getAttribute("[data-plugin]", "data-plugin");
    console.log("data-plugin:", v, "| errors:", errors.length ? errors : "none");
    if (v !== "transformed-by-plugin") throw new Error("plugin transform did not run: " + v);
    if (errors.length) throw new Error("console errors");
    console.log("PLUGIN TRANSFORM HOOK VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
