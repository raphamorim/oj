// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::io;
use std::path::{Path, PathBuf};

use lightningcss::css_modules;
use lightningcss::dependencies::{Dependency, DependencyOptions};
use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet};
use lightningcss::targets::{Browsers, Targets};

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

impl grass::Fs for DottedFs {
    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir() || dotted_stylesheet(p).is_some()
    }
    fn is_file(&self, p: &Path) -> bool {
        p.is_file() || index_target(p).is_some()
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        let bytes = if p.is_file() {
            std::fs::read(p)?
        } else {
            match index_target(p) {
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
            out.push_str(
                &line
                    .replace(".scss\"", "\"")
                    .replace(".scss'", "'")
                    .replace(".sass\"", "\"")
                    .replace(".sass'", "'"),
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
    let fs = DottedFs;
    let mut options = grass::Options::default().fs(&fs);
    if let Some(dir) = load_dir {
        options = options.load_path(dir);
    }
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
    compile_css_impl(url, source, minify, false)
}

pub fn compile_css_rebased(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    compile_css_impl(url, source, minify, true)
}

fn compile_css_impl(
    url: &str,
    source: &str,
    minify: bool,
    rebase: bool,
) -> Result<CssOutput, String> {
    let is_module = is_css_module(url);
    let options = ParserOptions {
        filename: url.to_string(),
        css_modules: is_module.then(|| css_modules::Config {
            pattern: css_modules::Pattern::parse("[name]_[local]_[hash]").expect("static pattern"),
            ..css_modules::Config::default()
        }),
        ..ParserOptions::default()
    };

    let mut stylesheet = StyleSheet::parse(source, options)
        .map_err(|err| format!("css parse error in {url}: {err}"))?;

    let targets = default_targets();
    stylesheet
        .minify(MinifyOptions {
            targets: targets.clone(),
            ..MinifyOptions::default()
        })
        .map_err(|err| format!("css transform error in {url}: {err}"))?;

    let base = if rebase { css_base_dir(url) } else { None };
    let result = stylesheet
        .to_css(PrinterOptions {
            minify,
            targets,
            analyze_dependencies: base.as_ref().map(|_| DependencyOptions::default()),
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {url}: {err}"))?;

    let mut css = result.code;
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
    fn parse_errors_are_reported_not_panicked() {
        assert!(compile_css("/x.css", "!!not-css!!", false).is_err());
    }
}
