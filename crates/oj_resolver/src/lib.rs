// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Resolution policy for oj, on top of `oxc_resolver` (a Rust port of
//! webpack's enhanced-resolve, shared with Rspack).
//!
//! The library solves the Node algorithm (exports maps, conditions, browser
//! field, symlinks). What lives HERE is our policy: extension order for a
//! TS-first React project, browser-flavored condition names, and — later —
//! the dedup rules that guarantee a single copy of react in the graph
//! (pnpm symlink identity; a wrong call here = "invalid hook call").

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
    /// Build a resolver for an app rooted at `root`. If the app has a
    /// `tsconfig.json`, its `paths` (the `@/*` alias convention) are wired in
    /// via oxc_resolver's tsconfig support — this IS resolve.alias for the
    /// TS+Vite apps that make up almost all of the target.
    pub fn new(root: &Path) -> Self {
        Self::with_conditions(root, &["browser", "import", "module", "default"].map(String::from))
    }

    /// Build a resolver with explicit `exports`/`imports` condition names. The
    /// client environment uses browser conditions; the SSR environment uses
    /// node conditions, so packages with conditional `exports` resolve their
    /// correct per-environment variant (Vite Environment API `resolve.conditions`).
    pub fn with_conditions(root: &Path, conditions: &[String]) -> Self {
        Self::with_options(root, conditions, &[])
    }

    /// As [`Self::with_conditions`], plus `resolve.alias` entries (`find`,
    /// `replacement`) from the app's config. These sit alongside tsconfig
    /// `paths`; a bare `find` matches the exact specifier or a `find/...`
    /// prefix (webpack/Vite semantics). A relative `replacement` (`./src`) is
    /// resolved against `root` to an absolute path, as Vite does.
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

    /// Resolve `specifier` as imported from the directory `importer_dir`.
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
        // `@/*` -> `src/*` from the playground tsconfig.json.
        let resolver = OjResolver::new(&playground_root());
        let resolved = resolver.resolve(&playground_src(), "@/App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "alias @/App -> {resolved:?}");
    }

    #[test]
    fn resolves_config_alias() {
        // A `resolve.alias` entry (`~` -> `./src`) rewrites the specifier prefix
        // and then resolves through the normal extension probing.
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
        // A package with conditional `exports`: the browser and node condition
        // sets resolve to different files (Vite Environment API resolve.conditions).
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
}
