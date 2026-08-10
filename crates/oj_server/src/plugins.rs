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
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Option<String>, String>>>>,
    counter: AtomicU64,
    _child: tokio::process::Child,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginHost")
    }
}

impl PluginHost {
    pub async fn spawn(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        let script = root.join(".oj-cache").join("plugin-host.mjs");
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, PLUGIN_HOST_JS)?;

        let mut child = tokio::process::Command::new("node")
            .arg(&script)
            .arg(plugins_file)
            .arg(config_json)
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
                // result is a string, or null (hook returned nothing), or error.
                let result = if let Some(err) = msg.get("error").and_then(|e| e.as_str()) {
                    Err(err.to_string())
                } else {
                    Ok(msg.get("result").and_then(|r| r.as_str()).map(str::to_string))
                };
                if let Some(tx) = reader_ref.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
        });
        Ok(host)
    }

    /// Call a hook with string args; `Ok(None)` means the hook returned nothing
    /// (e.g. no plugin resolved/loaded the id).
    async fn call(&self, hook: &str, args: &[&str]) -> Result<Option<String>, String> {
        let req_id = self.counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req_id, tx);
        let request = serde_json::json!({ "id": req_id, "hook": hook, "args": args });
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

    /// Run the plugins' `transform` hooks; returns the (possibly) transformed code.
    pub async fn transform(&self, code: &str, id: &str) -> Result<String, String> {
        Ok(self.call("transform", &[code, id]).await?.unwrap_or_else(|| code.to_string()))
    }

    /// Run `resolveId`; `Ok(None)` = no plugin owns this specifier.
    pub async fn resolve_id(&self, source: &str, importer: &str) -> Result<Option<String>, String> {
        self.call("resolveId", &[source, importer]).await
    }

    /// Run `load`; `Ok(None)` = no plugin provides this id.
    pub async fn load(&self, id: &str) -> Result<Option<String>, String> {
        self.call("load", &[id]).await
    }

    /// Run `handleHotUpdate`; `Ok(Some("full-reload" | "skip"))` = a plugin
    /// overrode default HMR for `file`, `Ok(None)` = proceed normally.
    pub async fn handle_hot_update(&self, file: &str, timestamp: u64) -> Result<Option<String>, String> {
        self.call("handleHotUpdate", &[file, &timestamp.to_string()]).await
    }

    /// Run `transformIndexHtml` (string / tag-array / {html,tags} forms all
    /// resolved host-side); returns the transformed HTML unchanged if no plugin
    /// touched it.
    pub async fn transform_index_html(&self, html: &str) -> Result<String, String> {
        Ok(self.call("transformIndexHtml", &[html]).await?.unwrap_or_else(|| html.to_string()))
    }
}
