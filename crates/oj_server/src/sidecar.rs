// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

pub const SIDECAR_JS: &str = include_str!("assets/tailwind-sidecar.mjs");
pub const PREPROCESS_JS: &str = include_str!("assets/css-preprocess.mjs");
pub const SVELTE_COMPILE_JS: &str = include_str!("assets/svelte-compile.mjs");

#[inline]
pub fn is_svelte(url: &str) -> bool {
    url.split('?').next().unwrap_or(url).ends_with(".svelte")
}

#[inline]
pub fn is_less(url: &str) -> bool {
    url.split('?').next().unwrap_or(url).ends_with(".less")
}

#[inline]
pub fn is_stylus(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    f.ends_with(".styl") || f.ends_with(".stylus")
}

#[inline]
pub fn is_tailwind_css(source: &str) -> bool {
    source.contains("@import \"tailwindcss\"")
        || source.contains("@import 'tailwindcss'")
        || source.contains("@tailwind ")
        || source.contains("@theme")
}

pub struct Sidecar {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>,
    counter: AtomicU64,
    base: String,
    _child: tokio::process::Child,
}

impl Sidecar {
    pub async fn spawn(root: &Path) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        Self::spawn_named(root, "tailwind-sidecar.mjs", SIDECAR_JS).await
    }

    pub async fn spawn_preprocess(root: &Path) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        Self::spawn_named(root, "css-preprocess.mjs", PREPROCESS_JS).await
    }

    pub async fn spawn_svelte(root: &Path) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        Self::spawn_named(root, "svelte-compile.mjs", SVELTE_COMPILE_JS).await
    }

    async fn spawn_named(
        root: &Path,
        name: &str,
        js: &str,
    ) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        let script = oj_cache::cache_root(&root).join(name);
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, js)?;

        let mut child = tokio::process::Command::new("node")
            .arg(&script)
            .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
            .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot spawn node for css sidecar: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let sidecar = std::sync::Arc::new(Sidecar {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            base: root.display().to_string(),
            _child: child,
        });

        let reader_ref = std::sync::Arc::clone(&sidecar);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let Some(id) = msg["id"].as_u64() else {
                    continue;
                };
                let result = match msg["css"].as_str() {
                    Some(css) => Ok(css.to_string()),
                    None => Err(msg["error"].as_str().unwrap_or("sidecar error").to_string()),
                };
                if let Some(tx) = reader_ref.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
        });
        Ok(sidecar)
    }

    pub async fn compile(&self, css: &str, from: &str) -> Result<String, String> {
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let request = serde_json::json!({ "id": id, "base": self.base, "css": css, "from": from });
        {
            let mut stdin = self.stdin.lock().await;
            if stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .is_err()
            {
                return Err("tailwind sidecar died (is tailwindcss installed?)".into());
            }
        }
        match tokio::time::timeout(std::time::Duration::from_secs(20), rx).await {
            Ok(Ok(result)) => result,
            _ => Err("tailwind sidecar timed out".into()),
        }
    }
}
