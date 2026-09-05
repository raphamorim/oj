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

/// The host's `getServeInfo` report: how requests are served.
#[derive(Debug, Default, Clone, Copy)]
pub struct ServeInfo {
    /// Loopback port of the configureServer middleware stack, when any plugin
    /// registered a middleware.
    pub middleware_port: Option<u16>,
    /// Real runner-backed Vite DevEnvironments were built (the Environment-API
    /// path): documents are served by the plugin middleware.
    pub runner_environments: bool,
}

impl ServeInfo {
    /// The `{ middlewarePort, runnerEnvironments }` shape, shared by the host's
    /// `getServeInfo` RPC reply and its `{ ojServeInfo: ... }` stdout push.
    fn from_json(v: &serde_json::Value) -> ServeInfo {
        ServeInfo {
            middleware_port: v
                .get("middlewarePort")
                .and_then(|p| p.as_u64())
                .and_then(|p| u16::try_from(p).ok()),
            runner_environments: v
                .get("runnerEnvironments")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
        }
    }
}

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
    // Vite's DEFAULT_CONFIG_FILES order (constants.ts): the first that exists
    // wins, so a root with several config files picks the same one Vite does.
    [
        "vite.config.js",
        "vite.config.mjs",
        "vite.config.ts",
        "vite.config.cjs",
        "vite.config.mts",
        "vite.config.cts",
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
    /// `publicDir`: a path, or `false` (no public directory).
    pub public_dir: Option<oj_config::BoolOrString>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub hmr_disabled: bool,
    pub fs_allow: Option<Vec<String>>,
    pub fs_strict: Option<bool>,
    pub define: Option<serde_json::Map<String, serde_json::Value>>,
    pub alias: Option<serde_json::Map<String, serde_json::Value>>,
    pub headers: Option<serde_json::Map<String, serde_json::Value>>,
    pub rollup_options: Option<serde_json::Value>,
    pub assets_inline_limit: Option<u64>,
    pub proxy: Option<serde_json::Value>,
    pub dedupe: Option<Vec<String>>,
    pub optimize_deps: Option<serde_json::Value>,
    /// The `build` block as the extractor normalized it (`outDir`, `sourcemap`,
    /// `minify`, `cssCodeSplit`, `target`, `ssr`); see `extractBuild` in
    /// vite-extract.mjs for the shapes it admits.
    pub build: Option<serde_json::Value>,
    /// `oxc.jsx` as normalized by the extractor (`{ jsx: { runtime, importSource,
    /// pragma, pragmaFrag } }`), and the `esbuild.jsx*` fields for older configs.
    pub oxc: Option<serde_json::Value>,
    pub esbuild: Option<serde_json::Value>,
    /// `ssr` block as normalized by the extractor (`noExternal`/`external`
    /// lists of names, globs or `{ regex }`, or `true`; `target`).
    pub ssr: Option<serde_json::Value>,
    /// A `mode` the config file itself names (resolved only when the CLI gave none).
    pub mode: Option<String>,
    /// `resolve.{extensions,mainFields,conditions,preserveSymlinks}`.
    pub resolve: Option<serde_json::Value>,
    /// The RAW config file's own top-level `resolve` block (the resolved one
    /// above carries Vite's client-environment conditions); consulted by the
    /// Node SSR consumers when the ssr environment is runner-backed.
    pub raw_resolve: Option<serde_json::Value>,
    /// `server.{strictPort,open}` normalized to booleans (`cors` is its own field).
    pub server_flags: Option<serde_json::Value>,
    /// `css.preprocessorOptions.<lang>.additionalData` (string form).
    pub css: Option<serde_json::Value>,
    pub env_prefix: Option<Vec<String>>,
    pub env_dir: Option<String>,
    /// `server.cors` (bool or options object) and `server.allowedHosts` (true or list).
    pub cors: Option<serde_json::Value>,
    pub allowed_hosts: Option<serde_json::Value>,
    /// `preview.*` (port, host, strictPort, open, cors, allowedHosts, headers, proxy).
    pub preview: Option<serde_json::Value>,
    /// `appType` (`spa` | `mpa` | `custom`).
    pub app_type: Option<String>,
    /// `html` block (`cspNonce`).
    pub html: Option<serde_json::Value>,
}

/// Why a run of the extractor produced nothing usable, or None when it did.
///
/// Kept separate from the reporting so the classification can be tested: the
/// interesting cases are a subprocess that wrote nothing at all and one that
/// wrote something that is not JSON, and reproducing either through a real
/// `node` is harder than it is worth.
fn extraction_failure(status: std::process::ExitStatus, stdout: &[u8], parse: Option<&str>) -> Option<String> {
    match parse {
        None => None,
        Some(_) if stdout.is_empty() => Some(format!(
            "wrote nothing at all and exited with {status}"
        )),
        Some(e) => Some(format!(
            "wrote {} bytes that are not JSON ({e}) and exited with {status}",
            stdout.len()
        )),
    }
}

/// How long the config-extraction subprocess may run before it is killed. The
/// extractor runs real plugin code (config hooks) and exits itself right after
/// emitting the result, so 60 s is generous headroom for a cold first run;
/// `OJ_EXTRACT_TIMEOUT=<seconds>` raises it for configs that legitimately take
/// longer. Unbounded was worse: a config hook that opened a socket or timer
/// used to be able to wedge boot forever (Vite has no bound here, but Vite is
/// also not waiting on a subprocess).
fn extraction_timeout() -> std::time::Duration {
    extraction_timeout_from(std::env::var("OJ_EXTRACT_TIMEOUT").ok().as_deref())
}

fn extraction_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(60);
    std::time::Duration::from_secs(secs)
}

/// `Command::output()` with a deadline: `Ok(None)` means the child ran past
/// `timeout` and was killed (and reaped). Both pipes are drained on threads
/// into shared buffers for the whole wait, so a chatty child can never
/// deadlock on a full pipe — and once the child itself has exited (or been
/// killed), the drain threads are given only a short grace to reach EOF before
/// being DETACHED with whatever the buffers hold: a grandchild spawned with
/// inherited stdio keeps the pipe write-ends open indefinitely, and joining
/// unboundedly on its EOF was exactly the boot wedge `OJ_EXTRACT_TIMEOUT`
/// exists to prevent.
fn bounded_output(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out_pipe = child.stdout.take().expect("piped stdout");
    let err_pipe = child.stderr.take().expect("piped stderr");
    fn drain(mut pipe: impl Read + Send + 'static, buf: Arc<Mutex<Vec<u8>>>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(&chunk[..n]),
                }
            }
        })
    }
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let out_thread = drain(out_pipe, Arc::clone(&out_buf));
    let err_thread = drain(err_pipe, Arc::clone(&err_buf));
    // Join with a grace bound, then detach: after the child is gone, EOF on the
    // pipes belongs to whoever else inherited them (a plugin's grandchild), and
    // the caller must never wait on that. Detached threads exit on their own
    // when the last writer closes; the snapshot below is what the caller gets.
    let grace_join = |t: std::thread::JoinHandle<()>| {
        let grace_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !t.is_finished() && std::time::Instant::now() < grace_deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if t.is_finished() {
            let _ = t.join();
        }
    };
    let snapshot = |buf: &Arc<Mutex<Vec<u8>>>| -> Vec<u8> {
        buf.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    };
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            grace_join(out_thread);
            grace_join(err_thread);
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    grace_join(out_thread);
    grace_join(err_thread);
    Ok(Some(std::process::Output {
        status,
        stdout: snapshot(&out_buf),
        stderr: snapshot(&err_buf),
    }))
}

/// Evaluate the app's `vite.config` for `command` ("serve" | "build") and `mode`.
/// A config exported as a function (`defineConfig(({ command, mode }) => ...)`)
/// branches on both, so a build must be extracted as a build: evaluating it as
/// `serve`/`development` silently picks the dev branch of `base`, `define`,
/// `build.outDir` and friends in production output.
pub fn extract_vite_values(root: &Path, command: &str, mode: &str) -> Option<ViteValues> {
    extract_vite_values_with(root, command, mode, true)
}

/// `mode_explicit`: false when `mode` is only the command's default (no CLI
/// `--mode`), which lets a `mode` named in the config file win, as in Vite.
fn extract_vite_values_with(
    root: &Path,
    command: &str,
    mode: &str,
    mode_explicit: bool,
) -> Option<ViteValues> {
    if plugins_file(root).is_some() {
        return None;
    }
    // The cache is keyed per (config, command, mode); a default-mode evaluation
    // can differ from an explicit one, so it gets its own key.
    let mode_key = if mode_explicit {
        mode.to_string()
    } else {
        format!("{mode}@default")
    };
    let mode_key = mode_key.as_str();
    let vite = vite_config_file(root)?;
    let store = oj_cache::config_extract::ConfigExtractStore::new(
        root,
        &format!(
            "{}:{}:{}",
            env!("CARGO_PKG_VERSION"),
            blake3::hash(VITE_EXTRACT_JS.as_bytes()).to_hex(),
            extraction_env_hash(std::env::vars())
        ),
    );
    if let Some(hit) = store.lookup(&vite, command, mode_key) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&hit.output) {
            print_extraction_stderr(&hit.stderr);
            let _ = CONFIG_DEPS.set(hit.deps);
            crate::boot_phase("vite-extract cache hit");
            return Some(parse_vite_values(&json));
        }
    }
    let cache = oj_cache::cache_root(root);
    let _ = std::fs::create_dir_all(&cache);
    // Several extractions run concurrently at boot (route tree, server-fn
    // resolver, config values), so everything here is per call or atomic: the
    // script lands via rename (a plain write truncates it under a concurrent
    // reader's import), and the result file is unique per call (a shared name
    // is read-and-deleted by whichever caller gets there first).
    static EXTRACT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = EXTRACT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let script = cache.join("oj-vite-extract.mjs");
    if std::fs::read(&script).ok().as_deref() != Some(VITE_EXTRACT_JS.as_bytes()) {
        let tmp = cache.join(format!("oj-vite-extract-{}-{seq}.tmp.mjs", std::process::id()));
        std::fs::write(&tmp, VITE_EXTRACT_JS).ok()?;
        std::fs::rename(&tmp, &script).ok()?;
    }
    // The JSON comes back through a file, not stdout: evaluating the config
    // runs plugin code (route generators, banners) that may print to stdout.
    let result_path = cache.join(format!("oj-vite-extract-{}-{seq}.tmp.json", std::process::id()));
    let mut cmd = std::process::Command::new("node");
    cmd.arg(&script)
        .arg(&vite)
        .arg(root)
        .arg(command)
        .arg(mode)
        .arg(if mode_explicit { "explicit" } else { "default" })
        .arg(&result_path)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
        .current_dir(root);
    // Bounded: the extractor exits itself after emitting, but the config's
    // plugin code runs before that and must never wedge boot forever.
    let timeout = extraction_timeout();
    let out = match bounded_output(&mut cmd, timeout) {
        Ok(Some(out)) => out,
        Ok(None) => {
            eprintln!(
                "oj: extracting {}: the config evaluation did not finish within {}s and was killed (raise OJ_EXTRACT_TIMEOUT for slower configs)",
                vite.display(),
                timeout.as_secs()
            );
            let _ = std::fs::remove_file(&result_path);
            return None;
        }
        Err(_) => return None,
    };
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    print_extraction_stderr(&stderr);
    let raw = std::fs::read(&result_path).unwrap_or_default();
    let _ = std::fs::remove_file(&result_path);
    let parsed = serde_json::from_slice::<serde_json::Value>(&raw);
    let parse_err = parsed.as_ref().err().map(|e| e.to_string());
    if let Some(why) = extraction_failure(out.status, &raw, parse_err.as_deref()) {
        eprintln!("oj: extracting {} {why}", vite.display());
    }
    let json: serde_json::Value = parsed.ok()?;
    // The extractor reports a config that failed to evaluate as `__ok: false`
    // (having printed the cause to stderr above). That is not a config with no
    // values, so never parse it into an empty ViteValues: return None and let the
    // caller decide whether a present-but-broken vite.config is an error.
    if json.get("__ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    // Stored once, under the same (config, command, mode_key) the lookup above
    // uses: a default-mode evaluation must not also masquerade as the explicit
    // `--mode <same>` entry, whose evaluation can differ.
    let deps: Vec<PathBuf> = json
        .get("__deps")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| d.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default();
    let _ = CONFIG_DEPS.set(deps.clone());
    store.store(
        &vite,
        command,
        mode_key,
        &deps,
        &String::from_utf8_lossy(&raw),
        &stderr,
    );
    crate::boot_phase("vite-extract cache miss (subprocess ran)");
    Some(parse_vite_values(&json))
}

/// What the config extractor wrote to stderr (Vite's own notices and oj's
/// "not applied" warnings), printed once per process. The config is loaded
/// several times in a dev session (the Start route tree, server-fn resolver and
/// client bundle each adopt it, and again after a rebuild), each replaying the
/// cached stderr; Vite prints its config warnings once at startup.
fn print_extraction_stderr(stderr: &str) {
    let fresh = unseen_extraction_lines(stderr);
    if !fresh.is_empty() {
        eprint!("{fresh}");
    }
}

fn unseen_extraction_lines(stderr: &str) -> String {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(std::collections::HashSet::new);
    let mut out = String::new();
    for line in stderr.lines() {
        if line.trim().is_empty() || seen.insert(line.to_string()) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The part of the process environment a vite.config can observe while it
/// evaluates (`process.env.VITE_*` and `NODE_ENV`), hashed into the extraction
/// cache key so an env change re-evaluates the config instead of serving the
/// values computed under the old one.
pub fn extraction_env_hash(vars: impl Iterator<Item = (String, String)>) -> String {
    let mut relevant: Vec<(String, String)> = vars
        .filter(|(k, _)| k == "NODE_ENV" || k.starts_with("VITE_"))
        .collect();
    relevant.sort();
    let mut hasher = blake3::Hasher::new();
    for (k, v) in relevant {
        hasher.update(k.as_bytes());
        hasher.update(&[b'=']);
        hasher.update(v.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

/// The files the config file imported, as the extractor reported them (Vite's
/// `configFileDependencies`): the dev server restarts when one changes.
static CONFIG_DEPS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();

pub fn config_dependencies() -> &'static [PathBuf] {
    CONFIG_DEPS.get().map(Vec::as_slice).unwrap_or(&[])
}

#[inline]
fn parse_vite_values(json: &serde_json::Value) -> ViteValues {
    ViteValues {
        base: json
            .get("base")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        public_dir: match json.get("publicDir") {
            Some(serde_json::Value::String(s)) => Some(oj_config::BoolOrString::Str(s.clone())),
            Some(serde_json::Value::Bool(false)) => Some(oj_config::BoolOrString::Bool(false)),
            _ => None,
        },
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
        fs_strict: json.get("fsStrict").and_then(|v| v.as_bool()),
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
        build: json.get("build").filter(|v| !v.is_null()).cloned(),
        oxc: json.get("oxc").filter(|v| !v.is_null()).cloned(),
        esbuild: json.get("esbuild").filter(|v| !v.is_null()).cloned(),
        ssr: json.get("ssr").filter(|v| !v.is_null()).cloned(),
        mode: json.get("mode").and_then(|v| v.as_str()).map(str::to_string),
        resolve: json.get("resolve").filter(|v| !v.is_null()).cloned(),
        raw_resolve: json.get("rawResolve").filter(|v| !v.is_null()).cloned(),
        server_flags: json.get("serverFlags").filter(|v| !v.is_null()).cloned(),
        css: json.get("css").filter(|v| !v.is_null()).cloned(),
        env_prefix: json.get("envPrefix").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        env_dir: json.get("envDir").and_then(|v| v.as_str()).map(str::to_string),
        cors: json.get("cors").filter(|v| !v.is_null()).cloned(),
        allowed_hosts: json.get("allowedHosts").filter(|v| !v.is_null()).cloned(),
        preview: json.get("preview").filter(|v| !v.is_null()).cloned(),
        app_type: json.get("appType").and_then(|v| v.as_str()).map(str::to_string),
        html: json.get("html").filter(|v| !v.is_null()).cloned(),
    }
}

#[inline]
pub fn adopt_vite_config_values(
    config: &mut oj_config::OjConfig,
    root: &Path,
    command: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(v) = extract_vite_values(root, command, mode) else {
        // No vite.config is fine: nothing to adopt. A vite.config that exists but
        // failed to evaluate is not: Vite fails hard here ("failed to load config
        // from ..."), and silently carrying on would build or serve with defaults
        // the app never asked for. An explicit oj.plugins file takes precedence over
        // vite.config (the extractor skips it then), so only the vite path is an
        // error. The extractor has already printed the underlying cause to stderr.
        if let Some(named) = VITE_CONFIG_OVERRIDE.get() {
            if !named.is_file() {
                return Err(format!(
                    "failed to load config from {}: --config names a file that does not exist",
                    named.display()
                ));
            }
        }
        if plugins_file(root).is_none() {
            if let Some(path) = vite_config_file(root) {
                return Err(format!("failed to load config from {}", path.display()));
            }
        }
        return Ok(());
    };
    merge_vite_values(config, v);
    Ok(())
}

/// Like `adopt_vite_config_values`, for a `mode` that is only the command's
/// default: the config file's own `mode` (if any) is honored and lands in
/// `config.mode` so the caller can reload under it.
pub fn adopt_vite_config_values_default_mode(
    config: &mut oj_config::OjConfig,
    root: &Path,
    command: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(v) = extract_vite_values_with(root, command, mode, false) else {
        // Same rule as `adopt_vite_config_values`: a present vite.config that
        // failed to evaluate is an error, a missing one is nothing to adopt.
        if let Some(named) = VITE_CONFIG_OVERRIDE.get() {
            if !named.is_file() {
                return Err(format!(
                    "failed to load config from {}: --config names a file that does not exist",
                    named.display()
                ));
            }
        }
        if plugins_file(root).is_none() {
            if let Some(path) = vite_config_file(root) {
                return Err(format!("failed to load config from {}", path.display()));
            }
        }
        return Ok(());
    };
    merge_vite_values(config, v);
    Ok(())
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
    if v.port.is_some()
        || v.host.is_some()
        || v.headers.is_some()
        || v.fs_allow.is_some()
        || v.fs_strict.is_some()
    {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.port.is_none() {
            sc.port = v.port;
        }
        if sc.host.is_none() {
            sc.host = v.host;
        }
        if sc.fs.is_none() {
            if v.fs_allow.is_some() || v.fs_strict.is_some() {
                sc.fs = Some(oj_config::FsConfig {
                    allow: v.fs_allow,
                    strict: v.fs_strict,
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
    if let Some(vb) = v.build.as_ref().and_then(|b| b.as_object()) {
        let build = config.build.get_or_insert_with(Default::default);
        let str_of = |k: &str| vb.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let bool_of = |k: &str| vb.get(k).and_then(|v| v.as_bool());
        if build.out_dir.is_none() {
            build.out_dir = str_of("outDir");
        }
        let bool_or_str = |k: &str| match vb.get(k) {
            Some(serde_json::Value::Bool(b)) => Some(oj_config::BoolOrString::Bool(*b)),
            Some(serde_json::Value::String(s)) => Some(oj_config::BoolOrString::Str(s.clone())),
            _ => None,
        };
        if build.sourcemap.is_none() {
            build.sourcemap = bool_or_str("sourcemap");
        }
        if build.minify.is_none() {
            build.minify = bool_or_str("minify");
        }
        if build.css_code_split.is_none() {
            build.css_code_split = bool_of("cssCodeSplit");
        }
        if build.target.is_none() {
            build.target = match vb.get("target") {
                Some(serde_json::Value::String(s)) => Some(oj_config::StringOrList::One(s.clone())),
                Some(serde_json::Value::Array(a)) => Some(oj_config::StringOrList::Many(
                    a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                )),
                _ => None,
            };
        }
        if build.empty_out_dir.is_none() {
            build.empty_out_dir = bool_of("emptyOutDir");
        }
        if build.module_preload.is_none() {
            build.module_preload = vb.get("modulePreload").filter(|v| !v.is_null()).cloned();
        }
        if build.ssr.is_none() {
            build.ssr = bool_or_str("ssr");
        }
        if build.copy_public_dir.is_none() {
            build.copy_public_dir = bool_of("copyPublicDir");
        }
        if build.ssr_manifest.is_none() {
            build.ssr_manifest = bool_or_str("ssrManifest");
        }
        if build.manifest.is_none() {
            build.manifest = bool_or_str("manifest");
        }
        if build.css_minify.is_none() {
            build.css_minify = bool_or_str("cssMinify");
        }
        if build.assets_dir.is_none() {
            build.assets_dir = str_of("assetsDir");
        }
        if build.report_compressed_size.is_none() {
            build.report_compressed_size = bool_of("reportCompressedSize");
        }
        if build.chunk_size_warning_limit.is_none() {
            build.chunk_size_warning_limit = vb.get("chunkSizeWarningLimit").and_then(|v| v.as_f64());
        }
        if build.write.is_none() {
            build.write = bool_of("write");
        }
        for (key, slot) in [
            ("watch", &mut build.watch),
            ("license", &mut build.license),
            ("commonjsOptions", &mut build.commonjs_options),
        ] {
            if slot.is_none() {
                *slot = vb.get(key).filter(|v| !v.is_null()).cloned();
            }
        }
        if build.css_target.is_none() {
            build.css_target = match vb.get("cssTarget") {
                Some(serde_json::Value::String(s)) => Some(oj_config::StringOrList::One(s.clone())),
                Some(serde_json::Value::Array(a)) => Some(oj_config::StringOrList::Many(
                    a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                )),
                _ => None,
            };
        }
        if build.lib.is_none() {
            build.lib = vb
                .get("lib")
                .cloned()
                .and_then(|l| serde_json::from_value::<oj_config::LibConfig>(l).ok());
        }
    }
    if v.cors.is_some() || v.allowed_hosts.is_some() {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.cors.is_none() {
            sc.cors = v.cors.and_then(|c| serde_json::from_value(c).ok());
        }
        if sc.allowed_hosts.is_none() {
            sc.allowed_hosts = v.allowed_hosts.and_then(|a| serde_json::from_value(a).ok());
        }
    }
    if let Some(preview) = v.preview {
        if let Ok(parsed) = serde_json::from_value::<oj_config::PreviewConfig>(preview) {
            let pc = config.preview.get_or_insert_with(Default::default);
            pc.port = pc.port.or(parsed.port);
            pc.host = pc.host.take().or(parsed.host);
            pc.strict_port = pc.strict_port.or(parsed.strict_port);
            pc.open = pc.open.take().or(parsed.open);
            pc.cors = pc.cors.take().or(parsed.cors);
            pc.allowed_hosts = pc.allowed_hosts.take().or(parsed.allowed_hosts);
            pc.headers = pc.headers.take().or(parsed.headers);
            pc.proxy = pc.proxy.take().or(parsed.proxy);
        }
    }
    if config.app_type.is_none() {
        config.app_type = v.app_type;
    }
    if config.oxc.is_none() {
        config.oxc = v.oxc;
    }
    if config.html.is_none() {
        config.html = v.html.and_then(|h| serde_json::from_value(h).ok());
    }
    if config.esbuild.is_none() {
        config.esbuild = v.esbuild;
    }
    // The ssr block merges PER-KEY, not whole-block: an oj.config.json that
    // sets one ssr key (say noExternal) must not drop the extractor's other
    // keys — above all `runnerBacked`, which ONLY extraction produces and every
    // consumer of the worker path reads, so it is always adopted.
    match (config.ssr.as_mut(), v.ssr) {
        (None, vssr) => config.ssr = vssr,
        (Some(existing), Some(vssr)) => {
            if let (Some(obj), Some(vobj)) = (existing.as_object_mut(), vssr.as_object()) {
                for (k, val) in vobj {
                    if k == "runnerBacked" || !obj.contains_key(k) {
                        obj.insert(k.clone(), val.clone());
                    }
                }
            }
        }
        (Some(_), None) => {}
    }
    if config.mode.is_none() {
        config.mode = v.mode;
    }
    if let Some(vr) = v.resolve.as_ref().and_then(|r| r.as_object()) {
        let rc = config.resolve.get_or_insert_with(Default::default);
        let list = |k: &str| {
            vr.get(k).and_then(|x| x.as_array()).map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
        };
        if rc.extensions.is_none() {
            rc.extensions = list("extensions");
        }
        if rc.main_fields.is_none() {
            rc.main_fields = list("mainFields");
        }
        if rc.conditions.is_none() {
            rc.conditions = list("conditions");
        }
        if rc.external_conditions.is_none() {
            rc.external_conditions = list("externalConditions");
        }
        if rc.preserve_symlinks.is_none() {
            rc.preserve_symlinks = vr.get("preserveSymlinks").and_then(|b| b.as_bool());
        }
    }
    if config.raw_resolve.is_none() {
        config.raw_resolve = v
            .raw_resolve
            .and_then(|r| serde_json::from_value::<oj_config::ResolveConfig>(r).ok());
    }
    if let Some(sf) = v.server_flags.as_ref().and_then(|s| s.as_object()) {
        if config.app_type.is_none() {
            config.app_type = sf.get("appType").and_then(|a| a.as_str()).map(str::to_string);
        }
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.strict_port.is_none() {
            sc.strict_port = sf.get("strictPort").and_then(|b| b.as_bool());
        }
        if sc.open.is_none() {
            sc.open = sf.get("open").and_then(|b| b.as_bool());
        }
        if sc.hmr.is_none() {
            sc.hmr = sf
                .get("hmr")
                .and_then(|h| serde_json::from_value::<oj_config::HmrOptions>(h.clone()).ok())
                .map(oj_config::HmrConfig::Options);
        }
        if sc.watch.is_none() {
            sc.watch = sf
                .get("watch")
                .and_then(|w| serde_json::from_value::<oj_config::WatchConfig>(w.clone()).ok());
        }
        if let Some(strict) = sf.get("fsStrict").and_then(|b| b.as_bool()) {
            let fs = sc.fs.get_or_insert_with(Default::default);
            if fs.strict.is_none() {
                fs.strict = Some(strict);
            }
        }
        if sf.get("skipWebSocketTokenCheck").and_then(|b| b.as_bool()) == Some(true) {
            let legacy = config.legacy.get_or_insert_with(Default::default);
            if legacy.skip_web_socket_token_check.is_none() {
                legacy.skip_web_socket_token_check = Some(true);
            }
        }
    }
    if let Some(css) = v.css.as_ref() {
        if config.css.is_none() {
            // The whole block (preprocessorOptions, devSourcemap, modules).
            config.css = serde_json::from_value::<oj_config::CssConfig>(css.clone()).ok();
        } else if let Some(po) = css.get("preprocessorOptions").and_then(|p| p.as_object()) {
            let cfg = config.css.as_mut().unwrap();
            let map = cfg.preprocessor_options.get_or_insert_with(Default::default);
            for (lang, opts) in po {
                let Some(data) = opts.get("additionalData").and_then(|d| d.as_str()) else {
                    continue;
                };
                let entry = map.entry(lang.clone()).or_default();
                if entry.additional_data.is_none() {
                    entry.additional_data = Some(data.to_string());
                }
            }
        }
    }
    if config.env_prefix.is_none() {
        if let Some(p) = v.env_prefix.filter(|p| !p.is_empty()) {
            config.env_prefix = Some(oj_config::StringOrList::Many(p));
        }
    }
    if config.env_dir.is_none() {
        config.env_dir = v.env_dir;
    }
}

pub struct PluginHost {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Option<String>, String>>>>,
    counter: AtomicU64,
    ws_out: Mutex<Option<tokio::sync::broadcast::Sender<String>>>,
    /// `{ ojServer: { action, ... } }` lines from the host: a plugin invalidating
    /// a module via server.moduleGraph, or server.restart().
    server_events: Mutex<Option<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>>,
    // In an Option so it can be taken + killed explicitly (the reader task holds
    // an Arc clone, so dropping the caller's Arc alone never triggers kill_on_drop).
    child: Mutex<Option<tokio::process::Child>>,
    /// The host's `{ ojServeInfo: ... }` control push: None until the host's
    /// top-level init completes. Subscribers see the info whenever the host
    /// eventually comes up, however slow the boot, and can activate the
    /// middleware path late instead of silently degrading to the SSR runner.
    serve_info_push: tokio::sync::watch::Sender<Option<ServeInfo>>,
    /// Whether the host finished its top-level init: flipped by the serve-info
    /// push or by the first RPC reply (the host's RPC listener only registers
    /// after every top-level await, so any reply proves init completed). RPC
    /// sends are gated on this — see `call`.
    initialized: tokio::sync::watch::Sender<bool>,
    /// The host's stdout closed (the process exited): fail calls fast instead
    /// of waiting out the init deadline or the per-call timeout. A watch so a
    /// waiter (`host_gone_wait`) can select on the death instead of polling.
    host_gone: tokio::sync::watch::Sender<bool>,
    /// When the host was spawned; the init deadline is measured from here, so
    /// boot RPCs share one deadline instead of stacking a fresh one each.
    spawned: tokio::time::Instant,
    /// Per-spawn init-wait policy: how long a call may wait for the host's
    /// top-level init. The boot/serve host takes the long init deadline (boot
    /// correctness depends on its snapshot RPCs), shared across calls and
    /// measured from spawn; a lazily spawned host (the SSR environment host,
    /// spawned on the first SSR request) takes the short per-call bound,
    /// measured from EACH call's own start (see `lazy`), so a wedged init
    /// degrades like a slow hook instead of freezing the watcher thread and
    /// browser-facing SSR transforms.
    init_wait: std::time::Duration,
    /// Whether this host was lazily spawned: its init wait is then anchored to
    /// each call's own start rather than to the spawn instant — a
    /// spawn-anchored short bound gave calls arriving after `spawn +
    /// init_wait` during a still-pending init a zero-length window (instant
    /// failure), where pre-init-gate semantics gave every call its own
    /// per-call timeout.
    lazy: bool,
    /// The env knob named when the init wait elapses (matches `init_wait`).
    init_knob: &'static str,
    /// Last "still initializing" progress line, so concurrent init-gated calls
    /// print one line per interval, not one each.
    init_progress: Mutex<std::time::Instant>,
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

/// How long one plugin hook may run before oj gives up on it. Vite has no
/// hook timeout at all; oj's default of 20 s keeps a hung plugin from wedging
/// the server, and `OJ_PLUGIN_TIMEOUT=<seconds>` raises it for plugins that
/// legitimately take longer (a large first-run codegen, a cold type check).
pub fn plugin_rpc_timeout() -> std::time::Duration {
    plugin_rpc_timeout_from(std::env::var("OJ_PLUGIN_TIMEOUT").ok().as_deref())
}

fn plugin_rpc_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(20);
    std::time::Duration::from_secs(secs)
}

/// How long the plugin host may take to finish its top-level init (loading the
/// config, config/configResolved/configureServer, a Miniflare boot) before an
/// RPC waiting on it gives up. The host answers RPCs only after init, so this
/// gates `call` instead of racing the per-call timeout against a slow boot;
/// Vite has no bound at all here (its startup simply awaits the hooks).
/// `OJ_PLUGIN_INIT_TIMEOUT=<seconds>` adjusts it.
pub fn plugin_init_timeout() -> std::time::Duration {
    plugin_init_timeout_from(std::env::var("OJ_PLUGIN_INIT_TIMEOUT").ok().as_deref())
}

fn plugin_init_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300);
    std::time::Duration::from_secs(secs)
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginHost")
    }
}

/// The init deadline one `call` waits out while the host is uninitialized: a
/// boot host shares the spawn-anchored deadline (`spawned + init_wait`), a
/// lazy host anchors `init_wait` to the CALL's own start so a call arriving
/// long after spawn still gets a full window (init landing releases it early).
fn call_init_deadline(
    lazy: bool,
    spawned: tokio::time::Instant,
    init_wait: std::time::Duration,
    now: tokio::time::Instant,
) -> tokio::time::Instant {
    if lazy {
        now + init_wait
    } else {
        spawned + init_wait
    }
}

/// The init-wait policy per spawn kind (see `PluginHost::init_wait`): a boot
/// host gets the long init deadline, a lazily spawned host the short per-call
/// bound — each named after the env knob that adjusts it.
fn init_wait_policy(lazy: bool) -> (std::time::Duration, &'static str) {
    if lazy {
        (plugin_rpc_timeout(), "OJ_PLUGIN_TIMEOUT")
    } else {
        (plugin_init_timeout(), "OJ_PLUGIN_INIT_TIMEOUT")
    }
}

impl PluginHost {
    /// Spawn a boot-time host: calls wait out the full init deadline
    /// (`OJ_PLUGIN_INIT_TIMEOUT`), because boot correctness depends on its
    /// snapshot RPCs (config defines, hook gates, serve info).
    pub async fn spawn(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(root, plugins_file, config_json, false).await
    }

    /// Spawn a lazily created host (the SSR environment host, created on the
    /// first SSR request): calls bound their init wait by the ordinary per-call
    /// timeout (`OJ_PLUGIN_TIMEOUT`), so a wedged init cannot freeze the single
    /// watcher thread or browser-facing SSR transforms for the long deadline.
    pub async fn spawn_lazy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(root, plugins_file, config_json, true).await
    }

    async fn spawn_with_policy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        lazy: bool,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        let script = oj_cache::cache_root(root).join("plugin-host.mjs");
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, PLUGIN_HOST_JS)?;

        // The host shares its stdout with plugin code (no console redirection),
        // so every oj protocol line is framed with a per-session random token
        // only this spawn and the host know: the reader below ignores unframed
        // lines, so a plugin's print — or attacker-controlled content a plugin
        // echoes — can never be parsed as a reply or a control push.
        let control_token = format!("oj{}:", crate::new_ws_token());
        let mut child = tokio::process::Command::new("node")
            .arg(&script)
            .arg(plugins_file)
            .arg(config_json)
            .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
            .env("OJ_CONTROL_TOKEN", &control_token)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot spawn node for plugin host: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let (init_wait, init_knob) = init_wait_policy(lazy);
        let host = std::sync::Arc::new(PluginHost {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            ws_out: Mutex::new(None),
            server_events: Mutex::new(None),
            child: Mutex::new(Some(child)),
            serve_info_push: tokio::sync::watch::channel(None).0,
            initialized: tokio::sync::watch::channel(false).0,
            host_gone: tokio::sync::watch::channel(false).0,
            spawned: tokio::time::Instant::now(),
            init_wait,
            lazy,
            init_knob,
            init_progress: Mutex::new(std::time::Instant::now()),
        });

        let resolver = std::sync::Arc::new(OjResolver::new(root));
        let root_buf: PathBuf = root.to_path_buf();
        let reader_ref = std::sync::Arc::clone(&host);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Only token-framed lines are protocol (see spawn); anything
                // else on this stream is a plugin's own print.
                let Some(line) = line.strip_prefix(control_token.as_str()) else {
                    continue;
                };
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if let Some(rpc) = msg["rpc"].as_u64() {
                    let method = msg["method"].as_str().unwrap_or("").to_string();
                    let args = msg["args"].as_array().cloned().unwrap_or_default();
                    handle_ctx_rpc(rpc, &method, &args, &resolver, &root_buf, &reader_ref.stdin)
                        .await;
                    continue;
                }
                if let Some(info) = msg.get("ojServeInfo") {
                    reader_ref
                        .serve_info_push
                        .send_replace(Some(ServeInfo::from_json(info)));
                    let _ = reader_ref.initialized.send_replace(true);
                    // ACK so the host stops re-pushing (it re-sends until
                    // acknowledged, healing a copy a plugin's unterminated
                    // partial write may have spliced).
                    let mut stdin = reader_ref.stdin.lock().await;
                    let _ = stdin.write_all(b"{\"ojServeInfoAck\":true}\n").await;
                    continue;
                }
                if msg.get("ojInit").is_some() {
                    // The host's unconditional init-complete signal, sent in
                    // BOTH modes: build mode has no ojServeInfo push, so
                    // without this the gate would only release on the first
                    // reply — a hanging first hook would wait out the whole
                    // init deadline blamed on initialization.
                    let _ = reader_ref.initialized.send_replace(true);
                    continue;
                }
                if let Some(ev) = msg.get("ojServer") {
                    let tx = reader_ref.server_events.lock().unwrap().clone();
                    if let Some(tx) = tx {
                        let _ = tx.send(ev.clone());
                    }
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
                // Any reply proves the host's top-level init completed: the RPC
                // listener only registers after every top-level await.
                let _ = reader_ref.initialized.send_replace(true);
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
            // stdout closed: the host exited. Fail everything pending now, and
            // every future call fast, instead of letting an init-gated call
            // wait out the whole init deadline on a dead process.
            let _ = reader_ref.host_gone.send_replace(true);
            let drained: Vec<_> = reader_ref
                .pending
                .lock()
                .unwrap()
                .drain()
                .map(|(_, tx)| tx)
                .collect();
            for tx in drained {
                let _ = tx.send(Err("plugin host exited".into()));
            }
        });
        Ok(host)
    }

    async fn call(&self, hook: &str, args: &[&str]) -> Result<Option<String>, String> {
        if *self.host_gone.borrow() {
            return Err("plugin host exited".into());
        }
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
        let mut rx = rx;
        // The host answers RPCs only after its top-level init completes (the
        // listener registers after every top-level await), so a call during a
        // slow boot must wait for init — bounded by this spawn's init-wait
        // policy (the long deadline on a boot host, the short per-call bound
        // on a lazy one), shared across calls and measured from spawn —
        // instead of racing its own per-call timeout against the boot and
        // permanently snapshotting wrong defaults. Fast boots are untouched:
        // initialized flips with the serve-info push, the ojInit signal, or
        // the first reply, all preceding any wait here.
        let mut init_rx = self.initialized.subscribe();
        if !*init_rx.borrow_and_update() {
            // A boot host's deadline is shared and spawn-anchored (boot RPCs
            // ride one deadline instead of stacking). A lazy host's is
            // per-call: each call gets its own full `init_wait` window from
            // its own start — spawn-anchoring the short bound made every call
            // arriving after `spawn + init_wait` (with init still pending)
            // fail instantly instead of degrading like a slow hook.
            let deadline = call_init_deadline(
                self.lazy,
                self.spawned,
                self.init_wait,
                tokio::time::Instant::now(),
            );
            let mut progress = tokio::time::interval_at(
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(30),
            );
            loop {
                tokio::select! {
                    // Deterministic when arms are simultaneously ready: a reply
                    // (or the init flip) racing an elapsed deadline must win.
                    biased;
                    res = &mut rx => {
                        // The reply itself proves init completed.
                        return match res {
                            Ok(result) => result,
                            Err(_) => Err("plugin host exited".into()),
                        };
                    }
                    changed = init_rx.changed() => {
                        if changed.is_err() || *init_rx.borrow() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        self.pending.lock().unwrap().remove(&req_id);
                        return Err(format!(
                            "plugin host still initializing after {}s running {hook} (raise {} for slower boots)",
                            self.init_wait.as_secs(),
                            self.init_knob,
                        ));
                    }
                    _ = progress.tick() => {
                        // One line per interval across concurrent waiters.
                        let elapsed = self.spawned.elapsed().as_secs();
                        let mut last = self
                            .init_progress
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if last.elapsed().as_secs() >= 29 {
                            *last = std::time::Instant::now();
                            eprintln!("oj: plugin host still initializing ({elapsed}s)…");
                        }
                    }
                }
            }
        }
        // Initialized: the ordinary per-call timeout applies unchanged.
        match tokio::time::timeout(plugin_rpc_timeout(), rx).await {
            Ok(Ok(result)) => result,
            _ => Err(format!(
                "plugin host timed out after {}s running {hook} (raise OJ_PLUGIN_TIMEOUT for slow plugins)",
                plugin_rpc_timeout().as_secs()
            )),
        }
    }

    /// Whether the host finished its top-level init (the serve-info push, or
    /// any RPC reply, whichever came first).
    pub fn is_initialized(&self) -> bool {
        *self.initialized.borrow()
    }

    /// The shared init deadline, measured from the host's spawn (this host's
    /// init-wait policy, so a lazily spawned host reports its short bound).
    /// A caller gating separate work on the host's initialization (the Start
    /// prewarm waiting for serve info) anchors to THIS deadline instead of
    /// starting a fresh full period of its own.
    pub fn init_deadline_at(&self) -> tokio::time::Instant {
        self.spawned + self.init_wait
    }

    /// Resolves when the host process has exited (its stdout closed). Lets a
    /// task holding an `Arc<PluginHost>` — which keeps every channel sender
    /// alive, so `changed().is_err()` can never observe the death — wait on
    /// the host dying instead of pinning it forever.
    pub(crate) async fn host_gone_wait(&self) {
        let mut rx = self.host_gone.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
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

    /// `ctx_json` is Vite's IndexHtmlTransformContext for the page (`path`,
    /// `filename`, and `originalUrl` in dev or `bundle` / `chunk` in a build);
    /// the host adds the dev server. A throwing hook is an `Err`, as in Vite,
    /// where it fails the request or the build.
    #[inline]
    pub async fn transform_index_html(&self, html: &str, ctx_json: &str) -> Result<String, String> {
        Ok(self
            .call("transformIndexHtml", &[html, ctx_json])
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

    /// `buildEnd(error?)`: Rollup passes the error that failed the build, so
    /// plugins see a failed build too (`None` for a successful one).
    #[inline]
    pub async fn build_end(&self, error: Option<&str>) -> Result<(), String> {
        match error {
            Some(e) => self.call("buildEnd", &[e]).await.map(|_| ()),
            None => self.call("buildEnd", &[]).await.map(|_| ()),
        }
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

    /// How the host serves requests: the loopback port of its configureServer
    /// middleware stack (when any plugin registered one), and whether it built
    /// real runner-backed Vite DevEnvironments (documents are then served by
    /// the plugin middleware, not the Node SSR runner). The host pushes this
    /// the moment its init completes, and RPCs are init-gated (see `call`), so
    /// a value that already arrived is returned without a round trip and the
    /// push is preferred at any point; only a host that blew the init deadline
    /// yields the default — the caller can then watch `serve_info_updates` for
    /// the late push instead of degrading silently.
    pub async fn serve_info(&self) -> ServeInfo {
        if let Some(info) = *self.serve_info_push.borrow() {
            return info;
        }
        let rpc = self.call("getServeInfo", &[]).await;
        // The push may have landed while the RPC ran (or failed); it is the
        // definitive value.
        if let Some(info) = *self.serve_info_push.borrow() {
            return info;
        }
        let Some(v) = rpc
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return ServeInfo::default();
        };
        ServeInfo::from_json(&v)
    }

    /// Subscribe to the host's `{ ojServeInfo }` push: `None` until the host's
    /// top-level init completes, then the definitive `ServeInfo` — however slow
    /// the boot. Lets the caller activate the plugin-middleware path late when
    /// the boot-time `serve_info` timed out.
    pub fn serve_info_updates(&self) -> tokio::sync::watch::Receiver<Option<ServeInfo>> {
        self.serve_info_push.subscribe()
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
    /// `define` entries the plugins' `config()` hooks contributed, as
    /// `(key, js expression)` pairs (a string value is the expression itself,
    /// anything else its JSON), so they reach oj's compile the way Vite's merged
    /// `config.define` does.
    pub async fn config_defines(&self) -> Vec<(String, String)> {
        let Ok(Some(raw)) = self.call("getPluginConfig", &[]).await else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return Vec::new();
        };
        v.get("define")
            .and_then(|d| d.as_object())
            .map(|d| {
                d.iter()
                    .map(|(k, v)| {
                        let expr = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        (k.clone(), expr)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

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

    /// The `filter.id` include patterns of every object-form `load` hook, as regex
    /// source strings. A dependency module is offered to plugin `load` only when
    /// its path matches one, so deps cost no RPC unless a plugin asked for them.
    pub async fn dep_load_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getDepLoadFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// The `filter.id` include patterns of every object-form `resolveId` hook, as
    /// regex source strings. A relative or absolute import matching one is offered
    /// to the plugins' resolveId before oj's own resolver (Vite runs plugin
    /// resolveId first for every id; oj gates the non-bare ones on a declared
    /// filter so unfiltered plugins cost no RPC per import).
    pub async fn resolve_id_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getResolveIdFilters", &[]).await else {
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

    pub fn set_server_events_sender(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) {
        *self.server_events.lock().unwrap() = Some(tx);
    }

    pub fn set_ws_sender(&self, tx: tokio::sync::broadcast::Sender<String>) {
        *self.ws_out.lock().unwrap() = Some(tx);
    }

    #[inline]
    pub async fn ws_message(&self, event: &str, data: &str) -> Result<(), String> {
        self.call("wsMessage", &[event, data]).await.map(|_| ())
    }

    /// An HMR client connected: the host fires `server.ws.on("connection")`
    /// listeners (Vite's ws server emits one per accepted socket).
    #[inline]
    pub async fn ws_connection(&self) -> Result<(), String> {
        self.call("wsConnection", &[]).await.map(|_| ())
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
mod extraction_failure_tests {
    use super::extraction_failure;

    // A status is only constructible by running something, and `true`/`false`
    // are the two the classification cares about.
    fn status(ok: bool) -> std::process::ExitStatus {
        std::process::Command::new(if ok { "true" } else { "false" })
            .status()
            .expect("a shell builtin binary")
    }

    #[test]
    fn valid_json_is_not_a_failure() {
        assert_eq!(extraction_failure(status(true), b"{}", None), None);
    }

    // The one that hid a real bug: the extractor skipped its own body and
    // exited 0, which is indistinguishable from a config with nothing in it
    // unless somebody says so.
    #[test]
    fn a_silent_successful_run_is_a_failure() {
        let why = extraction_failure(status(true), b"", Some("EOF while parsing a value"))
            .expect("nothing on stdout is not a config");
        assert!(why.contains("wrote nothing at all"), "{why}");
    }

    #[test]
    fn unparseable_output_reports_its_size_and_the_parse_error() {
        let why = extraction_failure(status(false), b"not json", Some("expected value"))
            .expect("output that is not JSON is not a config");
        assert!(why.contains("8 bytes"), "{why}");
        assert!(why.contains("expected value"), "{why}");
    }
}

#[cfg(test)]
mod vite_values_tests {
    use super::*;

    #[test]
    fn finds_commonjs_vite_config_formats() {
        for extension in ["cjs", "cts"] {
            let root = std::env::temp_dir().join(format!(
                "oj-config-format-{}-{extension}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join(format!("vite.config.{extension}"));
            std::fs::write(&path, "module.exports = {};").unwrap();
            assert_eq!(vite_config_file(&root), Some(path));
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn plugin_rpc_timeout_defaults_and_reads_env_seconds() {
        assert_eq!(plugin_rpc_timeout_from(None).as_secs(), 20);
        assert_eq!(plugin_rpc_timeout_from(Some("90")).as_secs(), 90);
        assert_eq!(plugin_rpc_timeout_from(Some(" 5 ")).as_secs(), 5);
        // Garbage and zero fall back to the default rather than disabling the guard.
        assert_eq!(plugin_rpc_timeout_from(Some("soon")).as_secs(), 20);
        assert_eq!(plugin_rpc_timeout_from(Some("0")).as_secs(), 20);
    }

    #[test]
    fn plugin_init_timeout_defaults_and_reads_env_seconds() {
        assert_eq!(plugin_init_timeout_from(None).as_secs(), 300);
        assert_eq!(plugin_init_timeout_from(Some("2")).as_secs(), 2);
        assert_eq!(plugin_init_timeout_from(Some("soon")).as_secs(), 300);
        assert_eq!(plugin_init_timeout_from(Some("0")).as_secs(), 300);
    }

    #[test]
    fn extraction_timeout_defaults_and_reads_env_seconds() {
        assert_eq!(extraction_timeout_from(None).as_secs(), 60);
        assert_eq!(extraction_timeout_from(Some("120")).as_secs(), 120);
        assert_eq!(extraction_timeout_from(Some("junk")).as_secs(), 60);
        assert_eq!(extraction_timeout_from(Some("0")).as_secs(), 60);
    }

    // The extraction subprocess wait is bounded: a config hook that keeps the
    // event loop alive past the deadline gets the child killed instead of
    // wedging boot forever; a child that finishes yields its full output.
    #[test]
    fn bounded_output_kills_past_the_deadline_and_collects_output_before_it() {
        let mut quick = std::process::Command::new("node");
        quick.arg("-e").arg("process.stdout.write('done')");
        let out = match bounded_output(&mut quick, std::time::Duration::from_secs(30)) {
            Ok(out) => out,
            // No node on this machine: nothing to test (extraction itself
            // cannot run either).
            Err(_) => return,
        };
        let out = out.expect("a finishing child is not a timeout");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"done");

        let mut hung = std::process::Command::new("node");
        hung.arg("-e").arg("setInterval(() => {}, 1000)");
        let started = std::time::Instant::now();
        let out = bounded_output(&mut hung, std::time::Duration::from_millis(300)).unwrap();
        assert!(out.is_none(), "a child past the deadline is killed and reported as a timeout");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the wait must end at the deadline, not at the child's leisure"
        );
    }

    // An inherited-stdio grandchild keeps the pipe write-ends open past the
    // child's own exit (and past a kill): the drain threads then never see
    // EOF, and joining them unboundedly wedged boot forever — the exact hole
    // the extraction timeout exists to close. The wait must end within the
    // timeout plus the short grace, with whatever output was captured.
    #[test]
    fn bounded_output_detaches_from_pipes_a_grandchild_holds_open() {
        // The child prints, spawns a long-lived grandchild with stdio:
        // "inherit", and exits immediately: its status is available at once,
        // but pipe EOF is 600 s away.
        let mut cmd = std::process::Command::new("node");
        cmd.arg("-e").arg(
            "process.stdout.write('partial');\
             require('child_process').spawn('sleep', ['600'], { stdio: 'inherit', detached: true }).unref();",
        );
        let started = std::time::Instant::now();
        let out = match bounded_output(&mut cmd, std::time::Duration::from_secs(10)) {
            Ok(out) => out,
            Err(_) => return, // no node on this machine
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "the exited child's output must be returned within the grace, not at the grandchild's EOF ({}s)",
            started.elapsed().as_secs()
        );
        let out = out.expect("the child exited before the deadline: not a timeout");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"partial", "output written before the exit is captured");

        // The kill path: a HANGING child whose grandchild also holds the
        // pipes must still come back as a timeout within timeout + grace.
        let mut hung = std::process::Command::new("node");
        hung.arg("-e").arg(
            "require('child_process').spawn('sleep', ['600'], { stdio: 'inherit', detached: true }).unref();\
             setInterval(() => {}, 1000);",
        );
        let started = std::time::Instant::now();
        let out = bounded_output(&mut hung, std::time::Duration::from_millis(300)).unwrap();
        assert!(out.is_none(), "a killed child is a timeout even with its pipes held open");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "the kill path must not block on the grandchild's EOF either"
        );
    }

    // The per-spawn init-wait policy: a boot host waits out the long init
    // deadline (boot correctness depends on its snapshot RPCs), a lazily
    // spawned host (the SSR environment host) only the short per-call bound,
    // so a wedged init cannot freeze the watcher thread for the long deadline.
    #[test]
    fn init_wait_policy_is_long_for_boot_hosts_and_short_for_lazy_ones() {
        let (boot_wait, boot_knob) = init_wait_policy(false);
        assert_eq!(boot_wait, plugin_init_timeout());
        assert_eq!(boot_knob, "OJ_PLUGIN_INIT_TIMEOUT");
        let (lazy_wait, lazy_knob) = init_wait_policy(true);
        assert_eq!(lazy_wait, plugin_rpc_timeout());
        assert_eq!(lazy_knob, "OJ_PLUGIN_TIMEOUT");
    }

    // A lazy host's init gate is per-call: a call arriving AFTER spawn +
    // init_wait (init still pending) gets its own full window from its own
    // start, never the spawn-anchored deadline's zero-length remainder. A boot
    // host keeps the shared spawn-anchored deadline.
    #[test]
    fn lazy_call_past_the_spawn_deadline_gets_its_own_init_window() {
        let wait = std::time::Duration::from_secs(20);
        let spawned = tokio::time::Instant::now();
        // A call 40 s after spawn, with the 20 s window long since elapsed.
        let now = spawned + std::time::Duration::from_secs(40);
        let lazy = call_init_deadline(true, spawned, wait, now);
        assert_eq!(lazy, now + wait, "the lazy window anchors to the call's own start");
        let boot = call_init_deadline(false, spawned, wait, now);
        assert_eq!(boot, spawned + wait, "the boot deadline stays shared and spawn-anchored");
        assert!(boot <= now, "sanity: the boot deadline has elapsed for this call");
    }

    // An oj.config.json that sets one ssr key (noExternal) must not drop the
    // extractor's verdict: the ssr block merges per-key, and `runnerBacked` —
    // which only extraction produces — is always adopted.
    #[test]
    fn merge_fills_ssr_per_key_and_always_adopts_runner_backed() {
        let mut config = oj_config::OjConfig::default();
        config.ssr = Some(serde_json::json!({ "noExternal": true }));
        let v = ViteValues {
            ssr: Some(serde_json::json!({
                "noExternal": ["from-vite"],
                "target": "webworker",
                "runnerBacked": true,
                "resolve": { "conditions": ["workerd"] }
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let ssr = config.ssr.as_ref().unwrap();
        assert_eq!(ssr["noExternal"], serde_json::json!(true), "the oj config's key wins");
        assert_eq!(ssr["target"], "webworker", "extractor keys fill where oj lacks them");
        assert_eq!(ssr["resolve"]["conditions"][0], "workerd");
        assert!(oj_config::ssr_runner_backed(&config), "the verdict survives an oj-side ssr key");

        // runnerBacked is always the extractor's, even against a (stale)
        // oj-side value: only extraction produces it.
        let mut config = oj_config::OjConfig::default();
        config.ssr = Some(serde_json::json!({ "runnerBacked": false }));
        let v = ViteValues {
            ssr: Some(serde_json::json!({ "runnerBacked": true })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert!(oj_config::ssr_runner_backed(&config));
    }

    #[test]
    fn extraction_env_hash_tracks_vite_vars_and_node_env_only() {
        let base = || {
            vec![
                ("PATH".to_string(), "/bin".to_string()),
                ("VITE_API".to_string(), "a".to_string()),
                ("NODE_ENV".to_string(), "development".to_string()),
            ]
        };
        let h0 = extraction_env_hash(base().into_iter());
        // Order-independent.
        let mut rev = base();
        rev.reverse();
        assert_eq!(h0, extraction_env_hash(rev.into_iter()));
        // Unrelated variables do not churn the key.
        let mut plus = base();
        plus.push(("TERM".to_string(), "xterm".to_string()));
        assert_eq!(h0, extraction_env_hash(plus.into_iter()));
        // A VITE_* or NODE_ENV change does.
        let mut vite = base();
        vite[1].1 = "b".to_string();
        assert_ne!(h0, extraction_env_hash(vite.into_iter()));
        let mut node = base();
        node[2].1 = "production".to_string();
        assert_ne!(h0, extraction_env_hash(node.into_iter()));
    }

    // Vite (constants.ts DEFAULT_CONFIG_FILES): js, mjs, ts, cjs, mts, cts; with
    // both a .ts and a .js present, Vite loads the .js.
    #[test]
    fn config_discovery_precedence_matches_vite() {
        let root = std::env::temp_dir().join(format!("oj-config-precedence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let order = ["js", "mjs", "ts", "cjs", "mts", "cts"];
        for ext in order.iter().rev() {
            std::fs::write(root.join(format!("vite.config.{ext}")), "export default {};").unwrap();
        }
        for ext in order {
            assert_eq!(
                vite_config_file(&root),
                Some(root.join(format!("vite.config.{ext}"))),
                "with every later format present, .{ext} wins"
            );
            std::fs::remove_file(root.join(format!("vite.config.{ext}"))).unwrap();
        }
        assert_eq!(vite_config_file(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

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
        assert_eq!(v.public_dir, Some("/abs/shared/public".into()));
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
            fs_strict: None,
            define: None,
            alias: None,
            headers: None,
            rollup_options: None,
            assets_inline_limit: None,
            proxy: None,
            dedupe: None,
            optimize_deps: None,
            build: None,
            oxc: None,
            esbuild: None,
            ssr: None,
            mode: None,
            resolve: None,
            raw_resolve: None,
            server_flags: None,
            css: None,
            env_prefix: None,
            env_dir: None,
            cors: None,
            allowed_hosts: None,
            preview: None,
            app_type: None,
            html: None,
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.base.as_deref(), Some("/vite-base/"));
        assert_eq!(config.public_dir, Some("shared/public".into()));
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
            fs_strict: None,
            define: None,
            alias: None,
            headers: None,
            rollup_options: None,
            assets_inline_limit: None,
            proxy: None,
            dedupe: None,
            optimize_deps: None,
            build: None,
            oxc: None,
            esbuild: None,
            ssr: None,
            mode: None,
            resolve: None,
            raw_resolve: None,
            server_flags: None,
            css: None,
            env_prefix: None,
            env_dir: None,
            cors: None,
            allowed_hosts: None,
            preview: None,
            app_type: None,
            html: None,
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.base.as_deref(), Some("/oj-base/"));
        assert_eq!(config.public_dir, Some("my-public".into()));
    }

    #[test]
    fn merge_adopts_server_fs_strict() {
        // `server.fs.strict: false` in a vite config reaches oj's FsConfig (Vite
        // skips the allow check entirely when strict is off) even with no allow list.
        let v = parse_vite_values(&serde_json::json!({ "fsStrict": false }));
        assert_eq!(v.fs_strict, Some(false));
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(&mut config, v);
        let fs = config.server.unwrap().fs.unwrap();
        assert_eq!(fs.strict, Some(false));
        assert!(fs.allow.is_none());

        // Alongside an allow list both land; an oj-side fs config still wins.
        let v = parse_vite_values(&serde_json::json!({ "fsStrict": true, "fsAllow": ["../shared"] }));
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(&mut config, v);
        let fs = config.server.unwrap().fs.unwrap();
        assert_eq!(fs.strict, Some(true));
        assert_eq!(fs.allow.as_deref(), Some(&["../shared".to_string()][..]));
        let absent = parse_vite_values(&serde_json::json!({}));
        assert_eq!(absent.fs_strict, None);
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

    #[test]
    fn parse_reads_build_block() {
        let v = parse_vite_values(&serde_json::json!({
            "build": { "outDir": "out", "sourcemap": true, "minify": false,
                       "cssCodeSplit": false, "target": "es2020", "ssr": "src/entry-server.ts" }
        }));
        let b = v.build.unwrap();
        assert_eq!(b["outDir"], "out");
        assert_eq!(b["sourcemap"], true);
        assert_eq!(b["ssr"], "src/entry-server.ts");
        assert!(parse_vite_values(&serde_json::json!({ "build": null })).build.is_none());
    }

    #[test]
    fn merge_adopts_build_fields_only_when_unset() {
        let mut config = oj_config::OjConfig::default();
        config.build = Some(oj_config::BuildConfig {
            out_dir: Some("oj-out".into()),
            ..Default::default()
        });
        let v = ViteValues {
            build: Some(serde_json::json!({
                "outDir": "vite-out", "sourcemap": true, "minify": false,
                "cssCodeSplit": false, "target": "es2020", "ssr": "src/server.ts"
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let b = config.build.unwrap();
        assert_eq!(b.out_dir.as_deref(), Some("oj-out"), "oj.config wins");
        assert_eq!(b.sourcemap, Some(oj_config::BoolOrString::Bool(true)));
        assert_eq!(b.minify, Some(oj_config::BoolOrString::Bool(false)));
        assert_eq!(b.css_code_split, Some(false));
        assert_eq!(b.target.as_ref().map(|t| t.to_vec()), Some(vec!["es2020".to_string()]));
        assert_eq!(b.ssr, Some(oj_config::BoolOrString::Str("src/server.ts".into())));
    }

    #[test]
    fn merge_adopts_ssr_block_and_ssr_manifest() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            build: Some(serde_json::json!({ "ssr": true, "ssrManifest": true })),
            ssr: Some(serde_json::json!({ "noExternal": ["ui-kit"], "target": "webworker" })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::ssr_manifest_name(&config).as_deref(), Some(".vite/ssr-manifest.json"));
        let e = oj_config::ssr_externals(&config);
        assert!(e.webworker() && !e.is_external_pkg("ui-kit"));
    }

    #[test]
    fn merge_adopts_vite_string_variants_and_empty_out_dir() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            build: Some(serde_json::json!({
                "sourcemap": "hidden", "minify": "terser", "target": ["es2020", "safari14"],
                "emptyOutDir": false
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::build_sourcemap(&config), oj_config::Sourcemap::Hidden);
        assert!(oj_config::build_minify(&config));
        assert_eq!(oj_config::build_targets(&config), vec!["es2020", "safari14"]);
        assert_eq!(config.build.unwrap().empty_out_dir, Some(false));
    }

    #[test]
    fn merge_ignores_build_values_of_the_wrong_shape() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            build: Some(serde_json::json!({ "outDir": 3, "sourcemap": 7, "target": {"x": 1} })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let b = config.build.unwrap();
        assert!(b.out_dir.is_none());
        assert!(b.sourcemap.is_none());
        assert!(b.target.is_none());
    }

    #[test]
    fn merge_adopts_jsx_blocks_when_unset() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            oxc: Some(serde_json::json!({ "jsx": { "importSource": "@emotion/react" } })),
            esbuild: Some(serde_json::json!({ "jsxFactory": "h" })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let s = oj_config::jsx_settings(&config);
        assert_eq!(s.import_source.as_deref(), Some("@emotion/react"));
        assert_eq!(s.pragma.as_deref(), Some("h"));

        let mut config = oj_config::OjConfig::default();
        config.oxc = Some(serde_json::json!({ "jsx": { "importSource": "preact" } }));
        let v = ViteValues {
            oxc: Some(serde_json::json!({ "jsx": { "importSource": "@emotion/react" } })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::jsx_settings(&config).import_source.as_deref(), Some("preact"), "oj.config wins");
    }

    #[test]
    fn merge_adopts_ssr_block_when_unset() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            ssr: Some(serde_json::json!({ "noExternal": ["lodash-es", { "regex": "^@acme/" }], "external": ["sharp"] })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let r = oj_config::ssr_externals(&config);
        assert!(r.is_no_external("lodash-es"));
        assert!(r.is_no_external("@acme/ui"));
        assert_eq!(r.is_external("sharp", true), Some(true));
    }

    #[test]
    fn merge_adopts_resolve_server_css_env_and_mode() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            mode: Some("staging".into()),
            resolve: Some(serde_json::json!({
                "extensions": [".ts", ".js"], "mainFields": ["module"],
                "conditions": ["custom"], "externalConditions": ["custom-ext"],
                "preserveSymlinks": true
            })),
            server_flags: Some(serde_json::json!({ "strictPort": true, "open": true })),
            css: Some(serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "@use 'x';" } } })),
            env_prefix: Some(vec!["VITE_".into(), "APP_".into()]),
            env_dir: Some("env".into()),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.mode.as_deref(), Some("staging"));
        let rc = config.resolve.as_ref().unwrap();
        assert_eq!(rc.extensions.as_deref(), Some(&[".ts".to_string(), ".js".to_string()][..]));
        assert_eq!(rc.main_fields.as_deref(), Some(&["module".to_string()][..]));
        assert_eq!(rc.conditions.as_deref(), Some(&["custom".to_string()][..]));
        assert_eq!(rc.external_conditions.as_deref(), Some(&["custom-ext".to_string()][..]));
        assert_eq!(rc.preserve_symlinks, Some(true));
        let sc = config.server.as_ref().unwrap();
        assert_eq!(sc.strict_port, Some(true));
        assert_eq!(sc.open, Some(true));
        let scss = &config.css.as_ref().unwrap().preprocessor_options.as_ref().unwrap()["scss"];
        assert_eq!(scss.additional_data.as_deref(), Some("@use 'x';"));
        assert_eq!(oj_config::env_prefixes(&config), vec!["VITE_".to_string(), "APP_".to_string()]);
        assert_eq!(config.env_dir.as_deref(), Some("env"));

        // oj.config values win.
        let mut config = oj_config::OjConfig::default();
        config.mode = Some("qa".into());
        config.env_dir = Some("cfg".into());
        merge_vite_values(&mut config, ViteValues { mode: Some("staging".into()), env_dir: Some("env".into()), ..Default::default() });
        assert_eq!(config.mode.as_deref(), Some("qa"));
        assert_eq!(config.env_dir.as_deref(), Some("cfg"));
    }

    #[test]
    fn merge_adopts_cors_and_allowed_hosts() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            cors: Some(serde_json::json!({ "origin": ["http://a.test"], "credentials": true })),
            allowed_hosts: Some(serde_json::json!([".corp.example"])),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let sc = config.server.unwrap();
        assert!(matches!(sc.cors, Some(oj_config::CorsConfig::Options(ref o)) if o.credentials == Some(true)));
        assert!(matches!(sc.allowed_hosts, Some(oj_config::AllowedHosts::List(ref l)) if l == &vec![".corp.example".to_string()]));
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(&mut config, ViteValues { cors: Some(serde_json::json!(false)), allowed_hosts: Some(serde_json::json!(true)), ..Default::default() });
        let sc = config.server.unwrap();
        assert!(matches!(sc.cors, Some(oj_config::CorsConfig::Toggle(false))));
        assert!(matches!(sc.allowed_hosts, Some(oj_config::AllowedHosts::All(true))));
    }

    #[test]
    fn merge_adopts_css_preprocessor_options() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            css: Some(serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "$b: red;", "loadPaths": ["styles"] } } })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::css_additional_data(&config, "scss").as_deref(), Some("$b: red;"));
        assert_eq!(oj_config::css_load_paths(&config, "scss"), vec!["styles".to_string()]);
    }

    #[test]
    fn extraction_stderr_lines_print_once_per_process() {
        let first = unseen_extraction_lines("oj: vite.config: worker config is not applied\nsome plugin notice\n");
        assert_eq!(first, "oj: vite.config: worker config is not applied\nsome plugin notice\n");
        let again = unseen_extraction_lines("oj: vite.config: worker config is not applied\nsome plugin notice\nnew line\n");
        assert_eq!(again, "new line\n", "only lines not printed before in this process come back");
        assert_eq!(unseen_extraction_lines(""), "");
    }
}
