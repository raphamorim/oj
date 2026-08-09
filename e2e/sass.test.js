// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Sass import: ./theme.scss uses a variable + nesting to color the h1. Verify
// the compiled rule applies (proves scss -> css -> injected stylesheet).
const { chromium } = require("playwright");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("h1");
    const color = await page.$eval("h1", (el) => getComputedStyle(el).color);
    console.log("h1 color:", color, "| errors:", errors.length ? errors : "none");
    if (color !== "rgb(40, 90, 160)") throw new Error("scss not applied: " + color);
    if (errors.length) throw new Error("console errors");
    console.log("SASS VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
