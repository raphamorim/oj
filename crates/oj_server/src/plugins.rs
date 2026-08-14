// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Persistent Node plugin host: runs Vite/Rollup-style plugin hooks against the
//! compile pipeline. JSON lines over stdio with correlation ids and a
//! background reader, so many transforms can be in flight at once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use oj_resolver::OjResolver;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

pub const PLUGIN_HOST_JS: &str = include_str!("assets/plugin-host.mjs");
pub const VITE_EXTRACT_JS: &str = include_str!("assets/vite-extract.mjs");

/// A file a plugin asked to emit via `this.emitFile` (asset form only). The
/// build collects these after `buildEnd` and writes them to the output dir.
#[derive(Debug)]
pub struct EmittedFile {
    pub file_name: String,
    pub source: String,
}

/// A `oj.plugins.{mjs,js}` at the app root default-exports a plugin array.
/// Returns its path if one exists.
pub fn plugins_file(root: &Path) -> Option<std::path::PathBuf> {
    ["oj.plugins.mjs", "oj.plugins.js"]
        .into_iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file())
}

/// Where a build's plugins come from, and in what shape the host should read
/// them: oj's own `oj.plugins.*` (a plugin array) or an app's `vite.config.*`
/// (the default export's `.plugins`).
pub enum PluginSource {
    /// An `oj.plugins.{mjs,js}`: default-exports a plugin array.
    OjPlugins(std::path::PathBuf),
    /// A compiled `vite.config.{ts,js,mjs}`; read plugins from `default.plugins`.
    ViteConfig(std::path::PathBuf),
}

/// The app's `vite.config.{ts,mts,mjs,js}`, if one exists.
pub fn vite_config_file(root: &Path) -> Option<std::path::PathBuf> {
    ["vite.config.ts", "vite.config.mts", "vite.config.mjs", "vite.config.js"]
        .into_iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file())
}

/// Resolve the app's plugin source. `oj.plugins.*` wins; otherwise the raw
/// `vite.config.{ts,js,mjs}` path (the Node host bundles it with the app's own
/// esbuild, local imports inlined and deps external, before reading `plugins`).
pub fn plugin_source(root: &Path) -> Option<PluginSource> {
    if let Some(p) = plugins_file(root) {
        return Some(PluginSource::OjPlugins(p));
    }
    vite_config_file(root).map(PluginSource::ViteConfig)
}

/// Config values lifted out of an app's `vite.config` (the subset oj honors).
#[derive(Debug, Default)]
pub struct ViteValues {
    pub base: Option<String>,
    pub public_dir: Option<String>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub define: Option<serde_json::Map<String, serde_json::Value>>,
    pub alias: Option<serde_json::Map<String, serde_json::Value>>,
    pub headers: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Extract `base`, `server.port`/`host`, and `define` from an app's
/// `vite.config` via a one-shot Node run (same loader the plugin host uses).
/// Returns `None` when `oj.plugins.*` is present (oj.config supplies values),
/// when there is no `vite.config`, or when extraction fails (e.g. a config
/// whose plugins assert during evaluation); callers fall back to defaults.
pub fn extract_vite_values(root: &Path) -> Option<ViteValues> {
    if plugins_file(root).is_some() {
        return None;
    }
    let vite = vite_config_file(root)?;
    let cache = root.join(".oj-cache");
    let _ = std::fs::create_dir_all(&cache);
    let script = cache.join("oj-vite-extract.mjs");
    std::fs::write(&script, VITE_EXTRACT_JS).ok()?;
    let out = std::process::Command::new("node")
        .arg(&script)
        .arg(&vite)
        .arg(root)
        .arg("serve")
        .arg("development")
        .current_dir(root)
        .output()
        .ok()?;
    if !out.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(parse_vite_values(&json))
}

/// Parse the JSON that `vite-extract.mjs` prints into typed values (pure, so it
/// is unit-testable without spawning node).
fn parse_vite_values(json: &serde_json::Value) -> ViteValues {
    ViteValues {
        base: json.get("base").and_then(|v| v.as_str()).map(str::to_string),
        public_dir: json.get("publicDir").and_then(|v| v.as_str()).map(str::to_string),
        port: json.get("port").and_then(|v| v.as_u64()).map(|p| p as u16),
        host: json.get("host").and_then(|v| v.as_str()).map(str::to_string),
        define: json.get("define").and_then(|v| v.as_object()).cloned(),
        alias: json.get("alias").and_then(|v| v.as_object()).cloned(),
        headers: json.get("headers").and_then(|v| v.as_object()).cloned(),
    }
}

/// Merge an app's `vite.config` values into `config` for any field oj.config
/// left unset (`base`, `server.port`/`host`, `define`, `resolve.alias`).
/// No-op unless the app is vite.config-configured (see [`extract_vite_values`]).
/// Precedence stays CLI > oj.config > vite.config > default. Shared by the dev
/// server and the production build.
pub fn adopt_vite_config_values(config: &mut oj_config::OjConfig, root: &Path) {
    let Some(v) = extract_vite_values(root) else {
        return;
    };
    merge_vite_values(config, v);
}

/// Merge extracted vite values into `config` for any field oj.config left unset
/// (config always wins). Pure, so it is unit-testable.
fn merge_vite_values(config: &mut oj_config::OjConfig, v: ViteValues) {
    if config.base.is_none() {
        config.base = v.base;
    }
    if config.public_dir.is_none() {
        config.public_dir = v.public_dir;
    }
    if let Some(vdef) = v.define {
        let def = config.define.get_or_insert_with(Default::default);
        for (k, val) in vdef {
            def.entry(k).or_insert(val);
        }
    }
    if v.port.is_some() || v.host.is_some() || v.headers.is_some() {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.port.is_none() {
            sc.port = v.port;
        }
        if sc.host.is_none() {
            sc.host = v.host;
        }
        if sc.headers.is_none() {
            if let Some(vheaders) = v.headers {
                let map = vheaders
                    .into_iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k, s.to_string())))
                    .collect::<std::collections::BTreeMap<_, _>>();
                if !map.is_empty() {
                    sc.headers = Some(map);
                }
            }
        }
    }
    if let Some(valias) = v.alias {
        if !valias.is_empty() {
            let rc = config.resolve.get_or_insert_with(Default::default);
            let map = rc.alias.get_or_insert_with(Default::default);
            for (find, replacement) in valias {
                if let Some(s) = replacement.as_str() {
                    map.entry(find).or_insert_with(|| s.to_string());
                }
            }
        }
    }
}

pub struct PluginHost {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Option<String>, String>>>>,
    counter: AtomicU64,
    _child: tokio::process::Child,
}

/// Handle a plugin-context RPC (Node to Rust) and write the reply to `stdin`.
/// Currently just `resolve`, so plugins' `this.resolve` uses oj's own resolver
/// (tsconfig aliases and all), keeping plugin resolution consistent with oj.
async fn handle_ctx_rpc(
    rpc: u64,
    method: &str,
    args: &[serde_json::Value],
    resolver: &OjResolver,
    root: &Path,
    stdin: &tokio::sync::Mutex<tokio::process::ChildStdin>,
) {
    let reply = match method {
        "resolve" => {
            let source = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let importer = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            // Rollup's this.resolve takes the importer file; oj's resolver takes
            // its directory. Empty importer: resolve from the app root.
            let dir = if importer.is_empty() {
                root.to_path_buf()
            } else {
                Path::new(importer).parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf())
            };
            match resolver.resolve(&dir, source) {
                Ok(p) => serde_json::json!({ "rpcReply": rpc, "result": p.display().to_string() }),
                Err(_) => serde_json::json!({ "rpcReply": rpc, "result": null }),
            }
        }
        // Back the plugin ModuleInfo (this.load / getModuleInfo): read the
        // module, compile it (TS/JSX stripped) for its code + static imports,
        // and resolve each import to an absolute id via oj's resolver.
        "moduleInfo" => {
            let id = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let path = Path::new(id);
            match std::fs::read_to_string(path) {
                Ok(src) => {
                    let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
                    // Non-JS (css/json/...) modules can't be compiled; hand back
                    // the raw source with no imports rather than erroring.
                    let (code, imports) = match oj_compiler::compile(
                        path,
                        &src,
                        &oj_compiler::CompileOptions::prod(),
                    ) {
                        Ok(out) => (out.code, out.imports),
                        Err(_) => (src, Vec::new()),
                    };
                    let imported_ids: Vec<String> = imports
                        .iter()
                        .map(|spec| {
                            resolver
                                .resolve(&dir, spec)
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|_| spec.clone())
                        })
                        .collect();
                    serde_json::json!({
                        "rpcReply": rpc,
                        "result": { "id": id, "code": code, "importedIds": imported_ids },
                    })
                }
                Err(_) => serde_json::json!({ "rpcReply": rpc, "result": null }),
            }
        }
        other => serde_json::json!({ "rpcReply": rpc, "error": format!("unknown ctx method: {other}") }),
    };
    let mut stdin = stdin.lock().await;
    let _ = stdin.write_all(format!("{reply}\n").as_bytes()).await;
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
            // Never outlive oj: a one-shot build drops the host when done, and
            // the dev server drops it on shutdown. Prevents leaked node procs.
            .kill_on_drop(true)
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

        let resolver = std::sync::Arc::new(OjResolver::new(root));
        let root_buf: PathBuf = root.to_path_buf();
        let reader_ref = std::sync::Arc::clone(&host);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                // A plugin-context call (this.resolve, ...) coming back from Node.
                if let Some(rpc) = msg["rpc"].as_u64() {
                    let method = msg["method"].as_str().unwrap_or("").to_string();
                    let args = msg["args"].as_array().cloned().unwrap_or_default();
                    // Handled inline: the reader processes one line at a time and
                    // the sending `call()` holds no stdin lock while awaiting.
                    handle_ctx_rpc(rpc, &method, &args, &resolver, &root_buf, &reader_ref.stdin).await;
                    continue;
                }
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

    /// Run the plugins' `transform` hooks. Returns the (possibly) transformed
    /// code plus the files those hooks registered via `this.addWatchFile` (so
    /// the caller can cache + re-apply the watch across warm-cache restarts).
    /// The host replies with a `{ code, watchFiles }` JSON envelope.
    pub async fn transform(&self, code: &str, id: &str) -> Result<(String, Vec<String>), String> {
        let Some(raw) = self.call("transform", &[code, id]).await? else {
            return Ok((code.to_string(), Vec::new()));
        };
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => {
                let out = v.get("code").and_then(|c| c.as_str()).unwrap_or(code).to_string();
                let watch = v
                    .get("watchFiles")
                    .and_then(|w| w.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                Ok((out, watch))
            }
            // A host that predates the envelope (or any non-JSON) returns raw code.
            Err(_) => Ok((raw, Vec::new())),
        }
    }

    /// Whether any loaded plugin defines `moduleParsed`. Queried once so oj only
    /// replays the hook on warm-cache serves when a plugin actually observes it.
    pub async fn has_module_parsed(&self) -> bool {
        matches!(self.call("hasModuleParsed", &[]).await, Ok(Some(s)) if s == "true")
    }

    /// Replay `moduleParsed` for a module served from oj's warm cache, where the
    /// `transform` hook (which normally fires it) did not re-run.
    pub async fn module_parsed(&self, id: &str) -> Result<(), String> {
        self.call("replayModuleParsed", &[id]).await.map(|_| ())
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

    /// Run `buildStart`: the build lifecycle is starting. Side-effect hook
    /// (plugins init state / clean output); the return value is ignored.
    pub async fn build_start(&self) -> Result<(), String> {
        self.call("buildStart", &[]).await.map(|_| ())
    }

    /// Run `buildEnd`: the module graph is complete. Side-effect hook; ignored.
    pub async fn build_end(&self) -> Result<(), String> {
        self.call("buildEnd", &[]).await.map(|_| ())
    }

    /// Run `renderStart`: the output phase is beginning. Side-effect hook.
    pub async fn render_start(&self) -> Result<(), String> {
        self.call("renderStart", &[]).await.map(|_| ())
    }

    /// Run `watchChange`: a watched file changed (Rollup watch hook). `event`
    /// is `create` / `update` / `delete`. Side-effect; ignored return.
    pub async fn watch_change(&self, file: &str, event: &str) -> Result<(), String> {
        self.call("watchChange", &[file, event]).await.map(|_| ())
    }

    /// Run `closeBundle`: the very last hook, after everything is written.
    pub async fn close_bundle(&self) -> Result<(), String> {
        self.call("closeBundle", &[]).await.map(|_| ())
    }

    /// Files plugins registered via `this.addWatchFile`. The dev watcher forces
    /// a full reload when one of these changes (even a non-source file oj would
    /// otherwise ignore). Absolute paths as the plugin passed them.
    pub async fn watch_files(&self) -> Result<Vec<String>, String> {
        let Some(json) = self.call("getWatchFiles", &[]).await? else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&json).map_err(|e| e.to_string())
    }

    /// Whether any active plugin defines a `generateBundle` hook; lets the
    /// build skip serializing the whole output bundle when nothing uses it.
    pub async fn has_generate_bundle(&self) -> bool {
        matches!(self.call("hasGenerateBundle", &[]).await, Ok(Some(s)) if s == "true")
    }

    /// Run `generateBundle`: hand the output bundle (JSON keyed by fileName) to
    /// the plugins' generateBundle hooks and return the possibly-mutated bundle
    /// JSON (chunk `code` / asset `source` edits). Plugins may also `emitFile`
    /// here; those are collected via [`Self::emitted_files`].
    pub async fn generate_bundle(&self, bundle_json: &str, is_write: bool) -> Result<Option<String>, String> {
        self.call("generateBundle", &[bundle_json, if is_write { "true" } else { "false" }]).await
    }

    /// Whether any active plugin defines a `renderChunk` hook.
    pub async fn has_render_chunk(&self) -> bool {
        matches!(self.call("hasRenderChunk", &[]).await, Ok(Some(s)) if s == "true")
    }

    /// Run `renderChunk` for one chunk: chain the plugins' hooks over `code`
    /// (with the chunk's metadata as JSON) and return the final code, or `None`
    /// if unchanged.
    pub async fn render_chunk(&self, code: &str, chunk_json: &str) -> Result<Option<String>, String> {
        self.call("renderChunk", &[code, chunk_json]).await
    }

    /// Whether any active plugin defines a `writeBundle` hook.
    pub async fn has_write_bundle(&self) -> bool {
        matches!(self.call("hasWriteBundle", &[]).await, Ok(Some(s)) if s == "true")
    }

    /// Run `writeBundle` (post-write side effects); the bundle is read-only here
    /// since files are already on disk, so any return is ignored.
    pub async fn write_bundle(&self, bundle_json: &str, is_write: bool) -> Result<(), String> {
        self.call("writeBundle", &[bundle_json, if is_write { "true" } else { "false" }]).await.map(|_| ())
    }

    /// The port of the `configureServer` middleware HTTP server, if any plugin
    /// registered dev-server middleware. `None` means no middleware to consult.
    pub async fn middleware_port(&self) -> Option<u16> {
        self.call("getMiddlewarePort", &[]).await.ok().flatten().and_then(|s| s.parse().ok())
    }

    /// Collect the assets plugins emitted via `this.emitFile` during the build,
    /// so the build can write them to the output dir.
    pub async fn emitted_files(&self) -> Result<Vec<EmittedFile>, String> {
        let Some(json) = self.call("getEmittedFiles", &[]).await? else {
            return Ok(Vec::new());
        };
        let arr: Vec<serde_json::Value> = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        Ok(arr
            .into_iter()
            .filter_map(|v| {
                Some(EmittedFile {
                    file_name: v.get("fileName")?.as_str()?.to_string(),
                    source: v.get("source")?.as_str()?.to_string(),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod vite_values_tests {
    use super::*;

    #[test]
    fn parse_reads_all_fields() {
        let json = serde_json::json!({
            "base": "/app/",
            "publicDir": "/abs/shared/public",
            "port": 3010,
            "host": "0.0.0.0",
            "define": { "__X__": "1" },
            "alias": { "@": "/src" },
            "headers": { "x-a": "b" }
        });
        let v = parse_vite_values(&json);
        assert_eq!(v.base.as_deref(), Some("/app/"));
        assert_eq!(v.public_dir.as_deref(), Some("/abs/shared/public"));
        assert_eq!(v.port, Some(3010));
        assert_eq!(v.host.as_deref(), Some("0.0.0.0"));
        assert!(v.define.unwrap().contains_key("__X__"));
        assert!(v.alias.unwrap().contains_key("@"));
        assert!(v.headers.unwrap().contains_key("x-a"));
    }

    #[test]
    fn parse_tolerates_nulls_and_missing() {
        let v = parse_vite_values(&serde_json::json!({ "base": null, "port": null }));
        assert!(v.base.is_none());
        assert!(v.public_dir.is_none());
        assert!(v.port.is_none());
        assert!(v.define.is_none());
    }

    #[test]
    fn merge_adopts_only_unset_fields() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            base: Some("/vite-base/".into()),
            public_dir: Some("shared/public".into()),
            port: Some(3010),
            host: Some("localhost".into()),
            define: None,
            alias: None,
            headers: None,
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.base.as_deref(), Some("/vite-base/"));
        assert_eq!(config.public_dir.as_deref(), Some("shared/public"));
        assert_eq!(config.server.unwrap().port, Some(3010));
    }

    #[test]
    fn merge_never_overrides_config() {
        let mut config = oj_config::OjConfig::default();
        config.base = Some("/oj-base/".into());
        config.public_dir = Some("my-public".into());
        let v = ViteValues {
            base: Some("/vite-base/".into()),
            public_dir: Some("shared/public".into()),
            port: None,
            host: None,
            define: None,
            alias: None,
            headers: None,
        };
        merge_vite_values(&mut config, v);
        // config values win over vite's
        assert_eq!(config.base.as_deref(), Some("/oj-base/"));
        assert_eq!(config.public_dir.as_deref(), Some("my-public"));
    }
}
