// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use oj_resolver::OjResolver;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

pub const PLUGIN_HOST_JS: &str = include_str!("assets/plugin-host.mjs");
pub const VITE_EXTRACT_JS: &str = include_str!("assets/vite-extract.mjs");

#[derive(Debug)]
pub struct EmittedFile {
    pub file_name: String,
    pub source: String,
}

/// A chunk a plugin asked oj to emit via `this.emitFile({ type: "chunk" })`.
#[derive(Debug, Clone)]
pub struct ChunkEmit {
    pub ref_id: String,
    pub id: String,
    pub name: Option<String>,
    pub file_name: Option<String>,
}

impl ChunkEmit {
    fn from_value(m: &serde_json::Value) -> Option<Self> {
        Some(Self {
            ref_id: m.get("referenceId")?.as_str()?.to_string(),
            id: m.get("id")?.as_str()?.to_string(),
            name: m.get("name").and_then(|x| x.as_str()).map(str::to_string),
            file_name: m.get("fileName").and_then(|x| x.as_str()).map(str::to_string),
        })
    }
}

#[inline]
pub fn plugins_file(root: &Path) -> Option<std::path::PathBuf> {
    ["oj.plugins.mjs", "oj.plugins.js"]
        .into_iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file())
}

pub enum PluginSource {
    OjPlugins(std::path::PathBuf),
    ViteConfig(std::path::PathBuf),
}

pub fn ssr_bridge_dir(root: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("OJ_SSR_BRIDGE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let id = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
    std::env::temp_dir().join(format!("oj-ssr-bridge-{}", &id.as_str()[..16]))
}

fn create_bridge_dir(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    true
}

pub fn remove_legacy_ssr_bridge(root: &Path) {
    let legacy = root.join(".oj-cache").join("start").join("ssr-bridge");
    if legacy != ssr_bridge_dir(root) {
        let _ = std::fs::remove_dir_all(&legacy);
    }
}

pub fn cleanup_ssr_bridge(root: &Path) {
    let _ = std::fs::remove_dir_all(ssr_bridge_dir(root));
}

pub fn disable_ssr_bridge(root: &Path) {
    let dir = ssr_bridge_dir(root);
    if !create_bridge_dir(&dir) {
        return;
    }
    let _ = std::fs::write(dir.join("disabled"), b"1");
}

#[cfg(unix)]
fn mkfifo_at(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::mkfifo(c.as_ptr(), 0o600) == 0 }
}

pub fn prepare_ssr_bridge(root: &Path) -> Option<PathBuf> {
    remove_legacy_ssr_bridge(root);
    let dir = ssr_bridge_dir(root);
    if !create_bridge_dir(&dir) {
        return None;
    }
    let _ = std::fs::remove_file(dir.join("disabled"));
    let _ = std::fs::remove_file(dir.join("ready"));
    #[cfg(unix)]
    {
        for name in ["req.fifo", "rep.fifo"] {
            let p = dir.join(name);
            let _ = std::fs::remove_file(&p);
            if !mkfifo_at(&p) {
                disable_ssr_bridge(root);
                return None;
            }
        }
        Some(dir)
    }
    #[cfg(not(unix))]
    {
        disable_ssr_bridge(root);
        None
    }
}

pub fn ensure_ssr_bridge(root: &Path) -> Option<PathBuf> {
    let dir = ssr_bridge_dir(root);
    if dir.join("req.fifo").exists()
        && dir.join("rep.fifo").exists()
        && !dir.join("disabled").exists()
    {
        return Some(dir);
    }
    prepare_ssr_bridge(root)
}

static VITE_CONFIG_OVERRIDE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_vite_config_override(path: std::path::PathBuf) {
    let _ = VITE_CONFIG_OVERRIDE.set(path);
}

#[inline]
pub fn vite_config_file(root: &Path) -> Option<std::path::PathBuf> {
    if let Some(p) = VITE_CONFIG_OVERRIDE.get() {
        return p.is_file().then(|| p.clone());
    }
    [
        "vite.config.ts",
        "vite.config.mts",
        "vite.config.mjs",
        "vite.config.js",
    ]
    .into_iter()
    .map(|f| root.join(f))
    .find(|p| p.is_file())
}

#[inline]
pub fn plugin_source(root: &Path) -> Option<PluginSource> {
    if VITE_CONFIG_OVERRIDE.get().is_some() {
        return vite_config_file(root).map(PluginSource::ViteConfig);
    }
    if let Some(p) = plugins_file(root) {
        return Some(PluginSource::OjPlugins(p));
    }
    vite_config_file(root).map(PluginSource::ViteConfig)
}

#[derive(Debug, Default)]
pub struct ViteValues {
    pub base: Option<String>,
    pub public_dir: Option<String>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub hmr_disabled: bool,
    pub fs_allow: Option<Vec<String>>,
    pub define: Option<serde_json::Map<String, serde_json::Value>>,
    pub alias: Option<serde_json::Map<String, serde_json::Value>>,
    pub headers: Option<serde_json::Map<String, serde_json::Value>>,
    pub rollup_options: Option<serde_json::Value>,
    pub assets_inline_limit: Option<u64>,
    pub proxy: Option<serde_json::Value>,
    pub dedupe: Option<Vec<String>>,
    pub optimize_deps: Option<serde_json::Value>,
}

pub fn extract_vite_values(root: &Path) -> Option<ViteValues> {
    if plugins_file(root).is_some() {
        return None;
    }
    let vite = vite_config_file(root)?;
    let store = oj_cache::config_extract::ConfigExtractStore::new(
        root,
        &format!(
            "{}:{}",
            env!("CARGO_PKG_VERSION"),
            blake3::hash(VITE_EXTRACT_JS.as_bytes()).to_hex()
        ),
    );
    if let Some(hit) = store.lookup(&vite, "serve", "development") {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&hit.output) {
            if !hit.stderr.is_empty() {
                eprint!("{}", hit.stderr);
            }
            crate::boot_phase("vite-extract cache hit");
            return Some(parse_vite_values(&json));
        }
    }
    let cache = oj_cache::cache_root(root);
    let _ = std::fs::create_dir_all(&cache);
    let script = cache.join("oj-vite-extract.mjs");
    std::fs::write(&script, VITE_EXTRACT_JS).ok()?;
    let out = std::process::Command::new("node")
        .arg(&script)
        .arg(&vite)
        .arg(root)
        .arg("serve")
        .arg("development")
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
        .current_dir(root)
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    if json.get("__ok").and_then(|v| v.as_bool()) == Some(true) {
        let deps: Vec<PathBuf> = json
            .get("__deps")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_str().map(PathBuf::from))
                    .collect()
            })
            .unwrap_or_default();
        store.store(
            &vite,
            "serve",
            "development",
            &deps,
            &String::from_utf8_lossy(&out.stdout),
            &stderr,
        );
    }
    crate::boot_phase("vite-extract cache miss (subprocess ran)");
    Some(parse_vite_values(&json))
}

#[inline]
fn parse_vite_values(json: &serde_json::Value) -> ViteValues {
    ViteValues {
        base: json
            .get("base")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        public_dir: json
            .get("publicDir")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        port: json.get("port").and_then(|v| v.as_u64()).map(|p| p as u16),
        host: json
            .get("host")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        hmr_disabled: json.get("hmr").and_then(|v| v.as_bool()) == Some(false),
        fs_allow: json.get("fsAllow").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        define: json.get("define").and_then(|v| v.as_object()).cloned(),
        alias: json.get("alias").and_then(|v| v.as_object()).cloned(),
        headers: json.get("headers").and_then(|v| v.as_object()).cloned(),
        rollup_options: json.get("rollupOptions").filter(|v| !v.is_null()).cloned(),
        assets_inline_limit: json.get("assetsInlineLimit").and_then(|v| v.as_u64()),
        proxy: json.get("proxy").filter(|v| !v.is_null()).cloned(),
        dedupe: json.get("dedupe").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        optimize_deps: json.get("optimizeDeps").filter(|v| !v.is_null()).cloned(),
    }
}

#[inline]
pub fn adopt_vite_config_values(config: &mut oj_config::OjConfig, root: &Path) {
    let Some(v) = extract_vite_values(root) else {
        return;
    };
    merge_vite_values(config, v);
}

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
    if v.hmr_disabled {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.hmr.is_none() {
            sc.hmr = Some(oj_config::HmrConfig::Toggle(false));
        }
    }
    if v.port.is_some() || v.host.is_some() || v.headers.is_some() || v.fs_allow.is_some() {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.port.is_none() {
            sc.port = v.port;
        }
        if sc.host.is_none() {
            sc.host = v.host;
        }
        if sc.fs.is_none() {
            if let Some(allow) = v.fs_allow {
                sc.fs = Some(oj_config::FsConfig {
                    allow: Some(allow),
                    strict: None,
                    deny: None,
                });
            }
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
    if let Some(ro) = v.rollup_options {
        let build = config.build.get_or_insert_with(Default::default);
        if build.rollup_options.is_none() && build.rolldown_options.is_none() {
            build.rollup_options = Some(ro);
        }
    }
    if let Some(limit) = v.assets_inline_limit {
        let build = config.build.get_or_insert_with(Default::default);
        build.assets_inline_limit.get_or_insert(limit);
    }
    if let Some(proxy) = v.proxy {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.proxy.is_none() {
            if let Ok(map) = serde_json::from_value::<
                std::collections::BTreeMap<String, oj_config::ProxyEntry>,
            >(proxy)
            {
                if !map.is_empty() {
                    sc.proxy = Some(map);
                }
            }
        }
    }
    if let Some(dedupe) = v.dedupe {
        if !dedupe.is_empty() {
            let rc = config.resolve.get_or_insert_with(Default::default);
            rc.dedupe.get_or_insert(dedupe);
        }
    }
    if let Some(od) = v.optimize_deps {
        if config.optimize_deps.is_none() {
            if let Ok(parsed) = serde_json::from_value::<oj_config::OptimizeDepsConfig>(od) {
                config.optimize_deps = Some(parsed);
            }
        }
    }
}

pub struct PluginHost {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Option<String>, String>>>>,
    counter: AtomicU64,
    ws_out: Mutex<Option<tokio::sync::broadcast::Sender<String>>>,
    // In an Option so it can be taken + killed explicitly (the reader task holds
    // an Arc clone, so dropping the caller's Arc alone never triggers kill_on_drop).
    child: Mutex<Option<tokio::process::Child>>,
}

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
            let dir = if importer.is_empty() {
                root.to_path_buf()
            } else {
                Path::new(importer)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf())
            };
            match resolver.resolve(&dir, source) {
                Ok(p) => serde_json::json!({ "rpcReply": rpc, "result": p.display().to_string() }),
                Err(_) => serde_json::json!({ "rpcReply": rpc, "result": null }),
            }
        }
        "moduleInfo" => {
            let id = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let path = Path::new(id);
            match std::fs::read_to_string(path) {
                Ok(src) => {
                    let dir = path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| root.to_path_buf());
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
        other => {
            serde_json::json!({ "rpcReply": rpc, "error": format!("unknown ctx method: {other}") })
        }
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
        let script = oj_cache::cache_root(root).join("plugin-host.mjs");
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, PLUGIN_HOST_JS)?;

        let mut child = tokio::process::Command::new("node")
            .arg(&script)
            .arg(plugins_file)
            .arg(config_json)
            .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot spawn node for plugin host: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let host = std::sync::Arc::new(PluginHost {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            ws_out: Mutex::new(None),
            child: Mutex::new(Some(child)),
        });

        let resolver = std::sync::Arc::new(OjResolver::new(root));
        let root_buf: PathBuf = root.to_path_buf();
        let reader_ref = std::sync::Arc::clone(&host);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if let Some(rpc) = msg["rpc"].as_u64() {
                    let method = msg["method"].as_str().unwrap_or("").to_string();
                    let args = msg["args"].as_array().cloned().unwrap_or_default();
                    handle_ctx_rpc(rpc, &method, &args, &resolver, &root_buf, &reader_ref.stdin)
                        .await;
                    continue;
                }
                if let Some(ws) = msg.get("ojWs") {
                    let tx = reader_ref.ws_out.lock().unwrap().clone();
                    if let Some(tx) = tx {
                        let payload = match ws.get("event").and_then(|e| e.as_str()) {
                            Some(event) => serde_json::json!({
                                "type": "custom",
                                "event": event,
                                "data": ws.get("data").cloned().unwrap_or(serde_json::Value::Null),
                            })
                            .to_string(),
                            None => ws
                                .get("data")
                                .filter(|d| d.is_object())
                                .map(|d| d.to_string())
                                .unwrap_or_default(),
                        };
                        if !payload.is_empty() {
                            let _ = tx.send(payload);
                        }
                    }
                    continue;
                }
                let Some(id) = msg["id"].as_u64() else {
                    continue;
                };
                let result = if let Some(err) = msg.get("error").and_then(|e| e.as_str()) {
                    Err(err.to_string())
                } else {
                    Ok(msg
                        .get("result")
                        .and_then(|r| r.as_str())
                        .map(str::to_string))
                };
                if let Some(tx) = reader_ref.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
        });
        Ok(host)
    }

    async fn call(&self, hook: &str, args: &[&str]) -> Result<Option<String>, String> {
        let req_id = self.counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req_id, tx);
        let request = serde_json::json!({ "id": req_id, "hook": hook, "args": args });
        {
            let mut stdin = self.stdin.lock().await;
            if stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .is_err()
            {
                self.pending.lock().unwrap().remove(&req_id);
                return Err("plugin host died".into());
            }
        }
        match tokio::time::timeout(std::time::Duration::from_secs(20), rx).await {
            Ok(Ok(result)) => result,
            _ => Err("plugin host timed out".into()),
        }
    }

    pub async fn transform(
        &self,
        code: &str,
        id: &str,
        resolved: &str,
    ) -> Result<(String, Vec<String>, Vec<String>, Vec<ChunkEmit>), String> {
        let Some(raw) = self.call("transform", &[code, id, resolved]).await? else {
            return Ok((code.to_string(), Vec::new(), Vec::new(), Vec::new()));
        };
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => {
                let out = v
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or(code)
                    .to_string();
                let str_array = |key: &str| {
                    v.get(key)
                        .and_then(|w| w.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let chunks = v
                    .get("emittedChunks")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(ChunkEmit::from_value).collect())
                    .unwrap_or_default();
                Ok((out, str_array("watchFiles"), str_array("maps"), chunks))
            }
            Err(_) => Ok((raw, Vec::new(), Vec::new(), Vec::new())),
        }
    }

    pub async fn seed_chunk_names(&self, map_json: &str) -> Result<Option<String>, String> {
        self.call("seedChunkNames", &[map_json]).await
    }

    #[inline]
    pub async fn has_module_parsed(&self) -> bool {
        matches!(self.call("hasModuleParsed", &[]).await, Ok(Some(s)) if s == "true")
    }

    #[inline]
    pub async fn module_parsed(&self, id: &str) -> Result<(), String> {
        self.call("replayModuleParsed", &[id]).await.map(|_| ())
    }

    #[inline]
    pub async fn resolve_id(&self, source: &str, importer: &str) -> Result<Option<String>, String> {
        self.call("resolveId", &[source, importer]).await
    }

    #[inline]
    pub async fn load(&self, id: &str) -> Result<Option<String>, String> {
        self.call("load", &[id]).await
    }

    #[inline]
    pub async fn handle_hot_update(
        &self,
        file: &str,
        timestamp: u64,
        change_type: &str,
        modules_json: &str,
    ) -> Result<Option<String>, String> {
        self.call(
            "handleHotUpdate",
            &[file, &timestamp.to_string(), change_type, modules_json],
        )
        .await
    }

    #[inline]
    pub async fn transform_index_html(&self, html: &str) -> Result<String, String> {
        Ok(self
            .call("transformIndexHtml", &[html])
            .await?
            .unwrap_or_else(|| html.to_string()))
    }

    #[inline]
    pub async fn build_start(&self) -> Result<Vec<ChunkEmit>, String> {
        let Some(raw) = self.call("buildStart", &[]).await? else {
            return Ok(Vec::new());
        };
        let chunks = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| {
                v.get("emittedChunks")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(ChunkEmit::from_value).collect())
            })
            .unwrap_or_default();
        Ok(chunks)
    }

    #[inline]
    pub async fn build_end(&self) -> Result<(), String> {
        self.call("buildEnd", &[]).await.map(|_| ())
    }

    #[inline]
    pub async fn render_start(&self) -> Result<(), String> {
        self.call("renderStart", &[]).await.map(|_| ())
    }

    #[inline]
    pub async fn watch_change(&self, file: &str, event: &str) -> Result<(), String> {
        self.call("watchChange", &[file, event]).await.map(|_| ())
    }

    #[inline]
    pub async fn close_bundle(&self) -> Result<(), String> {
        self.call("closeBundle", &[]).await.map(|_| ())
    }

    #[inline]
    pub async fn watch_files(&self) -> Result<Vec<String>, String> {
        let Some(json) = self.call("getWatchFiles", &[]).await? else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&json).map_err(|e| e.to_string())
    }

    #[inline]
    pub async fn has_generate_bundle(&self) -> bool {
        matches!(self.call("hasGenerateBundle", &[]).await, Ok(Some(s)) if s == "true")
    }

    #[inline]
    pub async fn generate_bundle(
        &self,
        bundle_json: &str,
        is_write: bool,
    ) -> Result<Option<String>, String> {
        self.call(
            "generateBundle",
            &[bundle_json, if is_write { "true" } else { "false" }],
        )
        .await
    }

    #[inline]
    pub async fn has_render_chunk(&self) -> bool {
        matches!(self.call("hasRenderChunk", &[]).await, Ok(Some(s)) if s == "true")
    }

    pub async fn render_chunk(
        &self,
        code: &str,
        chunk_json: &str,
    ) -> Result<Option<String>, String> {
        self.call("renderChunk", &[code, chunk_json]).await
    }

    #[inline]
    pub async fn has_write_bundle(&self) -> bool {
        matches!(self.call("hasWriteBundle", &[]).await, Ok(Some(s)) if s == "true")
    }

    #[inline]
    pub async fn write_bundle(&self, bundle_json: &str, is_write: bool) -> Result<(), String> {
        self.call(
            "writeBundle",
            &[bundle_json, if is_write { "true" } else { "false" }],
        )
        .await
        .map(|_| ())
    }

    #[inline]
    pub async fn middleware_port(&self) -> Option<u16> {
        self.call("getMiddlewarePort", &[])
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
    }

    /// Number of plugins still active after oj filters out the ones it
    /// reimplements natively (the React family). Defaults to 1 on RPC failure so
    /// an uncertain host is kept, never dropped by mistake.
    pub async fn plugin_count(&self) -> usize {
        self.call("getPluginCount", &[])
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }

    /// Env mutations made by plugin `config()` hooks in the host process (e.g.
    /// a plugin flipping a VITE_* flag). Empty on RPC failure.
    pub async fn env_delta(&self) -> std::collections::BTreeMap<String, String> {
        self.call("getEnvDelta", &[])
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Whether any active plugin has a `transform` hook. Defaults to true on RPC
    /// failure so the per-module transform pass is never skipped by mistake.
    pub async fn has_transform(&self) -> bool {
        self.call("getHasTransform", &[])
            .await
            .ok()
            .flatten()
            .map(|s| s == "true")
            .unwrap_or(true)
    }

    /// Whether any active plugin has a `load` hook. Vite runs `load` hooks before
    /// the filesystem read, so a plugin can replace an on-disk file's contents; oj
    /// gates that load-first pass on this so apps with no `load` hook pay nothing.
    /// Defaults to false on RPC failure (the fs read alone is always correct).
    pub async fn has_load(&self) -> bool {
        self.call("getHasLoad", &[])
            .await
            .ok()
            .flatten()
            .map(|s| s == "true")
            .unwrap_or(false)
    }

    /// The `filter.code` include patterns of every object-form transform hook, as
    /// regex source strings. oj gates dependency transforms on these so it only
    /// hands a dep to the transform RPC when a transform's own filter wants it.
    pub async fn dep_transform_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getDepTransformFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// Which HMR hooks any active plugin defines: (watchChange, handleHotUpdate).
    /// Defaults to (true, true) on RPC or parse failure so an HMR RPC is never
    /// skipped by mistake.
    pub async fn hmr_hooks(&self) -> (bool, bool) {
        let raw = match self.call("getHmrHooks", &[]).await {
            Ok(Some(s)) => s,
            _ => return (true, true),
        };
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => (
                v.get("watchChange")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(true),
                v.get("handleHotUpdate")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(true),
            ),
            Err(_) => (true, true),
        }
    }

    /// Kill the Node process now (used when the host has no active plugins).
    pub fn shutdown(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.start_kill();
        }
    }

    pub fn set_ws_sender(&self, tx: tokio::sync::broadcast::Sender<String>) {
        *self.ws_out.lock().unwrap() = Some(tx);
    }

    #[inline]
    pub async fn ws_message(&self, event: &str, data: &str) -> Result<(), String> {
        self.call("wsMessage", &[event, data]).await.map(|_| ())
    }

    #[inline]
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

    /// CSS that plugins (e.g. UnoCSS) routed through oj's `vite:css-post` shim.
    /// Returned as `(source_id, css)` pairs.
    pub async fn get_plugin_css(&self) -> Vec<(String, String)> {
        let Some(json) = self.call("getPluginCss", &[]).await.ok().flatten() else {
            return Vec::new();
        };
        serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| {
                v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let css = e.get("css")?.as_str()?.to_string();
                            let id = e.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            Some((id, css))
                        })
                        .collect()
                })
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod ssr_bridge_tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oj-bridge-test-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bridge_dir_defaults_outside_the_app_tree() {
        let root = Path::new("/some/app");
        let dir = ssr_bridge_dir(root);
        assert!(!dir.starts_with(root));
        assert!(dir.starts_with(std::env::temp_dir()));
        assert_eq!(dir, ssr_bridge_dir(root));
        assert_ne!(dir, ssr_bridge_dir(Path::new("/other/app")));
    }

    #[cfg(unix)]
    #[test]
    fn prepare_heals_the_legacy_in_tree_bridge_and_creates_a_private_dir() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("legacy");
        let legacy = root.join(".oj-cache").join("start").join("ssr-bridge");
        std::fs::create_dir_all(&legacy).unwrap();
        assert!(mkfifo_at(&legacy.join("req.fifo")));

        let dir = prepare_ssr_bridge(&root).expect("bridge dir");
        assert!(!legacy.exists(), "legacy in-tree bridge dir must be removed");
        assert!(!dir.starts_with(&root));
        assert!(dir.join("req.fifo").exists() && dir.join("rep.fifo").exists());
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        cleanup_ssr_bridge(&root);
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&root);
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
            hmr_disabled: false,
            fs_allow: None,
            define: None,
            alias: None,
            headers: None,
            rollup_options: None,
            assets_inline_limit: None,
            proxy: None,
            dedupe: None,
            optimize_deps: None,
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
            hmr_disabled: false,
            fs_allow: None,
            define: None,
            alias: None,
            headers: None,
            rollup_options: None,
            assets_inline_limit: None,
            proxy: None,
            dedupe: None,
            optimize_deps: None,
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.base.as_deref(), Some("/oj-base/"));
        assert_eq!(config.public_dir.as_deref(), Some("my-public"));
    }

    #[test]
    fn merge_adopts_proxy() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            proxy: Some(serde_json::json!({
                "/api": "http://localhost:3000",
                "/ws": { "target": "http://localhost:4000", "changeOrigin": true }
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let proxy = config.server.unwrap().proxy.unwrap();
        assert_eq!(proxy.get("/api").unwrap().target(), "http://localhost:3000");
        assert_eq!(proxy.get("/ws").unwrap().target(), "http://localhost:4000");
        assert!(proxy.get("/ws").unwrap().change_origin());
    }

    #[test]
    fn merge_adopts_rollup_options() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            rollup_options: Some(
                serde_json::json!({ "output": { "entryFileNames": "x/[name].js" } }),
            ),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let ro = oj_config::rolldown_options(&config).unwrap();
        assert_eq!(
            ro.pointer("/output/entryFileNames")
                .and_then(|v| v.as_str()),
            Some("x/[name].js")
        );
    }
}
