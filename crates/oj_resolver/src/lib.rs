// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

use oxc_resolver::{
    AliasValue, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};

pub struct OjResolver {
    inner: Resolver,
}

#[derive(Debug, thiserror::Error)]
#[error("cannot resolve '{specifier}' from '{importer}': {reason}")]
pub struct ResolveFailure {
    pub specifier: String,
    pub importer: PathBuf,
    pub reason: String,
}

impl OjResolver {
    pub fn new(root: &Path) -> Self {
        Self::with_conditions(root, &["browser", "import", "module", "default"].map(String::from))
    }

    pub fn with_conditions(root: &Path, conditions: &[String]) -> Self {
        Self::with_options(root, conditions, &[])
    }

    pub fn with_options(root: &Path, conditions: &[String], alias: &[(String, String)]) -> Self {
        let tsconfig = root.join("tsconfig.json");
        let alias = alias
            .iter()
            .map(|(find, replacement)| {
                let target = if replacement.starts_with('.') {
                    root.join(replacement).to_string_lossy().into_owned()
                } else {
                    replacement.clone()
                };
                (find.clone(), vec![AliasValue::Path(target)])
            })
            .collect();
        let options = ResolveOptions {
            extensions: [".tsx", ".ts", ".jsx", ".js", ".mjs", ".cjs", ".json"]
                .map(String::from)
                .to_vec(),
            condition_names: conditions.to_vec(),
            alias,
            tsconfig: tsconfig.is_file().then(|| {
                TsconfigDiscovery::Manual(TsconfigOptions {
                    config_file: tsconfig,
                    references: TsconfigReferences::Auto,
                })
            }),
            ..ResolveOptions::default()
        };
        Self { inner: Resolver::new(options) }
    }

    pub fn resolve(
        &self,
        importer_dir: &Path,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
        self.inner
            .resolve(importer_dir, specifier)
            .map(|resolution| resolution.full_path())
            .map_err(|err| ResolveFailure {
                specifier: specifier.to_string(),
                importer: importer_dir.to_path_buf(),
                reason: err.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playground_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../playground")
    }
    fn playground_src() -> PathBuf {
        playground_root().join("src")
    }

    #[test]
    fn resolves_relative_import_with_tsx_extension_probing() {
        let resolver = OjResolver::new(&playground_root());
        let resolved = resolver.resolve(&playground_src(), "./App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "got {resolved:?}");
    }

    #[test]
    fn resolves_tsconfig_paths_alias() {
        let resolver = OjResolver::new(&playground_root());
        let resolved = resolver.resolve(&playground_src(), "@/App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "alias @/App -> {resolved:?}");
    }

    #[test]
    fn resolves_config_alias() {
        let resolver = OjResolver::with_options(
            &playground_root(),
            &["browser", "import", "default"].map(String::from),
            &[("~".to_string(), "./src".to_string())],
        );
        let resolved = resolver.resolve(&playground_root(), "~/App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "alias ~/App -> {resolved:?}");
    }

    #[test]
    fn reports_unresolvable_specifier() {
        let resolver = OjResolver::new(&playground_root());
        let err = resolver.resolve(&playground_src(), "./does-not-exist").unwrap_err();
        assert_eq!(err.specifier, "./does-not-exist");
    }

    #[test]
    fn resolves_exports_per_condition() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dual");
        let browser = OjResolver::with_conditions(
            &dir,
            &["browser", "import", "default"].map(String::from),
        );
        let node =
            OjResolver::with_conditions(&dir, &["node", "import", "default"].map(String::from));
        assert!(browser.resolve(&dir, "dual-pkg").unwrap().ends_with("browser.js"));
        assert!(node.resolve(&dir, "dual-pkg").unwrap().ends_with("node.js"));
    }

    #[test]
    fn default_condition_resolves_the_fallback_export() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dual");
        let resolver = OjResolver::with_conditions(&dir, &["default".to_string()]);
        assert!(resolver.resolve(&dir, "dual-pkg").unwrap().ends_with("default.js"));
    }

    #[test]
    fn resolves_package_subpath_exports_and_enforces_encapsulation() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/subpath");
        let resolver = OjResolver::new(&dir);
        assert!(resolver.resolve(&dir, "sub-pkg").unwrap().ends_with("index.js"));
        assert!(resolver.resolve(&dir, "sub-pkg/feature").unwrap().ends_with("feature.js"));
        assert!(resolver.resolve(&dir, "sub-pkg/internal").is_err(), "unlisted subpath must not resolve");
    }

    #[test]
    fn resolves_json_css_and_explicit_extensions() {
        let resolver = OjResolver::new(&playground_root());
        let src = playground_src();
        assert!(resolver.resolve(&src, "./data.json").unwrap().ends_with("data.json"));
        assert!(resolver.resolve(&src, "./App.tsx").unwrap().ends_with("App.tsx"));
        assert!(
            resolver.resolve(&src, "./Counter.module.css").unwrap().ends_with("Counter.module.css"),
            "exact-path .css should resolve",
        );
    }
}
