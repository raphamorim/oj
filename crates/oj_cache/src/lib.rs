// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::PathBuf;

pub mod config_extract;
pub mod integrity;
pub mod start_bundle;

use serde::{Deserialize, Serialize};

pub mod start_codegen;

pub const CACHE_FORMAT: u32 = 5;

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
}
