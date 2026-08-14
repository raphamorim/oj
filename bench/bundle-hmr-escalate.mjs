// Bundle-mode HMR boundary escalation: editing a NON-component export in a
// mixed module (component + constant) must NOT full-reload. The boundary
// rejects its own update at runtime; oj should escalate to the importer and
// re-execute it in place (state/sentinel survive), not reload the page.
import { chromium } from "playwright";
import { spawn, execSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, rmSync, symlinkSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const OJ = path.join(repo, "target", "release", "oj");
const app = path.join(here, "apps", "bundle-escalate");
const donor = path.join(here, "apps", "app-1000");
const PORT = 5199;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

rmSync(app, { recursive: true, force: true });
mkdirSync(path.join(app, "src"), { recursive: true });
// mixed.tsx: a component boundary that ALSO exports a non-component constant.
const mixed = (label) => `import { useState } from "react";
export const LABEL = "${label}";
export function Panel() {
  const [n] = useState(1);
  return <div data-panel>{LABEL}:{n}</div>;
}
`;
writeFileSync(path.join(app, "src", "mixed.tsx"), mixed("v1"));
writeFileSync(
  path.join(app, "src", "App.tsx"),
  `import { Panel } from "./mixed";
export function App() {
  return <main data-done="yes"><h1>escalate</h1><Panel /></main>;
}
`,
);
writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";
import { App } from "./App";
createRoot(document.getElementById("root")!).render(<App />);
`,
);
writeFileSync(path.join(app, "index.html"), `<!doctype html><html><head><meta charset="utf-8"/></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>\n`);
writeFileSync(path.join(app, "package.json"), `{ "name": "bundle-escalate", "private": true, "type": "module" }\n`);
rmSync(path.join(app, "node_modules"), { recursive: true, force: true });
symlinkSync(path.join(donor, "node_modules"), path.join(app, "node_modules"), "dir");
rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });

try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}
const proc = spawn(OJ, ["dev", app, "--port", String(PORT), "--bundle"], { stdio: "ignore" });
let pass = true;
const check = (c, m) => { console.log(`  ${c ? "OK " : "FAIL"} ${m}`); if (!c) pass = false; };
try {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  let reloads = 0;
  page.on("load", () => reloads++); // counts the initial load + any full reload
  for (let i = 0; i < 200; i++) { try { await page.goto(`http://localhost:${PORT}/`, { timeout: 2000 }); break; } catch { await sleep(30); } }
  await page.waitForSelector("[data-panel]", { timeout: 30000 });
  check((await page.textContent("[data-panel]"))?.startsWith("v1"), "initial render shows v1");
  // Sentinel that only survives if the page is NOT fully reloaded.
  await page.evaluate(() => { window.__sentinel = "alive"; });
  const loadsBefore = reloads;
  // Edit the NON-component export.
  writeFileSync(path.join(app, "src", "mixed.tsx"), mixed("v2"));
  await page.waitForFunction(() => document.querySelector("[data-panel]")?.textContent?.startsWith("v2"), null, { timeout: 30000, polling: 16 });
  check(true, "panel updated to v2 after non-component edit");
  const sentinel = await page.evaluate(() => window.__sentinel);
  check(sentinel === "alive", `no full reload — sentinel survived (${JSON.stringify(sentinel)})`);
  check(reloads === loadsBefore, `no navigation/reload fired (loads: ${loadsBefore} -> ${reloads})`);
  await browser.close();
} finally {
  proc.kill("SIGKILL");
  try { execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" }); } catch {}
  rmSync(app, { recursive: true, force: true });
}
console.log(pass ? "\nBUNDLE-HMR ESCALATION PASSED" : "\nBUNDLE-HMR ESCALATION FAILED");
process.exit(pass ? 0 : 1);
