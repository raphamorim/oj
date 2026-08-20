// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

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
        let path = self.path_for(key)?;
        let bytes = fs::read(&path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(module) => Some(module),
            Err(_) => {
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    pub fn put(&self, key: &str, module: &CachedModule) {
        let Some(path) = self.path_for(key) else { return };
        let Some(parent) = path.parent() else { return };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        // A private temp name per writer, so the rename is the only way an
        // entry becomes visible: concurrent builds sharing one .oj-cache must
        // never be able to read a half-written entry.
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{seq}", std::process::id()));
        if fs::write(&tmp, serde_json::to_vec(module).unwrap_or_default()).is_ok() {
            if fs::rename(&tmp, &path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Keys come from [`Self::key`] and are therefore always a 64-character
    /// lowercase blake3 digest. Anything else is a caller bug, and letting it
    /// through would let the key steer reads and writes out of the cache
    /// directory (`../../..`), so it is refused instead.
    fn path_for(&self, key: &str) -> Option<PathBuf> {
        let is_digest = key.len() == 64
            && key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !is_digest {
            return None;
        }
        Some(self.dir.join(&key[..2]).join(format!("{key}.json")))
    }
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

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
        let path = cache.path_for(&key).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(cache.get(&key), None);
        assert!(!path.exists(), "corrupt entry must be removed");
    }

    #[test]
    fn shards_by_key_prefix_and_leaves_no_temp_file() {
        let cache = temp_cache("shard");
        let key = cache.key(b"s", "/u", "dev");
        cache.put(&key, &sample());
        let path = cache.path_for(&key).unwrap();
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
        let shard = path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(shard)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away: {leftovers:?}");
    }

    #[test]
    fn field_separators_prevent_key_collisions() {
        let cache = temp_cache("sep");
        let a = cache.key(b"bc", "a", "dev");
        let b = cache.key(b"c", "ab", "dev");
        assert_ne!(a, b, "url/source boundary must be unambiguous");
    }
}
