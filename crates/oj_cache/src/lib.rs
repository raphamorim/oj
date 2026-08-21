// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};

pub mod config_extract;
pub mod integrity;
pub mod start_bundle;

use serde::{Deserialize, Serialize};

pub mod start_codegen;

pub const CACHE_FORMAT: u32 = 5;

/// Version component of the on-disk cache root. Bump on any change to
/// cache semantics that older binaries would misread: binaries with
/// different versions never share mutable state.
pub const CACHE_ROOT_VERSION: u32 = 1;

/// Every store lives under this versioned root. Only `.gitignore` stays
/// at the bare `.oj-cache/` level.
pub fn cache_root(app_root: &Path) -> PathBuf {
    app_root
        .join(".oj-cache")
        .join(format!("v{CACHE_ROOT_VERSION}"))
}

/// Pre-v1 top-level cache entries a versioned binary must not leave
/// behind: removed on boot, the same heal pattern as the legacy
/// ssr-bridge dir. Unknown names are left alone (never read anyway).
const LEGACY_TOP_LEVEL: &[&str] = &[
    "start",
    "start-codegen",
    "start-bundle",
    "config-extract",
    "ssr",
    "deps",
    "v8",
    "graph-snapshot.json",
    "css-sidecar.mjs",
    "svelte-compile.mjs",
    "css-preprocess.mjs",
    "tailwind-sidecar.mjs",
    "oj-vite-config.mjs",
    "oj-vite-extract.mjs",
    "plugin-host.mjs",
    "optimize-deps.mjs",
    "ssr-env.json",
    "ssr-load-pack.jsonl",
    "ssr-resolve.jsonl",
    "ssr-bridge-pack.jsonl",
];

pub fn heal_legacy_layout(app_root: &Path) {
    let root = app_root.join(".oj-cache");
    for name in LEGACY_TOP_LEVEL {
        let path = root.join(name);
        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    // Pre-v1 PersistentCache shards: two-hex-digit dirs at the top level.
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.len() == 2
            && name.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && e.path().is_dir()
        {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedModule {
    pub code: String,
    pub map_data_url: Option<String>,
    pub imports: Vec<String>,
    pub is_boundary: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub require_map: Vec<(String, String)>,
    #[serde(default)]
    pub css_exports: Vec<(String, String)>,
    #[serde(default)]
    pub fs_allow: Vec<String>,
    #[serde(default)]
    pub watch_files: Vec<String>,
}

pub struct PersistentCache {
    dir: PathBuf,
    salt: String,
}

impl PersistentCache {
    pub fn new(dir: PathBuf, tool_version: &str) -> Self {
        Self {
            dir,
            salt: format!("{tool_version}:{CACHE_FORMAT}"),
        }
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
                let _ = fs::remove_file(self.path_for(key));
                None
            }
        }
    }

    pub fn put(&self, key: &str, module: &CachedModule) {
        let path = self.path_for(key);
        let Some(parent) = path.parent() else { return };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
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

    fn temp_cache(label: &str) -> PersistentCache {
        let dir =
            std::env::temp_dir().join(format!("oj-cache-test-{}-{label}", std::process::id()));
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
            watch_files: Vec::new(),
        }
    }

    #[test]
    fn roundtrip() {
        let cache = temp_cache("roundtrip");
        let key = cache.key(b"source", "/src/App.tsx", "dev");
        assert_eq!(cache.get(&key), None);
        cache.put(&key, &sample());
        assert_eq!(cache.get(&key), Some(sample()));
    }

    #[test]
    fn every_input_changes_the_key() {
        let cache = temp_cache("keys");
        let base = cache.key(b"source", "/src/App.tsx", "dev");
        assert_ne!(
            base,
            cache.key(b"source2", "/src/App.tsx", "dev"),
            "content"
        );
        assert_ne!(base, cache.key(b"source", "/src/Other.tsx", "dev"), "url");
        assert_ne!(base, cache.key(b"source", "/src/App.tsx", "prod"), "mode");
        let other_version = PersistentCache::new(std::env::temp_dir(), "9.9.9");
        assert_ne!(
            base,
            other_version.key(b"source", "/src/App.tsx", "dev"),
            "version"
        );
    }

    #[test]
    fn corrupt_entries_are_dropped_not_served() {
        let cache = temp_cache("corrupt");
        let key = cache.key(b"s", "/u", "dev");
        cache.put(&key, &sample());
        let path = cache.path_for(&key);
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(cache.get(&key), None);
        assert!(!path.exists(), "corrupt entry must be removed");
    }

    #[test]
    fn shards_by_key_prefix_and_leaves_no_temp_file() {
        let cache = temp_cache("shard");
        let key = cache.key(b"s", "/u", "dev");
        cache.put(&key, &sample());
        let path = cache.path_for(&key);
        assert_eq!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            &key[..2]
        );
        assert!(path.exists());
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file must be renamed away"
        );
    }

    #[test]
    fn field_separators_prevent_key_collisions() {
        let cache = temp_cache("sep");
        let a = cache.key(b"bc", "a", "dev");
        let b = cache.key(b"c", "ab", "dev");
        assert_ne!(a, b, "url/source boundary must be unambiguous");
    }

    #[test]
    fn cache_root_is_versioned() {
        let root = cache_root(Path::new("/app"));
        assert_eq!(
            root,
            Path::new("/app")
                .join(".oj-cache")
                .join(format!("v{CACHE_ROOT_VERSION}"))
        );
    }

    #[test]
    fn heal_removes_legacy_layout_and_keeps_the_rest() {
        let app = std::env::temp_dir().join(format!("oj-heal-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&app);
        let cache = app.join(".oj-cache");
        for dir in ["start/ssr-bridge", "start-bundle", "deps", "ab", "3f"] {
            fs::create_dir_all(cache.join(dir)).unwrap();
        }
        for file in ["ssr-resolve.jsonl", "graph-snapshot.json"] {
            fs::write(cache.join(file), b"x").unwrap();
        }
        fs::write(cache.join(".gitignore"), b"*\n").unwrap();
        fs::create_dir_all(cache_root(&app).join("start")).unwrap();
        fs::create_dir_all(cache.join("user-stuff")).unwrap();

        heal_legacy_layout(&app);

        for legacy in [
            "start",
            "start-bundle",
            "deps",
            "ab",
            "3f",
            "ssr-resolve.jsonl",
            "graph-snapshot.json",
        ] {
            assert!(!cache.join(legacy).exists(), "{legacy} must be removed");
        }
        assert!(cache.join(".gitignore").exists());
        assert!(cache_root(&app).join("start").exists());
        assert!(cache.join("user-stuff").exists(), "unknown dirs are left alone");
    }
}
