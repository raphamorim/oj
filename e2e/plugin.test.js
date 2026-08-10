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
    // config() contributed a define; configResolved() captured mode; transform
    // injected "<mode>:<define>".
    const cfg = await page.getAttribute("[data-plugin-config]", "data-plugin-config");
    console.log("data-plugin:", v, "| data-plugin-config:", cfg, "| errors:", errors.length ? errors : "none");
    if (v !== "transformed-by-plugin") throw new Error("plugin transform did not run: " + v);
    if (cfg !== "development:oj-plugin") throw new Error("config/configResolved handshake failed: " + cfg);
    // transformIndexHtml injected a meta tag and rewrote the title.
    const injected = await page.locator('meta[name="oj-plugin-injected"]').getAttribute("content");
    const title = await page.title();
    if (injected !== "yes") throw new Error("transformIndexHtml did not inject the meta tag");
    if (!title.includes("(plugin)")) throw new Error("transformIndexHtml did not rewrite the title: " + title);
    // enforce: two plugins listed post-then-pre append in pre->post order.
    const order = await page.getAttribute("[data-order]", "data-order");
    if (order !== "base-pre-post") throw new Error("enforce ordering wrong: " + order);
    // apply: only the serve-gated plugin runs in dev.
    const apply = await page.getAttribute("[data-apply]", "data-apply");
    if (apply !== "serve-only") throw new Error("apply gating wrong (expected serve-only): " + apply);
    if (errors.length) throw new Error("console errors");
    console.log("PLUGIN transform + config + transformIndexHtml + enforce/apply HOOKS VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
