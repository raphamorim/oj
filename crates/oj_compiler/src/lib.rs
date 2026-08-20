// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

pub mod bundle;
pub mod cjs;
pub mod glob;
pub mod interop;
pub mod json;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use memchr::memmem::Finder;
use oxc_allocator::Allocator;
use oxc_ast::ast::{Program, Statement, StringLiteral};
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxRuntime, ReactRefreshOptions, TransformOptions, Transformer};
use oxc_transformer_plugins::{ReplaceGlobalDefines, ReplaceGlobalDefinesConfig};

pub type ImportRewriter<'r> = dyn FnMut(&str) -> Option<String> + 'r;

/// Stack size for any thread that compiles a module.
///
/// Parsing, transforming and printing are all recursive descent: one frame per
/// level of bracket nesting. The 2 MiB a runtime thread gets by default runs
/// out somewhere under a thousand levels, and an overflow aborts the process
/// rather than failing the one file -- a generated or minified module can take
/// the whole dev server down with it. Hand-written source stays two orders of
/// magnitude below this; generated code does not.
pub const COMPILE_STACK_SIZE: usize = 16 * 1024 * 1024;

static F_IMPORT_META_ENV: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new("import.meta.env"));
static F_IMPORT_META_GLOB: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new("import.meta.glob"));

pub(crate) fn detect_refresh_registrations(program: &Program) -> bool {
    use oxc_ast::ast::{CallExpression, Expression};
    use oxc_ast_visit::{walk, Visit};

    struct Detector {
        found: bool,
    }
    impl<'a> Visit<'a> for Detector {
        fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
            if self.found {
                return;
            }
            if let Expression::Identifier(id) = &call.callee {
                if id.name == "$RefreshReg$" {
                    self.found = true;
                    return;
                }
            }
            walk::walk_call_expression(self, call);
        }
    }
    let mut detector = Detector { found: false };
    detector.visit_program(program);
    detector.found
}

static ENV_DEFINES: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

pub fn set_import_meta_env(defines: Vec<(String, String)>) {
    let _ = ENV_DEFINES.set(defines);
}

pub(crate) fn import_meta_env_defines(dev: bool) -> Vec<(String, String)> {
    if let Some(defines) = ENV_DEFINES.get() {
        return defines.clone();
    }
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
    pub dev: bool,
    pub refresh: bool,
    pub sourcemap: bool,
}

impl CompileOptions {
    pub fn dev() -> Self {
        Self {
            dev: true,
            refresh: true,
            sourcemap: true,
        }
    }

    pub fn prod() -> Self {
        Self {
            dev: false,
            refresh: false,
            sourcemap: true,
        }
    }
}

#[derive(Debug)]
pub struct CompileOutput {
    pub code: String,
    pub map_data_url: Option<String>,
    pub imports: Vec<String>,
    pub dynamic_imports: Vec<String>,
    pub is_refresh_boundary: bool,
}

impl CompileOutput {
    pub fn code_with_inline_map(&self) -> String {
        match &self.map_data_url {
            Some(url) => format!("{}\n//# sourceMappingURL={}\n", self.code, url),
            None => self.code.clone(),
        }
    }

    pub fn has_refresh_registrations(&self) -> bool {
        self.is_refresh_boundary
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

pub fn exports(source_text: &str, path: &Path) -> Vec<String> {
    let Ok(source_type) = SourceType::from_path(path) else {
        return Vec::new();
    };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    let mut names = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ExportDeclaration(decl) => {
                names.extend(bundle::binding_names(&decl.declaration));
            }
            Statement::ExportNamedDeclaration(decl) => {
                for spec in &decl.specifiers {
                    names.push(bundle::export_name(&spec.exported));
                }
            }
            Statement::ExportFromDeclaration(decl) => {
                for spec in &decl.specifiers {
                    names.push(bundle::export_name(&spec.exported));
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                if let Some(exported) = &decl.exported {
                    names.push(bundle::export_name(exported));
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
        return Err(CompileError::Parse {
            path: path.to_path_buf(),
            message,
        });
    }
    let mut program = parsed.program;

    let semantic_ret = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        // Required by the TypeScript transform to lower `enum`: without it the
        // transformer panics rather than reporting a diagnostic.
        .with_enum_eval(true)
        .build(&program);
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
        return Err(CompileError::Transform {
            path: path.to_path_buf(),
            message,
        });
    }

    let defines = import_meta_env_defines(opts.dev);
    let needs_defines = F_IMPORT_META_ENV.find(source_text.as_bytes()).is_some()
        || defines
            .iter()
            .any(|(k, _)| !k.starts_with("import.meta") && source_text.contains(k.as_str()));
    if needs_defines {
        if let Ok(config) = ReplaceGlobalDefinesConfig::new(&defines) {
            let _ = ReplaceGlobalDefines::new(&allocator, config)
                .build(transform_ret.scoping, &mut program);
        }
    }

    // Track synthesized expansions: they splice generated nodes with no faithful
    // source origin, so their sourcemap must be skipped (see codegen below).
    let mut synthesized = false;
    if F_IMPORT_META_GLOB.find(source_text.as_bytes()).is_some() {
        let dir = path.parent().unwrap_or(path);
        glob::expand(&allocator, dir, &mut program);
        synthesized = true;
    }

    // Expand dynamic-import-vars (import(`./x/${v}.js`)) in dev too — the build
    // path already does; without it these work in `oj build` but throw in dev.
    if source_text.contains("import(") {
        let dir = path.parent().unwrap_or(path);
        synthesized |= glob::expand_dynamic_import_vars(&allocator, dir, &mut program, source_text);
    }

    // new URL("./asset", import.meta.url) -> a hoisted ?url asset import.
    if source_text.contains("import.meta.url") {
        synthesized |= glob::expand_new_url_asset(&allocator, &mut program);
    }

    let (imports, dynamic_imports) =
        rewrite_module_specifiers(&allocator, &mut program, &mut rewriter);

    let is_refresh_boundary = opts.refresh && detect_refresh_registrations(&program);

    // Synthesized nodes (glob / dynamic-import-vars) carry generated-string spans
    // with no origin in this module's source; sourcemapping them panics oxc's
    // builder on out-of-range spans, so skip the map for those modules.
    let codegen_options = CodegenOptions {
        source_map_path: (opts.sourcemap && !synthesized).then(|| path.to_path_buf()),
        ..CodegenOptions::default()
    };
    let CodegenReturn { code, map, .. } =
        Codegen::new().with_options(codegen_options).build(&program);

    Ok(CompileOutput {
        code,
        map_data_url: map.map(|m| m.to_data_url()),
        imports,
        dynamic_imports,
        is_refresh_boundary,
    })
}

pub(crate) fn rewrite_module_specifiers_pub<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut ImportRewriter,
) -> (Vec<String>, Vec<String>) {
    let mut opt: Option<&mut ImportRewriter> = Some(rewriter);
    rewrite_module_specifiers(allocator, program, &mut opt)
}

fn rewrite_module_specifiers<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> (Vec<String>, Vec<String>) {
    let mut imports = Vec::new();
    let mut dynamic_imports = Vec::new();
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
    if let Some(rewriter) = rewriter.as_deref_mut() {
        let mut dyn_rewriter = DynamicImportRewriter {
            allocator,
            rewriter,
            dynamic: &mut dynamic_imports,
        };
        use oxc_ast_visit::VisitMut;
        dyn_rewriter.visit_program(program);
    }
    (imports, dynamic_imports)
}

struct DynamicImportRewriter<'a, 'b> {
    allocator: &'a Allocator,
    rewriter: &'b mut ImportRewriter<'b>,
    dynamic: &'b mut Vec<String>,
}

impl<'a> oxc_ast_visit::VisitMut<'a> for DynamicImportRewriter<'a, '_> {
    fn visit_import_expression(&mut self, it: &mut oxc_ast::ast::ImportExpression<'a>) {
        if let oxc_ast::ast::Expression::StringLiteral(lit) = &mut it.source {
            if let Some(new_spec) = (self.rewriter)(lit.value.as_str()) {
                lit.value = self.allocator.alloc_str(&new_spec).into();
                lit.raw = None;
            }
            self.dynamic.push(lit.value.to_string());
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
        assert_eq!(
            names,
            ["Thing", "default", "getUser", "listUsers", "x", "y"]
        );
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
        let out = compile(Path::new("App.tsx"), APP_TSX, &CompileOptions::prod()).unwrap();
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
            spec.starts_with('.')
                .then(|| format!("/resolved{}", spec.trim_start_matches('.')))
        };
        let out = compile_module(
            Path::new("Root.tsx"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        assert!(out.code.contains("\"/resolved/App\""), "{}", out.code);
        assert!(
            out.code.contains("\"/resolved/lib/helper\""),
            "{}",
            out.code
        );
        assert!(
            out.code.contains("\"react\""),
            "bare imports stay untouched"
        );
        assert!(out.imports.contains(&"/resolved/App".to_string()));
        assert!(out.imports.contains(&"react".to_string()));
        assert!(
            out.imports.iter().any(|i| i.contains("jsx-runtime")),
            "{:?}",
            out.imports
        );
    }

    #[test]
    fn reports_parse_errors_instead_of_panicking() {
        let err = compile(
            Path::new("Broken.tsx"),
            "const = <div>;",
            &CompileOptions::dev(),
        )
        .unwrap_err();
        assert!(matches!(err, CompileError::Parse { .. }));
    }

    #[test]
    fn replaces_import_meta_env_flags_per_mode() {
        let src = "export const mode = import.meta.env.MODE;\n\
                   export const dev = import.meta.env.DEV;\n\
                   export const prod = import.meta.env.PROD;";
        let prod = compile(Path::new("env.ts"), src, &CompileOptions::prod()).unwrap();
        assert!(
            !prod.code.contains("import.meta.env"),
            "defines must be replaced:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("\"production\""),
            "MODE is production:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("prod = true"),
            "PROD is true in prod:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("dev = false"),
            "DEV is false in prod:\n{}",
            prod.code
        );

        let dev = compile(Path::new("env.ts"), src, &CompileOptions::dev()).unwrap();
        assert!(
            dev.code.contains("\"development\""),
            "MODE is development:\n{}",
            dev.code
        );
        assert!(
            dev.code.contains("dev = true"),
            "DEV is true in dev:\n{}",
            dev.code
        );
        assert!(
            dev.code.contains("prod = false"),
            "PROD is false in dev:\n{}",
            dev.code
        );
    }

    #[test]
    fn dev_compile_expands_dynamic_import_vars() {
        // import(`./x/${v}.js`) must expand in the DEV compile path too — the
        // build path already does, so without this it works in `oj build` and
        // throws at runtime under `oj dev`.
        let dir = std::env::temp_dir().join(format!("oj-dynimport-{}", std::process::id()));
        let loc = dir.join("locales");
        std::fs::create_dir_all(&loc).unwrap();
        std::fs::write(loc.join("en.json"), "{}").unwrap();
        std::fs::write(loc.join("fr.json"), "{}").unwrap();
        let src = "export const load = (l) => import(`./locales/${l}.json`);\n";
        let out = compile(&dir.join("main.js"), src, &CompileOptions::dev()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.code.contains("./locales/en.json"),
            "dyn-import-var must expand in dev:\n{}",
            out.code
        );
        assert!(out.code.contains("./locales/fr.json"), "{}", out.code);
        assert!(
            out.code.contains("case "),
            "expanded to a switch over matches:\n{}",
            out.code
        );
    }

    #[test]
    fn dev_compile_rewrites_new_url_import_meta_url() {
        // new URL("./x", import.meta.url) -> a hoisted ?url asset import ref, so
        // the asset flows through oj's pipeline instead of 404-ing.
        let src = "export const w = new URL(\"./worker.js\", import.meta.url);\n";
        let out = compile(std::path::Path::new("m.js"), src, &CompileOptions::dev()).unwrap();
        assert!(
            out.code.contains("__oj_url_0"),
            "rewritten to hoisted asset ref:\n{}",
            out.code
        );
        assert!(
            out.code.contains("worker.js?url"),
            "hoisted ?url import:\n{}",
            out.code
        );
        assert!(
            !out.code.contains("new URL(\"./worker.js\""),
            "original literal replaced:\n{}",
            out.code
        );
    }

    #[test]
    fn erases_type_only_imports_from_code_and_collected_imports() {
        let src = r#"
import type { A } from "./types";
import { type B, c } from "./mixed";
import { d } from "./real";
export const used: A extends B ? number : number = c + d;
"#;
        let out = compile(Path::new("m.ts"), src, &CompileOptions::prod()).unwrap();
        assert!(
            !out.imports.iter().any(|i| i.contains("types")),
            "type-only import erased: {:?}",
            out.imports
        );
        assert!(
            !out.code.contains("./types"),
            "type-only source gone:\n{}",
            out.code
        );
        assert!(
            out.imports.iter().any(|i| i.contains("mixed")),
            "mixed import kept: {:?}",
            out.imports
        );
        assert!(
            !out.code.contains("type B"),
            "inline type specifier erased:\n{}",
            out.code
        );
        assert!(out.imports.iter().any(|i| i.contains("real")));
    }

    #[test]
    fn rewrites_dynamic_import_specifiers() {
        let src = r#"export async function load() { return import("./chunk"); }"#;
        let mut rewrite = |s: &str| -> Option<String> {
            s.starts_with('.')
                .then(|| format!("/res{}", s.trim_start_matches('.')))
        };
        let out = compile_module(
            Path::new("d.ts"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        assert!(
            out.code.contains("import(\"/res/chunk\")"),
            "dynamic import rewritten:\n{}",
            out.code
        );
        assert!(
            out.dynamic_imports.contains(&"/res/chunk".to_string()),
            "dynamic spec collected: {:?}",
            out.dynamic_imports
        );
        assert!(
            !out.imports.contains(&"/res/chunk".to_string()),
            "dynamic not in static imports"
        );
    }

    #[test]
    fn fast_refresh_only_in_dev_with_refresh_enabled() {
        let prod = compile(Path::new("C.tsx"), APP_TSX, &CompileOptions::prod()).unwrap();
        assert!(!prod.has_refresh_registrations());
        let dev_no_refresh = compile_module(
            Path::new("C.tsx"),
            APP_TSX,
            &CompileOptions {
                dev: true,
                refresh: false,
                sourcemap: false,
            },
            None,
        )
        .unwrap();
        assert!(!dev_no_refresh.has_refresh_registrations());
        assert!(
            dev_no_refresh.code.contains("jsx-dev-runtime"),
            "dev runtime regardless of refresh"
        );
    }

    #[test]
    fn rejects_unsupported_file_types() {
        let err = compile(Path::new("styles.css"), "body{}", &CompileOptions::prod()).unwrap_err();
        assert!(
            matches!(err, CompileError::UnsupportedFileType(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn sourcemap_toggle_and_inline_map_helper() {
        let no_map = compile_module(
            Path::new("a.ts"),
            "export const x = 1;",
            &CompileOptions {
                dev: false,
                refresh: false,
                sourcemap: false,
            },
            None,
        )
        .unwrap();
        assert!(no_map.map_data_url.is_none());
        assert_eq!(no_map.code_with_inline_map(), no_map.code);

        let with_map = compile(
            Path::new("a.ts"),
            "export const x = 1;",
            &CompileOptions::prod(),
        )
        .unwrap();
        assert!(with_map.map_data_url.is_some());
        assert!(with_map
            .code_with_inline_map()
            .contains("sourceMappingURL="));
    }

    #[test]
    fn exports_handles_reexports_and_never_panics() {
        let mut local = exports(
            r#"export const a = 1; export { a as b };"#,
            Path::new("m.ts"),
        );
        local.sort();
        assert_eq!(local, ["a", "b"]);
        assert!(exports("export { = ;", Path::new("bad.ts")).is_empty());
        assert!(exports("body{}", Path::new("x.css")).is_empty());
    }

    #[test]
    fn exports_captures_reexport_from_and_namespace_star() {
        let mut names = exports(r#"export { a, b as c } from "./mod";"#, Path::new("m.ts"));
        names.sort();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(
            exports(r#"export * as ns from "./mod";"#, Path::new("m.ts")),
            ["ns"]
        );
        assert!(exports(r#"export * from "./mod";"#, Path::new("m.ts")).is_empty());
        let mut mixed = exports(
            r#"export default function () {}
export { x } from "./a";
export * as z from "./b";"#,
            Path::new("m.ts"),
        );
        mixed.sort();
        assert_eq!(mixed, ["default", "x", "z"]);
    }
}
