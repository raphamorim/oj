// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! CSS compilation on Lightning CSS — the same engine inside Tailwind v4,
//! Vite's CSS pipeline, and Parcel. Handles plain CSS (syntax lowering,
//! optional minify) and CSS Modules scoping for `*.module.css`.

use lightningcss::css_modules;
use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{ParserOptions, StyleSheet};

#[derive(Debug)]
pub struct CssOutput {
    pub css: String,
    /// CSS Modules only: exported class name -> scoped name.
    pub exports: Option<Vec<(String, String)>>,
}

pub fn is_css_module(url: &str) -> bool {
    url.rsplit('/').next().is_some_and(|f| f.contains(".module."))
}

pub fn compile_css(url: &str, source: &str, minify: bool) -> Result<CssOutput, String> {
    let is_module = is_css_module(url);
    let options = ParserOptions {
        filename: url.to_string(),
        css_modules: is_module.then(|| css_modules::Config {
            // Deterministic, readable in dev: `Counter_button_<hash>`.
            pattern: css_modules::Pattern::parse("[name]_[local]_[hash]")
                .expect("static pattern"),
            ..css_modules::Config::default()
        }),
        ..ParserOptions::default()
    };

    let stylesheet = StyleSheet::parse(source, options)
        .map_err(|err| format!("css parse error in {url}: {err}"))?;

    let result = stylesheet
        .to_css(PrinterOptions { minify, ..PrinterOptions::default() })
        .map_err(|err| format!("css print error in {url}: {err}"))?;

    let exports = result.exports.map(|map| {
        let mut pairs: Vec<(String, String)> =
            map.into_iter().map(|(name, export)| (name, export.name)).collect();
        pairs.sort();
        pairs
    });

    Ok(CssOutput { css: result.code, exports })
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
    fn parse_errors_are_reported_not_panicked() {
        // Note: `body { color: ` is VALID per spec (EOF closes blocks,
        // invalid declarations drop) — needs a truly malformed rule.
        assert!(compile_css("/x.css", "!!not-css!!", false).is_err());
    }
}
