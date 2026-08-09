// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Compile-time `import.meta.glob` expansion (Vite-compatible).
//!
//! `import.meta.glob('./dir/*.js')` becomes an object literal mapping each
//! matched file (relative to the importing module) to a lazy importer
//! `() => import('./dir/a.js')`; with `{ eager: true }` it becomes references
//! to hoisted `import * as` bindings. Options: `eager`, `import` (pick a
//! named export), `query` (`?raw`/`?url`/custom). Patterns may be a string or
//! an array; `!`-prefixed patterns are negative filters.
//!
//! Runs before specifier rewriting, so the generated `import(...)` / `import`
//! statements are canonicalized to served URLs by the normal pipeline.

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, Expression, ObjectPropertyKind, Program, PropertyKey,
    Statement,
};
use oxc_ast_visit::{VisitMut, walk_mut};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Expand every `import.meta.glob(...)` in `program`. `dir` is the importing
/// module's directory (patterns and match keys are relative to it).
pub fn expand<'a>(allocator: &'a Allocator, dir: &Path, program: &mut Program<'a>) {
    let mut expander = GlobExpander { allocator, dir, hoisted: Vec::new(), uid: 0 };
    expander.visit_program(program);
    let hoisted = expander.hoisted;
    if hoisted.is_empty() {
        return;
    }
    // Prepend hoisted `import * as ...` statements (eager mode).
    let src: &str = allocator.alloc_str(&hoisted.join("\n"));
    let parsed = Parser::new(allocator, src, SourceType::mjs()).parse();
    let mut stmts: Vec<Statement> = parsed.program.body.into_iter().collect();
    stmts.reverse();
    for stmt in stmts {
        program.body.insert(0, stmt);
    }
}

struct GlobExpander<'a, 'd> {
    allocator: &'a Allocator,
    dir: &'d Path,
    hoisted: Vec<String>,
    uid: usize,
}

struct GlobOptions {
    eager: bool,
    import: Option<String>,
    query: String,
}

impl<'a> GlobExpander<'a, '_> {
    fn parse_expr(&self, source: &str) -> Option<Expression<'a>> {
        let source: &'a str = self.allocator.alloc_str(source);
        let parsed = Parser::new(self.allocator, source, SourceType::mjs()).parse();
        match parsed.program.body.into_iter().next() {
            Some(Statement::ExpressionStatement(es)) => Some(es.unbox().expression),
            _ => None,
        }
    }

    /// Build the replacement object-literal source, collecting hoisted eager
    /// imports as a side effect. Returns None if args aren't static literals.
    fn build_replacement(&mut self, args: &[Argument<'a>]) -> Option<String> {
        let patterns = collect_patterns(args.first()?)?;
        let opts = args.get(1).map(collect_options).unwrap_or(Some(GlobOptions {
            eager: false,
            import: None,
            query: String::new(),
        }))?;

        let matches = glob_matches(self.dir, &patterns);
        let mut entries: Vec<String> = Vec::new();
        for key in &matches {
            let spec = format!("{key}{}", opts.query);
            if opts.eager {
                let ident = format!("__oj_glob_{}", self.uid);
                self.uid += 1;
                match &opts.import {
                    Some(name) if name != "*" => self
                        .hoisted
                        .push(format!("import {{ {name} as {ident} }} from {spec:?};")),
                    _ => self.hoisted.push(format!("import * as {ident} from {spec:?};")),
                }
                entries.push(format!("{key:?}: {ident}"));
            } else {
                let importer = match &opts.import {
                    Some(name) if name != "*" => {
                        format!("() => import({spec:?}).then((m) => m[{name:?}])")
                    }
                    _ => format!("() => import({spec:?})"),
                };
                entries.push(format!("{key:?}: {importer}"));
            }
        }
        Some(format!("({{{}}})", entries.join(", ")))
    }
}

impl<'a> VisitMut<'a> for GlobExpander<'a, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        let replacement = match &*expr {
            Expression::CallExpression(call) if is_import_meta_glob(call) => {
                self.build_replacement(&call.arguments)
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            if let Some(new_expr) = self.parse_expr(&replacement) {
                *expr = new_expr;
                return;
            }
        }
        walk_mut::walk_expression(self, expr);
    }
}

fn is_import_meta_glob(call: &oxc_ast::ast::CallExpression) -> bool {
    let Expression::StaticMemberExpression(member) = &call.callee else { return false };
    member.property.name == "glob" && matches!(member.object, Expression::ImportMeta(_))
}

fn collect_patterns(arg: &Argument) -> Option<Vec<String>> {
    match arg.as_expression()? {
        Expression::StringLiteral(s) => Some(vec![s.value.to_string()]),
        Expression::ArrayExpression(arr) => {
            let mut out = Vec::new();
            for el in &arr.elements {
                if let ArrayExpressionElement::StringLiteral(s) = el {
                    out.push(s.value.to_string());
                } else {
                    return None; // non-literal -> bail, leave call untouched
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn collect_options(arg: &Argument) -> Option<GlobOptions> {
    let Some(Expression::ObjectExpression(obj)) = arg.as_expression() else { return None };
    let mut opts = GlobOptions { eager: false, import: None, query: String::new() };
    for prop in &obj.properties {
        let ObjectPropertyKind::ObjectProperty(p) = prop else { continue };
        let key = match &p.key {
            PropertyKey::StaticIdentifier(id) => id.name.as_str(),
            PropertyKey::StringLiteral(s) => s.value.as_str(),
            _ => continue,
        };
        match key {
            "eager" => {
                if let Expression::BooleanLiteral(b) = &p.value {
                    opts.eager = b.value;
                }
            }
            "import" => {
                if let Expression::StringLiteral(s) = &p.value {
                    opts.import = Some(s.value.to_string());
                }
            }
            "query" => {
                if let Expression::StringLiteral(s) = &p.value {
                    let q = s.value.as_str();
                    opts.query = if q.starts_with('?') { q.to_string() } else { format!("?{q}") };
                }
            }
            "as" => {
                if let Expression::StringLiteral(s) = &p.value {
                    opts.query = format!("?{}", s.value);
                }
            }
            _ => {}
        }
    }
    Some(opts)
}

/// Glob the filesystem relative to `dir`, returning importer-relative keys
/// (each prefixed `./` or `../`), sorted for deterministic output. Negative
/// (`!`) patterns filter the positive matches.
fn glob_matches(dir: &Path, patterns: &[String]) -> Vec<String> {
    let (negatives, positives): (Vec<&String>, Vec<&String>) =
        patterns.iter().partition(|p| p.starts_with('!'));
    // Match against dir-relative keys (no `./`), so strip both `!` and `./`.
    let neg_patterns: Vec<glob::Pattern> = negatives
        .iter()
        .filter_map(|p| glob::Pattern::new(p.trim_start_matches('!').trim_start_matches("./")).ok())
        .collect();

    let mut keys = std::collections::BTreeSet::new();
    for pat in positives {
        let abs = dir.join(pat);
        let Ok(paths) = glob::glob(&abs.to_string_lossy()) else { continue };
        for entry in paths.flatten() {
            if !entry.is_file() {
                continue;
            }
            let Ok(rel) = entry.strip_prefix(dir) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if neg_patterns.iter().any(|n| n.matches(&rel)) {
                continue;
            }
            let key = if rel.starts_with('.') { rel } else { format!("./{rel}") };
            keys.insert(key);
        }
    }
    keys.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_codegen::Codegen;

    fn expand_source(dir: &Path, src: &str) -> String {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, src, SourceType::mjs()).parse();
        let mut program = parsed.program;
        expand(&allocator, dir, &mut program);
        Codegen::new().build(&program).code
    }

    fn fixture_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-glob-{}-{label}", std::process::id()));
        let sub = dir.join("locales");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("en.json"), "{}").unwrap();
        std::fs::write(sub.join("fr.json"), "{}").unwrap();
        std::fs::write(sub.join("_ignore.json"), "{}").unwrap();
        dir
    }

    #[test]
    fn lazy_glob_expands_to_import_map() {
        let dir = fixture_dir("lazy");
        let out = expand_source(&dir, "const m = import.meta.glob('./locales/*.json');\n");
        assert!(out.contains(r#""./locales/en.json": () => import("./locales/en.json")"#), "{out}");
        assert!(out.contains("fr.json"), "{out}");
        assert!(!out.contains("import.meta.glob"), "call must be replaced: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eager_glob_hoists_star_imports() {
        let dir = fixture_dir("eager");
        let out = expand_source(&dir, "const m = import.meta.glob('./locales/*.json', { eager: true });\n");
        assert!(out.contains("import * as __oj_glob_0"), "{out}");
        assert!(out.contains(r#""./locales/en.json": __oj_glob_"#), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn negative_pattern_and_query_and_import() {
        let dir = fixture_dir("neg");
        let out = expand_source(
            &dir,
            "const m = import.meta.glob(['./locales/*.json', '!./locales/_*.json'], { query: 'raw', import: 'default' });\n",
        );
        assert!(!out.contains("_ignore"), "negative pattern must exclude: {out}");
        assert!(out.contains("?raw"), "query appended: {out}");
        assert!(out.contains(r#".then((m) => m["default"])"#), "import pick: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_literal_pattern_is_left_untouched() {
        let dir = fixture_dir("nonlit");
        let out = expand_source(&dir, "const p = 'x'; const m = import.meta.glob(p);\n");
        assert!(out.contains("import.meta.glob(p)"), "dynamic arg must be left as-is: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
