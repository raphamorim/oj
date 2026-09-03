// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The native plugin seam.
//!
//! An [`OjPlugin`] is a [`rolldown_plugin::Plugin`] (so it can take part in
//! the build like any rolldown plugin) plus a handful of defaulted methods the
//! dev server and the build both call: a pre-transformer AST pass that runs on
//! the parsed module before JSX and TypeScript are stripped, a side channel
//! that is cached with the module and replayed on warm starts, a CSS directive
//! and virtual sheets, an HMR invalidation hook, and cache and config
//! participation. Plugins are compiled in; the [`Registry`] holds the active
//! set and is what the hosts talk to.

pub mod build;
pub mod filter;

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

pub use filter::ModuleFilter;
pub use oj_compiler::{module_type_of, AstCx, PluginMeta};
pub use rolldown_plugin;
use rolldown_plugin::{Plugin, Pluginable};

/// rolldown's own alias, spelled out since it lives behind `__inner`.
pub type SharedPluginable = Arc<dyn Pluginable>;

/// What a pre-transformer reports for one module. Stored next to the compiled
/// code as `CachedModule.meta[plugin.name()]` and handed back through
/// `module_seen`, cold or warm.
pub type SideChannel = serde_json::Value;

/// What a change to one file invalidates beyond the module graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalidation {
    /// A stylesheet url (a served `.css` url or a `/@oj/<name>.css` virtual
    /// sheet) whose content depends on the changed file: the client gets a
    /// `css-update` for it.
    CssUrl(String),
    /// Another module must be treated as changed too.
    Module(std::path::PathBuf),
    /// Nothing finer than a full reload is safe.
    FullReload(String),
}

/// A stylesheet a plugin materializes from its own state. Served at
/// `/@oj/<name>.css` in dev and emitted as `assets/<name>-<hash>.css` in build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualSheet {
    pub name: String,
    pub css: String,
}

/// Per-stylesheet parse options a plugin can ask the CSS pipeline for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CssParseOpts {
    /// Keep going past rules Lightning CSS rejects instead of failing the
    /// sheet: for plugin-generated CSS that uses syntax the parser does not
    /// know yet.
    pub error_recovery: bool,
}

/// A native oj plugin. Every method has a default, so a plugin implements
/// only the archetype it is: an AST pass (`transform_filter` plus
/// `pre_transform_ast`), a CSS registry (`module_seen`, `virtual_css` or
/// `css_directive`, `invalidates`), or both.
pub trait OjPlugin: Plugin {
    /// Everything the plugin's output depends on besides the source: options
    /// and the plugin version. Folded into the persistent cache salt, so a
    /// plugin that forgets an option here serves stale output on warm start.
    fn cache_salt(&self) -> Option<String> {
        None
    }

    /// JS plugin names (Vite `plugin.name`) this plugin replaces. The plugin
    /// host drops them while this plugin is active.
    fn replaces_js_plugins(&self) -> &[&str] {
        &[]
    }

    /// The top-level `oj.config` key this plugin reads its options from.
    fn config_section(&self) -> Option<&'static str> {
        None
    }

    /// Which modules the AST pass wants. `None` means the plugin has no AST
    /// pass; `Some(filter)` gates `pre_transform_ast` before parsing, and an
    /// empty filter matches every JavaScript module.
    fn transform_filter(&self) -> Option<&ModuleFilter> {
        None
    }

    /// The AST pass. Runs on the parsed program (JSX and TypeScript still
    /// present) in dev and in build alike. Returns the module's side channel.
    fn pre_transform_ast(&self, _cx: &mut AstCx<'_, '_>) -> Result<Option<SideChannel>, String> {
        Ok(None)
    }

    /// Whether a module this plugin touched may be served from the mtime fast
    /// path. A plugin whose output for one module depends on other modules
    /// returns false for the modules that must be re-derived.
    fn mtime_cacheable(&self, _id: &Path) -> bool {
        true
    }

    /// A module entered or re-entered the graph. Called with the plugin's own
    /// side channel for it (or `None`) on cold compiles and on every cache hit,
    /// so a cross-module registry rebuilds on a warm start without retransforms.
    fn module_seen(&self, _id: &Path, _meta: Option<&SideChannel>) {}

    /// A module left the graph (deleted on disk).
    fn module_removed(&self, _id: &Path) {}

    /// A CSS at-rule this plugin owns, e.g. `@marker;`. The host replaces it
    /// with `css_for_directive()` at the end of the stylesheet pipeline; an
    /// oj-owned sentinel carries its position through the sidecars and
    /// Lightning CSS.
    fn css_directive(&self) -> Option<&'static str> {
        None
    }

    /// What the directive expands to right now.
    fn css_for_directive(&self) -> String {
        String::new()
    }

    /// Sheets built from the plugin's state, independent of any source file.
    fn virtual_css(&self) -> Vec<VirtualSheet> {
        Vec::new()
    }

    /// Parse options for a stylesheet url, when the plugin wants any.
    fn css_parse_options(&self, _url: &str) -> Option<CssParseOpts> {
        None
    }

    /// What a change to `changed` invalidates besides the module graph.
    fn invalidates(&self, _changed: &Path) -> Vec<Invalidation> {
        Vec::new()
    }
}

/// Object-safe view of [`OjPlugin`]. `rolldown_plugin::Plugin` has
/// `impl Future` methods so `dyn OjPlugin` cannot exist; the registry stores
/// this instead, and rolldown's own `Pluginable` for the build hooks.
pub trait DynOjPlugin: Send + Sync {
    fn name(&self) -> Cow<'static, str>;
    fn cache_salt(&self) -> Option<String>;
    fn replaces_js_plugins(&self) -> Vec<String>;
    fn config_section(&self) -> Option<&'static str>;
    fn transform_filter(&self) -> Option<&ModuleFilter>;
    fn pre_transform_ast(&self, cx: &mut AstCx<'_, '_>) -> Result<Option<SideChannel>, String>;
    fn mtime_cacheable(&self, id: &Path) -> bool;
    fn module_seen(&self, id: &Path, meta: Option<&SideChannel>);
    fn module_removed(&self, id: &Path);
    fn css_directive(&self) -> Option<&'static str>;
    fn css_for_directive(&self) -> String;
    fn virtual_css(&self) -> Vec<VirtualSheet>;
    fn css_parse_options(&self, url: &str) -> Option<CssParseOpts>;
    fn invalidates(&self, changed: &Path) -> Vec<Invalidation>;
}

impl<T: OjPlugin> DynOjPlugin for T {
    fn name(&self) -> Cow<'static, str> {
        Plugin::name(self)
    }
    fn cache_salt(&self) -> Option<String> {
        OjPlugin::cache_salt(self)
    }
    fn replaces_js_plugins(&self) -> Vec<String> {
        OjPlugin::replaces_js_plugins(self).iter().map(|s| s.to_string()).collect()
    }
    fn config_section(&self) -> Option<&'static str> {
        OjPlugin::config_section(self)
    }
    fn transform_filter(&self) -> Option<&ModuleFilter> {
        OjPlugin::transform_filter(self)
    }
    fn pre_transform_ast(&self, cx: &mut AstCx<'_, '_>) -> Result<Option<SideChannel>, String> {
        OjPlugin::pre_transform_ast(self, cx)
    }
    fn mtime_cacheable(&self, id: &Path) -> bool {
        OjPlugin::mtime_cacheable(self, id)
    }
    fn module_seen(&self, id: &Path, meta: Option<&SideChannel>) {
        OjPlugin::module_seen(self, id, meta)
    }
    fn module_removed(&self, id: &Path) {
        OjPlugin::module_removed(self, id)
    }
    fn css_directive(&self) -> Option<&'static str> {
        OjPlugin::css_directive(self)
    }
    fn css_for_directive(&self) -> String {
        OjPlugin::css_for_directive(self)
    }
    fn virtual_css(&self) -> Vec<VirtualSheet> {
        OjPlugin::virtual_css(self)
    }
    fn css_parse_options(&self, url: &str) -> Option<CssParseOpts> {
        OjPlugin::css_parse_options(self, url)
    }
    fn invalidates(&self, changed: &Path) -> Vec<Invalidation> {
        OjPlugin::invalidates(self, changed)
    }
}

struct Entry {
    name: String,
    oj: Arc<dyn DynOjPlugin>,
    rolldown: SharedPluginable,
}

/// The active native plugins, in registration order. Both hosts hold one
/// behind an `Arc`; an empty registry is the "no native plugin" case and
/// every query on it is a no-op.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
    /// The project root, the `cwd` relative id globs resolve against.
    root: String,
}

pub type SharedRegistry = Arc<Registry>;

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").field("plugins", &self.names()).finish()
    }
}

impl Registry {
    pub fn new(root: &Path) -> Self {
        Self {
            entries: Vec::new(),
            root: root.to_string_lossy().into_owned(),
        }
    }

    /// Add a plugin. Registering two plugins with the same name is an error:
    /// the name keys the cache meta and the config section.
    pub fn register<T: OjPlugin>(&mut self, plugin: Arc<T>) -> Result<(), String> {
        let name = Plugin::name(&*plugin).into_owned();
        if self.entries.iter().any(|e| e.name == name) {
            return Err(format!("native plugin `{name}` registered twice"));
        }
        let rolldown: SharedPluginable = Arc::clone(&plugin) as Arc<dyn Pluginable>;
        self.entries.push(Entry {
            name,
            oj: plugin,
            rolldown,
        });
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    pub fn plugins(&self) -> impl Iterator<Item = &dyn DynOjPlugin> {
        self.entries.iter().map(|e| &*e.oj)
    }

    /// The plugins as rolldown plugins, for the build's plugin list.
    pub fn rolldown_plugins(&self) -> Vec<SharedPluginable> {
        self.entries.iter().map(|e| Arc::clone(&e.rolldown)).collect()
    }

    /// The persistent-cache salt contribution: every plugin's name and
    /// `cache_salt`, so a plugin appearing, disappearing or changing options
    /// misses the cache. Empty when no plugin is registered.
    pub fn cache_salt(&self) -> String {
        let mut parts: Vec<String> = self
            .entries
            .iter()
            .map(|e| match e.oj.cache_salt() {
                Some(salt) => format!("{}={}", e.name, salt),
                None => e.name.clone(),
            })
            .collect();
        parts.sort();
        parts.join(";")
    }

    /// Union of `replaces_js_plugins` across active plugins, sorted, deduped.
    pub fn replaced_js_plugins(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.iter().flat_map(|e| e.oj.replaces_js_plugins()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// True when at least one plugin has an AST pass at all.
    pub fn has_ast_passes(&self) -> bool {
        self.entries.iter().any(|e| e.oj.transform_filter().is_some())
    }

    /// Whether any plugin's filter admits this module: the gate a host checks
    /// before parsing. `code` may be `None` when the source is not read yet,
    /// in which case only id and module type leaves can match.
    pub fn wants_pre_transform(&self, id: &Path, code: Option<&str>, module_type: &str) -> bool {
        let id = id.to_string_lossy();
        self.entries.iter().any(|e| {
            e.oj
                .transform_filter()
                .is_some_and(|f| f.matches(&id, code, Some(module_type), &self.root))
        })
    }

    /// Run every matching plugin's `pre_transform_ast` on the program, in
    /// registration order, collecting side channels by plugin name. The one
    /// pass shared by the dev compiler slot and the build adapter.
    pub fn run_pre_transform(&self, cx: &mut AstCx<'_, '_>) -> Result<PluginMeta, String> {
        let id = cx.path.to_string_lossy().into_owned();
        let mut meta = PluginMeta::new();
        for e in &self.entries {
            let Some(filter) = e.oj.transform_filter() else { continue };
            if !filter.matches(&id, Some(cx.source), Some(cx.module_type), &self.root) {
                continue;
            }
            if let Some(value) = e.oj.pre_transform_ast(cx).map_err(|m| format!("{}: {m}", e.name))? {
                meta.push((e.name.clone(), value));
            }
        }
        Ok(meta)
    }

    /// Whether every plugin allows the mtime fast path for this module.
    pub fn mtime_cacheable(&self, id: &Path) -> bool {
        self.entries.iter().all(|e| e.oj.mtime_cacheable(id))
    }

    /// Feed `module_seen` to every plugin with its own slice of the module's
    /// cached meta.
    pub fn module_seen(&self, id: &Path, meta: &[(String, SideChannel)]) {
        for e in &self.entries {
            let own = meta.iter().find(|(n, _)| *n == e.name).map(|(_, v)| v);
            e.oj.module_seen(id, own);
        }
    }

    pub fn module_removed(&self, id: &Path) {
        for e in &self.entries {
            e.oj.module_removed(id);
        }
    }

    /// Every directive any plugin owns, with the plugin that expands it.
    pub fn css_directives(&self) -> Vec<(&'static str, &dyn DynOjPlugin)> {
        self.entries
            .iter()
            .filter_map(|e| e.oj.css_directive().map(|d| (d, &*e.oj)))
            .collect()
    }

    /// All virtual sheets, in registration order.
    pub fn virtual_css(&self) -> Vec<VirtualSheet> {
        self.entries.iter().flat_map(|e| e.oj.virtual_css()).collect()
    }

    /// One virtual sheet by name.
    pub fn virtual_sheet(&self, name: &str) -> Option<VirtualSheet> {
        self.virtual_css().into_iter().find(|s| s.name == name)
    }

    /// The first plugin's parse options for a stylesheet url.
    pub fn css_parse_options(&self, url: &str) -> Option<CssParseOpts> {
        self.entries.iter().find_map(|e| e.oj.css_parse_options(url))
    }

    /// Everything the active plugins say a change invalidates.
    pub fn invalidates(&self, changed: &Path) -> Vec<Invalidation> {
        self.entries.iter().flat_map(|e| e.oj.invalidates(changed)).collect()
    }
}

/// Url a virtual sheet is served at in dev.
pub fn virtual_css_url(name: &str) -> String {
    format!("/@oj/{name}.css")
}

/// The sheet name a `/@oj/<name>.css` url refers to, if it is one.
pub fn virtual_css_name(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("/@oj/")?;
    let name = rest.split('?').next().unwrap_or(rest).strip_suffix(".css")?;
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rolldown_plugin::HookUsage;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct Marker {
        salt: Option<String>,
        seen: Mutex<Vec<(String, Option<SideChannel>)>>,
        removed: Mutex<Vec<String>>,
        filter: Option<ModuleFilter>,
    }

    impl Plugin for Marker {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("marker")
        }
        fn register_hook_usage(&self) -> HookUsage {
            HookUsage::empty()
        }
    }

    impl OjPlugin for Marker {
        fn cache_salt(&self) -> Option<String> {
            self.salt.clone()
        }
        fn replaces_js_plugins(&self) -> &[&str] {
            &["vite:marker", "vite:react-babel"]
        }
        fn transform_filter(&self) -> Option<&ModuleFilter> {
            self.filter.as_ref()
        }
        fn pre_transform_ast(&self, cx: &mut AstCx<'_, '_>) -> Result<Option<SideChannel>, String> {
            Ok(Some(serde_json::json!({ "path": cx.path.to_string_lossy(), "type": cx.module_type })))
        }
        fn module_seen(&self, id: &Path, meta: Option<&SideChannel>) {
            self.seen.lock().unwrap().push((id.to_string_lossy().into_owned(), meta.cloned()));
        }
        fn module_removed(&self, id: &Path) {
            self.removed.lock().unwrap().push(id.to_string_lossy().into_owned());
        }
        fn virtual_css(&self) -> Vec<VirtualSheet> {
            vec![VirtualSheet { name: "marker".into(), css: ".m{}".into() }]
        }
        fn invalidates(&self, changed: &Path) -> Vec<Invalidation> {
            if changed.extension().is_some_and(|e| e == "tsx") {
                vec![Invalidation::CssUrl(virtual_css_url("marker"))]
            } else {
                Vec::new()
            }
        }
    }

    #[derive(Debug)]
    struct Other;
    impl Plugin for Other {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("other")
        }
        fn register_hook_usage(&self) -> HookUsage {
            HookUsage::empty()
        }
    }
    impl OjPlugin for Other {
        fn replaces_js_plugins(&self) -> &[&str] {
            &["vite:react-babel", "vite:other"]
        }
    }

    fn registry(plugin: Marker) -> Registry {
        let mut r = Registry::new(Path::new("/app"));
        r.register(Arc::new(plugin)).unwrap();
        r
    }

    #[test]
    fn empty_registry_is_inert() {
        let r = Registry::new(Path::new("/app"));
        assert!(r.is_empty());
        assert_eq!(r.cache_salt(), "");
        assert!(r.replaced_js_plugins().is_empty());
        assert!(!r.has_ast_passes());
        assert!(!r.wants_pre_transform(Path::new("/app/a.tsx"), Some("x"), "tsx"));
        assert!(r.mtime_cacheable(Path::new("/app/a.tsx")));
        assert!(r.virtual_css().is_empty());
        assert!(r.invalidates(Path::new("/app/a.tsx")).is_empty());
        assert!(r.css_directives().is_empty());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let mut r = Registry::new(Path::new("/app"));
        r.register(Arc::new(Marker::default())).unwrap();
        assert!(r.register(Arc::new(Marker::default())).is_err());
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn cache_salt_folds_name_and_plugin_salt_in_stable_order() {
        let mut r = Registry::new(Path::new("/app"));
        r.register(Arc::new(Other)).unwrap();
        r.register(Arc::new(Marker { salt: Some("v1:opt=a".into()), ..Default::default() })).unwrap();
        assert_eq!(r.cache_salt(), "marker=v1:opt=a;other");
        let plain = registry(Marker::default());
        assert_eq!(plain.cache_salt(), "marker");
        let changed = registry(Marker { salt: Some("v1:opt=b".into()), ..Default::default() });
        assert_ne!(changed.cache_salt(), "marker=v1:opt=a;other");
    }

    #[test]
    fn replaced_js_plugins_is_a_sorted_union() {
        let mut r = Registry::new(Path::new("/app"));
        r.register(Arc::new(Marker::default())).unwrap();
        r.register(Arc::new(Other)).unwrap();
        assert_eq!(r.replaced_js_plugins(), vec!["vite:marker", "vite:other", "vite:react-babel"]);
    }

    #[test]
    fn filter_gates_the_pass_before_parsing() {
        let r = registry(Marker {
            filter: Some(ModuleFilter::new().include_id("**/*.tsx").include_code("__marker__")),
            ..Default::default()
        });
        assert!(r.has_ast_passes());
        assert!(r.wants_pre_transform(Path::new("/app/src/a.tsx"), None, "tsx"));
        assert!(r.wants_pre_transform(Path::new("/app/src/a.js"), Some("__marker__"), "js"));
        assert!(!r.wants_pre_transform(Path::new("/app/src/a.js"), Some("plain"), "js"));
        assert!(!r.wants_pre_transform(Path::new("/app/src/a.js"), None, "js"));
    }

    #[test]
    fn plugin_without_filter_has_no_pass() {
        let r = registry(Marker::default());
        assert!(!r.has_ast_passes());
        assert!(!r.wants_pre_transform(Path::new("/app/src/a.tsx"), Some("__marker__"), "tsx"));
    }

    #[test]
    fn run_pre_transform_collects_side_channels_for_matching_plugins_only() {
        let r = registry(Marker {
            filter: Some(ModuleFilter::new().include_code("__marker__")),
            ..Default::default()
        });
        let allocator = oxc_allocator::Allocator::default();
        let src = "const x = __marker__;";
        let path = Path::new("/app/src/a.tsx");
        let parsed = oxc_parser::Parser::new(&allocator, src, oxc_span::SourceType::from_path(path).unwrap()).parse();
        let mut program = parsed.program;
        let mut cx = AstCx {
            allocator: &allocator,
            program: &mut program,
            path,
            source: src,
            module_type: module_type_of(path),
            dev: true,
            ssr: false,
        };
        let meta = r.run_pre_transform(&mut cx).unwrap();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].0, "marker");
        assert_eq!(meta[0].1["type"], "tsx");

        let plain = "const x = 1;";
        let parsed = oxc_parser::Parser::new(&allocator, plain, oxc_span::SourceType::from_path(path).unwrap()).parse();
        let mut program = parsed.program;
        let mut cx = AstCx {
            allocator: &allocator,
            program: &mut program,
            path,
            source: plain,
            module_type: module_type_of(path),
            dev: true,
            ssr: false,
        };
        assert!(r.run_pre_transform(&mut cx).unwrap().is_empty());
    }

    #[test]
    fn module_seen_hands_each_plugin_its_own_meta() {
        let marker = Arc::new(Marker::default());
        let mut r = Registry::new(Path::new("/app"));
        r.register(Arc::clone(&marker)).unwrap();
        r.register(Arc::new(Other)).unwrap();
        let meta = vec![
            ("other".to_string(), serde_json::json!(1)),
            ("marker".to_string(), serde_json::json!({ "k": 2 })),
        ];
        r.module_seen(Path::new("/app/a.tsx"), &meta);
        r.module_seen(Path::new("/app/b.tsx"), &[]);
        r.module_removed(Path::new("/app/a.tsx"));
        let seen = marker.seen.lock().unwrap();
        assert_eq!(seen[0], ("/app/a.tsx".to_string(), Some(serde_json::json!({ "k": 2 }))));
        assert_eq!(seen[1], ("/app/b.tsx".to_string(), None));
        assert_eq!(*marker.removed.lock().unwrap(), vec!["/app/a.tsx".to_string()]);
    }

    #[test]
    fn virtual_css_and_invalidations_flow_through() {
        let r = registry(Marker::default());
        assert_eq!(r.virtual_sheet("marker").unwrap().css, ".m{}");
        assert!(r.virtual_sheet("nope").is_none());
        assert_eq!(
            r.invalidates(Path::new("/app/a.tsx")),
            vec![Invalidation::CssUrl("/@oj/marker.css".into())]
        );
        assert!(r.invalidates(Path::new("/app/a.css")).is_empty());
    }

    #[test]
    fn virtual_css_urls_round_trip() {
        assert_eq!(virtual_css_url("marker"), "/@oj/marker.css");
        assert_eq!(virtual_css_name("/@oj/marker.css"), Some("marker"));
        assert_eq!(virtual_css_name("/@oj/marker.css?t=1"), Some("marker"));
        assert_eq!(virtual_css_name("/@oj/client.js"), None);
        assert_eq!(virtual_css_name("/@oj/a/b.css"), None);
        assert_eq!(virtual_css_name("/src/a.css"), None);
    }

    #[test]
    fn rolldown_plugins_are_exposed_for_the_build() {
        let r = registry(Marker::default());
        let plugins = r.rolldown_plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].call_name(), "marker");
    }
}
