// SPDX-License-Identifier: MIT

const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const root = path.resolve(__dirname, "..");
const binary = process.env.OJ_BIN ?? path.join(root, "target", "debug", "oj");

const fixtures = [
  {
    name: "single-quoted attributes",
    stylesheet: "<link rel='stylesheet' href='/site.css'>",
    entry: "<script type='module' src='/main.js'></script>",
  },
  {
    name: "whitespace around attribute values",
    stylesheet: '<link rel = "stylesheet" href = "/site.css">',
    entry: '<script type = "module" src = "/main.js"></script>',
  },
  {
    name: "unquoted attributes",
    stylesheet: "<link rel=stylesheet href=/site.css>",
    entry: "<script type=module src=/main.js></script>",
  },
  {
    name: "attribute-name boundaries",
    stylesheet: "<link rel='stylesheet' data-href='/ignored.css' href='/site.css'>",
    entry: "<script data-type='module' data-src='/ignored.js' type='module' src='/main.js'></script>",
  },
];

for (const fixture of fixtures) {
  const project = fs.mkdtempSync(path.join(os.tmpdir(), "oj-html-attributes-"));
  try {
    fs.writeFileSync(path.join(project, "package.json"), JSON.stringify({ type: "module" }));
    fs.writeFileSync(path.join(project, "index.html"), `<html><head>${fixture.stylesheet}</head><body>${fixture.entry}</body></html>`);
    fs.writeFileSync(path.join(project, "main.js"), 'document.body.textContent = "ready";');
    fs.writeFileSync(path.join(project, "site.css"), "body { color: red; }");

    const build = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
    assert.equal(build.status, 0, `${fixture.name} must build successfully:\n${build.stdout}\n${build.stderr}`);

    const html = fs.readFileSync(path.join(project, "dist", "index.html"), "utf8");
    assert.match(html, /\/assets\/main-[^"'\s>]+\.js/, `${fixture.name} must rewrite the module entry:\n${html}`);
    // The stylesheet goes through the CSS pipeline (hashed, minified) and the
    // link is rewritten to it, whatever the attribute quoting.
    const cssRef = html.match(/\/assets\/site-[^"'\s>]+\.css/);
    assert.ok(cssRef, `${fixture.name} must rewrite the stylesheet link:\n${html}`);
    const css = fs.readFileSync(path.join(project, "dist", cssRef[0].replace(/^\//, "")), "utf8");
    assert.match(css, /color:\s*red/, `${fixture.name} must emit the stylesheet`);
    assert.ok(!fs.existsSync(path.join(project, "dist", "site.css")), `${fixture.name}: stylesheet is not copied verbatim`);
  } finally {
    fs.rmSync(project, { recursive: true, force: true });
  }
}

console.log("HTML-ATTRIBUTE-QUOTES E2E PASSED");
