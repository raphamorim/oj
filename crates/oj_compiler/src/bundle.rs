// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! ESM to registry-factory transform for full bundle mode (M4).
//!
//! Output shape per module: a plain statement list (the factory body) that
//! runs with `module`, `__oj_exports`, `__oj_require` in scope, provided by
//! the client runtime. Imports become `var _oj_mN = __oj_require("url")`
//! plus scope-aware member-access rewriting of every reference (via oxc
//! semantic; shadowed names are untouched). Exports become getter
//! registrations (`__oj_esm`) installed before the body runs, so circular
//! imports and hoisted function exports behave like real ESM live bindings.
//!
//! Node synthesis parses tiny snippets in the same arena (via `alloc_str`,
//! so atoms stay valid) and swaps the real AST nodes into their placeholders.

use std::collections::HashMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, ImportDeclarationSpecifier, ModuleExportName, Statement};
use oxc_ast_visit::{VisitMut, walk_mut};
use oxc_ecmascript::BoundNames;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_syntax::reference::ReferenceId;
use oxc_transformer::{JsxRuntime, ReactRefreshOptions, TransformOptions, Transformer};

use crate::{CompileError, ImportRewriter};

#[derive(Debug, Clone, PartialEq)]
pub enum FactoryKind {
    Esm,
    Cjs,
}

#[derive(Debug)]
pub struct FactoryOutput {
    /// Factory body statements (no wrapper; the chunk assembler wraps).
    pub code: String,
    /// Resolved local urls this module depends on (graph edges + crawl).
    pub imports: Vec<String>,
    /// CJS only: raw specifier to resolved url, resolved by the runtime's
    /// in-factory `require`.
    pub require_map: Vec<(String, String)>,
    pub kind: FactoryKind,
}

impl FactoryOutput {
    pub fn is_boundary(&self) -> bool {
        self.kind == FactoryKind::Esm && self.code.contains("$RefreshReg$(")
    }
}

/// Entry point used by the dev server in bundle mode.
pub fn compile_factory(
    path: &Path,
    url: &str,
    source_text: &str,
    resolve: &mut ImportRewriter,
) -> Result<FactoryOutput, CompileError> {
    let is_dep = url.starts_with("/node_modules/");
    if is_dep && !crate::cjs::has_module_syntax_pub(path, source_text) {
        return compile_cjs_factory(path, source_text, resolve);
    }
    compile_esm_factory(path, url, source_text, resolve, !is_dep)
}

/// CJS is already factory-shaped: `module`/`exports`/`require` come from the
/// runtime. Only NODE_ENV-replace, DCE, and collect the require map.
fn compile_cjs_factory(
    path: &Path,
    source_text: &str,
    resolve: &mut ImportRewriter,
) -> Result<FactoryOutput, CompileError> {
    let analyzed = crate::cjs::analyze_for_factory(path, source_text)?;
    let mut require_map = Vec::new();
    let mut imports = Vec::new();
    for spec in &analyzed.requires {
        if let Some(target) = resolve(spec) {
            if !imports.contains(&target) {
                imports.push(target.clone());
            }
            require_map.push((spec.clone(), target));
        }
    }
    Ok(FactoryOutput { code: analyzed.body, imports, require_map, kind: FactoryKind::Cjs })
}

struct Replacement {
    var: String,
    /// None = the namespace object itself; Some(name) = member access.
    member: Option<String>,
}

fn compile_esm_factory(
    path: &Path,
    url: &str,
    source_text: &str,
    resolve: &mut ImportRewriter,
    refresh: bool,
) -> Result<FactoryOutput, CompileError> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());

    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        let message = parsed
            .diagnostics
            .into_iter()
            .map(|d| format!("{:?}", d.with_source_code(source_text.to_string())))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileError::Parse { path: path.to_path_buf(), message });
    }
    let mut program = parsed.program;

    // Standard dev pipeline: TS strip, automatic JSX, refresh instrumentation.
    let scoping = SemanticBuilder::new().with_excess_capacity(2.0).build(&program);
    let scoping = scoping.semantic.into_scoping();
    let mut transform_options = TransformOptions::default();
    transform_options.jsx.jsx_plugin = true;
    transform_options.jsx.runtime = JsxRuntime::Automatic;
    transform_options.jsx.development = true;
    if refresh {
        transform_options.jsx.refresh = Some(ReactRefreshOptions::default());
    }
    let ret =
        Transformer::new(&allocator, path, &transform_options).build_with_scoping(scoping, &mut program);
    if !ret.diagnostics.is_empty() {
        let message =
            ret.diagnostics.into_iter().map(|d| format!("{d:?}")).collect::<Vec<_>>().join("\n");
        return Err(CompileError::Transform { path: path.to_path_buf(), message });
    }

    // import.meta.env static replacement: the factory is a plain function
    // where `import.meta` is invalid, so it must be replaced away here.
    if source_text.contains("import.meta.env") {
        use oxc_transformer_plugins::{ReplaceGlobalDefines, ReplaceGlobalDefinesConfig};
        let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
        let defines = crate::import_meta_env_defines(true);
        if let Ok(config) = ReplaceGlobalDefinesConfig::new(&defines) {
            let _ = ReplaceGlobalDefines::new(&allocator, config).build(scoping, &mut program);
        }
    }

    // Expand import.meta.glob before specifier rewriting (eager globs become
    // static imports the factory transform then handles).
    if source_text.contains("import.meta.glob") {
        crate::glob::expand(&allocator, path.parent().unwrap_or(path), &mut program);
    }

    // Canonicalize specifiers to urls (shared with unbundled mode).
    let _ = crate::rewrite_module_specifiers_pub(&allocator, &mut program, resolve);

    // Fresh semantic over the transformed program for reference resolution.
    // `with_build_nodes`: reference node-ids must be resolvable to spans.
    let semantic = SemanticBuilder::new().with_build_nodes(true).build(&program).semantic;

    // ---- plan imports/exports ------------------------------------------
    let mut import_vars: Vec<String> = Vec::new(); // url per _oj_mN
    let mut var_of_url: HashMap<String, usize> = HashMap::new();
    let mut replacements: HashMap<ReferenceId, Replacement> = HashMap::new();
    let mut getters: Vec<(String, String)> = Vec::new(); // exported name to getter body
    let mut stars: Vec<usize> = Vec::new();
    let mut has_default_expr = false;

    let mut var_for = |url: &str, import_vars: &mut Vec<String>| -> usize {
        if let Some(&i) = var_of_url.get(url) {
            return i;
        }
        let i = import_vars.len();
        import_vars.push(url.to_string());
        var_of_url.insert(url.to_string(), i);
        i
    };

    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                let target = decl.source.value.as_str().to_string();
                let vi = var_for(&target, &mut import_vars);
                for spec in decl.specifiers.iter().flatten() {
                    let (local, member) = match spec {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => (
                            &s.local,
                            Some(match &s.imported {
                                ModuleExportName::IdentifierName(n) => n.name.to_string(),
                                ModuleExportName::IdentifierReference(n) => n.name.to_string(),
                                ModuleExportName::StringLiteral(s) => s.value.to_string(),
                            }),
                        ),
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            (&s.local, Some("default".to_string()))
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => (&s.local, None),
                    };
                    let Some(symbol_id) = local.symbol_id.get() else { continue };
                    for &reference_id in
                        semantic.scoping().get_resolved_reference_ids(symbol_id)
                    {
                        // Key by ReferenceId; spans collide after transforms.
                        replacements.insert(
                            reference_id,
                            Replacement { var: format!("_oj_m{vi}"), member: member.clone() },
                        );
                    }
                }
            }
            Statement::ExportDeclaration(decl) => {
                for name in binding_names(&decl.declaration) {
                    getters.push((name.clone(), name));
                }
            }
            Statement::ExportNamedDeclaration(decl) => {
                for spec in &decl.specifiers {
                    let local = export_name(&spec.local);
                    let exported = export_name(&spec.exported);
                    getters.push((exported, local));
                }
            }
            Statement::ExportFromDeclaration(decl) => {
                let vi = var_for(decl.source.value.as_str(), &mut import_vars);
                for spec in &decl.specifiers {
                    let local = export_name(&spec.local);
                    let exported = export_name(&spec.exported);
                    getters.push((exported, format!("_oj_m{vi}.{local}")));
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                stars.push(var_for(decl.source.value.as_str(), &mut import_vars));
            }
            Statement::ExportDefaultDeclaration(decl) => {
                use oxc_ast::ast::ExportDefaultDeclarationKind as K;
                match &decl.declaration {
                    K::FunctionDeclaration(f) if f.id.is_some() => {
                        getters.push(("default".into(), f.id.as_ref().unwrap().name.to_string()));
                    }
                    K::ClassDeclaration(c) if c.id.is_some() => {
                        getters.push(("default".into(), c.id.as_ref().unwrap().name.to_string()));
                    }
                    _ => {
                        has_default_expr = true;
                        getters.push(("default".into(), "__oj_default".into()));
                    }
                }
            }
            _ => {}
        }
    }
    drop(semantic);

    // ---- rewrite references + import.meta ------------------------------
    let mut rewriter = RefRewriter { allocator: &allocator, replacements: &replacements, url };
    rewriter.visit_program(&mut program);

    // ---- statement surgery ----------------------------------------------
    let old_body = std::mem::replace(&mut program.body, oxc_allocator::Vec::new_in(&&allocator));
    let mut new_body = oxc_allocator::Vec::new_in(&&allocator);

    // Prologue: getters first (circular-safety), then dep requires, stars.
    let mut prologue = String::new();
    if has_default_expr {
        prologue.push_str("var __oj_default;\n");
    }
    if !getters.is_empty() {
        let entries: Vec<String> =
            getters.iter().map(|(name, expr)| format!("{name:?}: () => {expr}")).collect();
        prologue.push_str(&format!("__oj_esm(__oj_exports, {{ {} }});\n", entries.join(", ")));
    } else {
        prologue.push_str("__oj_esm(__oj_exports, {});\n");
    }
    for (i, target) in import_vars.iter().enumerate() {
        prologue.push_str(&format!("var _oj_m{i} = __oj_require({target:?});\n"));
    }
    for vi in &stars {
        prologue.push_str(&format!("__oj_export_star(_oj_m{vi}, __oj_exports);\n"));
    }
    for stmt in parse_snippet(&allocator, &prologue, path)? {
        new_body.push(stmt);
    }

    for stmt in old_body {
        match stmt {
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportAllDeclaration(_) => {} // handled in prologue
            Statement::ExportDeclaration(decl) => {
                // Keep the inner declaration; getters already point at it.
                new_body.push(Statement::from(decl.unbox().declaration));
            }
            Statement::ExportDefaultDeclaration(decl) => {
                use oxc_ast::ast::ExportDefaultDeclarationKind as K;
                match decl.unbox().declaration {
                    K::FunctionDeclaration(f) if f.id.is_some() => {
                        new_body.push(Statement::FunctionDeclaration(f));
                    }
                    K::ClassDeclaration(c) if c.id.is_some() => {
                        new_body.push(Statement::ClassDeclaration(c));
                    }
                    K::FunctionDeclaration(f) => {
                        push_default_assignment(
                            &allocator,
                            &mut new_body,
                            Expression::FunctionExpression(f),
                            path,
                        )?;
                    }
                    K::ClassDeclaration(c) => {
                        push_default_assignment(
                            &allocator,
                            &mut new_body,
                            Expression::ClassExpression(c),
                            path,
                        )?;
                    }
                    kind => {
                        // Only expression variants remain after the arms above.
                        push_default_assignment(
                            &allocator,
                            &mut new_body,
                            kind.into_expression(),
                            path,
                        )?;
                    }
                }
            }
            other => new_body.push(other),
        }
    }
    program.body = new_body;

    let code = Codegen::new().build(&program).code;
    Ok(FactoryOutput { code, imports: import_vars, require_map: Vec::new(), kind: FactoryKind::Esm })
}

/// `__oj_default = <placeholder>` with the real expression transplanted in.
fn push_default_assignment<'a>(
    allocator: &'a Allocator,
    body: &mut oxc_allocator::Vec<'a, Statement<'a>>,
    expr: Expression<'a>,
    path: &Path,
) -> Result<(), CompileError> {
    let mut stmts = parse_snippet(allocator, "__oj_default = 0;", path)?;
    let mut stmt = stmts.pop().expect("snippet has one statement");
    if let Statement::ExpressionStatement(es) = &mut stmt {
        if let Expression::AssignmentExpression(assign) = &mut es.expression {
            assign.right = expr;
        }
    }
    body.push(stmt);
    Ok(())
}

fn parse_snippet<'a>(
    allocator: &'a Allocator,
    source: &str,
    path: &Path,
) -> Result<Vec<Statement<'a>>, CompileError> {
    // Snippet text must live in the arena: the AST borrows string atoms.
    let source: &'a str = allocator.alloc_str(source);
    let parsed = Parser::new(allocator, source, SourceType::cjs()).parse();
    if parsed.panicked {
        return Err(CompileError::Transform {
            path: path.to_path_buf(),
            message: format!("internal: snippet failed to parse: {source}"),
        });
    }
    Ok(parsed.program.body.into_iter().collect())
}

pub(crate) fn export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(n) => n.name.to_string(),
        ModuleExportName::IdentifierReference(n) => n.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

pub(crate) fn binding_names(declaration: &oxc_ast::ast::Declaration) -> Vec<String> {
    use oxc_ast::ast::Declaration as D;
    let mut names = Vec::new();
    match declaration {
        D::VariableDeclaration(var) => {
            for declarator in &var.declarations {
                declarator.id.bound_names(&mut |ident| names.push(ident.name.to_string()));
            }
        }
        D::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                names.push(id.name.to_string());
            }
        }
        D::ClassDeclaration(c) => {
            if let Some(id) = &c.id {
                names.push(id.name.to_string());
            }
        }
        _ => {}
    }
    names
}

struct RefRewriter<'a, 'b> {
    allocator: &'a Allocator,
    replacements: &'b HashMap<ReferenceId, Replacement>,
    url: &'b str,
}

impl<'a> RefRewriter<'a, '_> {
    fn parse_expression(&self, source: &str) -> Option<Expression<'a>> {
        let source: &'a str = self.allocator.alloc_str(source);
        let parsed = Parser::new(self.allocator, source, SourceType::cjs()).parse();
        match parsed.program.body.into_iter().next() {
            Some(Statement::ExpressionStatement(es)) => Some(es.unbox().expression),
            _ => None,
        }
    }
}

impl<'a> VisitMut<'a> for RefRewriter<'a, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        let replacement_src = match &*expr {
            Expression::Identifier(ident) => ident
                .reference_id
                .get()
                .and_then(|id| self.replacements.get(&id))
                .map(|r| match &r.member {
                    Some(member) => format!("{}.{}", r.var, member),
                    None => r.var.clone(),
                }),
            // import.meta.url becomes the module's url as a string
            Expression::StaticMemberExpression(member)
                if matches!(member.object, Expression::ImportMeta(_))
                    && member.property.name == "url" =>
            {
                // Parenthesized: a lone string literal statement would parse
                // as a directive and yield an empty body.
                Some(format!("({:?})", self.url))
            }
            // import.meta.hot becomes module.hot (provided by the runtime)
            Expression::StaticMemberExpression(member)
                if matches!(member.object, Expression::ImportMeta(_))
                    && member.property.name == "hot" =>
            {
                Some("module.hot".to_string())
            }
            _ => None,
        };
        if let Some(src) = replacement_src {
            if let Some(new_expr) = self.parse_expression(&src) {
                *expr = new_expr;
            }
            return;
        }
        walk_mut::walk_expression(self, expr);
    }

    fn visit_object_property(&mut self, prop: &mut oxc_ast::ast::ObjectProperty<'a>) {
        // `{ A }` shorthand where A is imported must become `{ A: _oj_m0.A }`.
        if prop.shorthand {
            if let Expression::Identifier(ident) = &prop.value {
                let replaced = ident
                    .reference_id
                    .get()
                    .is_some_and(|id| self.replacements.contains_key(&id));
                if replaced {
                    prop.shorthand = false;
                }
            }
        }
        walk_mut::walk_object_property(self, prop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn factory(src: &str) -> FactoryOutput {
        let mut resolve = |spec: &str| -> Option<String> {
            (spec.starts_with('.')).then(|| format!("/src{}.tsx", spec.trim_start_matches('.')))
        };
        compile_esm_factory(Path::new("Mod.tsx"), "/src/Mod.tsx", src, &mut resolve, true).unwrap()
    }

    #[test]
    fn imports_become_requires_with_member_rewriting() {
        let out = factory(
            r#"
import { useState } from "react";
import Def, { helper as h } from "./util";
import * as NS from "./ns";
export function Widget() {
  const [n] = useState(0);
  return h(Def, NS.thing, n, { h });
}
"#,
        );
        let code = &out.code;
        assert!(code.contains(r#"var _oj_m0 = __oj_require("react")"#), "{code}");
        assert!(code.contains("_oj_m0.useState(0)"), "{code}");
        assert!(code.contains("_oj_m1.helper(_oj_m1.default, _oj_m2.thing"), "{code}");
        assert!(code.contains("h: _oj_m1.helper"), "shorthand must expand: {code}");
        assert!(!code.contains("import "), "{code}");
    }

    #[test]
    fn shadowed_names_are_not_rewritten() {
        let out = factory(
            r#"
import { x } from "./a";
export function f(x) { return x + 1; }
export const y = x;
"#,
        );
        assert!(out.code.contains("return x + 1"), "param x must stay: {}", out.code);
        assert!(out.code.contains("const y = _oj_m0.x"), "{}", out.code);
    }

    #[test]
    fn exports_become_getters_installed_before_body() {
        let out = factory(
            r#"
export const a = 1;
export default function App() { return null; }
export { a as b };
export * from "./other";
export { c } from "./third";
"#,
        );
        let code = &out.code;
        let esm_at = code.find("__oj_esm").unwrap();
        let body_at = code.find("const a = 1").unwrap();
        assert!(esm_at < body_at, "getters must be installed before the body: {code}");
        for expected in
            [r#""a": () => a"#, r#""default": () => App"#, r#""b": () => a"#, "__oj_export_star(", r#""c": () => _oj_m"#]
        {
            assert!(code.contains(expected), "missing {expected:?} in: {code}");
        }
        assert!(code.contains("function App()"), "hoisted decl kept: {code}");
    }

    #[test]
    fn anonymous_default_export_is_assigned() {
        let out = factory(r#"export default () => 42;"#);
        assert!(out.code.contains("__oj_default = () => 42"), "{}", out.code);
        assert!(out.code.contains(r#""default": () => __oj_default"#), "{}", out.code);
    }

    #[test]
    fn side_effect_imports_still_require() {
        let out = factory(r#"import "./global-setup"; export const x = 1;"#);
        assert!(out.code.contains(r#"__oj_require("/src/global-setup.tsx")"#), "{}", out.code);
    }

    #[test]
    fn cjs_factory_keeps_body_and_maps_requires() {
        let mut resolve = |spec: &str| Some(format!("/node_modules/{spec}/index.js"));
        let out = compile_cjs_factory(
            Path::new("x.js"),
            "var r = require('react'); exports.go = () => r;",
            &mut resolve,
        )
        .unwrap();
        assert_eq!(out.kind, FactoryKind::Cjs);
        assert_eq!(out.require_map, vec![("react".into(), "/node_modules/react/index.js".into())]);
        assert!(out.code.contains("require('react')") || out.code.contains("require(\"react\")"));
        assert!(!out.is_boundary(), "cjs never a refresh boundary");
    }

    #[test]
    fn import_meta_and_refresh_are_factory_safe() {
        let out = factory(
            r#"
export function Thing() { return import.meta.url; }
"#,
        );
        assert!(out.code.contains(r#"return "/src/Mod.tsx""#), "{}", out.code);
        assert!(out.is_boundary(), "component module registers refresh");
    }
}
