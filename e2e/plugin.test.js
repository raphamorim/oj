// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// npm plugin-hook compatibility: playground/oj.plugins.mjs default-exports a
// Vite/Rollup-style plugin whose `transform` hook replaces a marker in App.tsx.
// The dev server runs it (via the Node plugin host) before compiling, so the
// rendered page shows the transformed value.
const { chromium } = require("playwright");
const fs = require("node:fs");
const path = require("node:path");
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
    // this.resolve("@/Counter") went through oj's resolver (tsconfig alias) ->
    // the transform injected the resolved module's basename.
    const resolved = await page.getAttribute("[data-resolved]", "data-resolved");
    if (resolved !== "Counter.tsx") throw new Error("this.resolve wrong (expected Counter.tsx): " + resolved);
    // this.load + getModuleInfo on Counter: importedIds count : code has useState.
    const modinfo = await page.getAttribute("[data-modinfo]", "data-modinfo");
    if (modinfo !== "3:true") throw new Error("getModuleInfo/this.load wrong (expected 3:true): " + modinfo);
    // this.getModuleIds sees App.tsx (being transformed) + the loaded Counter.
    const moduleids = await page.getAttribute("[data-moduleids]", "data-moduleids");
    if (moduleids !== "2") throw new Error("getModuleIds wrong (expected 2): " + moduleids);
    // buildStart fired at dev-server startup, writing the command ("serve").
    const marker = path.join(__dirname, "..", "playground", ".oj-cache", "plugin-buildstart");
    const buildStart = fs.existsSync(marker) ? fs.readFileSync(marker, "utf8").trim() : "MISSING";
    if (buildStart !== "serve") throw new Error("buildStart marker wrong (expected serve): " + buildStart);
    // Environment API: this.environment.name is "client"; applyToEnvironment
    // gates the client plugin in and the ssr plugin out (marker untouched).
    const envName = await page.getAttribute("[data-env-name]", "data-env-name");
    if (envName !== "client") throw new Error("this.environment.name wrong (expected client): " + envName);
    const envClient = await page.getAttribute("[data-env-client]", "data-env-client");
    if (envClient !== "client-ran") throw new Error("applyToEnvironment client gate wrong: " + envClient);
    const envSsr = await page.getAttribute("[data-env-ssr]", "data-env-ssr");
    if (envSsr !== "__ENV_SSR__") throw new Error("applyToEnvironment ssr plugin should NOT run in client: " + envSsr);
    // Config define (global) + client-environment define, applied in dev too.
    const define = await page.getAttribute("[data-define]", "data-define");
    if (define !== "global-define|client-define") throw new Error("per-environment define wrong (expected global-define|client-define): " + define);
    // configureServer added a dev-server middleware owning /__oj_health.
    const health = await page.request.get("http://localhost:5199/__oj_health");
    const healthText = (await health.text()).trim();
    if (healthText !== "oj-plugin-mw-ok") throw new Error("configureServer middleware wrong: " + healthText);
    // moduleParsed fired for every parsed module (App.tsx among them).
    const parsed = (await (await page.request.get("http://localhost:5199/__oj_parsed")).text()).trim();
    if (!parsed.split(",").includes("App.tsx")) throw new Error("moduleParsed did not record App.tsx: " + parsed);
    if (errors.length) throw new Error("console errors");
    console.log("PLUGIN transform + config + transformIndexHtml + enforce/apply + buildStart + this.resolve + getModuleInfo + getModuleIds + configureServer + Environment API + per-env define HOOKS VERIFIED");
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error("FAIL:", e.message); process.exit(1); });
