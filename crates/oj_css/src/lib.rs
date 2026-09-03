// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use lightningcss::css_modules;
use lightningcss::dependencies::{Dependency, DependencyOptions};
use lightningcss::error::{Error as CssError, ParserError};
use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet};
use lightningcss::targets::{Browsers, Targets};

/// Recovered parse errors, printed the way a PostCSS plugin warning would be:
/// the stylesheet still compiles, the developer still learns what was dropped.
fn report_css_warnings(name: &str, warnings: &RwLock<Vec<CssError<ParserError<'_>>>>) {
    let Ok(list) = warnings.read() else {
        return;
    };
    const SHOWN: usize = 5;
    for w in list.iter().take(SHOWN) {
        eprintln!("oj: css warning in {name}: {w}");
    }
    if list.len() > SHOWN {
        eprintln!("oj: css warning in {name}: {} more", list.len() - SHOWN);
    }
}

#[derive(Debug)]
pub struct CssOutput {
    pub css: String,
    pub exports: Option<Vec<(String, String)>>,
}

pub fn is_css_module(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .is_some_and(|f| f.contains(".module."))
}

pub fn is_sass(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    f.ends_with(".scss") || f.ends_with(".sass")
}

fn with_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

// A dotted-basename stylesheet (`x.module.scss`) is presented to grass as a
// directory `x.module/` whose index is that file (see index_target). A relative
// import made from inside such a file then carries that phantom directory
// segment (grass resolves `@use 'sibling'` against `.../x.module/`). Collapse
// any intermediate segment `seg` for which `<...>/seg.scss` (or `.sass`) is a
// real file, since `seg` is really that file, not a directory. Returns a new
// path only when something was collapsed.
fn collapse_phantom_dirs(p: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = p.components().collect();
    let mut out = PathBuf::new();
    let mut changed = false;
    for (i, comp) in comps.iter().enumerate() {
        let candidate = out.join(comp);
        let is_last = i + 1 == comps.len();
        if !is_last
            && (with_ext(&candidate, "scss").is_file() || with_ext(&candidate, "sass").is_file())
        {
            // `candidate` names a dotted stylesheet file, so it is a phantom
            // directory: drop it and resolve siblings against its real parent.
            changed = true;
            continue;
        }
        out = candidate;
    }
    changed.then_some(out)
}

// If `p.scss`/`p.sass` exists as a real file, return it. Used to treat a
// dotted-basename stylesheet as resolvable.
fn dotted_stylesheet(p: &Path) -> Option<PathBuf> {
    for ext in ["scss", "sass"] {
        let c = with_ext(p, ext);
        if c.is_file() {
            return Some(c);
        }
    }
    if let Some(collapsed) = collapse_phantom_dirs(p) {
        for ext in ["scss", "sass"] {
            let c = with_ext(&collapsed, ext);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

// Map a grass directory-index probe `.../X/index.scss` (or _index/.sass) back
// to a real `.../X.scss` file when `X` is a dotted-basename stylesheet. The
// mapping is extension-faithful: an `index.sass` probe only matches a real
// `.sass` file, so scss content is never parsed as the indented syntax.
fn index_target(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?.to_str()?;
    let ext = match name {
        "index.scss" | "_index.scss" => "scss",
        "index.sass" | "_index.sass" => "sass",
        _ => return None,
    };
    let parent = p.parent()?;
    let c = with_ext(parent, ext);
    if c.is_file() {
        return Some(c);
    }
    // The dotted-basename file may sit behind a phantom directory carried in
    // from a relative import (see collapse_phantom_dirs).
    let collapsed = collapse_phantom_dirs(parent)?;
    let c = with_ext(&collapsed, ext);
    c.is_file().then_some(c)
}

// grass 0.13 treats a trailing `.segment` in an `@use`/`@import` path as an
// extension, so `@use "../css/variables.module"` never probes
// `variables.module.scss` and fails on the CSS-modules `.module.scss`
// convention. grass does probe the basename as a directory, so present a real
// `name.segment.scss` file as a directory whose index is that file. This makes
// resolution match dart-sass (what Vite uses), which treats the whole basename
// as the stylesheet name.
#[derive(Debug)]
struct DottedFs;

// grass resolves a directory import `@use "pkg"` by probing `pkg/index.scss` and
// `pkg/_index.scss` (or `.sass`). When `pkg` is an npm package reached through
// a node_modules load path, its package.json names the stylesheet instead:
// Vite's sass importer resolves with mainFields `["sass", "style"]` (then
// `main`). Map the probe to that file. Syntax follows the probed extension
// (grass parses by it), so an `index.sass` probe only accepts a `.sass` entry
// and an `index.scss` probe accepts `.scss` or plain `.css`.
fn package_entry(probe: &Path) -> Option<PathBuf> {
    let name = probe.file_name()?.to_str()?;
    let want_sass = match name {
        "index.scss" | "_index.scss" => false,
        "index.sass" | "_index.sass" => true,
        _ => return None,
    };
    let dir = probe.parent()?;
    let pkg = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(&pkg).ok()?;
    let accepts = |p: &Path| {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        p.is_file() && if want_sass { ext == "sass" } else { ext == "scss" || ext == "css" }
    };
    for field in ["sass", "style", "main"] {
        let Some(rel) = pkg.get(field).and_then(|v| v.as_str()) else {
            continue;
        };
        let p = dir.join(rel);
        if accepts(&p) {
            return Some(p);
        }
        let exts: &[&str] = if want_sass { &["sass"] } else { &["scss", "css"] };
        for ext in exts {
            let c = with_ext(&p, ext);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

// The real file behind a grass probe that is not itself a file: a dotted-basename
// stylesheet's phantom index, or an npm package's `sass`/`style`/`main` entry.
fn probe_target(p: &Path) -> Option<PathBuf> {
    index_target(p).or_else(|| package_entry(p))
}

impl grass::Fs for DottedFs {
    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir() || dotted_stylesheet(p).is_some()
    }
    fn is_file(&self, p: &Path) -> bool {
        p.is_file() || probe_target(p).is_some()
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        let bytes = if p.is_file() {
            std::fs::read(p)?
        } else {
            match probe_target(p) {
                Some(real) => std::fs::read(real)?,
                None => std::fs::read(p)?,
            }
        };
        // Nested `@use`/`@forward`/`@import` in read files need the same
        // extension stripping the entry gets, so a dotted specifier like
        // `@use 'colors.module.scss'` inside an imported stylesheet resolves
        // through DottedFs instead of grass probing the raw dotted+ext path.
        match String::from_utf8(bytes) {
            Ok(text) => Ok(strip_sass_import_ext(&text).into_bytes()),
            Err(e) => Ok(e.into_bytes()),
        }
    }
}

// grass also mishandles an explicit `.scss`/`.sass` extension on a dotted
// basename (`@use "../css/vars.module.scss"`), probing only CWD-relative and
// never through the load paths. Drop the extension on `@use`/`@forward`/
// `@import` specifiers so every import takes the bare-name path (which DottedFs
// resolves); Sass treats `@use "x"` and `@use "x.scss"` identically. `.css` is
// left alone (it stays a plain CSS import), as are `url(...)` lines.
fn strip_sass_import_ext(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let t = line.trim_start();
        let is_import = t.starts_with("@use") || t.starts_with("@forward") || t.starts_with("@import");
        if is_import && !line.contains("url(") {
            // `~pkg/...` is the webpack-era spelling of a node_modules import that
            // Vite's sass importer still accepts; the load paths cover it bare.
            out.push_str(
                &line
                    .replace(".scss\"", "\"")
                    .replace(".scss'", "'")
                    .replace(".sass\"", "\"")
                    .replace(".sass'", "'")
                    .replace("\"~", "\"")
                    .replace("'~", "'"),
            );
        } else {
            out.push_str(line);
        }
    }
    out
}

pub fn compile_sass(source: &str, load_dir: Option<&Path>) -> Result<String, String> {
    compile_sass_with(source, load_dir, None)
}

pub fn compile_sass_with(
    source: &str,
    load_dir: Option<&Path>,
    additional_data: Option<&str>,
) -> Result<String, String> {
    compile_sass_opts(
        source,
        &SassOptions {
            load_dir,
            additional_data,
            load_paths: &[],
        },
    )
}

/// How a Sass stylesheet is compiled: the importing file's directory, the
/// configured `css.preprocessorOptions.scss.loadPaths`/`includePaths`, and the
/// `node_modules` directories above it (so `@use "bootstrap/scss/bootstrap"` and
/// `@use "pkg"` resolve as in Vite), plus `additionalData` prepended.
#[derive(Debug, Default, Clone, Copy)]
pub struct SassOptions<'a> {
    pub load_dir: Option<&'a Path>,
    pub additional_data: Option<&'a str>,
    pub load_paths: &'a [PathBuf],
}

/// Every `node_modules` directory from `dir` up to the filesystem root, nearest
/// first: Sass' node resolution for bare `@use`/`@import` specifiers.
pub fn node_modules_load_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.file_name().is_some_and(|n| n == "node_modules") {
            cur = d.parent();
            continue;
        }
        let nm = d.join("node_modules");
        if nm.is_dir() {
            out.push(nm);
        }
        cur = d.parent();
    }
    out
}

pub fn compile_sass_opts(source: &str, opts: &SassOptions<'_>) -> Result<String, String> {
    let fs = DottedFs;
    let mut options = grass::Options::default().fs(&fs);
    if let Some(dir) = opts.load_dir {
        options = options.load_path(dir);
    }
    for p in opts.load_paths {
        options = options.load_path(p);
    }
    if let Some(dir) = opts.load_dir {
        for nm in node_modules_load_paths(dir) {
            options = options.load_path(nm);
        }
    }
    let additional_data = opts.additional_data;
    let stripped = strip_sass_import_ext(source);
    let source = match additional_data {
        Some(data) if !data.is_empty() => format!("{data}\n{stripped}"),
        _ => stripped,
    };
    grass::from_string(source, &options).map_err(|e| format!("sass error: {e}"))
}

fn default_targets() -> Targets {
    Targets::from(Browsers {
        chrome: Some(100 << 16),
        edge: Some(100 << 16),
        firefox: Some(100 << 16),
        safari: Some(14 << 16),
        ios_saf: Some(14 << 16),
        ..Browsers::default()
    })
}

pub fn compile_css(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    compile_css_impl(url, source, minify, false, false)
}

pub fn compile_css_rebased(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    compile_css_impl(url, source, minify, true, false)
}

/// `compile_css_rebased` plus an inline source map (`css.devSourcemap`): the
/// served CSS ends with a `sourceMappingURL` data URL mapping back to `url`,
/// with the (preprocessed) source embedded, so devtools show the stylesheet's
/// rules at their source lines.
pub fn compile_css_rebased_with_map(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    compile_css_impl(url, source, minify, true, true)
}

fn compile_css_impl(
    url: &str,
    source: &str,
    minify: bool,
    rebase: bool,
    source_map: bool,
) -> Result<CssOutput, String> {
    let is_module = is_css_module(url);
    let warnings = Arc::new(RwLock::new(Vec::new()));
    let options = ParserOptions {
        filename: url.to_string(),
        css_modules: is_module.then(|| css_modules::Config {
            pattern: css_modules::Pattern::parse("[name]_[local]_[hash]").expect("static pattern"),
            ..css_modules::Config::default()
        }),
        // Vite's default pipeline (postcss) never rejects a stylesheet over a
        // legacy hack (`*zoom: 1`, `_height`, IE `filter: progid:...`) or a
        // stray invalid rule; lightningcss does unless it may recover, in which
        // case the offending declaration/rule is dropped and reported as a
        // warning instead of failing the whole file.
        error_recovery: true,
        warnings: Some(Arc::clone(&warnings)),
        ..ParserOptions::default()
    };

    let mut stylesheet = StyleSheet::parse(source, options)
        .map_err(|err| format!("css parse error in {url}: {err}"))?;
    report_css_warnings(url, &warnings);

    let targets = default_targets();
    stylesheet
        .minify(MinifyOptions {
            targets: targets.clone(),
            ..MinifyOptions::default()
        })
        .map_err(|err| format!("css transform error in {url}: {err}"))?;

    let base = if rebase { css_base_dir(url) } else { None };
    let mut sm = parcel_sourcemap::SourceMap::new("");
    if source_map {
        let idx = sm.add_source(url);
        let _ = sm.set_source_content(idx as usize, source);
    }
    let result = stylesheet
        .to_css(PrinterOptions {
            minify,
            targets,
            analyze_dependencies: base.as_ref().map(|_| DependencyOptions::default()),
            source_map: source_map.then_some(&mut sm),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {url}: {err}"))?;

    let mut css = result.code;
    if source_map {
        // Sources are stored root-relative (`src/app.css`); `sourceRoot: "/"`
        // makes devtools resolve them to the served url (`/src/app.css`).
        let json = sm
            .to_json(Some("/"))
            .map_err(|err| format!("css sourcemap error in {url}: {err}"))?;
        css.push_str("\n/*# sourceMappingURL=data:application/json;base64,");
        css.push_str(&base64(json.as_bytes()));
        css.push_str(" */\n");
    }
    if let (Some(base), Some(deps)) = (base, result.dependencies) {
        for dep in deps {
            let (placeholder, orig) = match dep {
                Dependency::Url(u) => (u.placeholder, u.url),
                Dependency::Import(i) => (i.placeholder, i.url),
            };
            let replacement = rebase_relative(&orig, &base).unwrap_or(orig);
            css = css.replace(&placeholder, &replacement);
        }
    }

    let exports = result.exports.map(|map| {
        let mut pairs: Vec<(String, String)> = map
            .into_iter()
            .map(|(name, export)| (name, export.name))
            .collect();
        pairs.sort();
        pairs
    });

    Ok(CssOutput { css, exports })
}

/// Inline plain `@import` rules the way Vite's postcss-import step does, so a
/// stylesheet served in dev or concatenated into a build chunk carries its
/// imported rules instead of an `@import` the browser would resolve against the
/// wrong URL (or a bare specifier it cannot resolve at all). Relative imports
/// resolve against the importing file; bare ones (`normalize.css`, `~pkg/x`)
/// through the `node_modules` directories above it via the package's `style`
/// or `main` entry. Imports with a media query, `layer(...)` or `supports(...)`
/// and external urls are left as written. Every inlined file's relative `url()`
/// and remaining `@import` paths are rebased to `file`'s directory.
pub fn inline_imports(source: &str, file: &Path) -> Result<String, String> {
    let mut stack = vec![file.to_path_buf()];
    let out = inline_imports_depth(source, file, &mut stack)?;
    Ok(if out.contains("@import") { hoist_imports(&out) } else { out })
}

/// Move every statement-level `@import ...;` that survived inlining (external
/// urls, media-query imports, unresolvable specifiers) to the top, after a
/// leading `@charset`, since CSS requires imports to precede all other rules and
/// an inlined file's rules may now sit above them.
fn hoist_imports(css: &str) -> String {
    let mut imports: Vec<&str> = Vec::new();
    let mut body = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(pos) = rest.find("@import") {
        let (before, at) = rest.split_at(pos);
        let statement_start = before
            .trim_end()
            .chars()
            .next_back()
            .is_none_or(|c| matches!(c, ';' | '}' | '{'));
        let end = if statement_start { at.find(';') } else { None };
        let Some(end) = end else {
            body.push_str(before);
            body.push_str("@import");
            rest = &at[7..];
            continue;
        };
        body.push_str(before);
        imports.push(&at[..=end]);
        rest = &at[end + 1..];
    }
    body.push_str(rest);
    if imports.is_empty() {
        return body;
    }
    let mut out = String::with_capacity(css.len());
    let mut body_rest = body.as_str();
    if let Some(after) = body_rest.trim_start().strip_prefix("@charset") {
        if let Some(end) = after.find(';') {
            let stmt_end = body_rest.len() - after.len() + end + 1;
            out.push_str(&body_rest[..stmt_end]);
            out.push('\n');
            body_rest = &body_rest[stmt_end..];
        }
    }
    for imp in imports {
        out.push_str(imp.trim_start());
        out.push('\n');
    }
    out.push_str(body_rest);
    out
}

fn inline_imports_depth(source: &str, file: &Path, stack: &mut Vec<PathBuf>) -> Result<String, String> {
    if !source.contains("@import") || stack.len() > 32 {
        return Ok(source.to_string());
    }
    let dir = file.parent().unwrap_or(Path::new("."));
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(pos) = rest.find("@import") {
        let (before, at) = rest.split_at(pos);
        // Only a statement-level `@import` (preceded by nothing, `;`, `}` or `{`
        // modulo whitespace) counts; anything else is inside a string or comment.
        let statement_start = before
            .trim_end()
            .chars()
            .next_back()
            .is_none_or(|c| matches!(c, ';' | '}' | '{' | '/'));
        let parsed = if statement_start { parse_plain_import(&at[7..]) } else { None };
        let Some((spec, consumed, media)) = parsed else {
            out.push_str(before);
            out.push_str("@import");
            rest = &at[7..];
            continue;
        };
        let stmt_len = 7 + consumed;
        let resolved = resolve_css_import(&spec, dir);
        let Some(target) = resolved.filter(|t| !stack.iter().any(|s| s == t)) else {
            out.push_str(before);
            out.push_str(&at[..stmt_len]);
            rest = &at[stmt_len..];
            continue;
        };
        let child = std::fs::read_to_string(&target)
            .map_err(|e| format!("cannot read @import {spec} ({}): {e}", target.display()))?;
        stack.push(target.clone());
        let child = inline_imports_depth(&child, &target, stack)?;
        stack.pop();
        let child_dir = target.parent().unwrap_or(Path::new("."));
        let rebased = rebase_to_dir(&child, &target, child_dir, dir)?;
        out.push_str(before);
        match media {
            // `@import "x" print;` becomes `@media print { ... }` (postcss-import).
            Some(media) => {
                out.push_str("@media ");
                out.push_str(&media);
                out.push_str(" {\n");
                out.push_str(&rebased);
                out.push_str("\n}\n");
            }
            None => {
                out.push_str(&rebased);
                out.push('\n');
            }
        }
        rest = &at[stmt_len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `"x"`, `'x'`, `url(x)`, `url("x")`, optionally followed by a media query,
/// then `;`. Returns the specifier, how many bytes of `after` the statement
/// (through `;`) spans, and the media query if any. `layer(...)`/`supports(...)`
/// imports are not inlined (None).
fn parse_plain_import(after: &str) -> Option<(String, usize, Option<String>)> {
    let trimmed = after.trim_start();
    let ws = after.len() - trimmed.len();
    let (spec, used) = if let Some(inner) = trimmed.strip_prefix("url(") {
        let close = inner.find(')')?;
        let raw = inner[..close].trim().trim_matches(|c| c == '"' || c == '\'');
        (raw.to_string(), 4 + close + 1)
    } else {
        let quote = trimmed.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let close = trimmed[1..].find(quote)?;
        (trimmed[1..1 + close].to_string(), 1 + close + 1)
    };
    let tail = &trimmed[used..];
    let semi = tail.find(';')?;
    if tail[..semi].contains(['{', '}']) {
        return None;
    }
    let cond = tail[..semi].trim();
    // `layer(...)` / `supports(...)` imports keep their rule for the browser.
    if cond.contains("layer(") || cond.contains("supports(") || cond.starts_with("layer") {
        return None;
    }
    if spec.is_empty()
        || spec.starts_with("data:")
        || spec.starts_with("//")
        || spec.contains("://")
        || spec.starts_with('/')
    {
        return None;
    }
    let media = (!cond.is_empty()).then(|| cond.to_string());
    Some((spec, ws + used + semi + 1, media))
}

fn css_file_candidates(base: &Path) -> Vec<PathBuf> {
    let mut v = vec![base.to_path_buf()];
    if base.extension().is_none() {
        v.push(with_ext(base, "css"));
    }
    v.push(base.join("index.css"));
    v
}

/// postcss-import's resolution order: relative to the importer, then a
/// node_modules package (`style`/`main` entry for a bare package, a file inside
/// it otherwise), `~` prefix accepted.
pub fn resolve_css_import(spec: &str, dir: &Path) -> Option<PathBuf> {
    let spec = spec.strip_prefix('~').unwrap_or(spec);
    let spec = spec.split(['?', '#']).next().unwrap_or(spec);
    if spec.is_empty() {
        return None;
    }
    for c in css_file_candidates(&dir.join(spec)) {
        if c.is_file() {
            return Some(c);
        }
    }
    if spec.starts_with("./") || spec.starts_with("../") {
        return None;
    }
    let (pkg, rest) = split_package_specifier(spec)?;
    for nm in node_modules_load_paths(dir) {
        let pkg_dir = nm.join(pkg);
        if !pkg_dir.is_dir() {
            continue;
        }
        if rest.is_empty() {
            if let Ok(text) = std::fs::read_to_string(pkg_dir.join("package.json")) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    for field in ["style", "main"] {
                        if let Some(entry) = json.get(field).and_then(|v| v.as_str()) {
                            for c in css_file_candidates(&pkg_dir.join(entry)) {
                                if c.is_file() && c.extension().is_some_and(|e| e == "css") {
                                    return Some(c);
                                }
                            }
                        }
                    }
                }
            }
            let idx = pkg_dir.join("index.css");
            return idx.is_file().then_some(idx);
        }
        for c in css_file_candidates(&pkg_dir.join(rest)) {
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

fn split_package_specifier(spec: &str) -> Option<(&str, &str)> {
    let mut parts = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
    let pkg = if spec.starts_with('@') {
        let scope = parts.next()?;
        let name = parts.next()?;
        &spec[..scope.len() + 1 + name.len()]
    } else {
        parts.next()?
    };
    let rest = spec.get(pkg.len() + 1..).unwrap_or("");
    Some((pkg, rest))
}

/// Rewrite the relative `url()` / `@import` paths of a stylesheet that lives in
/// `from_dir` so they are correct from `to_dir` (where it is being inlined).
fn rebase_to_dir(css: &str, file: &Path, from_dir: &Path, to_dir: &Path) -> Result<String, String> {
    if !(css.contains("url(") || css.contains("@import")) || from_dir == to_dir {
        return Ok(css.to_string());
    }
    let name = file.to_string_lossy().into_owned();
    let warnings = Arc::new(RwLock::new(Vec::new()));
    let stylesheet = StyleSheet::parse(
        css,
        ParserOptions {
            filename: name.clone(),
            error_recovery: true,
            warnings: Some(Arc::clone(&warnings)),
            ..ParserOptions::default()
        },
    )
    .map_err(|err| format!("css parse error in {name}: {err}"))?;
    report_css_warnings(&name, &warnings);
    let result = stylesheet
        .to_css(PrinterOptions {
            analyze_dependencies: Some(DependencyOptions::default()),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {name}: {err}"))?;
    let mut out = result.code;
    for dep in result.dependencies.unwrap_or_default() {
        let (placeholder, orig) = match dep {
            Dependency::Url(u) => (u.placeholder, u.url),
            Dependency::Import(i) => (i.placeholder, i.url),
        };
        let replacement = if rebase_relative(&orig, "/x").is_none() {
            orig
        } else {
            let (path, suffix) = match orig.find(['?', '#']) {
                Some(i) => (&orig[..i], &orig[i..]),
                None => (orig.as_str(), ""),
            };
            format!("{}{}", relative_path(to_dir, &from_dir.join(path)), suffix)
        };
        out = out.replace(&placeholder, &replacement);
    }
    Ok(out)
}

/// `target` expressed relative to `from_dir`, with `/` separators and a leading
/// `./` when it does not start with `..`.
fn relative_path(from_dir: &Path, target: &Path) -> String {
    let norm = |p: &Path| -> Vec<String> {
        let mut segs: Vec<String> = Vec::new();
        for c in p.components() {
            match c {
                std::path::Component::ParentDir => {
                    segs.pop();
                }
                std::path::Component::CurDir | std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
                std::path::Component::Normal(s) => segs.push(s.to_string_lossy().into_owned()),
            }
        }
        segs
    };
    let from = norm(from_dir);
    let to = norm(target);
    let common = from.iter().zip(to.iter()).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), from.len() - common).collect();
    parts.extend(to[common..].iter().cloned());
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn css_base_dir(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if !path.starts_with('/') {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => Some(path[..i].to_string()),
        None => None,
    }
}

fn rebase_relative(spec: &str, base_dir: &str) -> Option<String> {
    if spec.is_empty()
        || spec.starts_with('/')
        || spec.starts_with('#')
        || spec.starts_with("data:")
        || spec.starts_with("//")
        || spec.contains("://")
    {
        return None;
    }
    let (path, suffix) = match spec.find(['?', '#']) {
        Some(i) => (&spec[..i], &spec[i..]),
        None => (spec, ""),
    };
    Some(format!("{}{}", posix_join(base_dir, path), suffix))
}

fn posix_join(base_dir: &str, rel: &str) -> String {
    let mut segments: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_css_passes_through_and_minifies() {
        let out = compile_css("/styles.css", "body {\n  color: red;\n}\n", true).unwrap();
        assert_eq!(out.css, "body{color:red}");
        assert!(out.exports.is_none());
    }

    #[test]
    fn rebases_relative_url_and_import_to_server_root() {
        // A stylesheet served at /src/app.css: its relative @import and url()
        // must become server-absolute so an injected <style> resolves them
        // against the server root, not the page URL.
        let src = "@import \"./base.css\";\n.a { background: url(./img/bg.png); }";
        let out = compile_css_rebased("/src/app.css", src, true).unwrap();
        assert!(out.css.contains("/src/base.css"), "import rebased: {}", out.css);
        assert!(
            out.css.contains("/src/img/bg.png"),
            "url rebased: {}",
            out.css
        );
        assert!(!out.css.contains("./"), "no relative refs remain: {}", out.css);
    }

    #[test]
    fn rebase_resolves_parent_segments_and_skips_external_urls() {
        let src = ".a { background: url(../assets/x.png); }\n\
                   .b { background: url(https://cdn.test/y.png); }\n\
                   .c { background: url(data:image/png;base64,AAAA); }";
        let out = compile_css_rebased("/src/ui/card.css", src, true).unwrap();
        assert!(out.css.contains("/src/assets/x.png"), "parent rebased: {}", out.css);
        assert!(out.css.contains("https://cdn.test/y.png"), "external kept: {}", out.css);
        assert!(
            out.css.contains("data:image/png;base64,AAAA"),
            "data URI kept: {}",
            out.css
        );
    }

    #[test]
    fn plain_compile_does_not_rebase_urls() {
        // The build/SSR path must be untouched: url() stays relative.
        let src = ".a { background: url(./bg.png); }";
        let out = compile_css("/src/app.css", src, true).unwrap();
        assert!(
            out.css.contains("./bg.png"),
            "non-rebased keeps the relative url: {}",
            out.css
        );
    }

    #[test]
    fn is_css_module_matches_only_the_filename() {
        assert!(is_css_module("/src/app.module.css"));
        assert!(is_css_module("app.module.scss"));
        assert!(is_css_module("/a/b.module.css?used"));
        assert!(!is_css_module("/src/styles.css"));
        assert!(!is_css_module("/module.styles/app.css"));
    }

    #[test]
    fn is_sass_strips_query_and_checks_extension() {
        assert!(is_sass("/src/theme.scss"));
        assert!(is_sass("vars.sass"));
        assert!(is_sass("/a/theme.scss?inline"));
        assert!(!is_sass("/a/theme.css"));
        assert!(!is_sass("/a/scss.ts"));
    }

    #[test]
    fn css_modules_scope_and_export_class_names() {
        let out = compile_css(
            "/src/Counter.module.css",
            ".button { padding: 1rem; } .button:hover { opacity: 0.9; }",
            false,
        )
        .unwrap();
        let exports = out.exports.expect("module exports");
        assert_eq!(exports.len(), 1);
        let (name, scoped) = &exports[0];
        assert_eq!(name, "button");
        assert_ne!(scoped, "button", "class must be scoped: {scoped}");
        assert!(out.css.contains(scoped.as_str()), "{}", out.css);
    }

    #[test]
    fn css_modules_scoped_name_matches_ssr_loader() {
        // The SSR loader (oj_server assets/start/loader.mjs cssModuleExports)
        // recomputes these names in JS so server rendering agrees with the
        // class map the client is served; ssr-loader-css-modules.test.mjs pins
        // the same literals. If this assertion changes (lightningcss upgrade,
        // pattern change), the loader must be updated to match.
        let out = compile_css("/src/Counter.module.css", ".button { color: red; }", false).unwrap();
        let exports = out.exports.expect("module exports");
        assert_eq!(
            exports,
            vec![("button".to_string(), "Counter-module_button_EjW_Uq".to_string())]
        );
    }

    #[test]
    fn sass_nesting_and_variables_compile() {
        let scss = "$pad: 1rem;\n.card { padding: $pad; .title { font-weight: bold; } }";
        let css = compile_sass(scss, None).unwrap();
        assert!(css.contains("padding: 1rem"), "variable resolved: {css}");
        assert!(css.contains(".card .title"), "nesting flattened: {css}");
    }

    #[test]
    fn sass_then_lightningcss_pipeline() {
        let css = compile_sass(".a { .b { color: red } }", None).unwrap();
        let out = compile_css("/x.scss", &css, true).unwrap();
        assert!(out.css.contains(".a .b{color:red}"), "{}", out.css);
    }

    #[test]
    fn additional_data_is_prepended_before_compiling() {
        // css.preprocessorOptions.scss.additionalData: a global variable the
        // stylesheet never declares must resolve because it is injected first.
        let scss = ".btn { color: $brand; }";
        let css = compile_sass_with(scss, None, Some("$brand: #f00;")).unwrap();
        assert!(css.contains("color: #f00"), "injected var resolved: {css}");
        // Without the injection the same source fails (undefined variable).
        assert!(
            compile_sass(scss, None).is_err(),
            "undeclared variable must fail without additionalData",
        );
        // Empty / absent additionalData leaves compilation unchanged.
        assert!(compile_sass_with(".a { color: red; }", None, Some("")).is_ok());
    }

    // grass 0.13 mis-reads a dotted basename (`variables.module`) as an
    // extension and never finds `variables.module.scss`; the DottedFs shim
    // makes the CSS-modules `.module.scss` convention resolve like dart-sass.
    #[test]
    fn sass_resolves_dotted_module_import() {
        let base = std::env::temp_dir().join(format!("oj-css-dotmod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("comp")).unwrap();
        std::fs::create_dir_all(base.join("css")).unwrap();
        std::fs::write(base.join("css/variables.module.scss"), "$c: #f00;").unwrap();
        // a plain (non-dotted) sibling must keep working too
        std::fs::write(base.join("css/plain.scss"), "$p: 2px;").unwrap();
        // Both the bare and the explicit-extension forms must resolve, matching
        // dart-sass / Vite (verified against dart-sass 1.51: both give the same
        // output). Excalidraw uses both across its stylesheets.
        for spec in ["../css/variables.module", "../css/variables.module.scss"] {
            let src = format!(
                "@use \"{spec}\" as v;\n@use \"../css/plain\" as p;\n.x {{ color: v.$c; margin: p.$p; }}"
            );
            let css = compile_sass(&src, Some(&base.join("comp")))
                .unwrap_or_else(|e| panic!("`{spec}` should resolve: {e}"));
            assert!(css.contains("color: red") || css.contains("#f00"), "{spec}: {css}");
            assert!(css.contains("margin: 2px"), "{spec}: {css}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    // A dotted `.module.scss` that `@use`s another dotted module which itself
    // `@use`s a relative dotted sibling: the nested relative import must resolve
    // against the imported file's real directory, not the phantom `x.module/`
    // directory grass sees it through. (The CSS-modules pattern in the wild.)
    #[test]
    fn sass_resolves_nested_relative_dotted_import() {
        let base = std::env::temp_dir().join(format!("oj-css-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("comp")).unwrap();
        std::fs::create_dir_all(base.join("shared")).unwrap();
        std::fs::write(base.join("shared/colors.module.scss"), "$c: #0f0;").unwrap();
        // base.module.scss imports a relative dotted sibling (colors.module).
        std::fs::write(
            base.join("shared/base.module.scss"),
            "@use \"colors.module\" as c;\n.base { color: c.$c; }",
        )
        .unwrap();
        // The entry (in comp/) imports base via a dir-relative path.
        let src = "@use \"../shared/base.module.scss\";\n.x { display: block; }";
        let css = compile_sass(src, Some(&base.join("comp")))
            .unwrap_or_else(|e| panic!("nested dotted @use should resolve: {e}"));
        assert!(css.contains("color: #0f0") || css.contains("color: green"), "{css}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sass_resolves_node_modules_packages_like_vite() {
        // `@use "pkg"` -> package.json `sass` (then `style`) entry; `@use
        // "pkg/path"` -> a file inside the package; `~pkg` is accepted; the
        // node_modules directory is found above the importing file.
        let base = std::env::temp_dir().join(format!("oj-css-nm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pkg = base.join("node_modules/@acme/tokens");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::create_dir_all(base.join("src/deep")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@acme/tokens","main":"index.js","sass":"src/index.scss"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("src/index.scss"), "$brand: #123456;\n").unwrap();
        std::fs::write(pkg.join("src/_mixins.scss"), "@mixin pad { padding: 4px; }\n").unwrap();
        let styled = base.join("node_modules/plain-css");
        std::fs::create_dir_all(&styled).unwrap();
        std::fs::write(styled.join("package.json"), r#"{"name":"plain-css","style":"dist/x.css"}"#).unwrap();
        std::fs::create_dir_all(styled.join("dist")).unwrap();
        std::fs::write(styled.join("dist/x.css"), ".plain { color: green; }\n").unwrap();

        let dir = base.join("src/deep");
        let scss = "@use \"@acme/tokens\" as t;\n@use \"~@acme/tokens/src/mixins\";\n@import \"plain-css\";\n.a { color: t.$brand; @include mixins.pad; }";
        let css = compile_sass(scss, Some(&dir)).unwrap();
        assert!(css.contains("#123456"), "package sass entry resolved: {css}");
        assert!(css.contains("padding: 4px"), "~pkg/path resolved: {css}");
        assert!(css.contains(".plain"), "style entry resolved: {css}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sass_load_paths_from_config_resolve_bare_imports() {
        let base = std::env::temp_dir().join(format!("oj-css-lp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("styles")).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("styles/_theme.scss"), "$accent: #abc;\n").unwrap();
        let opts = SassOptions {
            load_dir: Some(&base.join("src")),
            additional_data: Some("$pad: 2px;"),
            load_paths: &[base.join("styles")],
        };
        let css = compile_sass_opts("@use \"theme\";\n.a { color: theme.$accent; padding: $pad; }", &opts).unwrap();
        assert!(css.contains("#abc") && css.contains("2px"), "{css}");
        assert!(compile_sass("@use \"theme\";", Some(&base.join("src"))).is_err(), "not on the load path without config");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn inlines_relative_and_package_imports_and_rebases_urls() {
        let base = std::env::temp_dir().join(format!("oj-css-imp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/base")).unwrap();
        std::fs::create_dir_all(base.join("node_modules/normalize-fake")).unwrap();
        std::fs::write(base.join("src/vars.css"), ":root { --x: 1; }\n").unwrap();
        std::fs::write(
            base.join("src/base/reset.css"),
            "@import \"../vars.css\";\n.reset { background: url(./dot.png); }\n",
        )
        .unwrap();
        std::fs::write(base.join("node_modules/normalize-fake/package.json"), r#"{"name":"normalize-fake","style":"n.css"}"#).unwrap();
        std::fs::write(base.join("node_modules/normalize-fake/n.css"), ".norm { margin: 0; }\n").unwrap();
        std::fs::write(base.join("src/print.css"), ".print { display: none; }\n").unwrap();
        let app = base.join("src/app.css");
        let src = "@import \"./base/reset.css\";\n@import 'normalize-fake';\n@import url(https://cdn.test/x.css);\n@import \"./print.css\" print;\n@import \"./missing.css\" screen;\n.app { color: red; }\n";
        let out = inline_imports(src, &app).unwrap();
        assert!(out.contains("--x: 1"), "nested import inlined: {out}");
        assert!(out.contains(".reset"), "relative import inlined: {out}");
        assert!(out.contains("url(\"./base/dot.png\")") || out.contains("url(./base/dot.png)"), "url rebased to the entry dir: {out}");
        assert!(out.contains(".norm"), "package style entry inlined: {out}");
        assert!(out.contains("@import url(https://cdn.test/x.css);"), "external import kept: {out}");
        assert!(out.contains("@media print {\n.print { display: none; }"), "media import inlined as @media: {out}");
        assert!(out.contains("@import \"./missing.css\" screen;"), "unresolvable media import kept: {out}");
        // Kept imports are hoisted above the inlined rules (CSS requires it).
        let first_rule = out.find('{').unwrap();
        assert!(out.rfind("@import").unwrap() < first_rule, "imports hoisted first: {out}");
        assert!(!out.contains("@import \"./base/reset.css\""), "inlined import removed: {out}");
        assert!(out.contains(".app { color: red; }"), "own rules kept verbatim: {out}");
        // Compiles and, in dev, rebases against the served url of the entry.
        let compiled = compile_css_rebased("/src/app.css", &out, true).unwrap();
        assert!(compiled.css.contains("/src/base/dot.png"), "{}", compiled.css);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn import_inlining_survives_cycles_and_missing_files() {
        let base = std::env::temp_dir().join(format!("oj-css-cyc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.css"), "@import \"./b.css\";\n.a{}").unwrap();
        std::fs::write(base.join("b.css"), "@import \"./a.css\";\n.b{}").unwrap();
        let out = inline_imports("@import \"./b.css\";\n.a{}", &base.join("a.css")).unwrap();
        assert!(out.contains(".b{}") && out.contains(".a{}"), "{out}");
        assert!(out.contains("@import \"./a.css\";"), "the cycle edge stays as written: {out}");
        let missing = inline_imports("@import \"./nope.css\";\n.a{}", &base.join("a.css")).unwrap();
        assert!(missing.contains("@import \"./nope.css\";"), "unresolvable import left alone: {missing}");
        assert_eq!(relative_path(Path::new("/p/src"), Path::new("/p/src/base/dot.png")), "./base/dot.png");
        assert_eq!(relative_path(Path::new("/p/src/base"), Path::new("/p/vars.css")), "../../vars.css");
        assert_eq!(split_package_specifier("@acme/tokens/src/x.css"), Some(("@acme/tokens", "src/x.css")));
        assert_eq!(split_package_specifier("normalize.css"), Some(("normalize.css", "")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dev_sourcemap_is_appended_inline_and_names_the_source() {
        let out = compile_css_rebased_with_map("/src/app.css", ".a {\n  color: red;\n}\n.b { color: blue; }\n", false).unwrap();
        let marker = "/*# sourceMappingURL=data:application/json;base64,";
        let at = out.css.find(marker).expect("sourceMappingURL comment");
        let b64_json = out.css[at + marker.len()..].trim_end().trim_end_matches("*/").trim();
        // Decode the base64 back and check the map shape.
        let decoded = {
            let t = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut bits = 0u32; let mut n = 0; let mut bytes = Vec::new();
            for c in b64_json.bytes().filter(|c| *c != b'=') {
                let v = t.iter().position(|x| *x == c).unwrap() as u32;
                bits = bits << 6 | v; n += 6;
                if n >= 8 { n -= 8; bytes.push((bits >> n) as u8); bits &= (1 << n) - 1; }
            }
            String::from_utf8(bytes).unwrap()
        };
        assert!(decoded.contains("\"sources\":[\"src/app.css\"]") && decoded.contains("\"sourceRoot\":\"/\""), "{decoded}");
        assert!(decoded.contains("sourcesContent"), "source embedded: {decoded}");
        assert!(decoded.contains("\"mappings\":\"") && !decoded.contains("\"mappings\":\"\""), "non-empty mappings: {decoded}");
        // Without the flag nothing is appended.
        assert!(!compile_css_rebased("/src/app.css", ".a { color: red; }", false).unwrap().css.contains("sourceMappingURL"));
    }

    #[test]
    fn strip_sass_import_ext_only_touches_import_specifiers() {
        // extension dropped on @use/@forward/@import so grass takes the bare path
        assert_eq!(
            strip_sass_import_ext("@use \"../css/vars.module.scss\" as *;\n"),
            "@use \"../css/vars.module\" as *;\n"
        );
        assert_eq!(
            strip_sass_import_ext("@forward './theme.scss';\n"),
            "@forward './theme';\n"
        );
        // a plain CSS @import keeps its .css; url(...) is left alone
        assert_eq!(
            strip_sass_import_ext("@import \"reset.css\";\n"),
            "@import \"reset.css\";\n"
        );
        assert_eq!(
            strip_sass_import_ext("@import url(\"x.scss\");\n"),
            "@import url(\"x.scss\");\n"
        );
        // a `.scss` string in a normal declaration must not be rewritten
        assert_eq!(
            strip_sass_import_ext(".a { content: \"file.scss\"; }\n"),
            ".a { content: \"file.scss\"; }\n"
        );
    }

    #[test]
    fn sass_missing_import_still_errors() {
        let base = std::env::temp_dir().join(format!("oj-css-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let src = "@use \"./nope.module\" as *;\n.x { color: red; }";
        assert!(
            compile_sass(src, Some(&base)).is_err(),
            "a genuinely missing dotted import must still fail, not resolve to nothing"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn autoprefixing_applies_for_targets() {
        let out = compile_css("/p.css", ".x { user-select: none; }", true).unwrap();
        assert!(
            out.css.contains("-webkit-user-select"),
            "autoprefixed: {}",
            out.css
        );
    }

    #[test]
    fn the_browser_matrix_keeps_modern_syntax_and_downlevels_the_rest() {
        // The target matrix is the compatibility contract of every stylesheet oj
        // emits. Asserted through behaviour: syntax the configured versions
        // support has to survive, and syntax they do not has to be lowered. A
        // matrix that decoded to version 0 would downlevel everything.
        let out = compile_css(
            "/p.css",
            ".a { width: clamp(1px, 2vw, 3px); color: rgb(0 0 0 / 50%); aspect-ratio: 1/2 }",
            true,
        )
        .unwrap()
        .css;
        assert!(out.contains("clamp("), "clamp is supported: {out}");
        assert!(out.contains("#00000080"), "modern color syntax: {out}");
        assert!(out.contains("aspect-ratio"), "aspect-ratio is supported: {out}");
        assert!(!out.contains("max(1px"), "clamp must not be lowered: {out}");

        // Nesting and logical properties are not supported by the oldest target,
        // so they are lowered.
        let nested = compile_css("/p.css", ".a { .b { color: red } }", true).unwrap().css;
        assert_eq!(nested, ".a .b{color:red}");
        let logical = compile_css("/p.css", ".a { inset-inline-start: 1px }", true)
            .unwrap()
            .css;
        assert!(logical.contains("left:1px"), "logical props lowered: {logical}");
    }

    #[test]
    fn scoped_class_names_follow_the_name_local_hash_pattern() {
        // The scoped name shows up in devtools, in snapshots and in the exports
        // map, so its shape is part of the contract.
        let out = compile_css("/src/Counter.module.css", ".button { color: red }", false).unwrap();
        let (local, scoped) = out.exports.expect("exports").into_iter().next().unwrap();
        assert_eq!(local, "button");
        // `[name]` is the file stem with its dots flattened.
        let prefix = "Counter-module_button_";
        assert!(
            scoped.starts_with(prefix),
            "expected [name]_[local]_[hash], got {scoped}"
        );
        let hash = &scoped[prefix.len()..];
        assert!(!hash.is_empty(), "no hash in {scoped}");
        assert!(
            hash.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "hash is not a plain token: {scoped}"
        );
        // The hash is derived from the path, not the contents: editing a
        // stylesheet must not rename its classes, or every edit would invalidate
        // the markup that already references them.
        let after_edit = compile_css("/src/Counter.module.css", ".button { color: blue }", false)
            .unwrap()
            .exports
            .expect("exports")
            .remove(0)
            .1;
        assert_eq!(scoped, after_edit, "an edit must not rename a class");
    }

    #[test]
    fn parse_errors_are_recovered_not_panicked() {
        // Vite's postcss pipeline never fails a stylesheet over garbage it
        // cannot parse; the invalid rule is dropped and nothing else is lost.
        let out = compile_css("/x.css", "!!not-css!! {}\n.ok { color: red }", true).unwrap();
        assert_eq!(out.css, ".ok{color:red}");
    }

    #[test]
    fn legacy_hacks_do_not_fail_the_stylesheet() {
        // Bootstrap-3 era vendor CSS: `*zoom`, `_height` and IE `filter:
        // progid:` are declaration-level hacks postcss keeps and Vite serves.
        // lightningcss keeps `_height` and `progid:` verbatim but cannot parse
        // the star hack; with error recovery it drops just that declaration
        // and the rest of the rule (and file) survives.
        let src = ".clearfix { *zoom: 1; _height: 1px; color: red; }\n\
                   .g { filter: progid:DXImageTransform.Microsoft.gradient(startColorstr='#fff', endColorstr='#000'); background: blue; }\n\
                   .b { color: green; }";
        let out = compile_css("/vendor.css", src, true).unwrap();
        assert!(out.css.contains(".clearfix{_height:1px;color:red}"), "{}", out.css);
        assert!(out.css.contains("progid:DXImageTransform") && out.css.contains("background:#00f"), "{}", out.css);
        assert!(out.css.contains(".b{color:green}"), "{}", out.css);
        // The rebased (dev) path parses the same way.
        assert!(compile_css_rebased("/src/vendor.css", src, false).is_ok());
    }
}
