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

use oxc_resolver::{ResolveOptions, Resolver};

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
    pub fn new() -> Self {
        let options = ResolveOptions {
            extensions: [".tsx", ".ts", ".jsx", ".js", ".mjs", ".cjs", ".json"]
                .map(String::from)
                .to_vec(),
            condition_names: ["browser", "import", "module", "default"]
                .map(String::from)
                .to_vec(),
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

impl Default for OjResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playground_src() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../playground/src")
    }

    #[test]
    fn resolves_relative_import_with_tsx_extension_probing() {
        let resolver = OjResolver::new();
        let resolved = resolver.resolve(&playground_src(), "./App").unwrap();
        assert!(resolved.ends_with("App.tsx"), "got {resolved:?}");
    }

    #[test]
    fn reports_unresolvable_specifier() {
        let resolver = OjResolver::new();
        let err = resolver.resolve(&playground_src(), "./does-not-exist").unwrap_err();
        assert_eq!(err.specifier, "./does-not-exist");
    }
}
