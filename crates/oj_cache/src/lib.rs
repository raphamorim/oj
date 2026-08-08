// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Content-addressed persistent cache for compiled module outputs.
//!
//! Discipline (per the webpack-5/Turbopack trauma notes in the research):
//! every input is part of the key — source bytes, the serving URL, and a
//! salt that folds in tool version + cache format + compile mode. There is
//! no invalidation protocol to get wrong: content changes -> different key.
//! Stale entries are just never read again (GC is a TODO).
//!
//! Layout: `<dir>/<first two hex chars>/<hash>.json`. JSON for now —
//! debuggable with `cat`; a binary format is a profiling decision, not an
//! architectural one.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bump whenever the wrapper/glue/output shape changes so old caches
/// can never poison a new binary.
pub const CACHE_FORMAT: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedModule {
    pub code: String,
    pub map_data_url: Option<String>,
    pub imports: Vec<String>,
    pub is_boundary: bool,
    /// Bundle mode: "esm" or "cjs" factory kind ("" for unbundled entries).
    #[serde(default)]
    pub kind: String,
    /// Bundle mode, CJS only: raw require specifier -> resolved url.
    #[serde(default)]
    pub require_map: Vec<(String, String)>,
    /// CSS modules only: exported class name -> scoped name.
    #[serde(default)]
    pub css_exports: Vec<(String, String)>,
    /// Absolute out-of-root paths this module's rewritten /@fs/ urls point
    /// at; re-added to the server allow-set on every serve (cache hits too),
    /// so a cached module's /@fs/ imports stay servable across restarts.
    #[serde(default)]
    pub fs_allow: Vec<String>,
}

pub struct PersistentCache {
    dir: PathBuf,
    salt: String,
}

impl PersistentCache {
    /// `dir` is created lazily on first write.
    pub fn new(dir: PathBuf, tool_version: &str) -> Self {
        Self { dir, salt: format!("{tool_version}:{CACHE_FORMAT}") }
    }

    pub fn key(&self, source: &[u8], url: &str, mode: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.salt.as_bytes());
        hasher.update(&[0]);
        hasher.update(mode.as_bytes());
        hasher.update(&[0]);
        hasher.update(url.as_bytes());
        hasher.update(&[0]);
        hasher.update(source);
        hasher.finalize().to_hex().to_string()
    }

    pub fn get(&self, key: &str) -> Option<CachedModule> {
        let bytes = fs::read(self.path_for(key)).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(module) => Some(module),
            Err(_) => {
                // Corrupt entry (interrupted write, disk trouble): drop it
                // and recompile rather than serve garbage.
                let _ = fs::remove_file(self.path_for(key));
                None
            }
        }
    }

    pub fn put(&self, key: &str, module: &CachedModule) {
        let path = self.path_for(key);
        let Some(parent) = path.parent() else { return };
        if fs::create_dir_all(parent).is_err() {
            return; // cache is best-effort; never fail the compile over it
        }
        // Write-then-rename so a crash mid-write can't leave a torn entry
        // under the final name.
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, serde_json::to_vec(module).unwrap_or_default()).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let shard = key.get(..2).unwrap_or("00");
        self.dir.join(shard).join(format!("{key}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache() -> PersistentCache {
        let dir = std::env::temp_dir().join(format!("oj-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        PersistentCache::new(dir, "0.0.1-test")
    }

    fn sample() -> CachedModule {
        CachedModule {
            code: "export const x = 1;".into(),
            map_data_url: Some("data:application/json;base64,e30=".into()),
            imports: vec!["/node_modules/react/index.js".into()],
            is_boundary: true,
            kind: "esm".into(),
            require_map: vec![("react".into(), "/node_modules/react/index.js".into())],
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
        }
    }

    #[test]
    fn roundtrip() {
        let cache = temp_cache();
        let key = cache.key(b"source", "/src/App.tsx", "dev");
        assert_eq!(cache.get(&key), None);
        cache.put(&key, &sample());
        assert_eq!(cache.get(&key), Some(sample()));
    }

    #[test]
    fn every_input_changes_the_key() {
        let cache = temp_cache();
        let base = cache.key(b"source", "/src/App.tsx", "dev");
        assert_ne!(base, cache.key(b"source2", "/src/App.tsx", "dev"), "content");
        assert_ne!(base, cache.key(b"source", "/src/Other.tsx", "dev"), "url");
        assert_ne!(base, cache.key(b"source", "/src/App.tsx", "prod"), "mode");
        let other_version = PersistentCache::new(std::env::temp_dir(), "9.9.9");
        assert_ne!(base, other_version.key(b"source", "/src/App.tsx", "dev"), "version");
    }

    #[test]
    fn corrupt_entries_are_dropped_not_served() {
        let cache = temp_cache();
        let key = cache.key(b"s", "/u", "dev");
        cache.put(&key, &sample());
        let path = cache.path_for(&key);
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(cache.get(&key), None);
        assert!(!path.exists(), "corrupt entry must be removed");
    }
}
