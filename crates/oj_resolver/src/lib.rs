// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

use oxc_resolver::{
    AliasValue, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};

pub struct OjResolver {
    inner: Resolver,
    root: PathBuf,
    dedupe: Vec<String>,
}

/// The package id of a bare specifier: `react-dom/client` -> `react-dom`,
/// `@radix-ui/react-slot/x` -> `@radix-ui/react-slot`.
fn package_name(spec: &str) -> String {
    let mut it = spec.split('/');
    let first = it.next().unwrap_or("");
    if first.starts_with('@') {
        match it.next() {
            Some(second) => format!("{first}/{second}"),
            None => first.to_string(),
        }
    } else {
        first.to_string()
    }
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
        Self::with_options(root, conditions, &[], &[])
    }

    pub fn with_options(
        root: &Path,
        conditions: &[String],
        alias: &[(String, String)],
        dedupe: &[String],
    ) -> Self {
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
            // Vite's default package entry resolution: exports still wins (via
            // condition_names), these are the fallbacks so ESM-only-via-`module`
            // / `browser` deps don't fall back to their CJS `main`. `browser`
            // alias_fields honors the package.json `browser` object remap.
            main_fields: ["browser", "module", "jsnext:main", "jsnext", "main"]
                .map(String::from)
                .to_vec(),
            alias_fields: vec![vec!["browser".to_string()]],
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
        Self { inner: Resolver::new(options), root: root.to_path_buf(), dedupe: dedupe.to_vec() }
    }

    /// A bare import of a `resolve.dedupe` package resolves from the project
    /// root so nested / monorepo copies collapse to one instance (Vite parity).
    fn should_dedupe(&self, specifier: &str) -> bool {
        if self.dedupe.is_empty() || specifier.starts_with('.') || specifier.starts_with('/') {
            return false;
        }
        let pkg = package_name(specifier);
        self.dedupe.iter().any(|d| d == &pkg)
    }

    pub fn resolve(
        &self,
        importer_dir: &Path,
        specifier: &str,
    ) -> Result<PathBuf, ResolveFailure> {
        let deduped = self.should_dedupe(specifier);
        let base = if deduped { self.root.as_path() } else { importer_dir };
        match self.inner.resolve(base, specifier) {
            Ok(resolution) => Ok(resolution.full_path()),
            Err(err) => {
                // Dedupe from root can miss a package only installed nested;
                // fall back to the importer's dir before failing.
                if deduped {
                    if let Ok(resolution) = self.inner.resolve(importer_dir, specifier) {
                        return Ok(resolution.full_path());
                    }
                }
                Err(ResolveFailure {
                    specifier: specifier.to_string(),
                    importer: importer_dir.to_path_buf(),
                    reason: err.to_string(),
                })
            }
        }
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
            &[],
        );
        let resolved = resolver.resolve(&playground_root(), "~/App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "alias ~/App -> {resolved:?}");
    }

    #[test]
    fn dedupe_collapses_nested_copy_to_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dedupe");
        let nested_dir = root.join("pkg");
        let conds = ["import", "default"].map(String::from);
        // Without dedupe: the importer's nested copy wins.
        let plain = OjResolver::with_options(&root, &conds, &[], &[]);
        let n = plain.resolve(&nested_dir, "dep").unwrap();
        assert!(
            n.to_string_lossy().contains("pkg/node_modules/dep"),
            "without dedupe expected nested copy, got {n:?}",
        );
        // With dedupe: collapses to the root copy.
        let deduped = OjResolver::with_options(&root, &conds, &[], &["dep".to_string()]);
        let r = deduped.resolve(&nested_dir, "dep").unwrap();
        assert!(
            !r.to_string_lossy().contains("pkg/node_modules/dep")
                && r.to_string_lossy().contains("dedupe/node_modules/dep"),
            "with dedupe expected the root copy, got {r:?}",
        );
        // Subpaths of a deduped package dedupe too.
        let deduped2 = OjResolver::with_options(&root, &conds, &[], &["dep".to_string()]);
        assert!(deduped2.resolve(&nested_dir, "dep").is_ok());
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
    fn prefers_module_field_and_browser_object_remap() {
        // Vite default mainFields: an ESM-only package (module field, no exports)
        // must resolve to its module entry, not fall back to CJS main; and the
        // package.json `browser` object must remap the resolved file.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainfields");
        let r = OjResolver::new(&dir);
        assert!(
            r.resolve(&dir, "mf-pkg").unwrap().ends_with("esm.js"),
            "module field must win over main: {:?}",
            r.resolve(&dir, "mf-pkg")
        );
        assert!(
            r.resolve(&dir, "br-pkg").unwrap().ends_with("browser.js"),
            "browser object must remap main: {:?}",
            r.resolve(&dir, "br-pkg")
        );
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
