// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::watch;

const OPTIMIZE_JS: &str = include_str!("assets/optimize-deps.mjs");

pub struct DepMeta {
    pub file: String,
    pub needs_interop: bool,
}

pub type DepMap = HashMap<String, DepMeta>;

pub struct OptimizedDeps {
    rx: watch::Receiver<Option<Arc<DepMap>>>,
    dir: PathBuf,
}

impl OptimizedDeps {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn ready(&self) -> Arc<DepMap> {
        let mut rx = self.rx.clone();
        loop {
            if let Some(map) = rx.borrow().clone() {
                return map;
            }
            if rx.changed().await.is_err() {
                return Arc::new(DepMap::new());
            }
        }
    }

    pub fn disabled() -> Self {
        let (_tx, rx) = watch::channel(Some(Arc::new(DepMap::new())));
        OptimizedDeps { rx, dir: PathBuf::new() }
    }

    pub fn prepare(root: &Path, version: &str) -> Self {
        let dir = root.join(".oj-cache").join("deps");
        let hash = lockfile_hash(root, version);
        let (tx, rx) = watch::channel(None);

        if let Some(map) = load_manifest(&dir, &hash) {
            let _ = tx.send(Some(Arc::new(map)));
            return OptimizedDeps { rx, dir };
        }

        let root = root.to_path_buf();
        let dir_task = dir.clone();
        tokio::spawn(async move {
            let map = run_optimizer(&root, &dir_task, &hash).await.unwrap_or_default();
            let _ = tx.send(Some(Arc::new(map)));
        });
        OptimizedDeps { rx, dir }
    }
}

fn lockfile_hash(root: &Path, version: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(version.as_bytes());
    for name in ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lockb", "package.json"] {
        if let Ok(bytes) = std::fs::read(root.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(&bytes);
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn parse_metadata(v: &serde_json::Value) -> Option<DepMap> {
    let obj = v.as_object()?;
    let mut map = DepMap::new();
    for (dep, meta) in obj {
        let file = meta.get("file")?.as_str()?.to_string();
        let needs_interop = meta.get("needsInterop").and_then(serde_json::Value::as_bool).unwrap_or(true);
        map.insert(dep.clone(), DepMeta { file, needs_interop });
    }
    Some(map)
}

fn load_manifest(dir: &Path, hash: &str) -> Option<DepMap> {
    let raw = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("hash")?.as_str()? != hash {
        return None;
    }
    let map = parse_metadata(v.get("metadata")?)?;
    for m in map.values() {
        if !dir.join(&m.file).exists() {
            return None;
        }
    }
    Some(map)
}

async fn run_optimizer(root: &Path, dir: &Path, hash: &str) -> Option<DepMap> {
    let cache = root.join(".oj-cache");
    std::fs::create_dir_all(&cache).ok()?;
    let script = cache.join("optimize-deps.mjs");
    std::fs::write(&script, OPTIMIZE_JS).ok()?;
    let cfg = serde_json::json!({
        "root": root,
        "outDir": dir,
        "entries": [],
        "include": ["react/jsx-dev-runtime"],
    })
    .to_string();
    let out = tokio::process::Command::new("node")
        .arg(&script)
        .arg(&cfg)
        .current_dir(root)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let metadata = v.get("metadata")?;
    let map = parse_metadata(metadata)?;
    let manifest = serde_json::json!({ "hash": hash, "metadata": metadata });
    let _ = std::fs::write(dir.join("manifest.json"), manifest.to_string());
    Some(map)
}
