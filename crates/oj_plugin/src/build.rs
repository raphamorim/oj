// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build adapter: one rolldown plugin that gives every registered
//! `pre_transform_ast` the same call it gets in dev. rolldown's `transform`
//! hook is string in, string out, so for a module some plugin's filter admits
//! this parses once with oxc, runs the registry's pass, prints with a source
//! map and hands the result back. Modules no plugin wants are not parsed.

use std::borrow::Cow;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_parser::Parser;
use oxc_span::SourceType;
use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookTransformArgs, HookTransformOutput, HookTransformOutputMap, HookTransformReturn, HookUsage,
    Plugin, SharedTransformPluginContext,
};

use crate::{AstCx, PluginMeta, SharedRegistry};

#[derive(Debug)]
pub struct AstPassPlugin {
    registry: SharedRegistry,
    ssr: bool,
    sourcemap: bool,
}

impl AstPassPlugin {
    pub fn new(registry: SharedRegistry, ssr: bool, sourcemap: bool) -> Self {
        Self { registry, ssr, sourcemap }
    }
}

/// rolldown's module type as the vocabulary the filters use, for the kinds
/// the pass can parse.
fn js_module_type(module_type: &ModuleType) -> Option<(&'static str, SourceType)> {
    match module_type {
        ModuleType::Js => Some(("js", SourceType::mjs())),
        ModuleType::Jsx => Some(("jsx", SourceType::jsx())),
        ModuleType::Ts => Some(("ts", SourceType::ts())),
        ModuleType::Tsx => Some(("tsx", SourceType::tsx())),
        _ => None,
    }
}

/// Parse, run the registry's pass, print. Shared by the hook below and by
/// anything else that has a source string rather than a compile pipeline.
/// Returns `None` when no plugin wanted the module.
pub fn run_pass(
    registry: &crate::Registry,
    path: &Path,
    source: &str,
    module_type: &'static str,
    source_type: SourceType,
    ssr: bool,
    sourcemap: bool,
) -> Result<Option<PassOutput>, String> {
    if !registry.wants_pre_transform(path, Some(source), module_type) {
        return Ok(None);
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        let message = parsed
            .diagnostics
            .into_iter()
            .map(|d| format!("{:?}", d.with_source_code(source.to_string())))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("parse error in {}:\n{message}", path.display()));
    }
    let mut program = parsed.program;
    let mut cx = AstCx {
        allocator: &allocator,
        program: &mut program,
        path,
        source,
        module_type,
        dev: false,
        ssr,
    };
    let meta = registry.run_pre_transform(&mut cx)?;
    registry.module_seen(path, &meta);
    let options = CodegenOptions {
        source_map_path: sourcemap.then(|| path.to_path_buf()),
        ..CodegenOptions::default()
    };
    let CodegenReturn { code, map, .. } = Codegen::new().with_options(options).build(&program);
    Ok(Some(PassOutput {
        code,
        map: map.map(|m| m.into_owned()),
        meta,
    }))
}

#[derive(Debug)]
pub struct PassOutput {
    pub code: String,
    pub map: Option<oxc_sourcemap::SourceMap<'static>>,
    pub meta: PluginMeta,
}

impl Plugin for AstPassPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:native-plugins")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::Transform
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let result = (|| -> Result<Option<HookTransformOutput>, String> {
            if args.id.starts_with('\0') {
                return Ok(None);
            }
            let Some((module_type, source_type)) = js_module_type(args.module_type) else {
                return Ok(None);
            };
            let id = args.id.split('?').next().unwrap_or(args.id);
            let Some(out) = run_pass(
                &self.registry,
                Path::new(id),
                args.code,
                module_type,
                source_type,
                self.ssr,
                self.sourcemap,
            )?
            else {
                return Ok(None);
            };
            Ok(Some(HookTransformOutput {
                code: Some(out.code),
                map: match out.map {
                    Some(map) => HookTransformOutputMap::Sourcemap(Box::new(map)),
                    None => HookTransformOutputMap::Null,
                },
                ..Default::default()
            }))
        })();
        async move { result.map_err(|e| anyhow::anyhow!(e)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModuleFilter, OjPlugin, Registry, SideChannel};
    use std::sync::Arc;

    #[derive(Debug)]
    struct Rename;

    impl Plugin for Rename {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("rename")
        }
        fn register_hook_usage(&self) -> HookUsage {
            HookUsage::empty()
        }
    }

    impl OjPlugin for Rename {
        fn transform_filter(&self) -> Option<&ModuleFilter> {
            static F: std::sync::OnceLock<ModuleFilter> = std::sync::OnceLock::new();
            Some(F.get_or_init(|| ModuleFilter::new().include_code("__before__")))
        }
        fn pre_transform_ast(&self, cx: &mut AstCx<'_, '_>) -> Result<Option<SideChannel>, String> {
            use oxc_ast_visit::VisitMut;
            struct V<'a>(&'a Allocator, usize);
            impl<'a> VisitMut<'a> for V<'a> {
                fn visit_identifier_reference(&mut self, it: &mut oxc_ast::ast::IdentifierReference<'a>) {
                    if it.name == "__before__" {
                        it.name = self.0.alloc_str("__after__").into();
                        self.1 += 1;
                    }
                }
            }
            let mut v = V(cx.allocator, 0);
            v.visit_program(cx.program);
            Ok(Some(serde_json::json!({ "renamed": v.1 })))
        }
    }

    fn registry() -> Registry {
        let mut r = Registry::new(Path::new("/app"));
        r.register(Arc::new(Rename)).unwrap();
        r
    }

    #[test]
    fn pass_rewrites_and_maps_matching_modules() {
        let r = registry();
        let out = run_pass(
            &r,
            Path::new("/app/src/a.tsx"),
            "const x: number = __before__;\nexport const y = <div>{x}</div>;",
            "tsx",
            SourceType::tsx(),
            false,
            true,
        )
        .unwrap()
        .expect("matched");
        assert!(out.code.contains("__after__"), "{}", out.code);
        assert!(!out.code.contains("__before__"));
        assert!(out.code.contains("<div>"), "JSX is still present after the pass: {}", out.code);
        assert!(out.map.is_some());
        assert_eq!(out.meta, vec![("rename".to_string(), serde_json::json!({ "renamed": 1 }))]);
    }

    #[test]
    fn pass_skips_modules_no_plugin_wants() {
        let r = registry();
        let out = run_pass(&r, Path::new("/app/src/a.ts"), "export const x = 1;", "ts", SourceType::ts(), false, true).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn pass_reports_parse_errors() {
        let r = registry();
        let err = run_pass(&r, Path::new("/app/src/a.ts"), "const __before__ = ;", "ts", SourceType::ts(), false, false)
            .unwrap_err();
        assert!(err.contains("parse error"), "{err}");
    }

    #[test]
    fn only_js_module_types_are_parsed() {
        assert!(js_module_type(&ModuleType::Css).is_none());
        assert!(js_module_type(&ModuleType::Json).is_none());
        assert_eq!(js_module_type(&ModuleType::Tsx).unwrap().0, "tsx");
    }
}
