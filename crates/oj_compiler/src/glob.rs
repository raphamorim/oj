// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, Expression, NewExpression, ObjectPropertyKind, Program,
    PropertyKey, Statement,
};
use oxc_ast_visit::{VisitMut, walk_mut};
use oxc_parser::Parser;
use oxc_span::SourceType;

pub fn expand_source(source: &str, path: &Path) -> String {
    if !source.contains("import.meta.glob") {
        return source.to_string();
    }
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return source.to_string();
    }
    let mut program = parsed.program;
    let dir = path.parent().unwrap_or(path);
    expand(&allocator, dir, &mut program);
    oxc_codegen::Codegen::new().build(&program).code
}

pub fn expand<'a>(allocator: &'a Allocator, dir: &Path, program: &mut Program<'a>) {
    let mut expander = GlobExpander { allocator, dir, hoisted: Vec::new(), uid: 0 };
    expander.visit_program(program);
    let hoisted = expander.hoisted;
    if hoisted.is_empty() {
        return;
    }
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

pub fn expand_dynamic_import_vars<'a>(
    allocator: &'a Allocator,
    dir: &Path,
    program: &mut Program<'a>,
    source_text: &str,
) -> bool {
    let mut v = DynImportVars { allocator, dir, source: source_text, changed: false };
    v.visit_program(program);
    v.changed
}

/// Rewrites `new URL("./asset", import.meta.url)` into a hoisted `?url` asset
/// import referenced in place, so the asset flows through oj's normal asset
/// pipeline (Vite's asset-import-meta-url). Returns whether anything changed.
pub fn expand_new_url_asset<'a>(allocator: &'a Allocator, program: &mut Program<'a>) -> bool {
    let mut v = NewUrlAsset { allocator, hoisted: Vec::new(), uid: 0 };
    v.visit_program(program);
    if v.hoisted.is_empty() {
        return false;
    }
    let src: &'a str = allocator.alloc_str(&v.hoisted.join("\n"));
    let parsed = Parser::new(allocator, src, SourceType::mjs()).parse();
    let mut stmts: Vec<Statement> = parsed.program.body.into_iter().collect();
    stmts.reverse();
    for stmt in stmts {
        program.body.insert(0, stmt);
    }
    true
}

struct NewUrlAsset<'a> {
    allocator: &'a Allocator,
    hoisted: Vec<String>,
    uid: usize,
}

impl<'a> NewUrlAsset<'a> {
    fn asset_spec(&self, n: &NewExpression<'a>) -> Option<String> {
        let Expression::Identifier(id) = &n.callee else { return None };
        if id.name != "URL" || n.arguments.len() != 2 {
            return None;
        }
        let Expression::StringLiteral(spec) = n.arguments[0].as_expression()? else { return None };
        let spec = spec.value.as_str();
        if !(spec.starts_with("./") || spec.starts_with("../")) {
            return None;
        }
        // second arg must be `import.meta.url`
        let Expression::StaticMemberExpression(m) = n.arguments[1].as_expression()? else {
            return None;
        };
        if m.property.name != "url" || !matches!(&m.object, Expression::ImportMeta(_)) {
            return None;
        }
        Some(spec.to_string())
    }
}

impl<'a> VisitMut<'a> for NewUrlAsset<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::NewExpression(n) = &*expr {
            if let Some(spec) = self.asset_spec(n) {
                let ident = format!("__oj_url_{}", self.uid);
                self.uid += 1;
                self.hoisted.push(format!("import {ident} from {:?};", format!("{spec}?url")));
                let rep = format!("new URL({ident}, import.meta.url)");
                let src: &'a str = self.allocator.alloc_str(&rep);
                let parsed = Parser::new(self.allocator, src, SourceType::mjs()).parse();
                if let Some(Statement::ExpressionStatement(es)) =
                    parsed.program.body.into_iter().next()
                {
                    *expr = es.unbox().expression;
                }
                return;
            }
        }
        walk_mut::walk_expression(self, expr);
    }
}

pub fn expand_new_url_asset_source(source: &str, path: &Path) -> String {
    if !source.contains("import.meta.url") {
        return source.to_string();
    }
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return source.to_string();
    }
    let mut program = parsed.program;
    if expand_new_url_asset(&allocator, &mut program) {
        oxc_codegen::Codegen::new().build(&program).code
    } else {
        source.to_string()
    }
}

pub fn expand_dynamic_import_vars_source(source: &str, path: &Path) -> String {
    if !source.contains("import(") {
        return source.to_string();
    }
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return source.to_string();
    }
    let mut program = parsed.program;
    let dir = path.parent().unwrap_or(path);
    if expand_dynamic_import_vars(&allocator, dir, &mut program, source) {
        oxc_codegen::Codegen::new().build(&program).code
    } else {
        source.to_string()
    }
}

struct DynImportVars<'a, 'd, 's> {
    allocator: &'a Allocator,
    dir: &'d Path,
    source: &'s str,
    changed: bool,
}

impl<'a> DynImportVars<'a, '_, '_> {
    fn build(&self, imp: &oxc_ast::ast::ImportExpression<'a>) -> Option<String> {
        let Expression::TemplateLiteral(tpl) = &imp.source else { return None };
        if tpl.expressions.is_empty() {
            return None;
        }
        let mut pattern = String::new();
        for (i, q) in tpl.quasis.iter().enumerate() {
            let piece = q.value.cooked.as_ref().map(|c| c.as_str()).unwrap_or(q.value.raw.as_str());
            pattern.push_str(piece);
            if i < tpl.expressions.len() {
                pattern.push('*');
            }
        }
        if !(pattern.starts_with("./") || pattern.starts_with("../")) {
            return None;
        }
        let mut matches = glob_matches(self.dir, &[pattern]);
        matches.sort();
        if matches.is_empty() {
            return None;
        }
        let arg = self.source.get(tpl.span.start as usize..tpl.span.end as usize)?;
        let mut cases = String::new();
        for key in &matches {
            cases.push_str(&format!("case {key:?}: return import({key:?});"));
        }
        Some(format!(
            "(function (p) {{ switch (p) {{ {cases}default: return Promise.reject(new Error(\"Unknown variable dynamic import: \" + p)); }} }})({arg})"
        ))
    }
}

impl<'a> VisitMut<'a> for DynImportVars<'a, '_, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::ImportExpression(imp) = &*expr {
            if let Some(rep) = self.build(imp) {
                let src: &'a str = self.allocator.alloc_str(&rep);
                let parsed = Parser::new(self.allocator, src, SourceType::mjs()).parse();
                if let Some(Statement::ExpressionStatement(es)) =
                    parsed.program.body.into_iter().next()
                {
                    *expr = es.unbox().expression;
                    self.changed = true;
                    return;
                }
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
                    return None;
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

fn glob_matches(dir: &Path, patterns: &[String]) -> Vec<String> {
    let (negatives, positives): (Vec<&String>, Vec<&String>) =
        patterns.iter().partition(|p| p.starts_with('!'));
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
    fn dynamic_import_var_expands_to_switch() {
        let dir = fixture_dir("dynvar");
        let out = expand_dynamic_import_vars_source(
            "const load = (l) => import(`./locales/${l}.json`);\n",
            &dir.join("main.js"),
        );
        assert!(out.contains(r#"case "./locales/en.json": return import("./locales/en.json")"#), "{out}");
        assert!(out.contains("fr.json"), "{out}");
        assert!(out.contains("${l}"), "original template kept as runtime arg: {out}");
        assert!(out.contains("Unknown variable dynamic import"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_string_dynamic_import_untouched() {
        let dir = fixture_dir("dynplain");
        let out = expand_dynamic_import_vars_source("const m = import(\"./locales/en.json\");\n", &dir.join("main.js"));
        assert!(!out.contains("switch"), "string-literal import left alone: {out}");
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

    #[test]
    fn empty_glob_yields_empty_object() {
        let dir = fixture_dir("empty");
        let out = expand_source(&dir, "const m = import.meta.glob('./nope/*.json');\n");
        assert!(out.contains("= {}"), "no matches must expand to an empty object: {out}");
        assert!(!out.contains("import.meta.glob"), "call must be replaced: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recursive_double_star_matches_nested_files() {
        let dir = std::env::temp_dir().join(format!("oj-glob-{}-recursive", std::process::id()));
        std::fs::create_dir_all(dir.join("content").join("posts")).unwrap();
        std::fs::write(dir.join("content").join("top.md"), "").unwrap();
        std::fs::write(dir.join("content").join("posts").join("nested.md"), "").unwrap();
        let out = expand_source(&dir, "const m = import.meta.glob('./content/**/*.md');\n");
        assert!(out.contains("./content/posts/nested.md"), "recursive match missing: {out}");
        assert!(out.contains("./content/top.md"), "top-level match missing: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
