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
    let script = cache.join("oj-vite-extract.mjs");
    std::fs::write(&script, VITE_EXTRACT_JS).ok()?;
    let out = std::process::Command::new("node")
        .arg(&script)
        .arg(&vite)
        .arg(root)
        .arg(command)
        .arg(mode)
        .arg(if mode_explicit { "explicit" } else { "default" })
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
        .current_dir(root)
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    print_extraction_stderr(&stderr);
    let parsed = serde_json::from_slice::<serde_json::Value>(&out.stdout);
    let parse_err = parsed.as_ref().err().map(|e| e.to_string());
    if let Some(why) = extraction_failure(out.status, &out.stdout, parse_err.as_deref()) {
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
        &String::from_utf8_lossy(&out.stdout),
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
    if config.ssr.is_none() {
        config.ssr = v.ssr;
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
            server_events: Mutex::new(None),
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
        match tokio::time::timeout(plugin_rpc_timeout(), rx).await {
            Ok(Ok(result)) => result,
            _ => Err(format!(
                "plugin host timed out after {}s running {hook} (raise OJ_PLUGIN_TIMEOUT for slow plugins)",
                plugin_rpc_timeout().as_secs()
            )),
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
