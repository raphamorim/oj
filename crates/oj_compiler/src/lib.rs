// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The fused per-file compile pipeline.
//!
//! One arena, one parse, one fused transform pass-set, one codegen:
//! TSX/TS/JSX -> strip types -> JSX (automatic runtime) -> React Fast Refresh
//! instrumentation (dev) -> JS + sourcemap.
//!
//! This is the hot path of the whole tool. Everything here must stay
//! allocation-conscious and must never re-parse between stages — that
//! single-parse property is the speed edge over plugin-pipeline designs.

pub mod bundle;
pub mod cjs;
pub mod glob;
pub mod json;

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{Program, Statement, StringLiteral};
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxRuntime, ReactRefreshOptions, TransformOptions, Transformer};
use oxc_transformer_plugins::{ReplaceGlobalDefines, ReplaceGlobalDefinesConfig};

/// Maps an import specifier to a replacement (e.g. `./App` -> `/src/App.tsx`).
/// Returning `None` leaves the specifier untouched.
pub type ImportRewriter<'r> = dyn FnMut(&str) -> Option<String> + 'r;

/// `import.meta.env.*` define pairs, set once at dev/build startup from the
/// app's `.env` files. Unset (e.g. in unit tests) falls back to built-ins.
static ENV_DEFINES: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Install the env defines for this process (call once, before compiling).
pub fn set_import_meta_env(defines: Vec<(String, String)>) {
    let _ = ENV_DEFINES.set(defines);
}

pub(crate) fn import_meta_env_defines(dev: bool) -> Vec<(String, String)> {
    if let Some(defines) = ENV_DEFINES.get() {
        return defines.clone();
    }
    // Built-in fallback: no .env loaded (unit tests, minimal usage).
    let mode = if dev { "development" } else { "production" };
    vec![
        ("import.meta.env.BASE_URL".into(), "\"/\"".into()),
        ("import.meta.env.MODE".into(), format!("\"{mode}\"")),
        ("import.meta.env.DEV".into(), dev.to_string()),
        ("import.meta.env.PROD".into(), (!dev).to_string()),
        ("import.meta.env.SSR".into(), "false".into()),
        (
            "import.meta.env".into(),
            format!(
                "({{\"BASE_URL\":\"/\",\"MODE\":\"{mode}\",\"DEV\":{dev},\"PROD\":{prod},\"SSR\":false}})",
                prod = !dev
            ),
        ),
    ]
}

#[derive(Debug, Clone)]
pub struct CompileOptions {
    /// Dev mode: jsxDEV runtime, no pure annotations needed for shaking yet.
    pub dev: bool,
    /// Instrument components for React Fast Refresh ($RefreshReg$/$RefreshSig$).
    /// Only meaningful in dev.
    pub refresh: bool,
    /// Emit a sourcemap alongside the code.
    pub sourcemap: bool,
}

impl CompileOptions {
    pub fn dev() -> Self {
        Self { dev: true, refresh: true, sourcemap: true }
    }

    pub fn prod() -> Self {
        Self { dev: false, refresh: false, sourcemap: true }
    }
}

#[derive(Debug)]
pub struct CompileOutput {
    pub code: String,
    /// Sourcemap as a `data:` URL, ready to append as `//# sourceMappingURL=`.
    pub map_data_url: Option<String>,
    /// Final specifiers of static imports and re-exports, post-rewrite.
    /// Type-only imports are already erased and never appear here.
    pub imports: Vec<String>,
}

impl CompileOutput {
    /// Code with the sourcemap inlined, for dev serving.
    pub fn code_with_inline_map(&self) -> String {
        match &self.map_data_url {
            Some(url) => format!("{}\n//# sourceMappingURL={}\n", self.code, url),
            None => self.code.clone(),
        }
    }

    /// Whether the Fast Refresh transform registered any components here.
    /// Modules where this is true are HMR boundary candidates.
    pub fn has_refresh_registrations(&self) -> bool {
        self.code.contains("$RefreshReg$(")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),
    #[error("parse error in {path}:\n{message}")]
    Parse { path: PathBuf, message: String },
    #[error("transform error in {path}:\n{message}")]
    Transform { path: PathBuf, message: String },
}

pub fn compile(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
) -> Result<CompileOutput, CompileError> {
    compile_module(path, source_text, opts, None)
}

/// The module's exported binding names (named exports and re-export specifiers;
/// `default` when a default export is present). Used to generate client stubs
/// for server-only (`*.server.*`) modules. Returns empty on a parse failure.
pub fn exports(source_text: &str, path: &Path) -> Vec<String> {
    let Ok(source_type) = SourceType::from_path(path) else { return Vec::new() };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    let mut names = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            // `export const/function/class ...`
            Statement::ExportDeclaration(decl) => {
                names.extend(bundle::binding_names(&decl.declaration));
            }
            // `export { a, b as c }`
            Statement::ExportNamedDeclaration(decl) => {
                for spec in &decl.specifiers {
                    names.push(bundle::export_name(&spec.exported));
                }
            }
            Statement::ExportDefaultDeclaration(_) => names.push("default".to_string()),
            _ => {}
        }
    }
    names
}

pub fn compile_module(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
    mut rewriter: Option<&mut ImportRewriter>,
) -> Result<CompileOutput, CompileError> {
    let source_type = SourceType::from_path(path)
        .map_err(|_| CompileError::UnsupportedFileType(path.to_path_buf()))?;

    let allocator = Allocator::default();

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

    // Transformer needs scoping info; the transformer roughly triples scope counts.
    let semantic_ret = SemanticBuilder::new().with_excess_capacity(2.0).build(&program);
    let scoping = semantic_ret.semantic.into_scoping();

    let mut transform_options = TransformOptions::default();
    transform_options.jsx.jsx_plugin = true;
    transform_options.jsx.runtime = JsxRuntime::Automatic;
    transform_options.jsx.development = opts.dev;
    transform_options.jsx.jsx_self_plugin = opts.dev;
    transform_options.jsx.jsx_source_plugin = opts.dev;
    if opts.dev && opts.refresh {
        transform_options.jsx.refresh = Some(ReactRefreshOptions::default());
    }

    let transform_ret = Transformer::new(&allocator, path, &transform_options)
        .build_with_scoping(scoping, &mut program);
    if !transform_ret.diagnostics.is_empty() {
        let message = transform_ret
            .diagnostics
            .into_iter()
            .map(|d| format!("{:?}", d.with_source_code(source_text.to_string())))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileError::Transform { path: path.to_path_buf(), message });
    }

    // Vite-compatible static define replacement (import.meta.env plus any
    // config/environment `define`). Gated on a cheap substring test so the
    // common module pays nothing: run if the source references import.meta.env
    // or any non-import.meta defined name (e.g. a `__FLAG__` global define).
    let defines = import_meta_env_defines(opts.dev);
    let needs_defines = source_text.contains("import.meta.env")
        || defines
            .iter()
            .any(|(k, _)| !k.starts_with("import.meta") && source_text.contains(k.as_str()));
    if needs_defines {
        let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
        if let Ok(config) = ReplaceGlobalDefinesConfig::new(&defines) {
            let _ = ReplaceGlobalDefines::new(&allocator, config).build(scoping, &mut program);
        }
    }

    // Expand import.meta.glob before specifier rewriting, so the generated
    // import()/import statements get canonicalized to URLs by the rewriter.
    if source_text.contains("import.meta.glob") {
        let dir = path.parent().unwrap_or(path);
        glob::expand(&allocator, dir, &mut program);
    }

    let imports = rewrite_module_specifiers(&allocator, &mut program, &mut rewriter);

    let codegen_options = CodegenOptions {
        source_map_path: opts.sourcemap.then(|| path.to_path_buf()),
        ..CodegenOptions::default()
    };
    let CodegenReturn { code, map, .. } =
        Codegen::new().with_options(codegen_options).build(&program);

    Ok(CompileOutput { code, map_data_url: map.map(|m| m.to_data_url()), imports })
}

/// Collect (and optionally rewrite) the source specifiers of all static
/// imports and re-exports. Runs after the transforms, so JSX-runtime imports
/// injected by the automatic runtime are included and type-only imports are
/// already gone.
///
/// TODO(M2): dynamic `import("...")` with literal arguments needs an AST
/// visitor pass; top-level statements are enough for milestone 1.
/// Exposed for bundle mode, which shares specifier canonicalization.
pub(crate) fn rewrite_module_specifiers_pub<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut ImportRewriter,
) -> Vec<String> {
    let mut opt: Option<&mut ImportRewriter> = Some(rewriter);
    rewrite_module_specifiers(allocator, program, &mut opt)
}

fn rewrite_module_specifiers<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> Vec<String> {
    let mut imports = Vec::new();
    for stmt in program.body.iter_mut() {
        let source: Option<&mut StringLiteral> = match stmt {
            Statement::ImportDeclaration(decl) => Some(&mut decl.source),
            Statement::ExportFromDeclaration(decl) => Some(&mut decl.source),
            Statement::ExportAllDeclaration(decl) => Some(&mut decl.source),
            _ => None,
        };
        let Some(lit) = source else { continue };

        if let Some(rewriter) = rewriter.as_deref_mut() {
            if let Some(new_spec) = rewriter(lit.value.as_str()) {
                lit.value = allocator.alloc_str(&new_spec).into();
                lit.raw = None;
            }
        }
        imports.push(lit.value.to_string());
    }
    // Dynamic import("literal") gets the same canonicalization (needed for
    // bare specifiers, which the browser cannot resolve).
    if let Some(rewriter) = rewriter.as_deref_mut() {
        let mut dyn_rewriter = DynamicImportRewriter { allocator, rewriter, imports: &mut imports };
        use oxc_ast_visit::VisitMut;
        dyn_rewriter.visit_program(program);
    }
    imports
}

struct DynamicImportRewriter<'a, 'b> {
    allocator: &'a Allocator,
    rewriter: &'b mut ImportRewriter<'b>,
    imports: &'b mut Vec<String>,
}

impl<'a> oxc_ast_visit::VisitMut<'a> for DynamicImportRewriter<'a, '_> {
    fn visit_import_expression(&mut self, it: &mut oxc_ast::ast::ImportExpression<'a>) {
        if let oxc_ast::ast::Expression::StringLiteral(lit) = &mut it.source {
            if let Some(new_spec) = (self.rewriter)(lit.value.as_str()) {
                lit.value = self.allocator.alloc_str(&new_spec).into();
                lit.raw = None;
            }
            self.imports.push(lit.value.to_string());
        }
        oxc_ast_visit::walk_mut::walk_import_expression(self, it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_lists_named_and_default() {
        let src = r#"
export const getUser = async (id) => ({ id });
export function listUsers() { return []; }
export class Thing {}
const x = 1;
export { x, x as y };
export default function () {}
"#;
        let mut names = exports(src, Path::new("api.server.ts"));
        names.sort();
        assert_eq!(names, ["Thing", "default", "getUser", "listUsers", "x", "y"]);
    }

    const APP_TSX: &str = r#"
interface Props { label: string }

export function Counter({ label }: Props) {
  const [n, setN] = React.useState<number>(0);
  return <button onClick={() => setN(n + 1)}>{label}: {n}</button>;
}

import React from "react";
"#;

    #[test]
    fn strips_types_and_uses_automatic_runtime_in_prod() {
        let out =
            compile(Path::new("App.tsx"), APP_TSX, &CompileOptions::prod()).unwrap();
        assert!(!out.code.contains("interface"), "types must be stripped");
        assert!(!out.code.contains("<button"), "JSX must be transformed");
        assert!(
            out.code.contains("react/jsx-runtime"),
            "prod uses the automatic runtime:\n{}",
            out.code
        );
        assert!(out.map_data_url.is_some());
    }

    #[test]
    fn dev_uses_jsx_dev_runtime_and_instruments_fast_refresh() {
        let out = compile(Path::new("App.tsx"), APP_TSX, &CompileOptions::dev()).unwrap();
        assert!(
            out.code.contains("react/jsx-dev-runtime"),
            "dev uses jsxDEV:\n{}",
            out.code
        );
        assert!(
            out.code.contains("$RefreshReg$"),
            "components must be registered for Fast Refresh:\n{}",
            out.code
        );
        assert!(
            out.code.contains("$RefreshSig$"),
            "hook users must be signed for Fast Refresh:\n{}",
            out.code
        );
    }

    #[test]
    fn rewrites_relative_specifiers_and_collects_imports() {
        let src = r#"
import { App } from "./App";
export { helper } from "../lib/helper";
import { useState } from "react";
export function Root() {
  const [n] = useState(0);
  return <App key={n} />;
}
"#;
        let mut rewrite = |spec: &str| -> Option<String> {
            spec.starts_with('.').then(|| format!("/resolved{}", spec.trim_start_matches('.')))
        };
        let out = compile_module(
            Path::new("Root.tsx"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        assert!(out.code.contains("\"/resolved/App\""), "{}", out.code);
        assert!(out.code.contains("\"/resolved/lib/helper\""), "{}", out.code);
        assert!(out.code.contains("\"react\""), "bare imports stay untouched");
        assert!(out.imports.contains(&"/resolved/App".to_string()));
        assert!(out.imports.contains(&"react".to_string()));
        // the automatic runtime import injected by the JSX transform is visible
        assert!(out.imports.iter().any(|i| i.contains("jsx-runtime")), "{:?}", out.imports);
    }

    #[test]
    fn reports_parse_errors_instead_of_panicking() {
        let err = compile(Path::new("Broken.tsx"), "const = <div>;", &CompileOptions::dev())
            .unwrap_err();
        assert!(matches!(err, CompileError::Parse { .. }));
    }
}
