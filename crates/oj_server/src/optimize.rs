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
        OptimizedDeps {
            rx,
            dir: PathBuf::new(),
        }
    }

    pub fn prepare(root: &Path, version: &str, input: OptimizeInput) -> Self {
        let dir = oj_cache::cache_root(&root).join("deps");
        let hash = lockfile_hash(root, version, &input);
        let (tx, rx) = watch::channel(None);

        if let Some(map) = load_manifest(&dir, &hash) {
            let _ = tx.send(Some(Arc::new(map)));
            return OptimizedDeps { rx, dir };
        }

        let root = root.to_path_buf();
        let dir_task = dir.clone();
        tokio::spawn(async move {
            let map = run_optimizer(&root, &dir_task, &hash, &input)
                .await
                .unwrap_or_default();
            let _ = tx.send(Some(Arc::new(map)));
        });
        OptimizedDeps { rx, dir }
    }
}

/// Dependency-optimizer inputs derived from the resolved config
/// (`optimizeDeps.include/exclude/entries`, `resolve.dedupe`, `resolve.alias`).
#[derive(Default, Clone)]
pub struct OptimizeInput {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub entries: Vec<String>,
    pub dedupe: Vec<String>,
    pub alias: Vec<(String, String)>,
}

fn lockfile_hash(root: &Path, version: &str, input: &OptimizeInput) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(version.as_bytes());
    for name in [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
        "package.json",
    ] {
        if let Ok(bytes) = std::fs::read(root.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(&bytes);
        }
    }
    // Fold the optimizer config into the key so include/exclude/entries/dedupe/alias
    // changes invalidate a stale prebundle.
    for s in input
        .include
        .iter()
        .chain(&input.exclude)
        .chain(&input.entries)
        .chain(&input.dedupe)
    {
        hasher.update(b"\0");
        hasher.update(s.as_bytes());
    }
    for (find, replacement) in &input.alias {
        hasher.update(b"\0a");
        hasher.update(find.as_bytes());
        hasher.update(b"=");
        hasher.update(replacement.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn parse_metadata(v: &serde_json::Value) -> Option<DepMap> {
    let obj = v.as_object()?;
    let mut map = DepMap::new();
    for (dep, meta) in obj {
        let file = meta.get("file")?.as_str()?.to_string();
        let needs_interop = meta
            .get("needsInterop")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        map.insert(
            dep.clone(),
            DepMeta {
                file,
                needs_interop,
            },
        );
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

async fn run_optimizer(
    root: &Path,
    dir: &Path,
    hash: &str,
    input: &OptimizeInput,
) -> Option<DepMap> {
    let cache = oj_cache::cache_root(&root);
    std::fs::create_dir_all(&cache).ok()?;
    let script = cache.join("optimize-deps.mjs");
    std::fs::write(&script, OPTIMIZE_JS).ok()?;
    // react/jsx-dev-runtime is always prebundled (oj injects the dev JSX runtime);
    // merge it with any user optimizeDeps.include.
    let mut include = vec!["react/jsx-dev-runtime".to_string()];
    for dep in &input.include {
        if !include.contains(dep) {
            include.push(dep.clone());
        }
    }
    let alias: Vec<[&str; 2]> = input
        .alias
        .iter()
        .map(|(f, r)| [f.as_str(), r.as_str()])
        .collect();
    let cfg = serde_json::json!({
        "root": root,
        "outDir": dir,
        "entries": input.entries,
        "include": include,
        "exclude": input.exclude,
        "dedupe": input.dedupe,
        "alias": alias,
    })
    .to_string();
    let out = tokio::process::Command::new("node")
        .arg(&script)
        .arg(&cfg)
        .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
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
