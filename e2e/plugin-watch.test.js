// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// this.addWatchFile hook: the oj-watch plugin registers playground/plugin-watched.txt
// during App.tsx's transform. oj ignores a plain .txt change by default, but
// because a plugin watched it, the dev server forces a full reload — observable
// because a reload wipes a window marker.
const { chromium } = require("playwright");
const fs = require("fs");
const path = require("path");

const FILE = path.join(__dirname, "..", "playground", "plugin-watched.txt");

(async () => {
  const original = fs.readFileSync(FILE, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    // Load the page so App.tsx transforms and the plugin registers the watch.
    // domcontentloaded (not networkidle) — the HMR WebSocket keeps the
    // connection live, which makes networkidle flaky under the shared server.
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    await page.waitForSelector("main", { state: "attached", timeout: 15000 });
    await page.evaluate(() => (window.__watch_marker = 77));
    // Change the watched, non-source file: a full reload should drop the marker.
    fs.writeFileSync(FILE, "watched-v2\n");
    await page.waitForFunction(() => window.__watch_marker === undefined, { timeout: 15000 });
    console.log("addWatchFile forced a full reload on a plain .txt change (marker dropped)");
    console.log("PLUGIN addWatchFile HOOK VERIFIED");
  } finally {
    fs.writeFileSync(FILE, original);
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
