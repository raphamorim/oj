import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const here = path.dirname(fileURLToPath(import.meta.url));
const browser = await chromium.launch();
const page = await browser.newPage({
  viewport: { width: 1600, height: 900 },
  deviceScaleFactor: 2,
});
await page.goto("file://" + path.join(here, "card.html"));
const out = path.join(here, "oj-benchmarks.png");
await page.screenshot({ path: out, clip: { x: 0, y: 0, width: 1600, height: 900 } });
await browser.close();
console.log(`wrote ${out}`);
