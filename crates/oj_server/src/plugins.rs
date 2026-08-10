// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Persistent Node plugin host: runs Vite/Rollup-style plugin `transform` hooks
//! against the compile pipeline. Same shape as the Tailwind sidecar — JSON
//! lines over stdio with correlation ids and a background reader, so many
//! transforms can be in flight and a cancelled caller simply drops its slot.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

pub const PLUGIN_HOST_JS: &str = include_str!("assets/plugin-host.mjs");

/// The convention: a `oj.plugins.{mjs,js}` at the app root default-exports an
/// array of plugins. Returns its path if one exists.
pub fn plugins_file(root: &Path) -> Option<std::path::PathBuf> {
    ["oj.plugins.mjs", "oj.plugins.js"]
        .into_iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file())
}

pub struct PluginHost {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>,
    counter: AtomicU64,
    _child: tokio::process::Child,
}

impl PluginHost {
    pub async fn spawn(root: &Path, plugins_file: &Path) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        let script = root.join(".oj-cache").join("plugin-host.mjs");
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, PLUGIN_HOST_JS)?;

        let mut child = tokio::process::Command::new("node")
            .arg(&script)
            .arg(plugins_file)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot spawn node for plugin host: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let host = std::sync::Arc::new(PluginHost {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            _child: child,
        });

        let reader_ref = std::sync::Arc::clone(&host);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let Some(id) = msg["id"].as_u64() else { continue };
                let result = match msg["code"].as_str() {
                    Some(code) => Ok(code.to_string()),
                    None => Err(msg["error"].as_str().unwrap_or("plugin host error").to_string()),
                };
                if let Some(tx) = reader_ref.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
        });
        Ok(host)
    }

    /// Run the plugins' `transform` hooks on `code` for module `id`, returning
    /// the (possibly) transformed code.
    pub async fn transform(&self, code: &str, id: &str) -> Result<String, String> {
        let req_id = self.counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req_id, tx);
        let request = serde_json::json!({ "id": req_id, "code": code, "path": id });
        {
            let mut stdin = self.stdin.lock().await;
            if stdin.write_all(format!("{request}\n").as_bytes()).await.is_err() {
                self.pending.lock().unwrap().remove(&req_id);
                return Err("plugin host died".into());
            }
        }
        match tokio::time::timeout(std::time::Duration::from_secs(20), rx).await {
            Ok(Ok(result)) => result,
            _ => Err("plugin host timed out".into()),
        }
    }
}
