// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Hook filters in rolldown's vocabulary, evaluated in Rust before a module
//! is parsed. A plugin declares what it wants (`id`, `code` and module type
//! includes and excludes) and the host skips it for everything else, so an
//! inactive pass costs one glob match and one substring scan per module.

use rolldown_utils::filter_expression::{filter_exprs_interpreter, FilterExpr, FilterExprKind};
use rolldown_utils::js_regex::HybridRegex;
pub use rolldown_utils::pattern_filter::StringOrRegex;

/// rolldown's hook-filter semantics: any `include` match wins unless an
/// `exclude` matched first; with no includes at all everything not excluded
/// matches. Leaves read `id` (glob or regex against the file path), `code`
/// (substring or regex against the source) and `module_type` (`js`, `jsx`,
/// `ts`, `tsx`, ...).
#[derive(Debug, Default)]
pub struct ModuleFilter {
    exprs: Vec<FilterExprKind>,
}

impl ModuleFilter {
    /// Matches every module: a pass that wants to see all JavaScript.
    pub fn new() -> Self {
        Self::default()
    }

    /// Include modules whose path matches a glob (`**/*.tsx`, `src/**`).
    /// Relative globs resolve against the project root at match time.
    pub fn include_id(mut self, glob: &str) -> Self {
        self.add(FilterExprKind::Include(FilterExpr::Id(
            StringOrRegex::String(glob.to_string()).into(),
        )));
        self
    }

    /// Exclude modules whose path matches a glob.
    pub fn exclude_id(mut self, glob: &str) -> Self {
        self.add(FilterExprKind::Exclude(FilterExpr::Id(
            StringOrRegex::String(glob.to_string()).into(),
        )));
        self
    }

    /// Include modules whose path matches a JavaScript-style regex.
    pub fn include_id_regex(mut self, pattern: &str) -> Result<Self, String> {
        let re = HybridRegex::new(pattern).map_err(|e| e.to_string())?;
        self.add(FilterExprKind::Include(FilterExpr::Id(StringOrRegex::Regex(re).into())));
        Ok(self)
    }

    /// Exclude modules whose path matches a JavaScript-style regex.
    pub fn exclude_id_regex(mut self, pattern: &str) -> Result<Self, String> {
        let re = HybridRegex::new(pattern).map_err(|e| e.to_string())?;
        self.add(FilterExprKind::Exclude(FilterExpr::Id(StringOrRegex::Regex(re).into())));
        Ok(self)
    }

    /// Include modules whose source contains `needle` (a SIMD substring scan).
    pub fn include_code(mut self, needle: &str) -> Self {
        self.add(FilterExprKind::Include(FilterExpr::Code(StringOrRegex::String(
            needle.to_string(),
        ))));
        self
    }

    /// Exclude modules whose source contains `needle`.
    pub fn exclude_code(mut self, needle: &str) -> Self {
        self.add(FilterExprKind::Exclude(FilterExpr::Code(StringOrRegex::String(
            needle.to_string(),
        ))));
        self
    }

    /// Include modules whose source matches a JavaScript-style regex.
    pub fn include_code_regex(mut self, pattern: &str) -> Result<Self, String> {
        let re = HybridRegex::new(pattern).map_err(|e| e.to_string())?;
        self.add(FilterExprKind::Include(FilterExpr::Code(StringOrRegex::Regex(re))));
        Ok(self)
    }

    /// Include one module type (`js`, `jsx`, `ts`, `tsx`).
    pub fn include_module_type(mut self, module_type: &str) -> Self {
        self.add(FilterExprKind::Include(FilterExpr::ModuleType(module_type.to_string())));
        self
    }

    /// Exclude one module type.
    pub fn exclude_module_type(mut self, module_type: &str) -> Self {
        self.add(FilterExprKind::Exclude(FilterExpr::ModuleType(module_type.to_string())));
        self
    }

    /// Require every clause of `all` at once: `id` AND `code` AND module type.
    /// The builder methods above are each their own include, so two
    /// `include_*` calls mean "either"; this is the "both" form.
    pub fn include_all(mut self, all: Vec<FilterExpr>) -> Self {
        self.add(FilterExprKind::Include(FilterExpr::And(all)));
        self
    }

    /// A raw rolldown filter expression, for anything the builders do not cover.
    pub fn push(mut self, expr: FilterExprKind) -> Self {
        self.add(expr);
        self
    }

    // rolldown's interpreter answers on the first clause that matches, so an
    // exclude only wins if it is tested first: excludes are kept ahead of
    // includes regardless of the order the builder saw them.
    fn add(&mut self, expr: FilterExprKind) {
        match expr {
            FilterExprKind::Exclude(_) => {
                let at = self
                    .exprs
                    .iter()
                    .position(|e| matches!(e, FilterExprKind::Include(_)))
                    .unwrap_or(self.exprs.len());
                self.exprs.insert(at, expr);
            }
            FilterExprKind::Include(_) => self.exprs.push(expr),
        }
    }

    /// True when the filter has no clauses, so it matches every module.
    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    /// Whether a module passes. `code` may be omitted by hosts that do not
    /// have the source yet; `Code` leaves then never match.
    pub fn matches(&self, id: &str, code: Option<&str>, module_type: Option<&str>, cwd: &str) -> bool {
        filter_exprs_interpreter(&self.exprs, Some(id), code, module_type, None, cwd)
    }
}

/// Leaf constructors for `include_all`.
pub fn id(glob: &str) -> FilterExpr {
    FilterExpr::Id(StringOrRegex::String(glob.to_string()).into())
}

pub fn code(needle: &str) -> FilterExpr {
    FilterExpr::Code(StringOrRegex::String(needle.to_string()))
}

pub fn module_type(name: &str) -> FilterExpr {
    FilterExpr::ModuleType(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/app";

    #[test]
    fn empty_filter_matches_everything() {
        let f = ModuleFilter::new();
        assert!(f.matches("/app/src/a.tsx", None, Some("tsx"), CWD));
        assert!(f.matches("/app/node_modules/x/index.js", Some("x"), Some("js"), CWD));
    }

    #[test]
    fn id_glob_include_and_exclude() {
        let f = ModuleFilter::new().include_id("**/*.tsx").exclude_id("**/node_modules/**");
        assert!(f.matches("/app/src/App.tsx", None, None, CWD));
        assert!(!f.matches("/app/src/App.ts", None, None, CWD));
        assert!(!f.matches("/app/node_modules/pkg/Comp.tsx", None, None, CWD));
    }

    #[test]
    fn relative_glob_resolves_against_cwd() {
        let f = ModuleFilter::new().include_id("src/**/*.ts");
        assert!(f.matches("/app/src/lib/x.ts", None, None, CWD));
        assert!(!f.matches("/other/src/lib/x.ts", None, None, CWD));
    }

    #[test]
    fn code_include_needs_the_source() {
        let f = ModuleFilter::new().include_code("__marker__");
        assert!(f.matches("/app/a.js", Some("let x = __marker__;"), None, CWD));
        assert!(!f.matches("/app/a.js", Some("let x = 1;"), None, CWD));
        assert!(!f.matches("/app/a.js", None, None, CWD), "no code, no match");
    }

    #[test]
    fn exclude_wins_over_include() {
        let f = ModuleFilter::new().include_code("stylex").exclude_id("**/*.test.tsx");
        assert!(f.matches("/app/a.tsx", Some("import stylex from 'x'"), None, CWD));
        assert!(!f.matches("/app/a.test.tsx", Some("import stylex from 'x'"), None, CWD));
    }

    #[test]
    fn module_type_leaf() {
        let f = ModuleFilter::new().include_module_type("tsx").include_module_type("jsx");
        assert!(f.matches("/app/a.tsx", None, Some("tsx"), CWD));
        assert!(f.matches("/app/a.jsx", None, Some("jsx"), CWD));
        assert!(!f.matches("/app/a.ts", None, Some("ts"), CWD));
    }

    #[test]
    fn include_all_requires_every_clause() {
        let f = ModuleFilter::new().include_all(vec![id("**/*.tsx"), code("marker")]);
        assert!(f.matches("/app/a.tsx", Some("marker"), None, CWD));
        assert!(!f.matches("/app/a.tsx", Some("plain"), None, CWD));
        assert!(!f.matches("/app/a.ts", Some("marker"), None, CWD));
    }

    #[test]
    fn regex_ids_and_code() {
        let f = ModuleFilter::new()
            .include_id_regex(r"\.(t|j)sx$")
            .unwrap()
            .exclude_code("// oj-skip");
        assert!(f.matches("/app/a.jsx", Some("x"), None, CWD));
        assert!(!f.matches("/app/a.js", Some("x"), None, CWD));
        assert!(!f.matches("/app/a.jsx", Some("// oj-skip"), None, CWD));
        assert!(ModuleFilter::new().include_id_regex("(").is_err());
    }
}
