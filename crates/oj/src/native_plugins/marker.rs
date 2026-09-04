// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The example plugin: the smallest thing that exercises every part of the
//! seam end to end, so the seam is tested without a real compiler behind it.
//!
//! An AST pass rewrites the `__MARKER__` identifier in matching modules to a
//! string naming the module and records a side channel; a cross-module
//! registry, fed by `module_seen` (cold or from cache), materializes one
//! virtual sheet at `/@oj/marker.css` and expands the `@marker;` directive in
//! stylesheets; `invalidates` maps a changed module to the sheet's url.
//!
//! Active only when `oj.config` has a `marker` section or
//! `OJ_EXAMPLE_PLUGIN=1` is set.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use oj_plugin::rolldown_plugin::{HookUsage, Plugin};
use oj_plugin::{AstCx, Invalidation, ModuleFilter, OjPlugin, SideChannel, VirtualSheet};
use oxc_ast::ast::{Expression, IdentifierReference};
use oxc_ast_visit::{walk_mut, VisitMut};
use oxc_span::SPAN;

pub const NAME: &str = "example-marker";
pub const MARKER: &str = "__MARKER__";
pub const SHEET: &str = "marker";
pub const DIRECTIVE: &str = "@marker;";

#[derive(Debug)]
pub struct MarkerPlugin {
    /// Class prefix from the `marker.prefix` option (default `mk`).
    prefix: String,
    filter: ModuleFilter,
    /// Module path to the module's side channel: the class name and how
    /// many markers it had.
    registry: Mutex<BTreeMap<String, (String, u64)>>,
}

impl MarkerPlugin {
    pub fn from_config(config: &oj_config::OjConfig) -> Option<Self> {
        let section = config.config_section("marker");
        let env_on = std::env::var("OJ_EXAMPLE_PLUGIN").is_ok_and(|v| v == "1");
        if section.is_none() && !env_on {
            return None;
        }
        let prefix = section
            .and_then(|s| s.get("prefix"))
            .and_then(|p| p.as_str())
            .unwrap_or("mk")
            .to_string();
        Some(Self::new(prefix))
    }

    pub fn new(prefix: String) -> Self {
        Self {
            prefix,
            filter: ModuleFilter::new().include_code(MARKER).exclude_id("**/node_modules/**"),
            registry: Mutex::new(BTreeMap::new()),
        }
    }

    fn class_for(&self, path: &Path) -> String {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("module");
        let slug: String = stem
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect();
        format!("{}-{slug}", self.prefix)
    }

    fn sheet_css(&self) -> String {
        let registry = self.registry.lock().unwrap();
        let mut css = String::new();
        for (class, count) in registry.values() {
            css.push_str(&format!(".{class}{{--marker-count:{count}}}\n"));
        }
        css
    }
}

impl Plugin for MarkerPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(NAME)
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::empty()
    }
}

impl OjPlugin for MarkerPlugin {
    fn cache_salt(&self) -> Option<String> {
        Some(format!("v1:prefix={}", self.prefix))
    }

    fn replaces_js_plugins(&self) -> &[&str] {
        &["vite:example-marker"]
    }

    fn config_section(&self) -> Option<&'static str> {
        Some("marker")
    }

    fn transform_filter(&self) -> Option<&ModuleFilter> {
        Some(&self.filter)
    }

    fn pre_transform_ast(&self, cx: &mut AstCx<'_, '_>) -> Result<Option<SideChannel>, String> {
        struct Rewrite<'a, 'c> {
            allocator: &'a oxc_allocator::Allocator,
            class: &'c str,
            count: u64,
        }
        impl<'a> VisitMut<'a> for Rewrite<'a, '_> {
            fn visit_expression(&mut self, expr: &mut Expression<'a>) {
                if let Expression::Identifier(id) = expr {
                    let id: &IdentifierReference<'a> = id;
                    if id.name == MARKER {
                        self.count += 1;
                        let ast = oxc_ast::builder::AstBuilder::new(self.allocator);
                        let value: &'a str = self.allocator.alloc_str(self.class);
                        *expr = Expression::new_string_literal(SPAN, value, None, &ast);
                        return;
                    }
                }
                walk_mut::walk_expression(self, expr);
            }
        }
        let class = self.class_for(cx.path);
        let mut rewrite = Rewrite { allocator: cx.allocator, class: &class, count: 0 };
        rewrite.visit_program(cx.program);
        if rewrite.count == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::json!({ "class": class, "count": rewrite.count })))
    }

    fn module_seen(&self, id: &Path, meta: Option<&SideChannel>) {
        let key = id.to_string_lossy().into_owned();
        let mut registry = self.registry.lock().unwrap();
        match meta.and_then(|m| Some((m.get("class")?.as_str()?.to_string(), m.get("count")?.as_u64()?))) {
            Some(entry) => {
                registry.insert(key, entry);
            }
            None => {
                registry.remove(&key);
            }
        }
    }

    fn module_removed(&self, id: &Path) {
        self.registry.lock().unwrap().remove(&*id.to_string_lossy());
    }

    fn css_directive(&self) -> Option<&'static str> {
        Some(DIRECTIVE)
    }

    fn css_for_directive(&self) -> String {
        self.sheet_css()
    }

    fn virtual_css(&self) -> Vec<VirtualSheet> {
        vec![VirtualSheet { name: SHEET.into(), css: self.sheet_css() }]
    }

    fn invalidates(&self, changed: &Path) -> Vec<Invalidation> {
        // The host recompiles a changed module the pass wants before asking,
        // so a module that just gained a marker is already in the registry.
        let known = self.registry.lock().unwrap().contains_key(&*changed.to_string_lossy());
        if known {
            vec![Invalidation::CssUrl(oj_plugin::virtual_css_url(SHEET))]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn compile(plugin: &Arc<MarkerPlugin>, path: &str, src: &str) -> oj_compiler::CompileOutput {
        let mut registry = oj_plugin::Registry::new(Path::new("/app"));
        registry.register(Arc::clone(plugin)).unwrap();
        let mut pre = |cx: &mut AstCx<'_, '_>| registry.run_pre_transform(cx);
        oj_compiler::compile_module_full(
            Path::new(path),
            src,
            &oj_compiler::CompileOptions::dev(),
            None,
            &[],
            Some(&mut pre),
        )
        .unwrap()
    }

    #[test]
    fn rewrites_the_marker_and_reports_a_side_channel() {
        let plugin = Arc::new(MarkerPlugin::new("mk".into()));
        let out = compile(&plugin, "/app/src/Hello.tsx", "export const c = __MARKER__;\nexport const el = <b className={__MARKER__}>x</b>;");
        assert!(out.code.contains("\"mk-hello\""), "{}", out.code);
        assert!(!out.code.contains(MARKER));
        assert_eq!(out.meta, vec![(NAME.to_string(), serde_json::json!({ "class": "mk-hello", "count": 2 }))]);
    }

    #[test]
    fn unmarked_modules_are_untouched() {
        let plugin = Arc::new(MarkerPlugin::new("mk".into()));
        let out = compile(&plugin, "/app/src/Plain.tsx", "export const c = 1;");
        assert!(out.meta.is_empty());
    }

    #[test]
    fn registry_builds_the_sheet_from_module_seen_and_forgets_removed_modules() {
        let plugin = MarkerPlugin::new("mk".into());
        plugin.module_seen(Path::new("/app/src/B.tsx"), Some(&serde_json::json!({ "class": "mk-b", "count": 1 })));
        plugin.module_seen(Path::new("/app/src/A.tsx"), Some(&serde_json::json!({ "class": "mk-a", "count": 3 })));
        plugin.module_seen(Path::new("/app/src/C.tsx"), None);
        assert_eq!(plugin.sheet_css(), ".mk-a{--marker-count:3}\n.mk-b{--marker-count:1}\n");
        assert_eq!(plugin.css_for_directive(), plugin.virtual_css()[0].css);
        assert_eq!(plugin.invalidates(Path::new("/app/src/A.tsx")), vec![Invalidation::CssUrl("/@oj/marker.css".into())]);
        assert!(plugin.invalidates(Path::new("/app/src/nope.css")).is_empty());
        plugin.module_removed(Path::new("/app/src/A.tsx"));
        assert_eq!(plugin.sheet_css(), ".mk-b{--marker-count:1}\n");
    }

    #[test]
    fn activation_and_options_come_from_the_config_section() {
        let off: oj_config::OjConfig = serde_json::from_str("{}").unwrap();
        let on: oj_config::OjConfig = serde_json::from_str(r#"{"marker":{"prefix":"zz"}}"#).unwrap();
        std::env::remove_var("OJ_EXAMPLE_PLUGIN");
        assert!(MarkerPlugin::from_config(&off).is_none());
        let plugin = MarkerPlugin::from_config(&on).unwrap();
        assert_eq!(plugin.class_for(Path::new("/x/My Comp.tsx")), "zz-my-comp");
        assert_eq!(plugin.cache_salt().as_deref(), Some("v1:prefix=zz"));
        assert_eq!(plugin.config_section(), Some("marker"));
    }
}
