// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use lightningcss::css_modules;
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

pub fn compile_sass(source: &str, load_dir: Option<&Path>) -> Result<String, String> {
    let mut options = grass::Options::default();
    if let Some(dir) = load_dir {
        options = options.load_path(dir);
    }
    grass::from_string(source.to_string(), &options).map_err(|e| format!("sass error: {e}"))
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

    let result = stylesheet
        .to_css(PrinterOptions {
            minify,
            targets,
            ..PrinterOptions::default()
        })
        .map_err(|err| format!("css print error in {url}: {err}"))?;

    let exports = result.exports.map(|map| {
        let mut pairs: Vec<(String, String)> = map
            .into_iter()
            .map(|(name, export)| (name, export.name))
            .collect();
        pairs.sort();
        pairs
    });

    Ok(CssOutput {
        css: result.code,
        exports,
    })
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
